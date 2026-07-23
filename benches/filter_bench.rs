//! `kind:`/`ext:` filter latency for the search scan (Option A from
//! docs/design-search-chips.md — the predicate runs inside the index
//! scan). Same synthetic 200k index as `search_bench`. Guards two things:
//!
//! - `plain_report` vs `nofilter_report`: a query with no filters must
//!   cost the same as today's `search` (Option A branches straight to it),
//!   so wiring filters in never taxes plain search-as-you-type;
//! - `text_filter` / `filter_only`: the added cost when a filter *is*
//!   present, including the filter-only full-arena scan.
//!
//! (Post-filtering the results instead — "Option B" — was measured here
//! too and rejected: it silently under-returns past the result limit and
//! can't serve a filter-only query at all. See the design doc.)

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;

use filex::index::{ROOT, VolumeIndex};
use filex::listing::FileKind;
use filex::search_filter::Filter;

const DIRS: usize = 2_000;
const FILES_PER_DIR: usize = 100;
const LIMIT: usize = 500;

fn synthetic_index() -> VolumeIndex {
    const STEMS: [&str; 10] = [
        "report", "invoice", "photo", "backup", "notes", "main", "config", "readme", "data",
        "screenshot",
    ];
    const EXTS: [&str; 5] = ["txt", "rs", "pdf", "png", "tar.gz"];
    let mut index = VolumeIndex::new("/bench");
    for d in 0..DIRS {
        let dir = index.insert(ROOT, &format!("project-{d:04}"), true).expect("dir");
        for f in 0..FILES_PER_DIR {
            let name = format!("{}_{d:04}_{f:03}.{}", STEMS[(d + f) % STEMS.len()], EXTS[f % EXTS.len()]);
            index.insert(dir, &name, false).expect("file");
        }
    }
    index
}

fn bench_filter(c: &mut Criterion) {
    let index = synthetic_index();
    let code = [Filter::Kind(FileKind::Code)]; // .rs → 1 in 5 files
    let image = [Filter::Kind(FileKind::Image)]; // .png → 1 in 5 files

    let mut group = c.benchmark_group("filter_200k");

    // No-filter regression guard: these two must be equal.
    group.bench_function("plain_report", |b| {
        b.iter(|| black_box(index.search(black_box("report"), LIMIT)))
    });
    group.bench_function("nofilter_report", |b| {
        b.iter(|| black_box(index.search_filtered(black_box("report"), black_box(&[]), LIMIT)))
    });

    // Text + filter: name-match cuts the set, then the predicate runs.
    group.bench_function("text_filter", |b| {
        b.iter(|| black_box(index.search_filtered(black_box("report"), black_box(&code), LIMIT)))
    });

    // Filter-only (no text): full-arena predicate scan.
    group.bench_function("filter_only", |b| {
        b.iter(|| black_box(index.search_filtered(black_box(""), black_box(&image), LIMIT)))
    });

    group.finish();
}

criterion_group!(benches, bench_filter);
criterion_main!(benches);

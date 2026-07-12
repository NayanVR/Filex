//! Search-as-you-type latency benchmark for the volume index.
//!
//! Builds a synthetic 200k-entry index (nested dirs, realistic name shapes)
//! and measures query latency for short/long and rare/common needles.
//! Budget: a full scan must stay comfortably under one keystroke (~30ms);
//! see docs/indexing-architecture.md §3.

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;

use filex::index::{ROOT, VolumeIndex};

const DIRS: usize = 2_000;
const FILES_PER_DIR: usize = 100;

fn synthetic_index() -> VolumeIndex {
    const STEMS: [&str; 10] = [
        "report", "invoice", "photo", "backup", "notes", "main", "config", "readme", "data",
        "screenshot",
    ];
    const EXTS: [&str; 5] = ["txt", "rs", "pdf", "png", "tar.gz"];

    let mut index = VolumeIndex::new("/bench");
    for d in 0..DIRS {
        let dir = index
            .insert(ROOT, &format!("project-{d:04}"), true)
            .expect("insert dir");
        for f in 0..FILES_PER_DIR {
            let name = format!(
                "{}_{d:04}_{f:03}.{}",
                STEMS[(d + f) % STEMS.len()],
                EXTS[f % EXTS.len()],
            );
            index.insert(dir, &name, false).expect("insert file");
        }
    }
    index
}

fn bench_search(c: &mut Criterion) {
    let index = synthetic_index();
    assert_eq!(index.len(), DIRS + DIRS * FILES_PER_DIR);

    let mut group = c.benchmark_group("search_200k");
    // Single-char query: worst case, nearly everything matches.
    group.bench_function("single_char_e", |b| {
        b.iter(|| black_box(index.search(black_box("e"), 500)))
    });
    // Common stem: many matches, ranking + truncation dominated.
    group.bench_function("common_stem_report", |b| {
        b.iter(|| black_box(index.search(black_box("report"), 500)))
    });
    // Rare needle: full scan, almost no hits.
    group.bench_function("rare_needle", |b| {
        b.iter(|| black_box(index.search(black_box("zzz_no_such_file"), 500)))
    });
    // Path materialization for a page of results, as the UI does per query.
    group.bench_function("search_plus_paths", |b| {
        b.iter(|| {
            let hits = index.search(black_box("invoice"), 500);
            let paths: Vec<_> = hits.iter().filter_map(|h| index.path_of(h.id)).collect();
            black_box(paths)
        })
    });
    group.finish();
}

criterion_group!(benches, bench_search);
criterion_main!(benches);

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
    // Rare needle: full scan, almost no hits. NOTE: with no literal hits
    // this also triggers the fuzzy fallback (see below), so it is the
    // *combined* worst case, not the literal path alone.
    group.bench_function("rare_needle", |b| {
        b.iter(|| black_box(index.search(black_box("zzz_no_such_file"), 500)))
    });
    // The gate from docs/design-search-ranking.md decision 2: enough
    // literal hits to fill the limit means the fuzzy pass never runs.
    // This is the no-regression number — it must match the pre-fuzzy
    // literal path, since it executes exactly that code.
    group.bench_function("gate_closed_literal_only", |b| {
        b.iter(|| black_box(index.search(black_box("report"), 50)))
    });
    // Gate open, and the fallback has real work to do: "rpt" is a
    // subsequence of most names here, so nearly every entry gets scored.
    // The realistic worst case for the second pass.
    group.bench_function("gate_open_fuzzy_fallback", |b| {
        b.iter(|| black_box(index.search(black_box("rpt"), 500)))
    });
    // Overfetch is a heap-depth change, not a scan-work change: these
    // two should differ only marginally.
    group.bench_function("overfetch_limit_50", |b| {
        b.iter(|| black_box(index.search(black_box("report"), 50)))
    });
    group.bench_function("overfetch_limit_200", |b| {
        b.iter(|| black_box(index.search(black_box("report"), 200)))
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

/// The shape the UI actually asks for, which the cases above do **not**
/// cover and which is how the fuzzy-gate regression stayed hidden.
///
/// Every case above calls `VolumeIndex::search` directly, with a `limit`
/// the app never passes. The real keystroke goes through
/// `manager::search_all`, which multiplies the display limit by
/// `OVERFETCH` before handing it down — so the number reaching the fuzzy
/// gate is 2000 for the UI's 500, and 4004 for a Magic command's 1001.
/// A bench that never composes those two layers cannot see a bug that
/// only exists in their composition.
///
/// The pair to watch is `selective_query_*`: a query specific enough to
/// be worth typing has far fewer literal hits than the overfetched width,
/// which is exactly the case that used to pay for a second full scan.
fn bench_app_keystroke_path(c: &mut Criterion) {
    use filex::index::manager;
    use std::collections::HashMap;
    use std::sync::{Arc, RwLock};

    let indexes: Vec<filex::index::watcher::SharedIndex> =
        vec![Arc::new(RwLock::new(synthetic_index()))];
    let none = HashMap::new();

    let mut group = c.benchmark_group("app_keystroke_200k");
    // Broad prefix: far more literal hits than any gate, shut either way.
    group.bench_function("broad_prefix_re", |b| {
        b.iter(|| black_box(manager::search_all(&indexes, black_box("re"), &[], 500, &none)))
    });
    // THE case the fix moves. `invoice_010` has ~100 literal hits in this
    // corpus: comfortably above FUZZY_GATE, so the result list is useful
    // and the gate stays shut — but well under the overfetched 2000, so
    // the old gate opened and paid for a second full scan. Queries in
    // this window are the overwhelming majority of what anyone types.
    group.bench_function("useful_result_list_500", |b| {
        b.iter(|| {
            black_box(manager::search_all(&indexes, black_box("invoice_010"), &[], 500, &none))
        })
    });
    // Same query on the Magic command path, where the limit is
    // MAX_PLAN_OPS + 1 and the overfetch is therefore wider still.
    group.bench_function("useful_result_list_magic_1001", |b| {
        b.iter(|| {
            black_box(manager::search_all(&indexes, black_box("invoice_010"), &[], 1001, &none))
        })
    });
    // Below the gate (~10 literal hits): the fuzzy pass runs here under
    // either gate, on purpose. This is the recall the design is buying,
    // and the number that says what it costs.
    group.bench_function("sparse_result_fuzzy_runs", |b| {
        b.iter(|| {
            black_box(manager::search_all(&indexes, black_box("invoice_0100_0"), &[], 500, &none))
        })
    });
    // Nothing matches literally at all — the case decision 2 was written
    // for, and still exactly as fast as it was.
    group.bench_function("empty_result_fuzzy_runs", |b| {
        b.iter(|| black_box(manager::search_all(&indexes, black_box("zqxjw"), &[], 500, &none)))
    });
    group.finish();
}

/// Stage B: path materialization + frecency boost over the overfetched
/// candidate set. The point of this benchmark is to show it is *not* on
/// the critical path — microseconds against a scan measured in
/// milliseconds (docs/design-search-ranking.md decision 1).
fn bench_frecency_rerank(c: &mut Criterion) {
    use filex::index::manager;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::{Arc, RwLock};

    let index = synthetic_index();
    // A realistic visit history: recents::CAP entries, some of which are
    // hits for the query below.
    let table: HashMap<PathBuf, f32> = (0..filex::recents::CAP)
        .map(|i| (PathBuf::from(format!("/bench/project-{i:04}/report_{i:04}_000.pdf")), 4.0))
        .collect();
    let indexes = vec![Arc::new(RwLock::new(index)) as _];

    let mut group = c.benchmark_group("frecency_rerank");
    group.bench_function("search_all_no_frecency", |b| {
        b.iter(|| black_box(manager::search_all(&indexes, black_box("report"), &[], 50, &HashMap::new())))
    });
    group.bench_function("search_all_with_frecency", |b| {
        b.iter(|| black_box(manager::search_all(&indexes, black_box("report"), &[], 50, &table)))
    });
    group.finish();
}

criterion_group!(benches, bench_search, bench_frecency_rerank, bench_app_keystroke_path);
criterion_main!(benches);

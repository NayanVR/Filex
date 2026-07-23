//! `tag:` filter latency benchmark for the sidecar tag index.
//!
//! The `tag:` intersect is off the search-as-you-type hot path unless a
//! `tag:` token is present, but when it is, it scans the whole tag index
//! per keystroke (see docs/design-tags.md §Search). This measures that
//! scan at 100k tagged entries — it must stay comfortably under one
//! keystroke (~a few ms) so a `tag:` query types as smoothly as a plain
//! one. Uses only the public store API, so it exercises the real path.

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;

use std::collections::BTreeMap;
use std::path::PathBuf;

use filex::tags::{SidecarTags, Tag, TagColor};

const ENTRIES: usize = 100_000;

/// A 100k-entry sidecar where every 10th file carries "Work" (so the
/// common-tag scan returns ~10k paths) and one carries a rare tag. The
/// index file is written once and loaded (not 100k persisting
/// `set_tags` calls), so bench setup is O(n), not O(n²).
fn synthetic_store() -> (tempfile::TempDir, SidecarTags) {
    const STEMS: [&str; 5] = ["report", "invoice", "photo", "notes", "backup"];
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("tags.json");

    let mut map: BTreeMap<PathBuf, Vec<Tag>> = BTreeMap::new();
    for i in 0..ENTRIES {
        let path = format!("/home/u/project-{:04}/{}-{i}.txt", i % 2_000, STEMS[i % STEMS.len()]);
        let mut tags = vec![Tag::colored("Blue", TagColor::Blue)];
        if i % 10 == 0 {
            tags.push(Tag::new("Work"));
        }
        if i == ENTRIES - 1 {
            tags.push(Tag::new("Rare"));
        }
        map.insert(PathBuf::from(path), tags);
    }
    std::fs::write(&file, serde_json::to_string(&map).unwrap()).unwrap();
    (dir, SidecarTags::load(file))
}

fn bench_tag_filter(c: &mut Criterion) {
    let (_dir, store) = synthetic_store();

    let mut group = c.benchmark_group("tag_filter_100k");
    // Common tag: ~10k of 100k match — the scan plus path cloning.
    group.bench_function("common_tag_work", |b| {
        b.iter(|| black_box(store.paths_with_all_tags(black_box(&["work".into()]))))
    });
    // Two-tag AND: full scan, few matches.
    group.bench_function("and_work_rare", |b| {
        b.iter(|| {
            black_box(store.paths_with_all_tags(black_box(&["work".into(), "rare".into()])))
        })
    });
    // Rare tag: full scan, one match — the intersect's floor cost.
    group.bench_function("rare_tag", |b| {
        b.iter(|| black_box(store.paths_with_all_tags(black_box(&["rare".into()]))))
    });
    group.finish();
}

criterion_group!(benches, bench_tag_filter);
criterion_main!(benches);

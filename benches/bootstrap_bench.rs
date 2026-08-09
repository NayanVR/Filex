//! Cost of populating size/mtime during the bootstrap walk (search-chips
//! phase 2). The index stores names only today; adding size/mtime means a
//! `stat` per entry, because the real bootstrap is `jwalk` on every OS
//! (the `getattrlistbulk` fast path in the architecture doc is not built).
//! This measures that added cost directly: the same parallel walk over a
//! real ~30k-file tree, collecting file type only vs. also calling
//! `metadata()` (the stat). The delta is what size/mtime population costs
//! per bootstrap on this machine (warm cache).

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;
use std::time::UNIX_EPOCH;

use jwalk::WalkDir;

const DIRS: usize = 300;
const FILES_PER_DIR: usize = 100; // ~30k files

/// Build a real temp tree of ~30k files to walk.
fn synthetic_tree() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for d in 0..DIRS {
        let sub = dir.path().join(format!("project-{d:04}"));
        std::fs::create_dir(&sub).unwrap();
        for f in 0..FILES_PER_DIR {
            std::fs::write(sub.join(format!("file-{f:03}.txt")), b"x").unwrap();
        }
    }
    dir
}

/// Walk collecting only name + file type (today's bootstrap: no stat).
fn walk_names_only(root: &std::path::Path) -> usize {
    let mut n = 0;
    for dirent in WalkDir::new(root).follow_links(false) {
        let Ok(dirent) = dirent else { continue };
        let _ = black_box(dirent.file_type());
        n += 1;
    }
    n
}

/// Walk and additionally stat each entry for size + mtime (the phase-2
/// population).
fn walk_with_metadata(root: &std::path::Path) -> usize {
    let mut n = 0;
    for dirent in WalkDir::new(root).follow_links(false) {
        let Ok(dirent) = dirent else { continue };
        if let Ok(meta) = dirent.metadata() {
            let size = meta.len();
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            black_box((size, mtime));
        }
        n += 1;
    }
    n
}

fn bench_bootstrap(c: &mut Criterion) {
    let tree = synthetic_tree();
    let root = tree.path();

    let mut group = c.benchmark_group("bootstrap_30k");
    group.bench_function("names_only", |b| {
        b.iter(|| black_box(walk_names_only(root)))
    });
    group.bench_function("with_metadata", |b| {
        b.iter(|| black_box(walk_with_metadata(root)))
    });
    group.finish();
}

criterion_group!(benches, bench_bootstrap);
criterion_main!(benches);

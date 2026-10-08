use criterion::{Criterion, criterion_group, criterion_main};
use filex::{
    catalog::segment::{Identity, Record, Root, Segment},
    daemon::{
        ipc::Query,
        query,
        view::{Overlay, View},
    },
};
use std::{collections::HashMap, path::PathBuf, sync::Arc, sync::atomic::AtomicBool};

fn root() -> Vec<Root> {
    vec![Root {
        id: 1,
        path: "/bench".into(),
        device: 0,
    }]
}

fn bench(c: &mut Criterion) {
    c.bench_function("build_10k_segment", |b| {
        b.iter(|| {
            let records = (0..10_000).map(|i| Record {
                id: i + 1,
                parent: if i == 0 { 0 } else { 1 },
                root: 1,
                name: format!("file-{i}.txt").into_bytes(),
                flags: u8::from(i == 0),
                identity: Identity::default(),
                size: Some(i),
                mtime: Some(0),
            });
            Segment::build(records, root(), 0).unwrap()
        })
    });
}

/// A home-directory-shaped tree: 20 x 5^4 directories six levels deep, 64
/// entries per leaf (~820k records). IDs are assigned per directory as the
/// walk emits them, and common names repeat, so the deduplicated name pool
/// scatters a directory's names across many compressed pages — the layout
/// that made every reconcile lookup decode pages (FIL-27).
fn home_tree() -> Vec<Record> {
    const COMMON: [&str; 6] = [
        "index.js",
        "package.json",
        "README.md",
        "LICENSE",
        ".DS_Store",
        "mod.rs",
    ];
    let record = |id, parent, name: String, directory| Record {
        id,
        parent,
        root: 1,
        name: name.into_bytes(),
        flags: if directory { Record::DIRECTORY } else { 0 },
        identity: Identity::default(),
        size: None,
        mtime: None,
    };
    let mut records = vec![record(1, 0, "bench".into(), true)];
    let mut stack = vec![(1u64, 0usize)];
    while let Some((parent, depth)) = stack.pop() {
        let (count, directories) = match depth {
            0 => (20, true),
            1..=4 => (5, true),
            _ => (64, false),
        };
        for i in 0..count {
            let id = records.len() as u64 + 1;
            let name = match (directories, i % 8) {
                (true, _) => format!("dir-{depth}-{i}"),
                (false, n) if n < COMMON.len() => COMMON[n].to_string(),
                _ => format!("file-{id}.rs"),
            };
            records.push(record(id, parent, name, directories));
            if directories {
                stack.push((id, depth + 1));
            }
        }
    }
    records
}

/// Per-entry lookup cost of a reconcile walk over an unchanged tree.
/// `resolve_full_path` is the pre-FIL-27 enumerate lookup (root to leaf for
/// every entry); `child_of_walk_parent` is the current one (one level,
/// under the parent ID the walk already holds).
fn enumerate_lookup(c: &mut Criterion) {
    let records = home_tree();
    let base = Arc::new(Segment::build(records.clone(), root(), 0).unwrap());
    let view = View {
        base,
        layers: vec![],
        roots: root(),
        epoch: 0,
    };
    // 20k consecutive IDs from the middle of the tree, in walk order.
    let sample: Vec<(PathBuf, u64, Vec<u8>)> = records[400_000..420_000]
        .iter()
        .map(|r| (view.path(r.id).unwrap(), r.parent, r.name.clone()))
        .collect();
    let mut group = c.benchmark_group("enumerate_lookup_820k");
    group.sample_size(10);
    group.bench_function("resolve_full_path", |b| {
        b.iter(|| {
            sample
                .iter()
                .filter(|(path, _, _)| view.resolve(path).is_some())
                .count()
        })
    });
    group.bench_function("child_of_walk_parent", |b| {
        b.iter(|| {
            sample
                .iter()
                .filter(|(_, parent, name)| view.child(*parent, name).is_some())
                .count()
        })
    });
    group.finish();
}

/// Interactive search cost of a live overlay. Overlay records are searchable
/// without compaction, but every query scans them; FIL-27 lets small overlays
/// live much longer, so this tracks what that adds to search-as-you-type.
fn search_with_overlay(c: &mut Criterion) {
    let records = home_tree();
    let base = Arc::new(Segment::build(records.clone(), root(), 0).unwrap());
    let next = records.len() as u64 + 1;
    let query = Query {
        text: "report".into(),
        ..Default::default()
    };
    let (cancel, hot) = (AtomicBool::new(false), HashMap::new());
    let mut group = c.benchmark_group("search_overlay_820k");
    for size in [0u64, 5_000, 25_000] {
        let mut overlay = Overlay::default();
        for i in 0..size {
            // Half rewrite base records, half add new files.
            let mut record = if i % 2 == 0 {
                records[(i * 31 % (records.len() as u64 - 1) + 1) as usize].clone()
            } else {
                Record {
                    id: next + i,
                    parent: 1,
                    root: 1,
                    name: format!("new-{i}.txt").into_bytes(),
                    flags: 0,
                    identity: Identity::default(),
                    size: None,
                    mtime: None,
                }
            };
            record.size = Some(i);
            overlay.put(record.id, Some(record));
        }
        let view = View {
            base: base.clone(),
            layers: vec![Arc::new(overlay)],
            roots: root(),
            epoch: 0,
        };
        group.bench_function(format!("overlay_{size}"), |b| {
            b.iter(|| query::search(&view, &query, &cancel, &hot).unwrap())
        });
    }
    group.finish();
}

criterion_group!(benches, bench, enumerate_lookup, search_with_overlay);
criterion_main!(benches);

use filex::{
    catalog::{
        segment::{Identity, Record, Root, Segment},
        wal::{Delta, Transaction, Wal},
    },
    daemon::{
        ipc::{Client, Command, Query, Response},
        query, server,
        view::View,
    },
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
fn record(id: u64, parent: u64, name: &[u8], dir: bool) -> Record {
    Record {
        id,
        parent,
        root: 1,
        name: name.to_vec(),
        flags: u8::from(dir),
        identity: Identity {
            device: 1,
            key: id,
            birth: 1,
        },
        size: Some(id),
        mtime: Some(1700000000),
    }
}
#[test]
fn mapped_segment_roundtrip_and_corruption() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("segment.fx2");
    let roots = vec![Root {
        id: 1,
        path: "/test".into(),
        device: 1,
    }];
    let records = vec![
        record(1, 0, b"test", true),
        record(2, 1, b"Report.pdf", false),
        record(3, 1, b"XMLHttpRequest.rs", false),
        record(4, 1, &[255], false),
    ];
    let segment = Segment::build(records.clone(), roots, 42).unwrap();
    segment.save(&path).unwrap();
    drop(segment);
    let mapped = unsafe { Segment::open(&path) }.unwrap();
    assert_eq!(mapped.sequence, 42);
    for (i, r) in records.iter().enumerate() {
        assert_eq!(&mapped.record(i), r);
    }
    assert_eq!(mapped.search.search("report", 100).len(), 1);
    assert_eq!(mapped.search.search("http", 100).len(), 1);
    assert_eq!(mapped.search.fuzzy_names("xhr").len(), 1);
    drop(mapped);
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[128] ^= 1;
    std::fs::write(&path, bytes).unwrap();
    assert!(unsafe { Segment::open(&path) }.is_err());
}
#[test]
fn wal_recovers_torn_tail_and_rejects_complete_corruption() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wal");
    let (wmut, _) = Wal::open(&path).unwrap();
    let mut wal = wmut;
    wal.append(&Transaction {
        sequence: 1,
        deltas: vec![Delta::Upsert(record(1, 0, b"root", true))],
        next_id: 2,
    })
    .unwrap();
    drop(wal);
    let good = std::fs::read(&path).unwrap();
    for tail in [&[1u8][..], &[4, 0, 0, 0, 1, 2][..]] {
        let mut torn = good.clone();
        torn.extend_from_slice(tail);
        std::fs::write(&path, torn).unwrap();
        let (wal, tx) = Wal::open(&path).unwrap();
        assert_eq!(tx.len(), 1);
        drop(wal);
        assert_eq!(std::fs::read(&path).unwrap(), good);
    }
    let mut bad = good;
    bad[8] ^= 1;
    std::fs::write(path.clone(), bad).unwrap();
    assert!(Wal::open(&path).is_err());
}
#[track_caller]
fn wait_until(mut condition: impl FnMut() -> bool) {
    let start = Instant::now();
    while !condition() {
        assert!(start.elapsed() < Duration::from_secs(20), "timed out");
        std::thread::sleep(Duration::from_millis(20));
    }
}
#[test]
fn daemon_search_stream_live_move_recovery_and_identity() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("files");
    let data = dir.path().join("index");
    std::fs::create_dir(&root).unwrap();
    let root = root.canonicalize().unwrap();
    std::fs::create_dir(root.join("docs")).unwrap();
    std::fs::write(root.join("docs/report.pdf"), b"hello").unwrap();
    std::fs::write(root.join("report.txt"), b"text").unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let shutdown = stop.clone();
    let path = data.clone();
    let roots = vec![root.clone()];
    let thread = std::thread::spawn(move || server::run(&path, roots, shutdown).unwrap());
    wait_until(|| Client::connect(&data).is_ok());
    let client = Client::connect(&data).unwrap();
    wait_until(|| {
        client.status().is_ok_and(|s| {
            if s.error.is_some() {
                eprintln!("build error: {:?}", s.error);
            }
            !s.building && s.error.is_none() && !s.roots.is_empty()
        })
    });
    let cancel = AtomicBool::new(false);
    let query = Query {
        text: "report".into(),
        client: 1,
        ..Default::default()
    };
    let first = client.search(query.clone(), &cancel).unwrap();
    assert_eq!(first.hits.len(), 2);
    let original = first
        .hits
        .iter()
        .find(|h| h.name == "report.pdf")
        .unwrap()
        .clone();
    let mut complete = false;
    let mut count = 0;
    client
        .stream_matches(query.clone(), &cancel, |batch| {
            count += batch.hits.len();
            complete |= batch.total == Some(2);
            true
        })
        .unwrap();
    assert!(complete);
    assert_eq!(count, 2);
    std::fs::rename(root.join("docs"), root.join("moved")).unwrap();
    client
        .hint(1, vec![root.join("docs"), root.join("moved")])
        .unwrap();
    wait_until(|| {
        client.search(query.clone(), &cancel).is_ok_and(|p| {
            p.hits
                .iter()
                .any(|h| h.id == original.id && h.path == root.join("moved/report.pdf"))
        })
    });
    assert!(client.verify(original).is_err());
    std::fs::write(root.join("fresh.txt"), b"new").unwrap();
    client.hint(2, vec![root.join("fresh.txt")]).unwrap();
    let fresh = Query {
        text: "fresh".into(),
        client: 1,
        ..Default::default()
    };
    wait_until(|| {
        client
            .search(fresh.clone(), &cancel)
            .is_ok_and(|p| p.hits.len() == 1)
    });
    let allowed = Query {
        allowed: Some(vec![root.join("report.txt")]),
        ..query.clone()
    };
    assert_eq!(client.search(allowed, &cancel).unwrap().hits.len(), 1);
    let scoped = Query {
        scope: Some(root.join("moved")),
        ..query.clone()
    };
    assert_eq!(client.search(scoped, &cancel).unwrap().hits.len(), 1);
    assert!(matches!(
        client.call(Command::Status).unwrap(),
        Response::Status(_)
    ));
    stop.store(true, Ordering::Relaxed);
    thread.join().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let shutdown = stop.clone();
    let path = data.clone();
    let root_copy = root.clone();
    let thread = std::thread::spawn(move || server::run(&path, vec![root_copy], shutdown).unwrap());
    wait_until(|| Client::connect(&data).is_ok());
    let client = Client::connect(&data).unwrap();
    wait_until(|| client.status().is_ok_and(|s| !s.building));
    assert_eq!(client.search(fresh, &cancel).unwrap().hits.len(), 1);
    stop.store(true, Ordering::Relaxed);
    thread.join().unwrap();
}
#[test]
fn exhaustive_never_includes_fuzzy() {
    let segment = Segment::build(
        [
            record(1, 0, b"root", true),
            record(2, 1, b"report.txt", false),
        ],
        vec![Root {
            id: 1,
            path: "/test".into(),
            device: 1,
        }],
        0,
    )
    .unwrap();
    let view = View {
        base: Arc::new(segment),
        layers: Vec::new(),
        roots: vec![Root {
            id: 1,
            path: "/test".into(),
            device: 1,
        }],
        epoch: 0,
    };
    let q = Query {
        text: "repor".into(),
        ..Default::default()
    };
    assert_eq!(
        query::search(&view, &q, &AtomicBool::new(false), &Default::default())
            .unwrap()
            .hits
            .len(),
        1
    );
    let q = Query {
        text: "reprot".into(),
        ..q
    };
    let mut hits = 0;
    query::stream(&view, &q, &AtomicBool::new(false), |b| {
        hits += b.hits.len();
        Ok(())
    })
    .unwrap();
    assert_eq!(hits, 0);
}

#[test]
fn paging_filters_overlay_tombstones_and_cancellation() {
    use filex::{daemon::view::Overlay, search_filter::Filter};
    let roots = vec![Root {
        id: 1,
        path: "/test".into(),
        device: 1,
    }];
    let mut records = vec![record(1, 0, b"root", true)];
    records
        .extend((2..302).map(|id| record(id, 1, format!("report-{id:04}.txt").as_bytes(), false)));
    records.push(record(302, 1, b"report-final.pdf", false));
    let base = Arc::new(Segment::build(records, roots.clone(), 7).unwrap());
    let mut layer = Overlay::default();
    layer.put(2, None);
    layer.put(303, Some(record(303, 1, b"report-new.pdf", false)));
    let view = View {
        base,
        layers: vec![Arc::new(layer)],
        roots,
        epoch: 8,
    };
    let cancel = AtomicBool::new(false);
    let hot = Default::default();
    let q = Query {
        text: "report".into(),
        fuzzy: false,
        epoch_hint: Some(8),
        ..Default::default()
    };
    let mut ids = std::collections::HashSet::new();
    for offset in [0, 100, 200, 300] {
        let page = query::search(
            &view,
            &Query {
                offset,
                ..q.clone()
            },
            &cancel,
            &hot,
        )
        .unwrap();
        for h in page.hits {
            assert!(ids.insert(h.id), "duplicate across pages");
        }
    }
    assert_eq!(ids.len(), 301);
    assert!(!ids.contains(&2));
    let page = query::search(
        &view,
        &Query {
            filters: vec![Filter::Ext("pdf".into())],
            ..q.clone()
        },
        &cancel,
        &hot,
    )
    .unwrap();
    assert_eq!(page.hits.len(), 2);
    assert!(
        query::search(
            &view,
            &Query {
                epoch_hint: Some(7),
                ..q.clone()
            },
            &cancel,
            &hot
        )
        .is_err()
    );
    cancel.store(true, Ordering::Relaxed);
    assert!(query::search(&view, &q, &cancel, &hot).is_err());
}
#[test]
fn checkpoint_keeps_fallback_tail() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wal");
    let (mut wal, _) = Wal::open(&path).unwrap();
    for sequence in 1..=5 {
        wal.append(&Transaction {
            sequence,
            deltas: vec![],
            next_id: sequence,
        })
        .unwrap();
    }
    wal.checkpoint(3).unwrap();
    wal.append(&Transaction {
        sequence: 6,
        deltas: vec![],
        next_id: 7,
    })
    .unwrap();
    drop(wal);
    let (_, transactions) = Wal::open(&path).unwrap();
    assert_eq!(
        transactions.iter().map(|t| t.sequence).collect::<Vec<_>>(),
        vec![4, 5, 6]
    );
}
#[test]
fn self_exclusion_bursts_and_corrupt_generation_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let data = root.join("database");
    std::fs::write(root.join("first.txt"), b"1").unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let shutdown = stop.clone();
    let path = data.clone();
    let roots = vec![root.clone()];
    let thread = std::thread::spawn(move || server::run(&path, roots, shutdown).unwrap());
    wait_until(|| Client::connect(&data).is_ok());
    let client = Client::connect(&data).unwrap();
    wait_until(|| client.status().is_ok_and(|s| !s.building));
    let cancel = AtomicBool::new(false);
    let q = Query {
        fuzzy: false,
        client: 99,
        ..Default::default()
    };
    let mut paths = Vec::new();
    for n in 0..300 {
        let p = root.join(format!("burst-{n}.txt"));
        std::fs::write(&p, b"x").unwrap();
        paths.push(p);
    }
    client.hint(99, paths).unwrap();
    wait_until(|| {
        client
            .search(
                Query {
                    text: "burst-".into(),
                    limit: 400,
                    ..q.clone()
                },
                &cancel,
            )
            .is_ok_and(|p| p.hits.len() == 300)
    });
    let page = client
        .search(
            Query {
                limit: 400,
                ..q.clone()
            },
            &cancel,
        )
        .unwrap();
    assert!(page.hits.iter().all(|h| !h.path.starts_with(&data)));
    stop.store(true, Ordering::Relaxed);
    thread.join().unwrap();
    // A restart produces another independently recoverable generation.
    let stop = Arc::new(AtomicBool::new(false));
    let shutdown = stop.clone();
    let path = data.clone();
    let roots = vec![root.clone()];
    let thread = std::thread::spawn(move || server::run(&path, roots, shutdown).unwrap());
    wait_until(|| Client::connect(&data).is_ok());
    let client = Client::connect(&data).unwrap();
    wait_until(|| client.status().is_ok_and(|s| !s.building));
    stop.store(true, Ordering::Relaxed);
    thread.join().unwrap();
    let manifests = filex::catalog::manifest::candidates(&data).unwrap();
    assert!(manifests.len() >= 2);
    let latest = data.join(&manifests[0].1.segment);
    std::fs::write(latest, b"corrupt").unwrap();
    std::fs::write(root.join("offline.txt"), b"offline").unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let shutdown = stop.clone();
    let path = data.clone();
    let roots = vec![root.clone()];
    let thread = std::thread::spawn(move || server::run(&path, roots, shutdown).unwrap());
    wait_until(|| Client::connect(&data).is_ok());
    let client = Client::connect(&data).unwrap();
    wait_until(|| client.status().is_ok_and(|s| !s.building));
    assert_eq!(
        client
            .search(
                Query {
                    text: "offline".into(),
                    ..q
                },
                &cancel
            )
            .unwrap()
            .hits
            .len(),
        1
    );
    stop.store(true, Ordering::Relaxed);
    thread.join().unwrap();
}
#[test]
fn local_protocol_preserves_native_path_bytes() {
    use filex::catalog::segment::{os_name, raw_name};
    #[cfg(unix)]
    let raw = vec![b'/', b't', 255, b'.', b'x'];
    #[cfg(windows)]
    let raw = vec![255, 254, 0, 216];
    #[cfg(not(any(unix, windows)))]
    let raw = b"/test".to_vec();
    let path = std::path::PathBuf::from(os_name(&raw));
    let q = Query {
        scope: Some(path.clone()),
        allowed: Some(vec![path.clone()]),
        ..Default::default()
    };
    let bytes = serde_json::to_vec(&q).unwrap();
    let decoded: Query = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(raw_name(decoded.scope.unwrap().as_os_str()), raw);
    assert_eq!(decoded.allowed.unwrap(), vec![path]);
}

#[test]
fn ranked_pages_agree_with_exhaustive_stream() {
    let roots = vec![Root {
        id: 1,
        path: "/test".into(),
        device: 1,
    }];
    let mut records = vec![record(1, 0, b"root", true)];
    let words = ["fooBar", "foobar", "report", "annualReport", "bar", "data"];
    for i in 0..1500 {
        records.push(record(
            i + 2,
            1,
            format!(
                "{}-{:03}.{}",
                words[i as usize % words.len()],
                i % 120,
                if i % 3 == 0 { "pdf" } else { "txt" }
            )
            .as_bytes(),
            false,
        ));
    }
    let mut layer = filex::daemon::view::Overlay::default();
    for id in 2..80 {
        layer.put(
            id,
            Some(record(
                id,
                1,
                format!("bar-report-renamed-with-a-longer-name-{id}.txt").as_bytes(),
                false,
            )),
        );
    }
    let view = View {
        base: Arc::new(Segment::build(records, roots.clone(), 1).unwrap()),
        layers: vec![Arc::new(layer)],
        roots,
        epoch: 1,
    };
    let hot = std::collections::HashMap::from([(1401, 50), (1402, 100), (1300, 200)]);
    let cancel = AtomicBool::new(false);
    for text in ["bar", "report", "a", "data", "foo"] {
        let q = Query {
            text: text.into(),
            fuzzy: false,
            ..Default::default()
        };
        let mut expected = Vec::new();
        query::stream(&view, &q, &cancel, |batch| {
            expected.extend(batch.hits);
            Ok(())
        })
        .unwrap();
        expected.sort_by_cached_key(|h| {
            let folded = filex::catalog::normalize::nfc_fold(&h.name);
            (
                h.tier,
                std::cmp::Reverse(hot.get(&h.id).copied().unwrap_or(0)),
                folded.len(),
                folded,
                h.id,
            )
        });
        for offset in [0, 100, 200] {
            let actual = query::search(
                &view,
                &Query {
                    offset,
                    ..q.clone()
                },
                &cancel,
                &hot,
            )
            .unwrap();
            assert_eq!(
                actual.hits.iter().map(|h| h.id).collect::<Vec<_>>(),
                expected
                    .iter()
                    .skip(offset)
                    .take(100)
                    .map(|h| h.id)
                    .collect::<Vec<_>>(),
                "query {text}, page {offset}"
            );
        }
    }
}

#[test]
fn daemon_process_crash_replays_durable_changes_and_reconciles_offline_changes() {
    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("files");
    let data = dir.path().join("index");
    std::fs::create_dir(&root).unwrap();
    let spawn = || {
        Child(
            std::process::Command::new(env!("CARGO_BIN_EXE_filex-indexd"))
                .arg("--data-dir")
                .arg(&data)
                .arg(&root)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        )
    };
    let child = spawn();
    wait_until(|| Client::connect(&data).is_ok());
    let client = Client::connect(&data).unwrap();
    wait_until(|| client.status().is_ok_and(|s| !s.building));
    let cancel = AtomicBool::new(false);
    let q = Query {
        text: "durable".into(),
        request: 2,
        client: 77,
        ..Default::default()
    };
    std::fs::write(root.join("durable.txt"), b"x").unwrap();
    client.hint(1, vec![root.join("durable.txt")]).unwrap();
    wait_until(|| {
        client
            .search(q.clone(), &cancel)
            .is_ok_and(|p| p.hits.len() == 1)
    });
    let original = client.search(q.clone(), &cancel).unwrap().hits[0].id;
    client
        .call(Command::Cancel {
            client: 77,
            before_request: 2,
        })
        .unwrap();
    assert!(
        client
            .search(
                Query {
                    request: 1,
                    ..q.clone()
                },
                &cancel
            )
            .is_err()
    );
    assert_eq!(client.search(q.clone(), &cancel).unwrap().hits.len(), 1);
    drop(child);
    std::fs::write(root.join("offline.txt"), b"x").unwrap();
    let _child = spawn();
    wait_until(|| Client::connect(&data).is_ok());
    let client = Client::connect(&data).unwrap();
    wait_until(|| client.status().is_ok_and(|s| !s.building));
    assert_eq!(client.search(q, &cancel).unwrap().hits[0].id, original);
    assert_eq!(
        client
            .search(
                Query {
                    text: "offline".into(),
                    client: 78,
                    ..Default::default()
                },
                &cancel
            )
            .unwrap()
            .hits
            .len(),
        1
    );
}

#[test]
#[ignore = "10,000-file churn/resource qualification; run explicitly"]
fn ten_thousand_changes_converge_during_queries() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("files");
    let data = dir.path().join("index");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("anchor.txt"), b"x").unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let shutdown = stop.clone();
    let path = data.clone();
    let roots = vec![root.clone()];
    let thread = std::thread::spawn(move || server::run(&path, roots, shutdown).unwrap());
    wait_until(|| Client::connect(&data).is_ok());
    let client = Client::connect(&data).unwrap();
    wait_until(|| client.status().is_ok_and(|s| !s.building));
    let creating = root.clone();
    let files = std::thread::spawn(move || {
        let mut paths = Vec::new();
        for i in 0..10_000 {
            let p = creating.join(format!("burst-{i:05}.txt"));
            std::fs::write(&p, b"x").unwrap();
            paths.push(p);
        }
        paths
    });
    let cancel = AtomicBool::new(false);
    let q = Query {
        text: "t".into(),
        client: 555,
        ..Default::default()
    };
    let mut samples = Vec::new();
    while !files.is_finished() {
        let start = Instant::now();
        let page = client.search(q.clone(), &cancel).unwrap();
        assert!(!page.hits.is_empty() && page.hits.len() <= 100);
        samples.push(start.elapsed().as_secs_f64() * 1000.0);
        std::thread::sleep(Duration::from_millis(10));
    }
    let paths = files.join().unwrap();
    for (i, chunk) in paths.chunks(2048).enumerate() {
        client.hint(i as u64 + 1, chunk.to_vec()).unwrap();
    }
    wait_until(|| {
        let mut total = None;
        client
            .stream_matches(
                Query {
                    text: "burst-".into(),
                    client: 556,
                    ..Default::default()
                },
                &cancel,
                |b| {
                    if b.total.is_some() {
                        total = b.total;
                    }
                    true
                },
            )
            .is_ok()
            && total == Some(10_000)
    });
    samples.sort_by(f64::total_cmp);
    eprintln!(
        "churn queries={}, p95_ms={:.3}",
        samples.len(),
        samples[(samples.len() * 95 / 100).min(samples.len() - 1)]
    );
    stop.store(true, Ordering::Relaxed);
    thread.join().unwrap();
}

#[test]
fn magic_scope_stream_is_exhaustive_and_avoids_unrelated_folders() {
    let roots = vec![Root {
        id: 1,
        path: "/test".into(),
        device: 1,
    }];
    let mut records = vec![
        record(1, 0, b"root", true),
        record(2, 1, b"work", true),
        record(3, 2, b"nested", true),
        record(4, 3, b"report.pdf", false),
    ];
    for id in 5..10005 {
        records.push(record(id, 1, format!("outside-{id}.pdf").as_bytes(), false));
    }
    let mut overlay = filex::daemon::view::Overlay::default();
    overlay.put(5, Some(record(5, 2, b"moved-in.pdf", false)));
    overlay.put(4, None);
    let view = View {
        base: Arc::new(Segment::build(records, roots.clone(), 1).unwrap()),
        layers: vec![Arc::new(overlay)],
        roots,
        epoch: 1,
    };
    let cancel = AtomicBool::new(false);
    let mut full = Vec::new();
    query::stream(&view, &Query::default(), &cancel, |b| {
        full.extend(b.hits);
        Ok(())
    })
    .unwrap();
    let scope = std::path::PathBuf::from("/test/work");
    let expected: Vec<_> = full
        .iter()
        .filter(|h| h.path.starts_with(&scope))
        .map(|h| h.id)
        .collect();
    let mut found = Vec::new();
    let mut scanned = 0;
    query::stream(
        &view,
        &Query {
            scope: Some(scope),
            ..Default::default()
        },
        &cancel,
        |b| {
            scanned = b.scanned;
            found.extend(b.hits.into_iter().map(|h| h.id));
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(found, expected);
    assert_eq!(scanned, 3);
}

#[test]
fn magic_large_scope_falls_back_without_truncating() {
    let roots = vec![Root {
        id: 1,
        path: "/test".into(),
        device: 1,
    }];
    let mut records = vec![record(1, 0, b"root", true)];
    for id in 2..16400 {
        records.push(record(id, 1, format!("file-{id}.txt").as_bytes(), false));
    }
    let view = View {
        base: Arc::new(Segment::build(records, roots.clone(), 1).unwrap()),
        layers: vec![],
        roots,
        epoch: 1,
    };
    let mut count = 0;
    let mut total = None;
    query::stream(
        &view,
        &Query {
            scope: Some("/test".into()),
            ..Default::default()
        },
        &AtomicBool::new(false),
        |b| {
            count += b.hits.len();
            total = b.total;
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(count, 16398);
    assert_eq!(total, Some(16398));
}

#[test]
fn magic_literal_postings_include_renames_and_keep_complete_matches() {
    let roots = vec![Root {
        id: 1,
        path: "/test".into(),
        device: 1,
    }];
    let mut records = vec![record(1, 0, b"root", true)];
    for id in 2..10002 {
        records.push(record(id, 1, format!("file-{id}.txt").as_bytes(), false));
    }
    let mut overlay = filex::daemon::view::Overlay::default();
    overlay.put(2, Some(record(2, 1, b"rare-report.txt", false)));
    overlay.put(3, None);
    let view = View {
        base: Arc::new(Segment::build(records, roots.clone(), 1).unwrap()),
        layers: vec![Arc::new(overlay)],
        roots,
        epoch: 1,
    };
    let cancel = AtomicBool::new(false);
    let mut all = Vec::new();
    query::stream(&view, &Query::default(), &cancel, |b| {
        all.extend(b.hits);
        Ok(())
    })
    .unwrap();
    for text in ["rare", "file-2", ".txt", "absent"] {
        let expected: Vec<_> = all
            .iter()
            .filter(|h| h.name.contains(text))
            .map(|h| h.id)
            .collect();
        let mut found = Vec::new();
        let mut scanned = 0;
        query::stream(
            &view,
            &Query {
                text: text.into(),
                ..Default::default()
            },
            &cancel,
            |b| {
                scanned = b.scanned;
                found.extend(b.hits.into_iter().map(|h| h.id));
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(found, expected, "{text}");
        if text == "rare" {
            assert!(scanned <= 2);
        }
    }
}

#[test]
fn magic_indexed_predicates_match_full_scan_with_overlay_changes() {
    use filex::daemon::view::Overlay;
    use filex::listing::FileKind;
    use filex::search_filter::{Bound, Filter, ItemMeta};
    let roots = vec![Root {
        id: 1,
        path: "/test".into(),
        device: 1,
    }];
    let mut records = vec![record(1, 0, b"root", true), record(2, 1, b"folder", true)];
    for id in 3..20020 {
        let ext = if id % 7 == 0 { "pdf" } else { "txt" };
        let mut r = record(
            id,
            if id % 2 == 0 { 2 } else { 1 },
            format!("École-Straße-{id}.{ext}").as_bytes(),
            false,
        );
        r.size = (id % 11 != 0).then_some(id % 1024);
        r.mtime = (id % 13 != 0).then_some((id as i64 % 8 - 4) * 86400 + 71);
        records.push(r);
    }
    records.push(record(21000, 1, "native.Ä".as_bytes(), false));
    records.push(record(21001, 1, b"same-path.pdf", false));
    records.push(record(21002, 1, b"same-path.pdf", false));
    records.push(record(22000, 1, &[255, b'.', b'p', b'd', b'f'], false));
    let mut first = Overlay::default();
    first.put(7, Some(record(7, 1, b"renamed-out.txt", false)));
    first.put(8, Some(record(8, 1, b"new-match.pdf", false)));
    first.put(14, None);
    first.put(23000, Some(record(23000, 1, b"added.pdf", false)));
    first.put(23001, Some(record(23001, 999999, b"orphan.pdf", false)));
    let mut latest = Overlay::default();
    latest.put(8, Some(record(8, 1, b"latest.pdf", false)));
    latest.put(23000, None);
    // Deleting an ancestor hides unchanged descendants even if postings match.
    latest.put(2, None);
    let base = Segment::build(records, roots.clone(), 1).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("mapped");
    base.save(&file).unwrap();
    let view = View {
        base: Arc::new(unsafe { Segment::open(&file).unwrap() }),
        roots,
        layers: vec![Arc::new(first), Arc::new(latest)],
        epoch: 3,
    };
    let filters = vec![
        vec![],
        vec![Filter::Ext("pdf".into())],
        vec![Filter::Ext("Ä".into())],
        vec![Filter::Kind(FileKind::Document)],
        vec![Filter::Size(Bound::Range(127, 129))],
        vec![Filter::Modified(Bound::Range(-86400, 0))],
        vec![
            Filter::Ext("pdf".into()),
            Filter::Size(Bound::Lt(300)),
            Filter::Modified(Bound::Ge(-172800)),
        ],
        vec![Filter::Ext("missing".into())],
    ];
    // Independent oracle: evaluate every live record, without index planning.
    let all: Vec<_> = view.records().collect();
    for filters in filters {
        for text in ["", "STRASSE", "latest", ".pdf", "missing"] {
            for allowed in [
                None,
                Some(vec![
                    "/test/latest.pdf".into(),
                    "/test/native.Ä".into(),
                    "/test/same-path.pdf".into(),
                ]),
            ] {
                let q = Query {
                    text: text.into(),
                    filters: filters.clone(),
                    allowed,
                    limit: 1,
                    fuzzy: true,
                    ..Default::default()
                };
                let needle = filex::catalog::normalize::nfc_fold(text);
                let expected: Vec<_> = all
                    .iter()
                    .filter_map(|r| {
                        if r.parent == 0 {
                            return None;
                        }
                        let tier = query::literal_tier(&r.name, &needle)?;
                        let native = filex::catalog::segment::os_name(&r.name);
                        let name = native.to_string_lossy();
                        if !q.filters.iter().all(|f| {
                            f.matches(&ItemMeta {
                                name: &name,
                                is_dir: r.is_dir(),
                                size: r.size,
                                mtime: r.mtime,
                            })
                        }) {
                            return None;
                        }
                        let path = view.path(r.id)?;
                        if q.allowed
                            .as_ref()
                            .is_some_and(|paths| !paths.contains(&path))
                        {
                            return None;
                        }
                        Some((r.id, path, tier))
                    })
                    .collect();
                let mut actual = Vec::new();
                let mut total = None;
                query::stream(&view, &q, &AtomicBool::new(false), |batch| {
                    actual.extend(batch.hits.into_iter().map(|h| (h.id, h.path, h.tier)));
                    total = batch.total;
                    Ok(())
                })
                .unwrap();
                assert_eq!(actual, expected, "{q:?}");
                assert_eq!(total, Some(expected.len() as u64));
            }
        }
    }
}

#[test]
fn magic_metadata_skips_non_candidates_and_cancellation_never_completes() {
    use filex::search_filter::{Bound, Filter};
    let roots = vec![Root {
        id: 1,
        path: "/test".into(),
        device: 1,
    }];
    let records = (1..=20001).map(|id| {
        record(
            id,
            if id == 1 { 0 } else { 1 },
            if id == 19999 {
                b"rare.pdf"
            } else {
                b"common.txt"
            },
            id == 1,
        )
    });
    let view = View {
        base: Arc::new(Segment::build(records, roots.clone(), 1).unwrap()),
        roots,
        layers: vec![],
        epoch: 1,
    };
    let q = Query {
        filters: vec![Filter::Ext("pdf".into()), Filter::Size(Bound::Eq(19999))],
        ..Default::default()
    };
    let mut count = 0;
    query::stream(&view, &q, &AtomicBool::new(false), |batch| {
        count += batch.hits.len();
        assert!(batch.scanned <= 1);
        Ok(())
    })
    .unwrap();
    assert_eq!(count, 1);
    let cancel = AtomicBool::new(true);
    assert!(
        query::stream(&view, &q, &cancel, |_| panic!(
            "cancelled query emitted a batch"
        ))
        .is_err()
    );
    cancel.store(false, Ordering::Relaxed);
    let mut complete = false;
    assert!(
        query::stream(&view, &Query::default(), &cancel, |batch| {
            complete |= batch.total.is_some();
            cancel.store(true, Ordering::Relaxed);
            Ok(())
        })
        .is_err()
    );
    assert!(!complete);
    assert!(
        query::stream(
            &view,
            &Query {
                epoch_hint: Some(999),
                ..q
            },
            &AtomicBool::new(false),
            |_| Ok(())
        )
        .is_err()
    );
}

/// FIL-22: an inherited `SIG_IGN` (POSIX keeps ignored dispositions across
/// `exec`) must not stop launchd/systemd from shutting the daemon down
/// cleanly — it has to exit 0 and remove its endpoint like the in-process stop.
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn daemon_shuts_down_cleanly_on_sigterm_and_sigint_even_if_inherited_ignored() {
    use std::os::unix::process::CommandExt;
    for signal in [libc::SIGTERM, libc::SIGINT] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("files");
        let data = dir.path().join("index");
        std::fs::create_dir(&root).unwrap();
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_filex-indexd"));
        command
            .arg("--data-dir")
            .arg(&data)
            .arg(&root)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        // SAFETY: signal() is async-signal-safe, as pre_exec requires.
        unsafe {
            command.pre_exec(|| {
                libc::signal(libc::SIGTERM, libc::SIG_IGN);
                libc::signal(libc::SIGINT, libc::SIG_IGN);
                Ok(())
            });
        }
        let mut child = command.spawn().unwrap();
        wait_until(|| Client::connect(&data).is_ok());
        assert_eq!(unsafe { libc::kill(child.id() as i32, signal) }, 0);
        let start = Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break Some(status);
            }
            if start.elapsed() > Duration::from_secs(10) {
                break None;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let _ = child.kill();
        let _ = child.wait();
        assert!(
            status.is_some_and(|s| s.success()),
            "signal {signal}: {status:?}"
        );
        assert!(!data.join("endpoint.json").exists());
    }
}

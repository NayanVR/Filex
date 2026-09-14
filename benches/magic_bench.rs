//! Keystroke-path cost of the natural-language passes: phrase expansion
//! and Magic-mode command parsing (docs/design-magic-mode.md).
//!
//! Both of these are pure string work over a query a person typed — tens
//! of characters, not the 200k-entry arena the index benches scan — so
//! the numbers here should land in nanoseconds. The point is not to
//! optimize them; it is that CLAUDE.md requires anything plausibly
//! touching search-as-you-type latency to be measured rather than
//! assumed, and two changes qualify:
//!
//! - `phrases::expand`'s window grew from 3 words to 4 to fit `older
//!   than 30 days`, so every query word now costs one more table probe.
//!   `expand_plain` is the guard: a query that matches no phrase at all
//!   walks the full window at every position, which is the worst case,
//!   and it is the case ordinary filename search hits on every keystroke.
//! - `magic::parse` would run per keystroke once the Magic card is
//!   wired. `parse_rejects_filename` is the shape that matters — a plain
//!   filename query has no verb, so it must bail immediately.

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;

/// A fixed instant so relative-date parsing is deterministic across runs.
const NOW: i64 = 1_785_067_200;

fn bench_phrases(c: &mut Criterion) {
    let mut group = c.benchmark_group("phrases");

    // Worst case, and the common one: nothing matches, so every position
    // probes the full 4-word window before falling through to text.
    group.bench_function("expand_plain", |b| {
        b.iter(|| {
            black_box(filex::phrases::expand(
                black_box("quarterly earnings deck"),
                NOW,
            ))
        })
    });

    group.bench_function("expand_matching", |b| {
        b.iter(|| {
            black_box(filex::phrases::expand(
                black_box("photos from last week"),
                NOW,
            ))
        })
    });

    // The four-word comparative that motivated widening the window.
    group.bench_function("expand_comparative", |b| {
        b.iter(|| {
            black_box(filex::phrases::expand(
                black_box("screenshots older than 30 days"),
                NOW,
            ))
        })
    });

    group.finish();
}

fn bench_magic(c: &mut Criterion) {
    let mut group = c.benchmark_group("magic");

    // No verb in the first word ⇒ immediate `None`. This is what every
    // ordinary search keystroke pays.
    group.bench_function("parse_rejects_filename", |b| {
        b.iter(|| {
            black_box(filex::magic::parse(
                black_box("quarterly earnings deck"),
                NOW,
            ))
        })
    });

    group.bench_function("parse_delete", |b| {
        b.iter(|| {
            black_box(filex::magic::parse(
                black_box("delete screenshots older than 30 days"),
                NOW,
            ))
        })
    });

    group.bench_function("parse_move", |b| {
        b.iter(|| {
            black_box(filex::magic::parse(
                black_box("move pdfs modified this week to Documents"),
                NOW,
            ))
        })
    });

    group.finish();
}

/// Compare a small folder preview with a whole-index literal stream.
fn bench_magic_stream(c: &mut Criterion) {
    use filex::{
        catalog::segment::{Identity, Record, Root, Segment},
        daemon::{ipc::Query, query, view::View},
    };
    use std::sync::{Arc, atomic::AtomicBool};
    let roots = vec![Root {
        id: 1,
        path: "/bench".into(),
        device: 1,
    }];
    let record = |id, parent, name: Vec<u8>, flags| Record {
        id,
        parent,
        root: 1,
        name,
        flags,
        identity: Identity {
            device: 1,
            key: id,
            birth: 1,
        },
        size: Some(1),
        mtime: Some(NOW),
    };
    let mut records = vec![
        record(1, 0, b"bench".to_vec(), 1),
        record(2, 1, b"work".to_vec(), 1),
    ];
    for id in 3..100003 {
        records.push(record(
            id,
            if id < 103 { 2 } else { 1 },
            format!("document-{id}.pdf").into_bytes(),
            0,
        ));
    }
    let view = View {
        base: Arc::new(Segment::build(records, roots.clone(), 1).unwrap()),
        layers: vec![],
        roots,
        epoch: 1,
    };
    let mut group = c.benchmark_group("magic_stream");
    for (label, scope) in [
        ("folder_100_of_100k", Some("/bench/work".into())),
        ("all_100k", None),
    ] {
        let q = Query {
            text: "document".into(),
            scope,
            ..Default::default()
        };
        group.bench_function(label, |b| {
            b.iter(|| {
                query::stream(&view, &q, &AtomicBool::new(false), |batch| {
                    black_box(batch);
                    Ok(())
                })
                .unwrap();
            })
        });
    }
    let q = Query {
        text: "document-99999".into(),
        ..Default::default()
    };
    group.bench_function("rare_name_100k", |b| {
        b.iter(|| {
            query::stream(&view, &q, &AtomicBool::new(false), |batch| {
                black_box(batch);
                Ok(())
            })
            .unwrap();
        })
    });
    group.finish();
}

criterion_group!(benches, bench_phrases, bench_magic, bench_magic_stream);
criterion_main!(benches);

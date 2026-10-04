//! Local-only index gate runner; stdout contains aggregates, never corpus strings.
use anyhow::{Result, bail, ensure};
use filex::{
    catalog::segment::Segment,
    index_lab::{corpus, literal::LiteralIndex, workload},
};
use std::{path::Path, time::Instant};

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() == 3 && args[0] == "verify" {
        let old = unsafe { Segment::open(Path::new(&args[1]))? };
        let new = unsafe { Segment::open(Path::new(&args[2]))? };
        ensure!(
            old.len() == new.len() && old.roots == new.roots && old.sequence == new.sequence,
            "catalog header changed"
        );
        let mut binary_spool_record_bytes = 0u64;
        let mut json_spool_record_bytes = 0u64;
        for slot in 0..old.len() {
            let record = old.record(slot);
            binary_spool_record_bytes += 66 + record.name.len() as u64;
            json_spool_record_bytes += serde_json::to_vec(&record)?.len() as u64 + 1;
            ensure!(
                record == new.record(slot),
                "record changed at ordinal {slot}"
            );
        }
        ensure!(
            old.search.name_count() == new.search.name_count(),
            "name count changed"
        );
        ensure!(
            old.metadata_postings_equal(&new),
            "metadata postings changed"
        );
        for name in 0..old.search.name_count() as u32 {
            ensure!(
                old.search.name(name) == new.search.name(name)
                    && old.search.files(name).eq(new.search.files(name)),
                "name/postings changed at rank {name}"
            );
        }
        for (id, query) in workload::queries().iter().enumerate() {
            for limit in [1, 100, 202, 1000] {
                ensure!(
                    new.search.search(query, limit) == old.search.oracle(query, limit),
                    "rank mismatch at fixture {id}, limit {limit}"
                );
            }
            ensure!(
                old.search.substring(query, old.search.name_count())
                    == new.search.substring(query, new.search.name_count()),
                "recall mismatch at fixture {id}"
            );
            ensure!(
                old.search.fuzzy_names(query) == new.search.fuzzy_names(query),
                "fuzzy mismatch at fixture {id}"
            );
        }
        println!(
            "{}",
            serde_json::json!({"entries":new.len(),"names":new.search.name_count(),"records_and_postings_equal":true,"rank_recall_and_fuzzy_equal":true,"binary_spool_record_bytes":binary_spool_record_bytes,"json_spool_record_bytes":json_spool_record_bytes})
        );
        return Ok(());
    }
    if args.len() == 3 && args[0] == "compact-isolated" {
        let start = Instant::now();
        filex::daemon::builder::compact_to(Path::new(&args[1]), Path::new(&args[2]))?;
        println!(
            "{}",
            serde_json::json!({
                "compaction_seconds": start.elapsed().as_secs_f64(),
                "segment_bytes": std::fs::metadata(&args[2])?.len(),
            })
        );
        return Ok(());
    }
    if args.len() == 3 && args[0] == "build-json" {
        use std::io::BufRead;
        let reader = std::io::BufReader::new(std::fs::File::open(&args[1])?);
        let records = reader.lines().map(|l| {
            serde_json::from_str::<filex::catalog::segment::Record>(&l.expect("record I/O"))
                .expect("valid record")
        });
        let start = Instant::now();
        let index = Segment::build(
            records,
            vec![filex::catalog::segment::Root {
                id: 1,
                path: "/corpus".into(),
                device: 0,
            }],
            0,
        )?;
        index.save(Path::new(&args[2]))?;
        println!(
            "{}",
            serde_json::json!({"entries":index.len(),"search_bytes":index.search.bytes(),"build_seconds":start.elapsed().as_secs_f64(),"segment_bytes":std::fs::metadata(&args[2])?.len()})
        );
        return Ok(());
    }
    if args.len() == 3 && args[0] == "compact" {
        let base = unsafe { Segment::open(Path::new(&args[1]))? };
        let view = filex::daemon::view::View {
            roots: base.roots.clone(),
            epoch: base.sequence,
            base: std::sync::Arc::new(base),
            layers: vec![],
        };
        let start = Instant::now();
        let next = Segment::build(view.records(), view.roots.clone(), view.epoch)?;
        next.save(Path::new(&args[2]))?;
        println!(
            "{}",
            serde_json::json!({"entries":next.len(),"compaction_seconds":start.elapsed().as_secs_f64(),"segment_bytes":std::fs::metadata(&args[2])?.len()})
        );
        return Ok(());
    }
    if args.len() != 2 {
        bail!(
            "usage: filex-index-lab <analyze|literal|fm|mapped|mixed|typing|magic|profile|blocks> SEGMENT; <compact|compact-isolated|verify> INPUT OUTPUT; build-json RECORDS OUTPUT"
        );
    }
    let load_start = Instant::now();
    let index = unsafe { Segment::open(Path::new(&args[1]))? };
    let load_seconds = load_start.elapsed().as_secs_f64();
    if args[0] == "magic" {
        use filex::{
            daemon::{ipc::Query, query, view::View},
            listing::FileKind,
            search_filter::{Bound, Filter},
        };
        use std::sync::{Arc, atomic::AtomicBool};
        let view = View {
            roots: index.roots.clone(),
            epoch: index.sequence,
            base: Arc::new(index),
            layers: vec![],
        };
        let shapes = vec![
            ("", vec![Filter::Ext("pdf".into())]),
            (
                "",
                vec![
                    Filter::Ext("pdf".into()),
                    Filter::Size(Bound::Gt(10 * 1024 * 1024)),
                ],
            ),
            (
                "",
                vec![
                    Filter::Kind(FileKind::Image),
                    Filter::Modified(Bound::Ge(1704067200)),
                ],
            ),
            ("", vec![Filter::Ext("filex-unmatched-extension".into())]),
            ("", vec![Filter::Size(Bound::Gt(1024 * 1024 * 1024))]),
            ("", vec![Filter::Modified(Bound::Lt(0))]),
            ("screenshot", vec![Filter::Ext("png".into())]),
            (".rs", vec![Filter::Size(Bound::Gt(1024 * 1024))]),
            ("config", vec![Filter::Kind(FileKind::Directory)]),
            ("a", vec![]),
            ("", vec![Filter::Ext("pdf".into())]),
            ("config", vec![Filter::Kind(FileKind::Directory)]),
        ];
        println!(
            "{}",
            serde_json::json!({"entries":view.base.len(),"load_seconds":load_seconds})
        );
        for (id, (text, filters)) in shapes.into_iter().enumerate() {
            let q = Query {
                text: text.into(),
                filters,
                scope: (id >= 10).then(|| view.roots[0].path.clone()),
                fuzzy: false,
                ..Default::default()
            };
            let mut expected = None;
            for old in [true, false] {
                let mut times = Vec::new();
                let mut first_times = Vec::new();
                let mut final_scanned = 0;
                let mut final_count = 0;
                let mut final_complete = false;
                for _ in 0..3 {
                    let start = Instant::now();
                    let mut hits = Vec::new();
                    let mut complete = false;
                    let mut stopped = false;
                    let mut first = None;
                    let mut emit = |batch: filex::daemon::ipc::Batch| {
                        if first.is_none() && !batch.hits.is_empty() {
                            first = Some(start.elapsed().as_secs_f64() * 1000.0);
                        }
                        final_scanned = batch.scanned;
                        complete = batch.total.is_some();
                        hits.extend(batch.hits.into_iter().map(|h| (h.id, h.path, h.tier)));
                        if hits.len() > filex::magic::MAX_PLAN_OPS && !complete {
                            stopped = true;
                            anyhow::bail!("preview cap reached");
                        }
                        Ok(())
                    };
                    let cancel = AtomicBool::new(false);
                    let result = if old {
                        query::stream_before_indexed_magic(&view, &q, &cancel, &mut emit)
                    } else {
                        query::stream(&view, &q, &cancel, &mut emit)
                    };
                    if !stopped {
                        result?;
                    }
                    let elapsed = start.elapsed().as_secs_f64() * 1000.0;
                    hits.truncate(filex::magic::MAX_PLAN_OPS + 1);
                    if let Some(expected) = &expected {
                        ensure!(&hits == expected, "Magic result mismatch at fixture {id}");
                    } else {
                        expected = Some(hits.clone());
                    }
                    final_count = hits.len();
                    final_complete = complete;
                    times.push(elapsed);
                    first_times.push(first);
                }
                println!(
                    "{}",
                    serde_json::json!({"fixture_query_id":id,"before":old,"milliseconds":times,"first_hit_ms":first_times,"scanned":final_scanned,"hits":final_count,"complete":final_complete,"matches_baseline":true})
                );
            }
        }
        return Ok(());
    }
    if args[0] == "blocks" {
        use filex::search::blocks::BlockIndex;
        let queries = workload::queries();
        let expected: Vec<_> = queries
            .iter()
            .map(|q| index.search.oracle(q, 202))
            .collect();
        for (group, buckets) in [(16, 1024), (32, 2048), (64, 4096), (32, 4096), (64, 8192)] {
            let start = Instant::now();
            let blocks = BlockIndex::build(
                (0..index.search.name_count() as u32).map(|id| index.search.name(id)),
                group,
                buckets,
            );
            let starts = index.search.boundary_blocks(group, 1024);
            let build_seconds = start.elapsed().as_secs_f64();
            let mut worst = 0f64;
            for (qid, query) in queries.iter().enumerate() {
                assert_eq!(
                    index.search.search_blocks(&blocks, &starts, query, 202),
                    expected[qid],
                    "query {qid}"
                );
                let mut samples = Vec::new();
                for _ in 0..50 {
                    let start = Instant::now();
                    std::hint::black_box(index.search.search_blocks(&blocks, &starts, query, 202));
                    samples.push(start.elapsed().as_secs_f64() * 1e6);
                }
                samples.sort_by(f64::total_cmp);
                worst = worst.max(samples[47]);
                println!(
                    "{}",
                    serde_json::json!({"group":group,"buckets":buckets,"query_id":qid,"p95_micros":samples[47]})
                );
            }
            println!(
                "{}",
                serde_json::json!({"group":group,"buckets":buckets,"substring_signature_bytes":blocks.bytes(),"boundary_signature_bytes":starts.bytes(),"accelerator_bytes":blocks.bytes()+starts.bytes(),"build_seconds":build_seconds,"worst_p95_micros":worst,"oracle_matches":true})
            );
        }
        return Ok(());
    }
    if args[0] == "mapped" || args[0] == "mixed" || args[0] == "typing" || args[0] == "profile" {
        println!(
            "{}",
            serde_json::json!({"load_seconds":load_seconds,"entries":index.len()})
        );
        let view = filex::daemon::view::View {
            roots: index.roots.clone(),
            epoch: index.sequence,
            base: std::sync::Arc::new(index),
            layers: vec![],
        };
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let hot = Default::default();
        if args[0] == "mixed" || args[0] == "typing" {
            let queries: Vec<_> = workload::queries()
                .into_iter()
                .flat_map(|text| {
                    if args[0] == "typing" {
                        text.char_indices()
                            .map(|(at, c)| text[..at + c.len_utf8()].to_owned())
                            .collect::<Vec<_>>()
                    } else {
                        vec![text]
                    }
                })
                .map(|text| filex::daemon::ipc::Query {
                    text,
                    ..Default::default()
                })
                .collect();
            let mut timings = vec![Vec::new(); queries.len()];
            let mut first_pass = Vec::new();
            // Rotate the starting fixture each round; preserve each prefix's
            // progression within a typing fixture. No identical-query warming.
            for round in 0..20 {
                for offset in 0..queries.len() {
                    let id = (offset + round * 7) % queries.len();
                    let start = Instant::now();
                    std::hint::black_box(filex::daemon::query::search(
                        &view,
                        &queries[id],
                        &cancel,
                        &hot,
                    )?);
                    let elapsed = start.elapsed().as_secs_f64() * 1e6;
                    timings[id].push(elapsed);
                    if round == 0 {
                        first_pass.push(elapsed);
                    }
                }
            }
            let mut all: Vec<_> = timings.iter().flatten().copied().collect();
            all.sort_by(f64::total_cmp);
            first_pass.sort_by(f64::total_cmp);
            println!(
                "{}",
                serde_json::json!({"queries":all.len(),"p50_micros":all[all.len()/2],"p95_micros":all[all.len()*95/100],"p99_micros":all[all.len()*99/100],"first_pass_p95_micros":first_pass[first_pass.len()*95/100]})
            );
            for (id, samples) in timings.iter_mut().enumerate() {
                samples.sort_by(f64::total_cmp);
                println!(
                    "{}",
                    serde_json::json!({"fixture_query_id":id,"p50_micros":samples[9],"p95_micros":samples[18]})
                );
            }
        } else {
            for (id, text) in workload::queries().into_iter().enumerate() {
                if args[0] == "profile" {
                    let start = Instant::now();
                    let estimate = view
                        .base
                        .search
                        .estimate(filex::catalog::normalize::nfc_fold(&text).as_bytes());
                    let estimate_us = start.elapsed().as_secs_f64() * 1e6;
                    let start = Instant::now();
                    let names = view.base.search.search(&text, 202).len();
                    let search_us = start.elapsed().as_secs_f64() * 1e6;
                    let start = Instant::now();
                    let fuzzy = view.base.search.fuzzy_names(&text).len();
                    println!(
                        "{}",
                        serde_json::json!({"query_id":id,"estimate":estimate,"estimate_us":estimate_us,"names":names,"search_us":search_us,"fuzzy":fuzzy,"fuzzy_us":start.elapsed().as_secs_f64()*1e6})
                    );
                }
                let query = filex::daemon::ipc::Query {
                    text,
                    ..Default::default()
                };
                let mut samples = Vec::new();
                let mut hits = 0;
                let mut examined = 0;
                for _ in 0..50 {
                    let start = Instant::now();
                    let page = filex::daemon::query::search(&view, &query, &cancel, &hot)?;
                    samples.push(start.elapsed().as_secs_f64() * 1e6);
                    hits = page.hits.len();
                    examined = page.examined;
                }
                samples.sort_by(f64::total_cmp);
                println!(
                    "{}",
                    serde_json::json!({"fixture_query_id":id,"p50_micros":samples[24],"p95_micros":samples[47],"hits":hits,"examined":examined})
                );
            }
        }
        #[cfg(unix)]
        {
            let output = std::process::Command::new("ps")
                .args(["-o", "rss=", "-p", &std::process::id().to_string()])
                .output()?;
            if let Ok(kib) = String::from_utf8_lossy(&output.stdout)
                .trim()
                .parse::<u64>()
            {
                println!("{}", serde_json::json!({"warm_rss_bytes":kib*1024}));
            }
        }
        return Ok(());
    } else if args[0] == "analyze" {
        println!(
            "{}",
            serde_json::to_string_pretty(&corpus::analyze(
                (0..index.len()).filter(|&i| index.parent(i) != 0).map(|i| {
                    filex::index_lab::corpus::Record {
                        raw: index.raw_name(i),
                        directory: index.is_dir(i),
                        size: index.size(i),
                        mtime: index.mtime(i),
                    }
                })
            ))?
        );
    } else if args[0] == "literal" || args[0] == "fm" {
        let start = Instant::now();
        let names: Vec<String> = (0..index.len())
            .filter(|&i| index.parent(i) != 0)
            .filter_map(|i| String::from_utf8(index.raw_name(i).into_owned()).ok())
            .collect();
        let literal = LiteralIndex::build_suffix(names.iter().map(String::as_str))?;
        drop(names);
        drop(index);
        println!(
            "{}",
            serde_json::json!({"names": literal.name_count(), "suffixes": literal.suffix_count(), "bytes": literal.bytes(), "wavelet_bytes": literal.wavelet_bytes(), "build_seconds": start.elapsed().as_secs_f64()})
        );
        if args[0] == "fm" {
            let mut selected = false;
            let mut stop = false;
            for wavelet in [false, true] {
                let comparison = workload::compare_fm(&literal, wavelet);
                if comparison.passes_latency_and_saving {
                    selected = true;
                    stop |= comparison.total_search_bytes > 150 * 1024 * 1024;
                }
                println!("{}", serde_json::to_string(&comparison)?);
                ensure!(
                    comparison.ranges_and_top_100_match,
                    "FM correctness gate failed"
                );
            }
            if !selected {
                stop = literal.bytes() > 150 * 1024 * 1024;
            }
            ensure!(!stop, "Stage 2 combined search storage budget exceeded");
            return Ok(());
        }
        let mut passed = true;
        for (id, query) in workload::queries().iter().enumerate() {
            let report = workload::measure(&literal, query, id);
            passed &= report.rank_matches_oracle
                && report.file_rank_matches_oracle
                && report.complete_literal_recall
                && report.p95_micros <= 10_000.0;
            println!("{}", serde_json::to_string(&report)?);
        }
        ensure!(
            passed,
            "Stage 1 real-corpus correctness/latency gate failed"
        );
    } else {
        bail!("unknown index-lab mode");
    }
    Ok(())
}

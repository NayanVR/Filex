//! Local-only v2 gate runner; stdout contains aggregates, never corpus strings.
use anyhow::{Result, bail, ensure};
use filex::{
    catalog::segment::Segment,
    index_lab::{corpus, literal::LiteralIndex, workload},
};
use std::{path::Path, time::Instant};

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
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
        bail!("usage: filex-index-lab <analyze|literal|fm|mapped> V2_SEGMENT");
    }
    let load_start = Instant::now();
    let index = unsafe { Segment::open(Path::new(&args[1]))? };
    let load_seconds = load_start.elapsed().as_secs_f64();
    if args[0] == "mapped" {
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
        for (id, text) in workload::queries().into_iter().enumerate() {
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
                serde_json::json!({"fixture_query_id":id,"p95_micros":samples[47],"hits":hits,"examined":examined})
            );
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
        let literal = LiteralIndex::build(
            (0..index.len())
                .filter(|&i| index.parent(i) != 0)
                .map(|i| filex::index_lab::corpus::Record {
                    raw: index.raw_name(i),
                    directory: index.is_dir(i),
                    size: index.size(i),
                    mtime: index.mtime(i),
                })
                .filter_map(|record| std::str::from_utf8(record.raw).ok()),
        )?;
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
        bail!("usage: filex-index-lab <analyze|literal|fm|mapped> V2_SEGMENT");
    }
    Ok(())
}

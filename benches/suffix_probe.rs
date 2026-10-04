//! Stage 1 packed suffix + wavelet benchmark. No v1 runtime search path.
//! Run: cargo bench --no-default-features --features index-v2-lab --bench suffix_probe
use filex::index_lab::{literal::LiteralIndex, workload};
use std::time::Instant;
fn main() {
    let dirs = std::env::var("FILEX_SUFFIX_DIRS")
        .ok()
        .map(|v| v.parse::<usize>().expect("integer FILEX_SUFFIX_DIRS"))
        .unwrap_or(5000);
    let names = workload::synthetic(dirs);
    let start = Instant::now();
    let index = LiteralIndex::build_suffix(names.iter().map(String::as_str)).unwrap();
    drop(names);
    println!(
        "{}",
        serde_json::json!({"names": index.name_count(), "suffixes": index.suffix_count(), "bytes": index.bytes(), "wavelet_bytes": index.wavelet_bytes(), "build_seconds": start.elapsed().as_secs_f64()})
    );
    for (id, query) in workload::queries().iter().enumerate() {
        let report = workload::measure(&index, query, id);
        println!("{}", serde_json::to_string(&report).unwrap());
        assert!(
            report.rank_matches_oracle
                && report.file_rank_matches_oracle
                && report.complete_literal_recall,
            "fixture {id} correctness gate failed"
        );
    }
    if dirs == 5000 {
        assert!(
            index.bytes() <= 100 * 1024 * 1024,
            "Stage 1 505,000-name memory gate failed"
        );
    }
}

//! Checked-in synthetic shapes; never derived from developer filenames.
use super::literal::LiteralIndex;
use serde::Serialize;
use std::{hint::black_box, time::Instant};

pub fn synthetic(directories: usize) -> Vec<String> {
    const STEMS: [&str; 10] = [
        "report",
        "invoice",
        "photo",
        "backup",
        "notes",
        "main",
        "config",
        "readme",
        "data",
        "screenshot",
    ];
    const EXTS: [&str; 5] = ["txt", "rs", "pdf", "png", "tar.gz"];
    let mut names = Vec::with_capacity(directories * 101);
    for directory in 0..directories {
        names.push(format!("project-{directory:04}"));
        for file in 0..100 {
            names.push(format!(
                "{}_{directory:04}_{file:03}.{}",
                STEMS[(directory + file) % STEMS.len()],
                EXTS[file % EXTS.len()]
            ));
        }
    }
    names
}

pub fn queries() -> Vec<String> {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("../../tests/fixtures/index-v2/workload.json")).unwrap();
    fixture["queries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|q| q.as_str().unwrap().to_owned())
        .collect()
}

#[derive(Serialize)]
pub struct QueryShape {
    pub fixture_query_id: usize,
    pub query_bytes: usize,
    pub tier_reached: Option<u8>,
    pub filter_kinds: Vec<u8>,
    pub matching_names: usize,
    pub returned_names: usize,
    pub p50_micros: f64,
    pub p95_micros: f64,
    pub cancelled: bool,
    pub rank_matches_oracle: bool,
    pub file_rank_matches_oracle: bool,
    pub complete_literal_recall: bool,
}

/// Counts and timings only. Even a caller-supplied private query cannot appear
/// in the serialized observation. This lab has no UI telemetry hook.
pub fn measure(index: &LiteralIndex, query: &str, fixture_query_id: usize) -> QueryShape {
    let expected = index.oracle(query, 100);
    let actual = index.search(query, 100);
    let expected_files: Vec<_> = expected
        .iter()
        .flat_map(|&(tier, name)| index.files(name).map(move |file| (tier, name, file)))
        .take(100)
        .collect();
    let file_rank_matches_oracle = index.search_files(query, 100) == expected_files;
    let mut all = index
        .oracle(query, usize::MAX)
        .into_iter()
        .map(|(_, id)| id)
        .collect::<Vec<_>>();
    all.sort_unstable();
    let complete_literal_recall = index.substring(query, index.name_count()) == all;
    for _ in 0..10 {
        black_box(index.search_files(black_box(query), 100));
    }
    let mut samples = Vec::with_capacity(200);
    for _ in 0..200 {
        let start = Instant::now();
        black_box(index.search_files(black_box(query), 100));
        samples.push(start.elapsed().as_secs_f64() * 1_000_000.0);
    }
    samples.sort_by(f64::total_cmp);
    QueryShape {
        fixture_query_id,
        query_bytes: query.len(),
        tier_reached: actual.last().map(|(tier, _)| *tier as u8),
        filter_kinds: Vec::new(),
        matching_names: all.len(),
        returned_names: actual.len(),
        p50_micros: samples[99],
        p95_micros: samples[189],
        cancelled: false,
        rank_matches_oracle: actual == expected,
        file_rank_matches_oracle,
        complete_literal_recall,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn observations_do_not_serialize_query_text() {
        let index = LiteralIndex::build(["test.txt"]).unwrap();
        let report = measure(&index, "private-query-sentinel", 0);
        let output = serde_json::to_string(&report).unwrap();
        assert!(!output.contains("private-query-sentinel"));
        assert!(!output.contains("test.txt"));
    }
}

//! Privacy-safe aggregate corpus measurement. No name/path-bearing report fields.
use super::normalize::{boundaries, nfc_fold, nfkc_fold};
pub struct Record<'a> {
    pub raw: std::borrow::Cow<'a, [u8]>,
    pub directory: bool,
    pub size: Option<u64>,
    pub mtime: Option<i64>,
}
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

#[derive(Default, Serialize)]
pub struct NormalizationStats {
    pub unique_names: usize,
    pub total_bytes: u64,
    pub unique_bytes: u64,
}

#[derive(Default, Serialize)]
pub struct CorpusReport {
    pub entries: usize,
    pub files: usize,
    pub directories: usize,
    pub invalid_utf8: usize,
    pub unique_raw_names: usize,
    pub raw_bytes: u64,
    pub unique_raw_bytes: u64,
    pub nfc_full_fold: NormalizationStats,
    pub nfkc_full_fold: NormalizationStats,
    pub normalized_byte_length_histogram: BTreeMap<u32, usize>,
    pub folded_frequency_histogram: BTreeMap<u32, usize>,
    pub unique_folded_suffixes: u64,
    pub unique_folded_boundaries: usize,
    pub extension_cardinality: usize,
    pub known_metadata: usize,
    pub size_bucket_cardinality: usize,
    pub mtime_day_cardinality: usize,
    pub estimated_packed_suffix_bytes: u64,
    pub estimated_wavelet_bytes: u64,
    pub estimated_fm_bitplane_bytes: u64,
    pub estimated_packed_posting_bytes: u64,
}

fn bucket(n: usize) -> u32 {
    if n == 0 {
        0
    } else {
        usize::BITS - n.leading_zeros()
    }
}
fn wavelet_bytes(len: u64, levels: u32) -> u64 {
    // u64 bit words plus u32 rank checkpoints every 512 bits.
    (len.div_ceil(64) * 8 + (len / 512 + 1) * 4) * u64::from(levels)
}

pub fn analyze<'a>(records: impl Iterator<Item = Record<'a>>) -> CorpusReport {
    let mut report = CorpusReport::default();
    let mut raw = HashSet::new();
    let mut nfc = HashMap::<String, (usize, BTreeSet<usize>)>::new();
    let mut nfkc = HashSet::new();
    let mut extensions = HashSet::new();
    let mut sizes = HashSet::new();
    let mut days = HashSet::new();
    for record in records {
        report.entries += 1;
        report.directories += usize::from(record.directory);
        report.files += usize::from(!record.directory);
        report.raw_bytes += record.raw.len() as u64;
        raw.insert(record.raw.clone().into_owned());
        if let Some(size) = record.size {
            report.known_metadata += 1;
            sizes.insert(if size == 0 {
                0
            } else {
                64 - size.leading_zeros()
            });
        }
        if let Some(mtime) = record.mtime {
            days.insert(mtime.div_euclid(86400));
        }
        let Ok(name) = std::str::from_utf8(&record.raw) else {
            report.invalid_utf8 += 1;
            continue;
        };
        let folded = nfc_fold(name);
        if let Some((_, ext)) = folded.rsplit_once('.') {
            extensions.insert(ext.to_owned());
        }
        report.nfc_full_fold.total_bytes += folded.len() as u64;
        *report
            .normalized_byte_length_histogram
            .entry(bucket(folded.len()))
            .or_default() += 1;
        let (count, starts) = nfc.entry(folded).or_default();
        *count += 1;
        starts.extend(
            boundaries(name)
                .into_iter()
                .map(|p| nfc_fold(&name[..p]).len()),
        );
        let compatibility = nfkc_fold(name);
        report.nfkc_full_fold.total_bytes += compatibility.len() as u64;
        nfkc.insert(compatibility);
    }
    report.unique_raw_names = raw.len();
    report.unique_raw_bytes = raw.iter().map(|n| n.len() as u64).sum();
    report.nfc_full_fold.unique_names = nfc.len();
    report.nfc_full_fold.unique_bytes = nfc.keys().map(|n| n.len() as u64).sum();
    report.nfkc_full_fold.unique_names = nfkc.len();
    report.nfkc_full_fold.unique_bytes = nfkc.iter().map(|n| n.len() as u64).sum();
    for (count, _) in nfc.values() {
        *report
            .folded_frequency_histogram
            .entry(bucket(*count))
            .or_default() += 1;
    }
    report.unique_folded_suffixes = report.nfc_full_fold.unique_bytes;
    report.unique_folded_boundaries = nfc.values().map(|(_, starts)| starts.len()).sum();
    report.extension_cardinality = extensions.len();
    report.size_bucket_cardinality = sizes.len();
    report.mtime_day_cardinality = days.len();
    report.estimated_packed_suffix_bytes = report.unique_folded_suffixes * 4;
    report.estimated_wavelet_bytes = wavelet_bytes(
        report.unique_folded_suffixes,
        bucket(nfc.len().saturating_sub(1)),
    );
    report.estimated_fm_bitplane_bytes =
        wavelet_bytes(report.unique_folded_suffixes + nfc.len() as u64, 8);
    report.estimated_packed_posting_bytes = report.entries as u64 * 4;
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn aggregates_duplicates_invalid_names_and_known_zero_metadata() {
        let names: &[&[u8]] = &[
            b"Index.rs",
            b"index.rs",
            "Straße".as_bytes(),
            b"STRASSE",
            &[255],
        ];
        let report = analyze(names.iter().map(|raw| Record {
            raw: (*raw).into(),
            directory: false,
            size: Some(0),
            mtime: Some(-1),
        }));
        assert_eq!(report.entries, 5);
        assert_eq!(report.invalid_utf8, 1);
        assert_eq!(report.unique_raw_names, 5);
        assert_eq!(report.nfc_full_fold.unique_names, 2);
        assert_eq!(report.known_metadata, 5);
        assert_eq!(report.mtime_day_cardinality, 1);
        let output = serde_json::to_string(&report).unwrap();
        for name in ["Index.rs", "index.rs", "Straße", "STRASSE"] {
            assert!(!output.contains(name));
        }
    }
}

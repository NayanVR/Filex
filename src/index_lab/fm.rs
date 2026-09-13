//! Two experimental FM representations. Neither is selected for production until
//! real-corpus latency and total-memory gates pass. No locate/extract samples are
//! needed: results resolve through the document matrix and name dictionary.
use crate::search::wavelet::WaveletMatrix;
use std::ops::Range;

const ALPHABET: usize = 257;
const BLOCK: usize = 512;

pub enum Occurrences {
    /// Cache-local sequential scan after a sampled symbol-count checkpoint.
    Sampled {
        bwt: Vec<u16>,
        checkpoints: Vec<[u32; ALPHABET]>,
    },
    /// Succinct wavelet-matrix symbol rank; up to nine bitplanes, no retained BWT.
    Wavelet(WaveletMatrix),
}
pub struct FmIndex {
    occurrences: Occurrences,
    cumulative: [usize; ALPHABET],
    len: usize,
    skip: usize,
}
impl FmIndex {
    pub fn sampled(bwt: Vec<u16>, skip: usize) -> Self {
        let mut counts = [0u32; ALPHABET];
        let mut checkpoints = Vec::with_capacity(bwt.len() / BLOCK + 1);
        for (position, &symbol) in bwt.iter().enumerate() {
            if position % BLOCK == 0 {
                checkpoints.push(counts);
            }
            counts[symbol as usize] += 1;
        }
        if bwt.len() % BLOCK == 0 {
            checkpoints.push(counts);
        }
        let cumulative = cumulative(&bwt);
        let len = bwt.len();
        Self {
            occurrences: Occurrences::Sampled { bwt, checkpoints },
            cumulative,
            len,
            skip,
        }
    }
    pub fn wavelet(bwt: Vec<u16>, skip: usize) -> Self {
        let cumulative = cumulative(&bwt);
        let len = bwt.len();
        Self {
            occurrences: Occurrences::Wavelet(WaveletMatrix::new(
                bwt.into_iter().map(u32::from).collect(),
            )),
            cumulative,
            len,
            skip,
        }
    }
    fn rank(&self, symbol: u16, end: usize) -> usize {
        match &self.occurrences {
            Occurrences::Sampled { bwt, checkpoints } => {
                let block = end / BLOCK;
                checkpoints[block][symbol as usize] as usize
                    + bwt[block * BLOCK..end]
                        .iter()
                        .filter(|&&s| s == symbol)
                        .count()
            }
            Occurrences::Wavelet(matrix) => matrix.rank(u32::from(symbol), end),
        }
    }
    pub fn range(&self, needle: &[u8]) -> Range<usize> {
        if needle.is_empty() || needle.contains(&0) {
            return 0..0;
        }
        let (mut left, mut right) = (0, self.len);
        for &byte in needle.iter().rev() {
            let symbol = u16::from(byte) + 1;
            left = self.cumulative[symbol as usize] + self.rank(symbol, left);
            right = self.cumulative[symbol as usize] + self.rank(symbol, right);
            if left == right {
                return 0..0;
            }
        }
        assert!(left >= self.skip && right >= self.skip);
        left - self.skip..right - self.skip
    }
    pub fn bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + match &self.occurrences {
                Occurrences::Sampled { bwt, checkpoints } => {
                    bwt.capacity() * 2 + checkpoints.capacity() * ALPHABET * 4
                }
                Occurrences::Wavelet(matrix) => matrix.bytes(),
            }
    }
}
fn cumulative(bwt: &[u16]) -> [usize; ALPHABET] {
    let mut counts = [0; ALPHABET];
    for &symbol in bwt {
        counts[symbol as usize] += 1;
    }
    let mut total = 0;
    for count in &mut counts {
        let n = *count;
        *count = total;
        total += n;
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::literal::LiteralIndex;
    #[test]
    fn both_representations_match_suffix_ranges_and_ranked_oracle() {
        let mut names = vec![
            "banana".to_owned(),
            "ana".to_owned(),
            "École".to_owned(),
            "東京".to_owned(),
            "Straße".to_owned(),
            "xana".to_owned(),
        ];
        names.extend((0..100).map(|i| format!("report-{i}.rs")));
        for names in [vec![], vec!["a".to_owned()], names] {
            let index = LiteralIndex::build(names.iter().map(String::as_str)).unwrap();
            for fm in [
                FmIndex::sampled(index.fm_input().0, index.name_count() + 1),
                FmIndex::wavelet(index.fm_input().0, index.name_count() + 1),
            ] {
                let queries = names
                    .iter()
                    .flat_map(|name| {
                        let folded = crate::catalog::normalize::nfc_fold(name);
                        let positions: Vec<_> = folded
                            .char_indices()
                            .map(|(p, _)| p)
                            .chain(std::iter::once(folded.len()))
                            .collect();
                        let mut queries = Vec::new();
                        for (i, &start) in positions.iter().enumerate() {
                            for &end in &positions[i + 1..] {
                                queries.push(folded[start..end].to_owned());
                            }
                        }
                        queries
                    })
                    .chain([
                        "not-present".to_owned(),
                        "é".to_owned(),
                        "foobarbaz".to_owned(),
                    ]);
                for query in queries {
                    let actual = fm.range(query.as_bytes());
                    let expected = index.range(query.as_bytes());
                    assert!(actual == expected || (actual.is_empty() && expected.is_empty()));
                    assert_eq!(
                        index.search_with_range(&query, 100, |q| fm.range(q)),
                        index.oracle(&query, 100)
                    );
                }
            }
        }
    }
}

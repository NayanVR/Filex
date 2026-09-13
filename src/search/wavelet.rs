//! Packed bitvectors and ranked, distinct wavelet-matrix range retrieval.
//! A query visits occupied value prefixes; it never enumerates occurrences.
use crate::catalog::storage::{Packed, Reader, Span, Writer};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::ops::Range;

#[derive(Debug)]
pub struct RankBits {
    words: Packed<u64>,
    ranks: Packed<u32>,
    len: usize,
}

impl RankBits {
    pub(crate) fn remap(&mut self, reader: &Reader) -> Result<()> {
        self.words.remap(reader)?;
        self.ranks.remap(reader)?;
        Ok(())
    }

    pub fn new(bits: impl IntoIterator<Item = bool>) -> Self {
        let mut words = Vec::<u64>::new();
        let mut len = 0;
        for bit in bits {
            if len % 64 == 0 {
                words.push(0);
            }
            if bit {
                *words.last_mut().unwrap() |= 1 << (len % 64);
            }
            len += 1;
        }
        assert!(
            len <= u32::MAX as usize,
            "bitvector exceeds packed position range"
        );
        let mut ranks = Vec::with_capacity(words.len() / 8 + 1);
        ranks.push(0);
        let mut total = 0;
        for (i, word) in words.iter().enumerate() {
            total += word.count_ones();
            if (i + 1) % 8 == 0 {
                ranks.push(total);
            }
        }
        words.shrink_to_fit();
        Self {
            words: words.into(),
            ranks: ranks.into(),
            len,
        }
    }
    pub fn rank1(&self, end: usize) -> usize {
        assert!(end <= self.len);
        let word = end / 64;
        let bits = end % 64;
        self.ranks[word / 8] as usize
            + self.words[(word / 8) * 8..word]
                .iter()
                .map(|w| w.count_ones() as usize)
                .sum::<usize>()
            + if bits == 0 {
                0
            } else {
                (self.words[word] & ((1u64 << bits) - 1)).count_ones() as usize
            }
    }
    pub fn bytes(&self) -> usize {
        self.words.capacity() * 8 + self.ranks.capacity() * 4
    }
}

#[derive(Debug)]
pub struct WaveletMatrix {
    levels: Vec<RankBits>,
    zeros: Vec<usize>,
    len: usize,
}

impl WaveletMatrix {
    pub(crate) fn remap(&mut self, reader: &Reader) -> Result<()> {
        for level in &mut self.levels {
            level.remap(reader)?;
        }
        Ok(())
    }

    pub fn new(mut values: Vec<u32>) -> Self {
        let len = values.len();
        let width = 32 - values.iter().copied().max().unwrap_or(0).leading_zeros();
        let mut levels = Vec::with_capacity(width as usize);
        let mut zeros = Vec::with_capacity(width as usize);
        let mut next = vec![0; len];
        for shift in (0..width).rev() {
            let bits = RankBits::new(values.iter().map(|v| v & (1 << shift) != 0));
            let zero_count = len - bits.rank1(len);
            let (mut zero, mut one) = (0, zero_count);
            for &value in &values {
                let position = if value & (1 << shift) == 0 {
                    &mut zero
                } else {
                    &mut one
                };
                next[*position] = value;
                *position += 1;
            }
            std::mem::swap(&mut values, &mut next);
            levels.push(bits);
            zeros.push(zero_count);
        }
        Self { levels, zeros, len }
    }

    /// Lowest distinct values in ascending order, capped at `limit`.
    pub fn top_k(&self, range: Range<usize>, limit: usize) -> Vec<u32> {
        assert!(range.start <= range.end && range.end <= self.len);
        let mut found = Vec::with_capacity(limit.min(range.len()));
        let mut pending = vec![(0, range.start, range.end, 0u32)];
        while let Some((level, left, right, value)) = pending.pop() {
            if left == right || found.len() == limit {
                continue;
            }
            if level == self.levels.len() {
                found.push(value);
                continue;
            }
            let bits = &self.levels[level];
            let (l1, r1) = (bits.rank1(left), bits.rank1(right));
            // Stack order visits zero child first, hence static rank order.
            if l1 != r1 {
                pending.push((
                    level + 1,
                    self.zeros[level] + l1,
                    self.zeros[level] + r1,
                    (value << 1) | 1,
                ));
            }
            if left - l1 != right - r1 {
                pending.push((level + 1, left - l1, right - r1, value << 1));
            }
        }
        found
    }
    pub fn rank(&self, value: u32, end: usize) -> usize {
        assert!(end <= self.len);
        if self.levels.len() < 32 && value >> self.levels.len() != 0 {
            return 0;
        }
        let (mut left, mut right) = (0, end);
        for (level, bits) in self.levels.iter().enumerate() {
            let (l1, r1) = (bits.rank1(left), bits.rank1(right));
            if value & (1 << (self.levels.len() - level - 1)) == 0 {
                left -= l1;
                right -= r1;
            } else {
                left = self.zeros[level] + l1;
                right = self.zeros[level] + r1;
            }
        }
        right - left
    }
    pub fn bytes(&self) -> usize {
        self.levels.iter().map(RankBits::bytes).sum::<usize>()
            + self.levels.capacity() * std::mem::size_of::<RankBits>()
            + self.zeros.capacity() * std::mem::size_of::<usize>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ranks_at_word_edges() {
        for len in [0, 1, 63, 64, 65, 127, 128, 129, 511, 512, 513, 1024] {
            let bits = RankBits::new((0..len).map(|i| i % 3 == 0));
            for end in 0..=len {
                assert_eq!(bits.rank1(end), (0..end).filter(|i| i % 3 == 0).count());
            }
        }
    }
    #[test]
    fn every_range_matches_exhaustive_ranked_distinct_oracle() {
        let mut seed = 17u32;
        for len in [0, 1, 2, 33, 65, 129] {
            let values: Vec<_> = (0..len)
                .map(|_| {
                    seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                    (seed >> 16) % 31
                })
                .collect();
            let matrix = WaveletMatrix::new(values.clone());
            for left in 0..=len {
                for right in left..=len {
                    let mut expected = values[left..right].to_vec();
                    expected.sort_unstable();
                    expected.dedup();
                    for limit in [0, 1, 3, 100] {
                        assert_eq!(
                            matrix.top_k(left..right, limit),
                            expected.iter().take(limit).copied().collect::<Vec<_>>()
                        );
                    }
                }
            }
        }
        assert_eq!(
            WaveletMatrix::new(vec![0; 1000]).top_k(0..1000, 100),
            vec![0]
        );
        assert_eq!(
            WaveletMatrix::new(vec![u32::MAX, 0, 17]).top_k(0..3, 3),
            vec![0, 17, u32::MAX]
        );
    }
}

#[derive(Serialize, Deserialize)]
pub struct BitsImage {
    words: Span,
    ranks: Span,
    len: usize,
}
#[derive(Serialize, Deserialize)]
pub struct MatrixImage {
    levels: Vec<BitsImage>,
    zeros: Vec<usize>,
    len: usize,
}
impl RankBits {
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn save(&self, writer: &mut Writer) -> Result<BitsImage> {
        Ok(BitsImage {
            words: self.words.save(writer)?,
            ranks: self.ranks.save(writer)?,
            len: self.len,
        })
    }
    pub fn load(reader: &Reader, image: BitsImage) -> Result<Self> {
        let bits = Self {
            words: Packed::load(reader, image.words)?,
            ranks: Packed::load(reader, image.ranks)?,
            len: image.len,
        };
        ensure!(
            bits.words.len() == bits.len.div_ceil(64)
                && bits.ranks.len() == bits.words.len() / 8 + 1,
            "invalid rank lengths"
        );
        let mut total = 0;
        for (i, word) in bits.words.iter().enumerate() {
            if i % 8 == 0 {
                ensure!(bits.ranks[i / 8] == total, "invalid rank checkpoint");
            }
            total += word.count_ones();
        }
        ensure!(total as usize <= bits.len, "invalid bitvector count");
        if bits.words.len() % 8 == 0 {
            ensure!(
                bits.ranks[bits.words.len() / 8] == total,
                "invalid final rank"
            );
        }
        Ok(bits)
    }
}
impl WaveletMatrix {
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn save(&self, writer: &mut Writer) -> Result<MatrixImage> {
        Ok(MatrixImage {
            levels: self
                .levels
                .iter()
                .map(|b| b.save(writer))
                .collect::<Result<_>>()?,
            zeros: self.zeros.clone(),
            len: self.len,
        })
    }
    pub fn load(reader: &Reader, image: MatrixImage) -> Result<Self> {
        ensure!(
            image.levels.len() <= 32 && image.levels.len() == image.zeros.len(),
            "invalid wavelet levels"
        );
        let levels = image
            .levels
            .into_iter()
            .map(|b| RankBits::load(reader, b))
            .collect::<Result<Vec<_>>>()?;
        for (bits, zeros) in levels.iter().zip(&image.zeros) {
            ensure!(
                bits.len() == image.len && *zeros == image.len - bits.rank1(image.len),
                "invalid wavelet partition"
            );
        }
        Ok(Self {
            levels,
            zeros: image.zeros,
            len: image.len,
        })
    }
}

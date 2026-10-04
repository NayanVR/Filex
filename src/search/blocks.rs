//! Transposed gram signatures over rank-ordered groups of unique names.
//! Collisions admit extra candidates; every candidate is verified against text.
use crate::catalog::storage::{Packed, Reader, Span, Writer};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

pub struct BlockIndex {
    bits: Packed<u64>,
    names: usize,
    group: usize,
    buckets: usize,
    starts_only: bool,
}

#[derive(Serialize, Deserialize)]
pub struct BlockImage {
    hash_version: u32,
    bits: Span,
    names: usize,
    group: usize,
    buckets: usize,
    starts_only: bool,
}

fn buckets_for(gram: &[u8], buckets: usize) -> [usize; 2] {
    let mut key = gram.len() as u32;
    for &byte in gram {
        key = (key << 8) | u32::from(byte);
    }
    key ^= key >> 16;
    key = key.wrapping_mul(0x7feb352d);
    key ^= key >> 15;
    key = key.wrapping_mul(0x846ca68b);
    key ^= key >> 16;
    [
        key as usize & (buckets - 1),
        (key >> 16) as usize & (buckets - 1),
    ]
}

impl BlockIndex {
    pub fn build<S: AsRef<str>>(
        names: impl ExactSizeIterator<Item = S>,
        group: usize,
        buckets: usize,
    ) -> Self {
        assert!(group.is_power_of_two() && buckets.is_power_of_two());
        let count = names.len();
        let words = count.div_ceil(group).div_ceil(64);
        let mut bits = vec![0u64; words * buckets];
        for (id, name) in names.enumerate() {
            let name = name.as_ref();
            let block = id / group;
            for width in 1..=3.min(name.len()) {
                for gram in name.as_bytes().windows(width) {
                    for hash in buckets_for(gram, buckets) {
                        bits[hash * words + block / 64] |= 1 << (block % 64);
                    }
                }
            }
        }
        Self {
            bits: bits.into(),
            names: count,
            group,
            buckets,
            starts_only: false,
        }
    }

    pub fn build_starts<S: AsRef<str>>(
        names: impl ExactSizeIterator<Item = S>,
        group: usize,
        buckets: usize,
        is_boundary: impl Fn(usize, usize) -> bool,
    ) -> Self {
        assert!(group.is_power_of_two() && buckets.is_power_of_two());
        let count = names.len();
        let words = count.div_ceil(group).div_ceil(64);
        let mut bits = vec![0u64; words * buckets];
        for (id, name) in names.enumerate() {
            let name = name.as_ref();
            let block = id / group;
            for (position, _) in name.char_indices().filter(|(p, _)| is_boundary(id, *p)) {
                for width in 1..=3.min(name.len() - position) {
                    let gram = &name.as_bytes()[position..position + width];
                    for hash in buckets_for(gram, buckets) {
                        bits[hash * words + block / 64] |= 1 << (block % 64);
                    }
                }
            }
        }
        Self {
            bits: bits.into(),
            names: count,
            group,
            buckets,
            starts_only: true,
        }
    }

    /// Visits candidates in static name-rank order. False stops traversal.
    pub fn visit(&self, needle: &[u8], mut visit: impl FnMut(usize) -> bool) {
        self.visit_groups(&self.matching_words(needle), |block| {
            for id in block * self.group..((block + 1) * self.group).min(self.names) {
                if !visit(id) {
                    return false;
                }
            }
            true
        });
    }

    pub fn estimate(&self, needle: &[u8]) -> usize {
        (self
            .matching_words(needle)
            .iter()
            .map(|word| word.count_ones() as usize)
            .sum::<usize>()
            * self.group)
            .min(self.names)
    }

    /// Both signatures must admit the group. Useful for boundary starts: the
    /// initial gram alone should never admit groups excluded by the full query.
    pub fn visit_intersection(
        &self,
        other: &Self,
        needle: &[u8],
        mut visit: impl FnMut(usize) -> bool,
    ) {
        assert!(self.names == other.names && self.group == other.group);
        let mut words = self.matching_words(needle);
        for (word, other) in words.iter_mut().zip(other.matching_words(needle)) {
            *word &= other;
        }
        self.visit_groups(&words, |block| {
            for id in block * self.group..((block + 1) * self.group).min(self.names) {
                if !visit(id) {
                    return false;
                }
            }
            true
        });
    }

    fn matching_words(&self, needle: &[u8]) -> Vec<u64> {
        if needle.is_empty() || self.names == 0 {
            return Vec::new();
        }
        let words = self.names.div_ceil(self.group).div_ceil(64);
        let width = needle.len().min(3);
        let needle = if self.starts_only {
            &needle[..width]
        } else {
            needle
        };
        let mut hashes: Vec<_> = needle
            .windows(width)
            .flat_map(|g| buckets_for(g, self.buckets))
            .collect();
        hashes.sort_unstable();
        hashes.dedup();
        // Additional grams only reduce false positives. Bound work for huge queries.
        hashes.truncate(32);
        (0..words)
            .map(|word| {
                let mut matches = u64::MAX;
                for &hash in &hashes {
                    matches &= self.bits[hash * words + word];
                    if matches == 0 {
                        break;
                    }
                }
                matches
            })
            .collect()
    }

    fn visit_groups(&self, words: &[u64], mut visit: impl FnMut(usize) -> bool) {
        for (word, &matches) in words.iter().enumerate() {
            let mut matches = matches;
            while matches != 0 {
                let block = word * 64 + matches.trailing_zeros() as usize;
                matches &= matches - 1;
                if block * self.group < self.names && !visit(block) {
                    return;
                }
            }
        }
    }

    pub fn bytes(&self) -> usize {
        self.bits.len() * 8
    }
    pub fn validate_pair(&self, other: &Self) -> Result<()> {
        ensure!(
            self.names == other.names && self.group == other.group,
            "incompatible block signature groups"
        );
        Ok(())
    }
    pub fn validate<S: AsRef<str>>(
        &self,
        names: impl ExactSizeIterator<Item = S>,
        is_boundary: impl Fn(usize, usize) -> bool,
    ) -> Result<()> {
        ensure!(names.len() == self.names, "signature name count mismatch");
        let words = self.names.div_ceil(self.group).div_ceil(64);
        for (id, name) in names.enumerate() {
            let name = name.as_ref();
            let block = id / self.group;
            for width in 1..=3.min(name.len()) {
                for (position, gram) in name.as_bytes().windows(width).enumerate() {
                    if self.starts_only
                        && (!name.is_char_boundary(position) || !is_boundary(id, position))
                    {
                        continue;
                    }
                    for hash in buckets_for(gram, self.buckets) {
                        ensure!(
                            self.bits[hash * words + block / 64] & (1 << (block % 64)) != 0,
                            "incomplete block signature"
                        );
                    }
                }
            }
        }
        Ok(())
    }
    pub fn save(&self, writer: &mut Writer) -> Result<BlockImage> {
        Ok(BlockImage {
            hash_version: 1,
            bits: self.bits.save(writer)?,
            names: self.names,
            group: self.group,
            buckets: self.buckets,
            starts_only: self.starts_only,
        })
    }
    pub fn load(
        reader: &Reader,
        image: BlockImage,
        names: usize,
        starts_only: bool,
    ) -> Result<Self> {
        ensure!(
            image.hash_version == 1
                && image.names == names
                && image.starts_only == starts_only
                && matches!(image.group, 16 | 32 | 64 | 128)
                && matches!(image.buckets, 1024 | 2048 | 4096 | 8192),
            "invalid block signature dimensions"
        );
        let bits = Packed::load(reader, image.bits)?;
        ensure!(
            bits.len() == names.div_ceil(image.group).div_ceil(64) * image.buckets,
            "invalid block signatures"
        );
        Ok(Self {
            bits,
            names,
            group: image.group,
            buckets: image.buckets,
            starts_only,
        })
    }
    pub(crate) fn remap(&mut self, reader: &Reader) -> Result<()> {
        self.bits.remap(reader)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn signatures_roundtrip_and_missing_grams_are_rejected() {
        let names = ["alphaBeta", "東京.txt", "banana", "x-beta"];
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blocks");
        let mut writer = Writer::create(&path).unwrap();
        let index = BlockIndex::build(names.into_iter(), 64, 4096);
        let image = index.save(&mut writer).unwrap();
        writer.finish(&image).unwrap();
        let reader = unsafe { Reader::open(&path) }.unwrap();
        let loaded =
            BlockIndex::load(&reader, reader.metadata().unwrap(), names.len(), false).unwrap();
        loaded.validate(names.into_iter(), |_, _| false).unwrap();
        let starts = BlockIndex::build_starts(names.into_iter(), 64, 1024, |_, p| p == 0);
        starts.validate(names.into_iter(), |_, p| p == 0).unwrap();
        for needle in ["a", "alpha", "東", "banana", "x-", "absent"] {
            let mut found = Vec::new();
            starts.visit_intersection(&loaded, needle.as_bytes(), |id| {
                if names[id].starts_with(needle) {
                    found.push(id);
                }
                true
            });
            assert_eq!(
                found,
                names
                    .iter()
                    .enumerate()
                    .filter_map(|(i, n)| n.starts_with(needle).then_some(i))
                    .collect::<Vec<_>>()
            );
        }
        let bad = BlockIndex {
            bits: vec![0; index.bits.len()].into(),
            ..index
        };
        assert!(bad.validate(names.into_iter(), |_, _| false).is_err());
        assert!(
            starts
                .validate_pair(&BlockIndex::build(names.into_iter(), 32, 1024))
                .is_err()
        );
    }
    #[test]
    fn candidates_have_no_false_negatives_across_groups_and_unicode() {
        let names: Vec<_> = (0..2100).map(|i| format!("東京-{i}-Straße.txt")).collect();
        for group in [16, 32, 64, 128] {
            let index = BlockIndex::build(names.iter().map(String::as_str), group, 1024);
            for needle in [
                "東",
                "京-2",
                "Straße",
                "txt",
                "0",
                "10",
                "210",
                "missing",
                "aaabbbcccdddeeefffggghhh",
            ] {
                let mut found = Vec::new();
                index.visit(needle.as_bytes(), |id| {
                    if names[id].contains(needle) {
                        found.push(id);
                    }
                    true
                });
                let expected: Vec<_> = names
                    .iter()
                    .enumerate()
                    .filter_map(|(i, n)| n.contains(needle).then_some(i))
                    .collect();
                assert_eq!(found, expected);
            }
        }
    }
}

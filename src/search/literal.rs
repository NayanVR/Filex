//! Ranked unique-name search. Compact gram blocks select candidates; text
//! verification preserves exact structural tiers. Legacy FM segments remain readable.
use super::blocks::{BlockImage, BlockIndex};
use super::wavelet::{RankBits, WaveletMatrix};
use super::{
    fm::{FmImage, FmIndex},
    wavelet::{BitsImage, MatrixImage},
};
use crate::catalog::columns::{Column, ColumnImage};
use crate::catalog::pool::{BytePool, PoolImage};
use crate::catalog::storage::{Packed, Reader, Span, Writer};
use crate::catalog::{
    normalize::{boundaries, nfc_fold},
    postings::{Postings, PostingsImage},
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::{cmp::Ordering, collections::BTreeMap, ops::Range};

enum Retrieval {
    Blocks {
        substrings: BlockIndex,
        starts: BlockIndex,
    },
    Legacy {
        suffixes: Packed<u32>,
        docs: WaveletMatrix,
        boundary_bits: RankBits,
        boundary_docs: WaveletMatrix,
        fm: Option<FmIndex>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Tier {
    Exact,
    Prefix,
    Boundary,
    Substring,
    Fuzzy,
}

/// Name rank is (folded byte length, folded lexical order). Occurrence-level
/// priors belong to the catalog/posting merge, not this names-only laboratory.
pub struct LiteralIndex {
    postings: Postings,
    text: BytePool,
    offsets: Column<u32>,
    retrieval: Retrieval,
    sorted: Packed<u32>,
    fuzzy: super::fuzzy::FuzzyIndex,
    prefix_docs: WaveletMatrix,
    // Original-name boundaries survive normalization and deduplication.
    boundaries: RankBits,
}

impl LiteralIndex {
    #[cfg(test)]
    pub(crate) fn use_legacy_offsets(&mut self) {
        self.postings.use_legacy_offsets();
        self.fuzzy.use_legacy_offsets();
    }

    pub(crate) fn remap(&mut self, reader: &Reader) -> Result<()> {
        self.text.remap(reader)?;
        self.offsets.remap(reader)?;
        self.sorted.remap(reader)?;
        self.postings.remap(reader)?;
        self.prefix_docs.remap(reader)?;
        self.boundaries.remap(reader)?;
        self.fuzzy.remap(reader)?;
        match &mut self.retrieval {
            Retrieval::Blocks { substrings, starts } => {
                substrings.remap(reader)?;
                starts.remap(reader)?;
            }
            Retrieval::Legacy {
                docs,
                boundary_bits,
                boundary_docs,
                fm,
                ..
            } => {
                docs.remap(reader)?;
                boundary_bits.remap(reader)?;
                boundary_docs.remap(reader)?;
                if let Some(fm) = fm {
                    fm.remap(reader)?;
                }
            }
        }
        Ok(())
    }

    pub fn build<'a>(names: impl IntoIterator<Item = &'a str>) -> Result<Self> {
        Self::build_internal(names, false)
    }
    #[cfg(any(test, feature = "index-v2-lab"))]
    pub fn build_suffix<'a>(names: impl IntoIterator<Item = &'a str>) -> Result<Self> {
        Self::build_internal(names, true)
    }
    fn build_internal<'a>(names: impl IntoIterator<Item = &'a str>, legacy: bool) -> Result<Self> {
        let mut dictionary = BTreeMap::<String, (Vec<usize>, Vec<u32>)>::new();
        for (file, name) in names.into_iter().enumerate() {
            ensure!(
                !name.contains('\0'),
                "NUL is reserved as the name separator"
            );
            let folded = nfc_fold(name);
            if folded.is_empty() {
                continue;
            }
            ensure!(file < u32::MAX as usize, "file count exceeds ordinal range");
            let (positions, postings) = dictionary.entry(folded).or_default();
            postings.push(file as u32);
            for boundary in boundaries(name) {
                let position = nfc_fold(&name[..boundary]).len();
                if !positions.contains(&position) {
                    positions.push(position);
                }
            }
        }
        let mut names: Vec<_> = dictionary.into_iter().collect();
        names.sort_unstable_by(|(a, _), (b, _)| a.len().cmp(&b.len()).then_with(|| a.cmp(b)));
        for (_, (positions, _)) in &mut names {
            positions.sort_unstable();
        }
        let fuzzy =
            super::fuzzy::FuzzyIndex::build(names.iter().enumerate().map(
                |(id, (name, (positions, _)))| (id as u32, name.as_str(), positions.as_slice()),
            ))?;
        let bytes = names
            .iter()
            .try_fold(0usize, |sum, (name, _)| sum.checked_add(name.len() + 1))
            .ok_or_else(|| anyhow::anyhow!("name text size overflow"))?;
        ensure!(
            bytes <= u32::MAX as usize,
            "name text exceeds packed offset range"
        );
        let mut text = Vec::with_capacity(bytes);
        let mut offsets = Vec::with_capacity(names.len() + 1);
        let mut boundary_positions = Vec::new();
        let postings =
            Postings::build(names.iter_mut().map(|(_, (_, list))| std::mem::take(list)))?;
        for (name, (mut positions, _)) in names {
            offsets.push(text.len() as u32);
            positions.sort_unstable();
            positions.dedup();
            boundary_positions.extend(
                positions
                    .into_iter()
                    .filter(|p| *p < name.len())
                    .map(|p| (text.len() + p) as u32),
            );
            text.extend_from_slice(name.as_bytes());
            text.push(0);
        }
        offsets.push(text.len() as u32);
        crate::catalog::storage::release_builder_memory();
        let boundaries = RankBits::new(
            (0..text.len()).map(|p| boundary_positions.binary_search(&(p as u32)).is_ok()),
        );
        drop(boundary_positions);
        let retrieval = if legacy {
            let mut suffixes = Vec::with_capacity(text.len() - names_count(&offsets));
            suffixes.extend(
                text.iter()
                    .enumerate()
                    .filter(|(_, byte)| **byte != 0)
                    .map(|(i, _)| i as u32),
            );
            suffixes.shrink_to_fit();
            // Full-text ordering agrees with the FM BWT. NUL separators still
            // prevent a literal query from matching across names.
            suffixes.sort_unstable_by(|a, b| text[*a as usize..].cmp(&text[*b as usize..]));
            let starts =
                RankBits::new((0..text.len()).map(|p| offsets.binary_search(&(p as u32)).is_ok()));
            let is_boundary = |offset: u32| {
                boundaries.rank1(offset as usize + 1) != boundaries.rank1(offset as usize)
            };
            let boundary_bits = RankBits::new(suffixes.iter().map(|&offset| is_boundary(offset)));
            let docs = WaveletMatrix::new(
                suffixes
                    .iter()
                    .map(|&p| starts.rank1(p as usize + 1) as u32 - 1)
                    .collect(),
            );
            let boundary_docs = WaveletMatrix::new(
                suffixes
                    .iter()
                    .filter(|&&p| is_boundary(p))
                    .map(|&p| starts.rank1(p as usize + 1) as u32 - 1)
                    .collect(),
            );
            Retrieval::Legacy {
                suffixes: suffixes.into(),
                docs,
                boundary_bits,
                boundary_docs,
                fm: None,
            }
        } else {
            let names = || {
                (0..offsets.len() as u32 - 1)
                    .map(|id| std::str::from_utf8(name(&text, &offsets, id)).unwrap())
            };
            Retrieval::Blocks {
                substrings: BlockIndex::build(names(), 64, 4096),
                starts: BlockIndex::build_starts(names(), 64, 1024, |id, p| {
                    let offset = offsets[id] as usize + p;
                    boundaries.rank1(offset + 1) != boundaries.rank1(offset)
                }),
            }
        };
        let mut sorted: Vec<_> = (0..offsets.len() as u32 - 1).collect();
        sorted.sort_unstable_by(|&a, &b| name(&text, &offsets, a).cmp(name(&text, &offsets, b)));
        let prefix_docs = WaveletMatrix::new(sorted.clone());
        let built = Self {
            postings,
            text: if legacy {
                BytePool::plain(text)
            } else {
                BytePool::build_search(text)?
            },
            offsets: if legacy {
                Column::Legacy(offsets.into())
            } else {
                offsets.into()
            },
            retrieval,
            fuzzy,
            sorted: sorted.into(),
            prefix_docs,
            boundaries,
        };
        Ok(built)
    }
    fn name_range(&self, id: u32) -> Range<usize> {
        self.offsets.get(id as usize) as usize..self.offsets.get(id as usize + 1) as usize - 1
    }
    pub fn name(&self, id: u32) -> Cow<'_, str> {
        match self.text.read(self.name_range(id)) {
            Cow::Borrowed(bytes) => {
                Cow::Borrowed(std::str::from_utf8(bytes).expect("validated UTF-8"))
            }
            Cow::Owned(bytes) => Cow::Owned(String::from_utf8(bytes).expect("validated UTF-8")),
        }
    }
    pub fn files(&self, name: u32) -> impl Iterator<Item = u32> + '_ {
        self.postings.get(name)
    }
    pub fn search_files(&self, query: &str, limit: usize) -> Vec<(Tier, u32, u32)> {
        self.search(query, limit)
            .into_iter()
            .flat_map(|(tier, name)| self.files(name).map(move |file| (tier, name, file)))
            .take(limit)
            .collect()
    }
    pub fn name_count(&self) -> usize {
        self.offsets.len() - 1
    }
    pub fn suffix_count(&self) -> usize {
        self.text.len() - self.name_count()
    }
    pub fn range(&self, query: &[u8]) -> Range<usize> {
        let Retrieval::Legacy { suffixes, fm, .. } = &self.retrieval else {
            panic!("suffix ranges are not available for block retrieval");
        };
        if let Some(fm) = fm {
            return fm.range(query);
        }
        #[cfg(not(any(test, feature = "index-v2-lab")))]
        let _ = suffixes;
        #[cfg(not(any(test, feature = "index-v2-lab")))]
        {
            panic!("unbuilt FM index");
        }
        #[cfg(any(test, feature = "index-v2-lab"))]
        {
            if query.contains(&0) {
                return 0..0;
            }
            let lower = suffixes.partition_point(|&p| {
                prefix_cmp(suffix(self.text.as_plain(), p), query) == Ordering::Less
            });
            let upper = suffixes.partition_point(|&p| {
                prefix_cmp(suffix(self.text.as_plain(), p), query) != Ordering::Greater
            });
            lower..upper
        }
    }
    /// Substring-only rank oracle comparison, independent of match tiers.
    pub fn substring(&self, query: &str, limit: usize) -> Vec<u32> {
        let query = nfc_fold(query);
        if query.is_empty() || query.contains('\0') || limit == 0 {
            return Vec::new();
        }
        match &self.retrieval {
            Retrieval::Legacy { docs, .. } => docs.top_k(self.range(query.as_bytes()), limit),
            Retrieval::Blocks {
                substrings: blocks, ..
            } => {
                let mut found = Vec::new();
                blocks.visit(query.as_bytes(), |id| {
                    if self.name(id as u32).contains(&query) {
                        found.push(id as u32);
                    }
                    found.len() < limit
                });
                found
            }
        }
    }
    /// Enumerate complete literal postings with cancellation, without rank or
    /// result limits. Coarse signatures are always verified against name bytes.
    pub(crate) fn exhaustive_candidates(
        &self,
        query: &str,
        files: usize,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> anyhow::Result<super::candidates::Candidates> {
        use std::sync::atomic::Ordering;
        let query = nfc_fold(query);
        let mut found = super::candidates::Candidates::empty(files);
        anyhow::ensure!(!cancel.load(Ordering::Relaxed), "stream cancelled");
        if query.is_empty() || query.contains('\0') {
            return Ok(found);
        }
        let finder = memchr::memmem::Finder::new(query.as_bytes());
        let mut cursor = self.text.cursor();
        let mut visit = |id: usize| {
            if cancel.load(Ordering::Relaxed) {
                return false;
            }
            if finder
                .find(cursor.read(self.name_range(id as u32)))
                .is_some()
            {
                for (n, slot) in self.files(id as u32).enumerate() {
                    if n % 4096 == 0 && cancel.load(Ordering::Relaxed) {
                        return false;
                    }
                    found.insert(slot as usize);
                }
            }
            true
        };
        match &self.retrieval {
            Retrieval::Blocks { substrings, .. } => substrings.visit(query.as_bytes(), &mut visit),
            // Legacy dictionaries remain readable without constructing new
            // persisted accelerators or an uninterruptible ranked result list.
            Retrieval::Legacy { .. } => {
                for id in 0..self.name_count() {
                    if !visit(id) {
                        break;
                    }
                }
            }
        }
        anyhow::ensure!(!cancel.load(Ordering::Relaxed), "stream cancelled");
        Ok(found)
    }

    pub fn search(&self, query: &str, limit: usize) -> Vec<(Tier, u32)> {
        match &self.retrieval {
            Retrieval::Blocks { substrings, starts } => {
                self.search_blocks(substrings, starts, query, limit)
            }
            Retrieval::Legacy { .. } => {
                self.search_with_range(query, limit, |needle| self.range(needle))
            }
        }
    }
    /// Planning estimate only: signatures can overestimate matching names.
    pub fn estimate(&self, folded: &[u8]) -> usize {
        match &self.retrieval {
            Retrieval::Blocks { substrings, .. } => substrings.estimate(folded),
            Retrieval::Legacy { .. } => self.range(folded).len(),
        }
    }
    #[cfg(feature = "index-v2-lab")]
    pub fn boundary_blocks(&self, group: usize, buckets: usize) -> BlockIndex {
        BlockIndex::build_starts(
            (0..self.name_count() as u32).map(|id| self.name(id)),
            group,
            buckets,
            |id, p| {
                let offset = self.offsets.get(id) as usize + p;
                self.boundaries.rank1(offset + 1) != self.boundaries.rank1(offset)
            },
        )
    }
    /// Exact tier/rank retrieval through a conservative block accelerator.
    pub fn search_blocks(
        &self,
        blocks: &super::blocks::BlockIndex,
        starts: &super::blocks::BlockIndex,
        query: &str,
        limit: usize,
    ) -> Vec<(Tier, u32)> {
        let query = nfc_fold(query);
        if query.is_empty() || query.contains('\0') || limit == 0 {
            return Vec::new();
        }
        let needle = query.as_bytes();
        let mut cursor = self.text.cursor();
        let lower = self
            .sorted
            .partition_point(|&id| cursor.read(self.name_range(id)) < needle);
        let upper = self.sorted.partition_point(|&id| {
            prefix_cmp(cursor.read(self.name_range(id)), needle) != Ordering::Greater
        });
        let mut found = Vec::with_capacity(limit.min(self.name_count()));
        if let Some(&id) = self.sorted.get(lower)
            && cursor.read(self.name_range(id)) == needle
        {
            found.push((Tier::Exact, id));
        }
        self.merge(
            &mut found,
            Tier::Prefix,
            self.prefix_docs.top_k(lower..upper, limit),
            limit,
        );
        if found.len() == limit {
            return found;
        }
        let finder = memchr::memmem::Finder::new(needle);
        starts.visit_intersection(blocks, needle, |id| {
            let text = cursor.read(self.name_range(id as u32));
            if text.starts_with(needle) {
                return true;
            }
            let mut from = 0;
            while let Some(p) = finder.find(&text[from..]) {
                let at = from + p;
                let offset = self.offsets.get(id) as usize + at;
                if self.boundaries.rank1(offset + 1) != self.boundaries.rank1(offset) {
                    found.push((Tier::Boundary, id as u32));
                    return found.len() < limit;
                }
                from = at + 1;
            }
            true
        });
        if found.len() == limit {
            return found;
        }
        let seen: std::collections::HashSet<_> = found.iter().map(|(_, id)| *id).collect();
        blocks.visit(needle, |id| {
            if !seen.contains(&(id as u32))
                && finder
                    .find(cursor.read(self.name_range(id as u32)))
                    .is_some()
            {
                found.push((Tier::Substring, id as u32));
            }
            found.len() < limit
        });
        found
    }
    pub fn search_with_range(
        &self,
        query: &str,
        limit: usize,
        discover: impl FnOnce(&[u8]) -> Range<usize>,
    ) -> Vec<(Tier, u32)> {
        let Retrieval::Legacy {
            docs,
            boundary_bits,
            boundary_docs,
            ..
        } = &self.retrieval
        else {
            panic!("range discovery requires the legacy retrieval matrices");
        };
        let query = nfc_fold(query);
        if query.is_empty() || query.contains('\0') || limit == 0 {
            return Vec::new();
        }
        let needle = query.as_bytes();
        let lower = self
            .sorted
            .partition_point(|&id| self.name(id).as_bytes() < needle);
        let upper = self.sorted.partition_point(|&id| {
            prefix_cmp(self.name(id).as_bytes(), needle) != Ordering::Greater
        });
        let mut found = Vec::with_capacity(limit.min(self.name_count()));
        if let Some(&id) = self.sorted.get(lower)
            && self.name(id) == query
        {
            found.push((Tier::Exact, id));
        }
        if found.len() == limit {
            return found;
        }
        self.merge(
            &mut found,
            Tier::Prefix,
            self.prefix_docs.top_k(lower..upper, limit),
            limit,
        );
        if found.len() == limit {
            return found;
        }
        let range = discover(needle);
        self.merge(
            &mut found,
            Tier::Boundary,
            boundary_docs.top_k(
                boundary_bits.rank1(range.start)..boundary_bits.rank1(range.end),
                limit,
            ),
            limit,
        );
        if found.len() == limit {
            return found;
        }
        self.merge(&mut found, Tier::Substring, docs.top_k(range, limit), limit);
        found
    }
    fn merge(&self, found: &mut Vec<(Tier, u32)>, tier: Tier, ids: Vec<u32>, limit: usize) {
        // Each retrieval tier already contains distinct IDs. Avoid quadratic
        // duplicate checks when pagination requests thousands of names.
        let seen: std::collections::HashSet<_> = found.iter().map(|(_, id)| *id).collect();
        for id in ids {
            if found.len() == limit {
                break;
            }
            if !seen.contains(&id) {
                found.push((tier, id));
            }
        }
    }
    /// Exhaustive, test-only oracle; never called by interactive retrieval.
    #[cfg(any(test, feature = "index-v2-lab"))]
    pub fn oracle(&self, query: &str, limit: usize) -> Vec<(Tier, u32)> {
        let query = nfc_fold(query);
        if query.is_empty() || query.contains('\0') {
            return Vec::new();
        }
        let mut matches = Vec::new();
        for id in 0..self.name_count() as u32 {
            let name = self.name(id);
            let tier = if name == query {
                Some(Tier::Exact)
            } else if name.starts_with(&query) {
                Some(Tier::Prefix)
            } else if name.contains(&query) {
                let boundary = name.char_indices().any(|(p, _)| {
                    let offset = self.offsets.get(id as usize) as usize + p;
                    name[p..].starts_with(&query)
                        && self.boundaries.rank1(offset + 1) != self.boundaries.rank1(offset)
                });
                Some(if boundary {
                    Tier::Boundary
                } else {
                    Tier::Substring
                })
            } else {
                None
            };
            if let Some(tier) = tier {
                matches.push((tier, id));
            }
        }
        matches.sort_unstable();
        matches.truncate(limit);
        matches
    }
    pub fn bytes(&self) -> usize {
        self.fuzzy.bytes()
            + self.postings.bytes()
            + self.text.bytes()
            + self.offsets.bytes()
            + self.sorted.capacity() * 4
            + self.prefix_docs.bytes()
            + self.boundaries.bytes()
            + match &self.retrieval {
                Retrieval::Blocks { substrings, starts } => substrings.bytes() + starts.bytes(),
                Retrieval::Legacy {
                    suffixes,
                    docs,
                    boundary_bits,
                    boundary_docs,
                    fm,
                } => {
                    suffixes.capacity() * 4
                        + docs.bytes()
                        + boundary_bits.bytes()
                        + boundary_docs.bytes()
                        + fm.as_ref().map_or(0, FmIndex::bytes)
                }
            }
    }
    /// BWT uses a unique zero sentinel and byte+1 symbols (257 symbols).
    /// Separator/sentinel suffixes sort before all filename suffixes, so FM
    /// intervals for nonempty literals map into `docs` by subtracting `skip`.
    pub fn fm_input(&self) -> (Vec<u16>, usize) {
        let Retrieval::Legacy { suffixes, .. } = &self.retrieval else {
            panic!("FM input requires the suffix laboratory");
        };
        let mut separators: Vec<_> = self
            .text
            .as_plain()
            .iter()
            .enumerate()
            .filter(|(_, b)| **b == 0)
            .map(|(i, _)| i as u32)
            .collect();
        separators.sort_unstable_by(|a, b| {
            self.text.as_plain()[*a as usize..].cmp(&self.text.as_plain()[*b as usize..])
        });
        let skip = separators.len() + 1;
        let bwt = std::iter::once(self.text.len() as u32)
            .chain(separators)
            .chain(suffixes.iter().copied())
            .map(|offset| {
                if offset == 0 {
                    0
                } else {
                    u16::from(self.text.as_plain()[offset as usize - 1]) + 1
                }
            })
            .collect();
        (bwt, skip)
    }
    pub fn suffix_bytes(&self) -> usize {
        match &self.retrieval {
            Retrieval::Legacy { suffixes, .. } => suffixes.capacity() * 4,
            _ => 0,
        }
    }
    pub fn wavelet_bytes(&self) -> usize {
        self.prefix_docs.bytes()
            + match &self.retrieval {
                Retrieval::Legacy {
                    docs,
                    boundary_docs,
                    ..
                } => docs.bytes() + boundary_docs.bytes(),
                _ => 0,
            }
    }
}
fn names_count(offsets: &[u32]) -> usize {
    offsets.len().saturating_sub(1)
}
fn name<'a>(text: &'a [u8], offsets: &[u32], id: u32) -> &'a [u8] {
    &text[offsets[id as usize] as usize..offsets[id as usize + 1] as usize - 1]
}
#[cfg(any(test, feature = "index-v2-lab"))]
fn suffix(text: &[u8], offset: u32) -> &[u8] {
    let tail = &text[offset as usize..];
    &tail[..memchr::memchr(0, tail).unwrap_or(tail.len())]
}
fn prefix_cmp(text: &[u8], query: &[u8]) -> Ordering {
    let shared = text.len().min(query.len());
    text[..shared].cmp(&query[..shared]).then_with(|| {
        if text.len() < query.len() {
            Ordering::Less
        } else {
            Ordering::Equal
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compact_matches_suffix_ranking_recall_and_fuzzy_across_blocks() {
        let mut names: Vec<_> = (0..1400)
            .map(|i| format!("project{i}-XMLHttpRequest.rs"))
            .collect();
        names.extend(
            [
                "Straße",
                "STRASSE",
                "E\u{301}cole",
                "École",
                "東京",
                "banana",
                "XMLHTTP",
                "xmlhttp",
            ]
            .map(str::to_owned),
        );
        let compact = LiteralIndex::build(names.iter().map(String::as_str)).unwrap();
        let legacy = LiteralIndex::build_suffix(names.iter().map(String::as_str))
            .unwrap()
            .into_fm();
        for query in [
            "p",
            "r",
            "14",
            "http",
            "request",
            "XML",
            "strasse",
            "é",
            "東京",
            "ana",
            ".rs",
            "",
            "\0",
            "projct",
            "not-present",
        ] {
            for limit in [0, 1, 3, 100, 202, 1000, usize::MAX] {
                assert_eq!(compact.search(query, limit), legacy.oracle(query, limit));
                assert_eq!(
                    compact.substring(query, limit),
                    legacy.substring(query, limit)
                );
            }
            assert_eq!(compact.fuzzy_names(query), legacy.fuzzy_names(query));
        }
    }
    #[test]
    fn literal_tiers_rank_recall_and_unicode() {
        let names = [
            "report",
            "Report",
            "report.pdf",
            "my-report.txt",
            "xreport",
            "XMLHttpRequest.rs",
            "École",
            "E\u{301}cole",
            "Straße",
            "strasse.txt",
            "東京.txt",
            "banana",
            "foo",
            "bar",
        ];
        let index = LiteralIndex::build(names).unwrap();
        assert_eq!(index.name_count(), names.len() - 2);
        for name in names {
            let folded = nfc_fold(name);
            let ends: Vec<_> = folded
                .char_indices()
                .map(|(p, _)| p)
                .chain(std::iter::once(folded.len()))
                .collect();
            for (i, &left) in ends.iter().enumerate() {
                for &right in &ends[i + 1..] {
                    for limit in [0, 1, 3, 100, usize::MAX] {
                        assert_eq!(
                            index.search(&folded[left..right], limit),
                            index.oracle(&folded[left..right], limit)
                        );
                    }
                }
            }
        }
        assert!(index.search("o\0b", 10).is_empty());
        assert!(index.search("foobar", 10).is_empty());
        assert!(index.search("", 10).is_empty());
        assert!(LiteralIndex::build(["\0"]).is_err());
        assert!(LiteralIndex::build([]).unwrap().search("a", 100).is_empty());
    }
    #[test]
    fn checked_in_relevance_order() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/index-v2/workload.json"))
                .unwrap();
        for case in fixture["relevance"].as_array().unwrap() {
            let names: Vec<_> = case["names"]
                .as_array()
                .unwrap()
                .iter()
                .map(|n| n.as_str().unwrap())
                .collect();
            let index = LiteralIndex::build(names).unwrap();
            let actual: Vec<_> = index
                .search(case["query"].as_str().unwrap(), 100)
                .into_iter()
                .map(|(_, id)| index.name(id).to_owned())
                .collect();
            let expected: Vec<_> = case["expected"]
                .as_array()
                .unwrap()
                .iter()
                .map(|n| nfc_fold(n.as_str().unwrap()))
                .collect();
            assert_eq!(actual, expected);
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct LiteralImage {
    fuzzy: super::fuzzy::FuzzyImage,
    text: PoolImage,
    offsets: ColumnImage,
    sorted: Span,
    postings: PostingsImage,
    #[serde(flatten)]
    retrieval: RetrievalImage,
    prefix_docs: MatrixImage,
    boundaries: BitsImage,
}
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum RetrievalImage {
    Blocks {
        block_search: BlockImage,
        boundary_search: BlockImage,
    },
    Legacy {
        docs: MatrixImage,
        boundary_bits: BitsImage,
        boundary_docs: MatrixImage,
        fm: FmImage,
    },
}
impl LiteralIndex {
    pub fn into_fm(mut self) -> Self {
        if matches!(self.retrieval, Retrieval::Legacy { fm: None, .. }) {
            let (bwt, skip) = self.fm_input();
            if let Retrieval::Legacy { suffixes, fm, .. } = &mut self.retrieval {
                *suffixes = Vec::new().into();
                crate::catalog::storage::release_builder_memory();
                *fm = Some(FmIndex::new(bwt, skip));
            }
        }
        self
    }
    pub fn save(&self, writer: &mut Writer) -> Result<LiteralImage> {
        Ok(LiteralImage {
            fuzzy: self.fuzzy.save(writer)?,
            text: self.text.save(writer)?,
            offsets: self.offsets.save(writer)?,
            sorted: self.sorted.save(writer)?,
            postings: self.postings.save(writer)?,
            retrieval: match &self.retrieval {
                Retrieval::Blocks { substrings, starts } => RetrievalImage::Blocks {
                    block_search: substrings.save(writer)?,
                    boundary_search: starts.save(writer)?,
                },
                Retrieval::Legacy {
                    docs,
                    boundary_bits,
                    boundary_docs,
                    fm,
                    ..
                } => RetrievalImage::Legacy {
                    docs: docs.save(writer)?,
                    boundary_bits: boundary_bits.save(writer)?,
                    boundary_docs: boundary_docs.save(writer)?,
                    fm: fm
                        .as_ref()
                        .ok_or_else(|| anyhow::anyhow!("unconverted suffix laboratory"))?
                        .save(writer)?,
                },
            },
            prefix_docs: self.prefix_docs.save(writer)?,
            boundaries: self.boundaries.save(writer)?,
        })
    }
    pub fn load(reader: &Reader, image: LiteralImage) -> Result<Self> {
        let text = BytePool::load_search(reader, image.text)?;
        let offsets = Column::<u32>::load(reader, image.offsets)?;
        ensure!(
            offsets.first() == Some(0) && offsets.last() == Some(text.len() as u32),
            "invalid name offsets"
        );
        for pair in offsets.pairs() {
            ensure!(
                pair[0] < pair[1]
                    && (pair[1] as usize) <= text.len()
                    && text.read(pair[1] as usize - 1..pair[1] as usize)[0] == 0,
                "invalid name boundary"
            );
            ensure!(
                std::str::from_utf8(&text.read(pair[0] as usize..pair[1] as usize - 1)).is_ok(),
                "invalid name UTF-8"
            );
        }
        let count = offsets.len() - 1;
        let result = Self {
            fuzzy: super::fuzzy::FuzzyIndex::load(reader, image.fuzzy)?,
            text,
            offsets,
            sorted: Packed::load(reader, image.sorted)?,
            postings: Postings::load(reader, image.postings)?,
            retrieval: match image.retrieval {
                RetrievalImage::Blocks {
                    block_search,
                    boundary_search,
                } => Retrieval::Blocks {
                    substrings: BlockIndex::load(reader, block_search, count, false)?,
                    starts: BlockIndex::load(reader, boundary_search, count, true)?,
                },
                RetrievalImage::Legacy {
                    docs,
                    boundary_bits,
                    boundary_docs,
                    fm,
                } => Retrieval::Legacy {
                    suffixes: Vec::new().into(),
                    docs: WaveletMatrix::load(reader, docs)?,
                    boundary_bits: RankBits::load(reader, boundary_bits)?,
                    boundary_docs: WaveletMatrix::load(reader, boundary_docs)?,
                    fm: Some(FmIndex::load(reader, fm)?),
                },
            },
            prefix_docs: WaveletMatrix::load(reader, image.prefix_docs)?,
            boundaries: RankBits::load(reader, image.boundaries)?,
        };
        ensure!(
            result.sorted.len() == result.name_count()
                && result
                    .sorted
                    .iter()
                    .all(|&n| (n as usize) < result.name_count()),
            "invalid sorted names"
        );
        ensure!(
            result.prefix_docs.len() == result.name_count()
                && result.boundaries.len() == result.text.len(),
            "invalid name rank or boundary dimensions"
        );
        if let Retrieval::Legacy {
            docs,
            boundary_bits,
            boundary_docs,
            ..
        } = &result.retrieval
        {
            ensure!(
                docs.len() == result.suffix_count()
                    && boundary_bits.len() == result.suffix_count()
                    && boundary_docs.len() == boundary_bits.rank1(boundary_bits.len()),
                "invalid document matrix"
            );
        }
        if let Retrieval::Blocks { substrings, starts } = &result.retrieval {
            substrings.validate_pair(starts)?;
            let names = || (0..result.name_count() as u32).map(|id| result.name(id));
            substrings.validate(names(), |_, _| false)?;
            starts.validate(names(), |id, p| {
                let offset = result.offsets.get(id) as usize + p;
                result.boundaries.rank1(offset + 1) != result.boundaries.rank1(offset)
            })?;
        }
        ensure!(
            result.postings.len() == result.name_count(),
            "invalid name postings"
        );
        result.fuzzy.validate(result.name_count())?;
        Ok(result)
    }
}

impl LiteralIndex {
    pub fn fuzzy_names(&self, query: &str) -> Vec<u32> {
        let query = nfc_fold(query);
        self.fuzzy
            .candidates(&query)
            .into_iter()
            .filter(|&(id, acronym)| {
                acronym
                    || self
                        .name(id)
                        .split(|c: char| !c.is_alphanumeric())
                        .any(|word| super::fuzzy::one_edit(word, &query))
            })
            .map(|(id, _)| id)
            .collect()
    }
}

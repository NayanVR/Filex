//! Ranked unique-name search. Compact gram blocks select candidates; text
//! verification preserves exact structural tiers.
use super::blocks::{BlockImage, BlockIndex};
use super::wavelet::{BitsImage, MatrixImage, RankBits, WaveletMatrix};
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
    substrings: BlockIndex,
    starts: BlockIndex,
    sorted: Packed<u32>,
    fuzzy: super::fuzzy::FuzzyIndex,
    prefix_docs: WaveletMatrix,
    // Original-name boundaries survive normalization and deduplication.
    boundaries: RankBits,
}

impl LiteralIndex {
    pub(crate) fn remap(&mut self, reader: &Reader) -> Result<()> {
        self.text.remap(reader)?;
        self.offsets.remap(reader)?;
        self.sorted.remap(reader)?;
        self.postings.remap(reader)?;
        self.prefix_docs.remap(reader)?;
        self.boundaries.remap(reader)?;
        self.fuzzy.remap(reader)?;
        self.substrings.remap(reader)?;
        self.starts.remap(reader)?;
        Ok(())
    }

    pub fn build<'a>(names: impl IntoIterator<Item = &'a str>) -> Result<Self> {
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
        let names = || {
            (0..offsets.len() as u32 - 1)
                .map(|id| std::str::from_utf8(name(&text, &offsets, id)).unwrap())
        };
        let substrings = BlockIndex::build(names(), 64, 4096);
        let starts = BlockIndex::build_starts(names(), 64, 1024, |id, p| {
            let offset = offsets[id] as usize + p;
            boundaries.rank1(offset + 1) != boundaries.rank1(offset)
        });
        let mut sorted: Vec<_> = (0..offsets.len() as u32 - 1).collect();
        sorted.sort_unstable_by(|&a, &b| name(&text, &offsets, a).cmp(name(&text, &offsets, b)));
        let prefix_docs = WaveletMatrix::new(sorted.clone());
        let built = Self {
            postings,
            text: BytePool::build_search(text)?,
            offsets: offsets.into(),
            substrings,
            starts,
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
    /// Substring-only rank oracle comparison, independent of match tiers.
    pub fn substring(&self, query: &str, limit: usize) -> Vec<u32> {
        let query = nfc_fold(query);
        if query.is_empty() || query.contains('\0') || limit == 0 {
            return Vec::new();
        }
        let mut found = Vec::new();
        self.substrings.visit(query.as_bytes(), |id| {
            if self.name(id as u32).contains(&query) {
                found.push(id as u32);
            }
            found.len() < limit
        });
        found
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
        self.substrings.visit(query.as_bytes(), &mut visit);
        anyhow::ensure!(!cancel.load(Ordering::Relaxed), "stream cancelled");
        Ok(found)
    }

    /// Planning estimate only: signatures can overestimate matching names.
    pub fn estimate(&self, folded: &[u8]) -> usize {
        self.substrings.estimate(folded)
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
    pub fn search(&self, query: &str, limit: usize) -> Vec<(Tier, u32)> {
        self.search_blocks(&self.substrings, &self.starts, query, limit)
    }
    /// Exact tier/rank retrieval through a conservative block accelerator.
    /// Takes the blocks explicitly so the lab can try other block sizes.
    pub fn search_blocks(
        &self,
        blocks: &BlockIndex,
        starts: &BlockIndex,
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
            + self.substrings.bytes()
            + self.starts.bytes()
    }
}
fn name<'a>(text: &'a [u8], offsets: &[u32], id: u32) -> &'a [u8] {
    &text[offsets[id as usize] as usize..offsets[id as usize + 1] as usize - 1]
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
    fn blocks_match_the_oracle_across_groups() {
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
        let index = LiteralIndex::build(names.iter().map(String::as_str)).unwrap();
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
                assert_eq!(index.search(query, limit), index.oracle(query, limit));
                let mut all: Vec<_> = index
                    .oracle(query, usize::MAX)
                    .into_iter()
                    .map(|(_, id)| id)
                    .collect();
                all.sort_unstable();
                all.truncate(limit);
                assert_eq!(index.substring(query, limit), all);
            }
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
                .map(|(_, id)| index.name(id).into_owned())
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
    block_search: BlockImage,
    boundary_search: BlockImage,
    prefix_docs: MatrixImage,
    boundaries: BitsImage,
}
impl LiteralIndex {
    pub fn save(&self, writer: &mut Writer) -> Result<LiteralImage> {
        Ok(LiteralImage {
            fuzzy: self.fuzzy.save(writer)?,
            text: self.text.save(writer)?,
            offsets: self.offsets.save(writer)?,
            sorted: self.sorted.save(writer)?,
            postings: self.postings.save(writer)?,
            block_search: self.substrings.save(writer)?,
            boundary_search: self.starts.save(writer)?,
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
            substrings: BlockIndex::load(reader, image.block_search, count, false)?,
            starts: BlockIndex::load(reader, image.boundary_search, count, true)?,
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
        result.substrings.validate_pair(&result.starts)?;
        let names = || (0..result.name_count() as u32).map(|id| result.name(id));
        result.substrings.validate(names(), |_, _| false)?;
        result.starts.validate(names(), |id, p| {
            let offset = result.offsets.get(id) as usize + p;
            result.boundaries.rank1(offset + 1) != result.boundaries.rank1(offset)
        })?;
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

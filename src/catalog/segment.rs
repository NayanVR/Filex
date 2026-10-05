//! Immutable catalog with block-packed columns and compact ranked name search.
pub use super::path_codec::{os_name, raw_name};
use super::{
    columns::{Column, ColumnImage},
    pool::{BytePool, PoolImage},
    postings::{Postings, PostingsImage},
    storage::{Packed, Reader, Span, Writer},
};
use crate::search::literal::{LiteralImage, LiteralIndex};
use crate::search_filter::{Filter, ItemMeta};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    borrow::Cow,
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
};

// Metadata presence bits are persisted alongside the public file-kind bits.
const HAS_SIZE: u8 = 4;
const HAS_MTIME: u8 = 8;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Identity {
    pub device: u64,
    pub key: u64,
    pub birth: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// Monotonic identity within the database; never reused after deletion.
    pub id: u64,
    /// Zero identifies the root record; all other values refer to a directory.
    pub parent: u64,
    pub root: u32,
    pub name: Vec<u8>,
    pub flags: u8,
    pub identity: Identity,
    pub size: Option<u64>,
    pub mtime: Option<i64>,
}
impl Record {
    pub const DIRECTORY: u8 = 1;
    pub const SYMLINK: u8 = 2;

    pub fn is_dir(&self) -> bool {
        self.flags & Self::DIRECTORY != 0
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Root {
    pub id: u32,
    #[serde(with = "crate::catalog::path_codec")]
    pub path: PathBuf,
    pub device: u64,
}

pub struct Segment {
    root_counts: BTreeMap<u32, u64>,
    root_files: Vec<(u32, u64)>,
    pub search: LiteralIndex,
    pub roots: Vec<Root>,
    pub sequence: u64,
    ids: Column<u64>,
    parents: Column<u64>,
    root_ids: Column<u32>,
    names: Column<u32>,
    devices: Column<u64>,
    native_order: Column<u32>,
    flags: Packed<u8>,
    keys: Column<u64>,
    births: Column<u64>,
    sizes: Column<u64>,
    mtimes: Column<i64>,
    raw: BytePool,
    raw_offsets: Column<u32>,
    children: Column<u32>,
    meta_keys: Vec<String>,
    meta_postings: Postings,
}
#[derive(Serialize, Deserialize)]
struct Image {
    root_files: Vec<(u32, u64)>,
    normalization: u32,
    roots: Vec<Root>,
    sequence: u64,
    search: LiteralImage,
    devices: ColumnImage,
    native_order: ColumnImage,
    ids: ColumnImage,
    parents: ColumnImage,
    root_ids: ColumnImage,
    names: ColumnImage,
    flags: Span,
    keys: ColumnImage,
    births: ColumnImage,
    sizes: ColumnImage,
    mtimes: ColumnImage,
    raw: PoolImage,
    raw_offsets: ColumnImage,
    children: ColumnImage,
    meta_keys: Vec<String>,
    meta_postings: PostingsImage,
}
impl Segment {
    pub fn build(
        records: impl IntoIterator<Item = Record>,
        roots: Vec<Root>,
        sequence: u64,
    ) -> Result<Self> {
        let (
            mut ids,
            mut parents,
            mut root_ids,
            mut names,
            mut flags,
            mut keys,
            mut births,
            mut sizes,
            mut mtimes,
        ) = (
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );
        let mut devices = Vec::new();
        let mut dictionary = HashMap::<Vec<u8>, u32>::new();
        let mut raw = Vec::new();
        let mut raw_offsets = vec![0u32];
        let mut metadata = BTreeMap::<String, Vec<u32>>::new();
        for record in records {
            ensure!(
                record.id > ids.last().copied().unwrap_or(0),
                "catalog IDs must increase"
            );
            let ordinal = ids.len() as u32;
            let raw_id = if let Some(&id) = dictionary.get(&record.name) {
                id
            } else {
                let id = raw_offsets.len() as u32 - 1;
                raw.extend_from_slice(&record.name);
                ensure!(
                    raw.len() < u32::MAX as usize,
                    "raw names exceed segment size"
                );
                raw_offsets.push(raw.len() as u32);
                dictionary.insert(record.name.clone(), id);
                id
            };
            let native = os_name(&record.name);
            let display = native.to_string_lossy();
            if record.parent != 0 {
                for key in meta_keys(
                    &display,
                    record.is_dir(),
                    record.size,
                    record.mtime,
                    record.root,
                ) {
                    metadata.entry(key).or_default().push(ordinal);
                }
            }
            ids.push(record.id);
            parents.push(record.parent);
            root_ids.push(record.root);
            names.push(raw_id);
            flags.push(
                record.flags
                    | if record.size.is_some() { HAS_SIZE } else { 0 }
                    | if record.mtime.is_some() { HAS_MTIME } else { 0 },
            );
            devices.push(record.identity.device);
            keys.push(record.identity.key);
            births.push(record.identity.birth);
            sizes.push(record.size.unwrap_or(0));
            mtimes.push(record.mtime.unwrap_or(0));
        }
        drop(dictionary);
        ids.shrink_to_fit();
        parents.shrink_to_fit();
        root_ids.shrink_to_fit();
        names.shrink_to_fit();
        flags.shrink_to_fit();
        keys.shrink_to_fit();
        births.shrink_to_fit();
        sizes.shrink_to_fit();
        mtimes.shrink_to_fit();
        devices.shrink_to_fit();
        raw.shrink_to_fit();
        raw_offsets.shrink_to_fit();
        let (meta_keys, lists): (Vec<_>, Vec<_>) = metadata.into_iter().unzip();
        let meta_postings = Postings::build_runs(lists)?;
        ensure!(ids.len() < u32::MAX as usize, "too many catalog files");
        super::storage::release_builder_memory();
        let mut children: Vec<u32> = (0..ids.len() as u32).collect();
        children.sort_unstable_by(|&a, &b| {
            parents[a as usize].cmp(&parents[b as usize]).then_with(|| {
                let (a, b) = (names[a as usize] as usize, names[b as usize] as usize);
                raw[raw_offsets[a] as usize..raw_offsets[a + 1] as usize]
                    .cmp(&raw[raw_offsets[b] as usize..raw_offsets[b + 1] as usize])
            })
        });
        let mut native_order: Vec<u32> = (0..ids.len() as u32).collect();
        native_order.sort_unstable_by_key(|&i| {
            (
                root_ids[i as usize],
                devices[i as usize],
                keys[i as usize],
                births[i as usize],
            )
        });
        let root_files = parents
            .iter()
            .enumerate()
            .filter(|(_, p)| **p == 0)
            .map(|(i, _)| (root_ids[i], ids[i]))
            .collect();
        let mut root_counts = BTreeMap::new();
        for (&r, &p) in root_ids.iter().zip(&parents) {
            if p != 0 {
                *root_counts.entry(r).or_insert(0) += 1;
            }
        }
        let ids: Column<_> = ids.into();
        let parents: Column<_> = parents.into();
        let root_ids: Column<_> = root_ids.into();
        let names: Column<_> = names.into();
        let devices: Column<_> = devices.into();
        let native_order: Column<_> = native_order.into();
        let keys: Column<_> = keys.into();
        let births: Column<_> = births.into();
        let sizes: Column<_> = sizes.into();
        let mtimes: Column<_> = mtimes.into();
        let children: Column<_> = children.into();
        super::storage::release_builder_memory();
        let search = LiteralIndex::build(names.iter().enumerate().map(|(i, n)| {
            if parents.get(i) == 0 {
                ""
            } else {
                std::str::from_utf8(
                    &raw[raw_offsets[n as usize] as usize..raw_offsets[n as usize + 1] as usize],
                )
                .unwrap_or("")
            }
        }))?;
        Ok(Self {
            root_counts,
            root_files,
            devices,
            native_order,
            search,
            roots,
            sequence,
            ids,
            parents,
            root_ids,
            names,
            flags: flags.into(),
            keys,
            births,
            sizes,
            mtimes,
            raw: BytePool::build(raw)?,
            raw_offsets: raw_offsets.into(),
            children,
            meta_keys,
            meta_postings,
        })
    }
    pub fn root_count(&self, root: u32) -> u64 {
        self.root_counts.get(&root).copied().unwrap_or(0)
    }
    pub fn len(&self) -> usize {
        self.ids.len()
    }
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }
    pub fn slot(&self, id: u64) -> Option<usize> {
        self.ids.binary_search(&id).ok()
    }
    pub fn id(&self, slot: usize) -> u64 {
        self.ids.get(slot)
    }
    pub fn raw_name(&self, slot: usize) -> Cow<'_, [u8]> {
        let n = self.names.get(slot) as usize;
        self.raw
            .read(self.raw_offsets.get(n) as usize..self.raw_offsets.get(n + 1) as usize)
    }
    pub fn record(&self, slot: usize) -> Record {
        self.record_projected(slot, true, true)
    }
    pub fn record_projected(&self, slot: usize, size: bool, mtime: bool) -> Record {
        let root = self.root_ids.get(slot);
        Record {
            id: self.ids.get(slot),
            parent: self.parents.get(slot),
            root,
            name: self.raw_name(slot).into_owned(),
            flags: self.flags[slot] & (Record::DIRECTORY | Record::SYMLINK),
            identity: Identity {
                device: self.devices.get(slot),
                key: self.keys.get(slot),
                birth: self.births.get(slot),
            },
            size: (size && self.flags[slot] & HAS_SIZE != 0).then(|| self.sizes.get(slot)),
            mtime: (mtime && self.flags[slot] & HAS_MTIME != 0).then(|| self.mtimes.get(slot)),
        }
    }
    pub fn find_native(&self, root: u32, key: Identity) -> Option<u64> {
        if key.key == 0 {
            return None;
        }
        self.native_order
            .binary_search_by_key(&(root, key.device, key.key, key.birth), |i| {
                (
                    self.root_ids.get(i as usize),
                    self.devices.get(i as usize),
                    self.keys.get(i as usize),
                    self.births.get(i as usize),
                )
            })
            .ok()
            .map(|i| self.ids.get(self.native_order.get(i) as usize))
    }
    pub fn root_file(&self, root: u32) -> Option<u64> {
        self.root_files
            .iter()
            .find(|(r, _)| *r == root)
            .map(|(_, id)| *id)
    }
    pub fn size(&self, slot: usize) -> Option<u64> {
        (self.flags[slot] & HAS_SIZE != 0).then(|| self.sizes.get(slot))
    }
    pub fn mtime(&self, slot: usize) -> Option<i64> {
        (self.flags[slot] & HAS_MTIME != 0).then(|| self.mtimes.get(slot))
    }
    pub fn is_dir(&self, slot: usize) -> bool {
        self.flags[slot] & Record::DIRECTORY != 0
    }
    pub fn path_part(&self, slot: usize) -> (u64, u32, Cow<'_, [u8]>) {
        (
            self.parents.get(slot),
            self.root_ids.get(slot),
            self.raw_name(slot),
        )
    }
    pub fn parent(&self, slot: usize) -> u64 {
        self.parents.get(slot)
    }
    pub fn child_ids(&self, parent: u64) -> impl Iterator<Item = u64> + '_ {
        let lo = self
            .children
            .partition_point(|i| self.parents.get(i as usize) < parent);
        let hi = self
            .children
            .partition_point(|i| self.parents.get(i as usize) <= parent);
        (lo..hi).map(|i| self.ids.get(self.children.get(i) as usize))
    }
    pub fn child(&self, parent: u64, name: &[u8]) -> Option<usize> {
        self.children
            .binary_search_by(|i| {
                self.parents
                    .get(i as usize)
                    .cmp(&parent)
                    .then_with(|| self.raw_name(i as usize).as_ref().cmp(name))
            })
            .ok()
            .map(|i| self.children.get(i) as usize)
    }
    pub fn matches(&self, slot: usize, filters: &[Filter]) -> bool {
        if filters.is_empty() {
            return true;
        }
        let native = os_name(&self.raw_name(slot));
        let name = native.to_string_lossy();
        filters.iter().all(|f| {
            f.matches(&ItemMeta {
                name: &name,
                is_dir: self.flags[slot] & Record::DIRECTORY != 0,
                size: if matches!(f, Filter::Size(_)) && self.flags[slot] & HAS_SIZE != 0 {
                    Some(self.sizes.get(slot))
                } else {
                    None
                },
                mtime: if matches!(f, Filter::Modified(_)) && self.flags[slot] & HAS_MTIME != 0 {
                    Some(self.mtimes.get(slot))
                } else {
                    None
                },
            })
        })
    }
    /// Compressed coarse index candidates; exact metadata predicates verify edges.
    pub fn filter_candidates(&self, filters: &[Filter], limit: usize) -> Option<Vec<u32>> {
        let mut best: Option<(usize, Vec<usize>)> = None;
        for filter in filters {
            if matches!(filter, Filter::Tag(_)) {
                continue;
            }
            let keys: Vec<_> = self
                .meta_keys
                .iter()
                .enumerate()
                .filter_map(|(i, k)| coarse_matches(k, filter).then_some(i))
                .collect();
            let bound = keys
                .iter()
                .map(|&i| self.meta_postings.cardinality_bound(i))
                .sum::<usize>();
            if best.as_ref().is_none_or(|(n, _)| bound < *n) {
                best = Some((bound, keys));
            }
        }
        best.filter(|(bound, _)| *bound < limit).map(|(_, keys)| {
            keys.into_iter()
                .flat_map(|i| self.meta_postings.get(i as u32))
                .collect()
        })
    }

    /// Complete coarse candidates, intersected across metadata predicates.
    /// Unlike interactive retrieval, this never discards a large posting list.
    pub(crate) fn exhaustive_candidates(
        &self,
        filters: &[Filter],
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<Option<crate::search::candidates::Candidates>> {
        use crate::search::candidates::Candidates;
        use std::sync::atomic::Ordering;
        let mut lists = Vec::new();
        for filter in filters {
            ensure!(!cancel.load(Ordering::Relaxed), "stream cancelled");
            // Historical extension keys use Unicode lowercasing, while exact
            // predicates use ASCII lowercasing. Verify non-ASCII extensions
            // directly so old on-disk keys cannot exclude a valid match.
            if matches!(filter, Filter::Tag(_))
                || matches!(filter, Filter::Ext(ext) if !ext.is_ascii())
            {
                continue;
            }
            let mut keys = Vec::new();
            for (i, key) in self.meta_keys.iter().enumerate() {
                if i % 4096 == 0 {
                    ensure!(!cancel.load(Ordering::Relaxed), "stream cancelled");
                }
                if coarse_matches(key, filter) {
                    keys.push(i);
                }
            }
            let bound: usize = keys
                .iter()
                .map(|&i| self.meta_postings.cardinality_bound(i))
                .sum();
            lists.push((bound, keys));
        }
        lists.sort_unstable_by_key(|(bound, _)| *bound);
        let mut result: Option<Candidates> = None;
        for (_, keys) in lists {
            let mut next = Candidates::empty(self.len());
            for key in keys {
                for (i, slot) in self.meta_postings.get(key as u32).enumerate() {
                    if i % 4096 == 0 {
                        ensure!(!cancel.load(Ordering::Relaxed), "stream cancelled");
                    }
                    next.insert(slot as usize);
                }
            }
            if let Some(current) = &mut result {
                current.intersect(&next);
            } else {
                result = Some(next);
            }
            if result.as_ref().is_some_and(|set| set.count() == 0) {
                break;
            }
        }
        Ok(result)
    }

    /// Reject numeric bucket-edge false positives without decoding a filename.
    pub(crate) fn matches_numeric(&self, slot: usize, filters: &[Filter]) -> bool {
        filters.iter().all(|filter| match filter {
            Filter::Size(bound) => self.size(slot).is_some_and(|n| bound.matches(n)),
            Filter::Modified(bound) => self.mtime(slot).is_some_and(|n| bound.matches(n)),
            _ => true,
        })
    }

    #[cfg(feature = "index-v2-lab")]
    pub fn metadata_postings_equal(&self, other: &Self) -> bool {
        self.meta_keys == other.meta_keys
            && (0..self.meta_keys.len()).all(|id| {
                self.meta_postings
                    .get(id as u32)
                    .eq(other.meta_postings.get(id as u32))
            })
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let mut w = Writer::create(path)?;
        let image = Image {
            root_files: self.root_files.clone(),
            normalization: super::normalize::VERSION,
            roots: self.roots.clone(),
            sequence: self.sequence,
            search: self.search.save(&mut w)?,
            devices: self.devices.save(&mut w)?,
            native_order: self.native_order.save(&mut w)?,
            ids: self.ids.save(&mut w)?,
            parents: self.parents.save(&mut w)?,
            root_ids: self.root_ids.save(&mut w)?,
            names: self.names.save(&mut w)?,
            flags: self.flags.save(&mut w)?,
            keys: self.keys.save(&mut w)?,
            births: self.births.save(&mut w)?,
            sizes: self.sizes.save(&mut w)?,
            mtimes: self.mtimes.save(&mut w)?,
            raw: self.raw.save(&mut w)?,
            raw_offsets: self.raw_offsets.save(&mut w)?,
            children: self.children.save(&mut w)?,
            meta_keys: self.meta_keys.clone(),
            meta_postings: self.meta_postings.save(&mut w)?,
        };
        w.finish(&image)
    }
    /// # Safety
    /// Active generation files must remain immutable until all readers finish.
    pub unsafe fn open(path: &Path) -> Result<Self> {
        let reader = unsafe { Reader::open(path)? };
        let image: Image = reader.metadata()?;
        ensure!(
            image.normalization == super::normalize::VERSION,
            "normalization mismatch"
        );
        let mut segment = Self {
            root_counts: BTreeMap::new(),
            root_files: image.root_files,
            devices: Column::load(&reader, image.devices)?,
            native_order: Column::load(&reader, image.native_order)?,
            search: LiteralIndex::load(&reader, image.search)?,
            roots: image.roots,
            sequence: image.sequence,
            ids: Column::load(&reader, image.ids)?,
            parents: Column::load(&reader, image.parents)?,
            root_ids: Column::load(&reader, image.root_ids)?,
            names: Column::load(&reader, image.names)?,
            flags: Packed::load(&reader, image.flags)?,
            keys: Column::load(&reader, image.keys)?,
            births: Column::load(&reader, image.births)?,
            sizes: Column::load(&reader, image.sizes)?,
            mtimes: Column::load(&reader, image.mtimes)?,
            raw: BytePool::load(&reader, image.raw)?,
            raw_offsets: Column::load(&reader, image.raw_offsets)?,
            children: Column::load(&reader, image.children)?,
            meta_keys: image.meta_keys,
            meta_postings: Postings::load(&reader, image.meta_postings)?,
        };
        for (root, parent) in segment.root_ids.iter().zip(segment.parents.iter()) {
            if parent != 0 {
                *segment.root_counts.entry(root).or_insert(0) += 1;
            }
        }
        segment.validate()?;
        // Validation touches every column. Fresh mappings let query-only
        // processes keep cold metadata pages out of their resident working set.
        segment.remap(&reader.fresh_mapping()?)?;
        // Decoded validation text must not survive as allocator cache in the
        // long-lived query process after the bounded caches have been reset.
        super::storage::release_builder_memory();

        Ok(segment)
    }

    fn validate(&self) -> Result<()> {
        let n = self.len();
        ensure!(
            [
                self.devices.len(),
                self.native_order.len(),
                self.parents.len(),
                self.root_ids.len(),
                self.names.len(),
                self.flags.len(),
                self.keys.len(),
                self.births.len(),
                self.sizes.len(),
                self.mtimes.len(),
                self.children.len()
            ]
            .iter()
            .all(|&len| len == n),
            "catalog column mismatch"
        );
        ensure!(
            self.ids.pairs().all(|w| w[0] < w[1]) && self.ids.first() != Some(0),
            "invalid file IDs"
        );
        ensure!(
            self.raw_offsets.first() == Some(0)
                && self.raw_offsets.last() == Some(self.raw.len() as u32)
                && self.raw_offsets.pairs().all(|w| w[0] <= w[1]),
            "invalid raw names"
        );
        ensure!(
            self.names
                .iter()
                .all(|n| (n as usize) + 1 < self.raw_offsets.len())
                && self
                    .children
                    .iter()
                    .chain(self.native_order.iter())
                    .all(|i| (i as usize) < n),
            "invalid catalog references"
        );
        let mut root_ids = std::collections::HashSet::new();
        ensure!(
            self.roots
                .iter()
                .all(|root| root.id != 0 && root_ids.insert(root.id)),
            "duplicate or zero root ID"
        );
        for slot in 0..n {
            ensure!(
                self.roots.iter().any(|r| r.id == self.root_ids.get(slot)),
                "unknown root"
            );
            let parent = self.parents.get(slot);
            ensure!(
                parent != 0 || self.flags[slot] & Record::DIRECTORY != 0,
                "root record is not a directory"
            );
            ensure!(
                parent == 0
                    || self
                        .slot(parent)
                        .is_some_and(|p| self.flags[p] & Record::DIRECTORY != 0),
                "missing parent"
            );
        }
        ensure!(
            self.meta_keys.len() == self.meta_postings.len()
                && self.meta_keys.windows(2).all(|w| w[0] < w[1]),
            "invalid metadata keys"
        );
        for i in 0..self.meta_postings.len() {
            ensure!(
                self.meta_postings.get(i as u32).all(|v| (v as usize) < n),
                "invalid metadata ordinal"
            );
        }
        for i in 0..self.search.name_count() {
            ensure!(
                self.search.files(i as u32).all(|v| (v as usize) < n),
                "invalid search ordinal"
            );
        }
        ensure!(
            self.root_files
                .iter()
                .all(|(r, id)| self
                    .slot(*id)
                    .is_some_and(|i| self.parents.get(i) == 0 && self.root_ids.get(i) == *r)),
            "invalid root records"
        );
        let mut root_files = std::collections::HashSet::new();
        ensure!(
            self.root_files.len() == self.parents.iter().filter(|&parent| parent == 0).count()
                && self
                    .root_files
                    .iter()
                    .all(|&(root, _)| root_files.insert(root)),
            "incomplete or duplicate root lookup"
        );
        let mut color = vec![0u8; n];
        for start in 0..n {
            let mut chain = Vec::new();
            let mut at = start;
            while color[at] == 0 {
                color[at] = 1;
                chain.push(at);
                if self.parents.get(at) == 0 {
                    break;
                }
                let parent = self.slot(self.parents.get(at)).unwrap();
                ensure!(
                    self.root_ids.get(parent) == self.root_ids.get(at),
                    "parent crosses roots"
                );
                at = parent;
                ensure!(color[at] != 1, "parent cycle");
            }
            for i in chain {
                color[i] = 2;
            }
        }
        self.validate_ordered_indexes()?;
        Ok(())
    }

    fn remap(&mut self, reader: &Reader) -> Result<()> {
        self.ids.remap(reader)?;
        self.parents.remap(reader)?;
        self.root_ids.remap(reader)?;
        self.names.remap(reader)?;
        self.devices.remap(reader)?;
        self.native_order.remap(reader)?;
        self.flags.remap(reader)?;
        self.keys.remap(reader)?;
        self.births.remap(reader)?;
        self.sizes.remap(reader)?;
        self.mtimes.remap(reader)?;
        self.raw.remap(reader)?;
        self.raw_offsets.remap(reader)?;
        self.children.remap(reader)?;
        self.meta_postings.remap(reader)?;
        self.search.remap(reader)?;
        Ok(())
    }

    fn validate_ordered_indexes(&self) -> Result<()> {
        // Child order jumps between dictionary pages. Decode once for this
        // startup-only pass, then release the temporary text before remapping.
        // Queries continue to use the bounded page cache.
        let raw = self.raw.validation_text()?;
        let name = |slot: usize| {
            let id = self.names.get(slot) as usize;
            &raw[self.raw_offsets.get(id) as usize..self.raw_offsets.get(id + 1) as usize]
        };
        // Bounds alone are insufficient: duplicate or unsorted ordinals make
        // binary child/native lookups silently miss valid records.
        let mut seen = vec![false; self.len()];
        for (name, ordinals) in [("child", &self.children), ("native", &self.native_order)] {
            seen.fill(false);
            for ordinal in ordinals.iter() {
                ensure!(
                    !std::mem::replace(&mut seen[ordinal as usize], true),
                    "duplicate {name} ordinal"
                );
            }
        }
        ensure!(
            self.children.pairs().all(|pair| {
                let (left, right) = (pair[0] as usize, pair[1] as usize);
                (self.parents.get(left), name(left)) <= (self.parents.get(right), name(right))
            }),
            "unsorted child lookup"
        );
        let native_key = |slot: usize| {
            (
                self.root_ids.get(slot),
                self.devices.get(slot),
                self.keys.get(slot),
                self.births.get(slot),
            )
        };
        ensure!(
            self.native_order
                .pairs()
                .all(|pair| { native_key(pair[0] as usize) <= native_key(pair[1] as usize) }),
            "unsorted native lookup"
        );
        Ok(())
    }
}
fn meta_keys(
    name: &str,
    dir: bool,
    size: Option<u64>,
    mtime: Option<i64>,
    root: u32,
) -> Vec<String> {
    let mut keys = vec![
        format!("r:{root}"),
        format!("k:{:?}", crate::listing::FileKind::of(name, dir)),
    ];
    if let Some(ext) = Path::new(name).extension() {
        keys.push(format!("e:{}", ext.to_string_lossy().to_lowercase()));
    }
    if let Some(n) = size {
        keys.push(format!(
            "s:{}",
            if n == 0 { 0 } else { 64 - n.leading_zeros() }
        ));
    }
    if let Some(n) = mtime {
        keys.push(format!("m:{}", n.div_euclid(86400)));
    }
    keys
}
fn coarse_matches(key: &str, filter: &Filter) -> bool {
    match filter {
        Filter::Ext(ext) => key.strip_prefix("e:") == Some(ext.as_str()),
        Filter::Kind(kind) => key == format!("k:{kind:?}"),
        Filter::Size(bound) => key
            .strip_prefix("s:")
            .and_then(|n| n.parse::<u32>().ok())
            .is_some_and(|b| {
                let lo = if b == 0 { 0 } else { 1u64 << (b - 1) };
                let hi = if b >= 64 { u64::MAX } else { (1u64 << b) - 1 };
                overlaps(*bound, lo, hi)
            }),
        Filter::Modified(bound) => key
            .strip_prefix("m:")
            .and_then(|n| n.parse::<i64>().ok())
            .is_some_and(|day| {
                overlaps(
                    *bound,
                    day.saturating_mul(86400),
                    day.saturating_mul(86400).saturating_add(86399),
                )
            }),
        Filter::Tag(_) => false,
    }
}
fn overlaps<T: PartialOrd + Copy>(bound: crate::search_filter::Bound<T>, lo: T, hi: T) -> bool {
    use crate::search_filter::Bound::*;
    match bound {
        Lt(n) => lo < n,
        Le(n) => lo <= n,
        Gt(n) => hi > n,
        Ge(n) => hi >= n,
        Eq(n) => lo <= n && hi >= n,
        Range(a, b) => hi >= a && lo <= b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compressed_names_roundtrip_native_bytes_across_pages() {
        let records: Vec<_> = (1..=5000)
            .map(|id| {
                let mut name =
                    format!("shared-École-Straße-東京-XMLHttpRequest-{id:06}.txt").into_bytes();
                if id % 7 == 0 {
                    name.push(255);
                }
                Record {
                    id,
                    parent: if id == 1 { 0 } else { 1 },
                    root: 1,
                    name,
                    flags: u8::from(id == 1),
                    identity: Identity::default(),
                    size: Some(id),
                    mtime: Some(-1),
                }
            })
            .collect();
        let segment = Segment::build(
            records.clone(),
            vec![Root {
                id: 1,
                path: "/fixture".into(),
                device: 0,
            }],
            0,
        )
        .unwrap();
        assert!(matches!(segment.raw, BytePool::Deflate { .. }));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("compressed");
        segment.save(&path).unwrap();
        let mapped = unsafe { Segment::open(&path).unwrap() };
        for (slot, record) in records.iter().enumerate() {
            assert_eq!(&mapped.record(slot), record);
            assert_eq!(mapped.child(record.parent, &record.name), Some(slot));
        }
        for query in ["é", "STRASSE", "東京", "http", "txt", "000007", "missing"] {
            assert_eq!(
                mapped.search.search(query, 202),
                segment.search.oracle(query, 202)
            );
        }
    }

    fn fixture() -> Segment {
        let records = (1..=3).map(|id| Record {
            id,
            parent: if id == 1 { 0 } else { 1 },
            root: 1,
            name: format!("file-{id}").into_bytes(),
            flags: u8::from(id == 1),
            identity: Identity {
                key: id,
                ..Default::default()
            },
            size: None,
            mtime: None,
        });
        Segment::build(
            records,
            vec![Root {
                id: 1,
                path: "/fixture".into(),
                device: 0,
            }],
            0,
        )
        .unwrap()
    }

    fn rejected_after_save(segment: Segment) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("invalid.fx2");
        segment.save(&path).unwrap();
        drop(segment);
        // A valid checksum cannot compensate for invalid lookup structures.
        assert!(unsafe { Segment::open(&path) }.is_err());
    }

    #[test]
    fn rejects_duplicate_and_unsorted_lookup_ordinals() {
        let mut segment = fixture();
        segment.children = vec![0, 0, 2].into();
        rejected_after_save(segment);
        let mut segment = fixture();
        segment.children = vec![0, 2, 1].into();
        rejected_after_save(segment);
        let mut segment = fixture();
        segment.native_order = vec![0, 2, 1].into();
        rejected_after_save(segment);
    }

    #[test]
    fn rejects_incomplete_root_lookup() {
        let mut segment = fixture();
        segment.root_files.clear();
        rejected_after_save(segment);
    }

    #[test]
    fn legacy_generation_rebuild_preserves_large_ids_native_identity_and_signed_dates() {
        use sha2::{Digest, Sha256};
        let records: Vec<_> = (0..520u64)
            .map(|i| Record {
                id: (1 << 40) + i * 3,
                parent: if i == 0 { 0 } else { 1 << 40 },
                root: 1,
                name: format!("File-{i}-Straße.txt").into_bytes(),
                flags: u8::from(i == 0),
                identity: Identity {
                    device: 99,
                    key: u64::MAX - i,
                    birth: (1 << 63) + i,
                },
                size: Some(u64::MAX - i),
                mtime: Some(i64::MIN + i as i64),
            })
            .collect();
        let mut old = Segment::build(
            records.clone(),
            vec![Root {
                id: 1,
                path: "/fixture".into(),
                device: 99,
            }],
            7,
        )
        .unwrap();
        macro_rules! plain {
            ($($field:ident),+) => { $(old.$field = Column::Legacy(old.$field.iter().collect::<Vec<_>>().into());)+ };
        }
        plain!(
            ids,
            parents,
            root_ids,
            names,
            devices,
            native_order,
            keys,
            births,
            sizes,
            mtimes,
            children,
            raw_offsets
        );
        old.raw = BytePool::plain(old.raw.read(0..old.raw.len()).into_owned());
        old.meta_postings = Postings::build(
            (0..old.meta_postings.len()).map(|i| old.meta_postings.get(i as u32).collect()),
        )
        .unwrap();
        old.meta_postings.use_legacy_offsets();
        old.search = LiteralIndex::build_suffix(records.iter().map(|r| {
            if r.parent == 0 {
                ""
            } else {
                std::str::from_utf8(&r.name).unwrap()
            }
        }))
        .unwrap()
        .into_fm();
        old.search.use_legacy_offsets();
        let dir = tempfile::tempdir().unwrap();
        let old_path = dir.path().join("old.fx2");
        old.save(&old_path).unwrap();
        drop(old);
        let mut bytes = std::fs::read(&old_path).unwrap();
        bytes[..8].copy_from_slice(b"FXSEG002");
        let end = bytes.len() - 32;
        let hash = Sha256::digest(&bytes[..end]);
        bytes[end..].copy_from_slice(&hash);
        std::fs::write(&old_path, bytes).unwrap();
        let old = unsafe { Segment::open(&old_path) }.unwrap();
        let new = Segment::build(
            (0..old.len()).map(|i| old.record(i)),
            old.roots.clone(),
            old.sequence,
        )
        .unwrap();
        let new_path = dir.path().join("new.fx3");
        new.save(&new_path).unwrap();
        assert_eq!(&std::fs::read(&new_path).unwrap()[..8], b"FXSEG004");
        let new = unsafe { Segment::open(&new_path) }.unwrap();
        for (slot, record) in records.iter().enumerate() {
            assert_eq!(&old.record(slot), record);
            assert_eq!(&new.record(slot), record);
            assert_eq!(new.find_native(1, record.identity), Some(record.id));
        }
        for query in ["f", "strasse", "txt", "-51", "missing"] {
            assert_eq!(old.search.search(query, 202), new.search.search(query, 202));
        }
    }
}

//! Unified immutable catalog plus selected FM search structures.
use super::{
    postings::Postings,
    storage::{Packed, Reader, Span, Writer},
};
use crate::search::literal::{LiteralImage, LiteralIndex};
use crate::search_filter::{Filter, ItemMeta};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    ffi::OsString,
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
    ids: Packed<u64>,
    parents: Packed<u64>,
    root_ids: Packed<u32>,
    names: Packed<u32>,
    devices: Packed<u64>,
    native_order: Packed<u32>,
    flags: Packed<u8>,
    keys: Packed<u64>,
    births: Packed<u64>,
    sizes: Packed<u64>,
    mtimes: Packed<i64>,
    raw: Packed<u8>,
    raw_offsets: Packed<u32>,
    children: Packed<u32>,
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
    devices: Span,
    native_order: Span,
    ids: Span,
    parents: Span,
    root_ids: Span,
    names: Span,
    flags: Span,
    keys: Span,
    births: Span,
    sizes: Span,
    mtimes: Span,
    raw: Span,
    raw_offsets: Span,
    children: Span,
    meta_keys: Vec<String>,
    meta_postings: (Span, Span),
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
        let meta_postings = Postings::build(lists)?;
        ensure!(ids.len() < u32::MAX as usize, "too many catalog files");
        super::storage::release_builder_memory();
        let search = LiteralIndex::build(names.iter().enumerate().map(|(i, &n)| {
            if parents[i] == 0 {
                ""
            } else {
                std::str::from_utf8(
                    &raw[raw_offsets[n as usize] as usize..raw_offsets[n as usize + 1] as usize],
                )
                .unwrap_or("")
            }
        }))?
        .into_fm();
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
        Ok(Self {
            root_counts,
            root_files,
            devices: devices.into(),
            native_order: native_order.into(),
            search,
            roots,
            sequence,
            ids: ids.into(),
            parents: parents.into(),
            root_ids: root_ids.into(),
            names: names.into(),
            flags: flags.into(),
            keys: keys.into(),
            births: births.into(),
            sizes: sizes.into(),
            mtimes: mtimes.into(),
            raw: raw.into(),
            raw_offsets: raw_offsets.into(),
            children: children.into(),
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
        self.ids[slot]
    }
    pub fn raw_name(&self, slot: usize) -> &[u8] {
        let n = self.names[slot] as usize;
        &self.raw[self.raw_offsets[n] as usize..self.raw_offsets[n + 1] as usize]
    }
    pub fn record(&self, slot: usize) -> Record {
        self.record_projected(slot, true, true)
    }
    pub fn record_projected(&self, slot: usize, size: bool, mtime: bool) -> Record {
        let root = self.root_ids[slot];
        Record {
            id: self.ids[slot],
            parent: self.parents[slot],
            root,
            name: self.raw_name(slot).to_vec(),
            flags: self.flags[slot] & (Record::DIRECTORY | Record::SYMLINK),
            identity: Identity {
                device: self.devices[slot],
                key: self.keys[slot],
                birth: self.births[slot],
            },
            size: (size && self.flags[slot] & HAS_SIZE != 0).then(|| self.sizes[slot]),
            mtime: (mtime && self.flags[slot] & HAS_MTIME != 0).then(|| self.mtimes[slot]),
        }
    }
    pub fn find_native(&self, root: u32, key: Identity) -> Option<u64> {
        if key.key == 0 {
            return None;
        }
        self.native_order
            .binary_search_by_key(&(root, key.device, key.key, key.birth), |&i| {
                (
                    self.root_ids[i as usize],
                    self.devices[i as usize],
                    self.keys[i as usize],
                    self.births[i as usize],
                )
            })
            .ok()
            .map(|i| self.ids[self.native_order[i] as usize])
    }
    pub fn root_file(&self, root: u32) -> Option<u64> {
        self.root_files
            .iter()
            .find(|(r, _)| *r == root)
            .map(|(_, id)| *id)
    }
    pub fn size(&self, slot: usize) -> Option<u64> {
        (self.flags[slot] & HAS_SIZE != 0).then(|| self.sizes[slot])
    }
    pub fn mtime(&self, slot: usize) -> Option<i64> {
        (self.flags[slot] & HAS_MTIME != 0).then(|| self.mtimes[slot])
    }
    pub fn is_dir(&self, slot: usize) -> bool {
        self.flags[slot] & Record::DIRECTORY != 0
    }
    pub fn path_part(&self, slot: usize) -> (u64, u32, &[u8]) {
        (self.parents[slot], self.root_ids[slot], self.raw_name(slot))
    }
    pub fn parent(&self, slot: usize) -> u64 {
        self.parents[slot]
    }
    pub fn child_ids(&self, parent: u64) -> impl Iterator<Item = u64> + '_ {
        let lo = self
            .children
            .partition_point(|&i| self.parents[i as usize] < parent);
        let hi = self
            .children
            .partition_point(|&i| self.parents[i as usize] <= parent);
        self.children[lo..hi].iter().map(|&i| self.ids[i as usize])
    }
    pub fn child(&self, parent: u64, name: &[u8]) -> Option<usize> {
        self.children
            .binary_search_by(|&i| {
                self.parents[i as usize]
                    .cmp(&parent)
                    .then_with(|| self.raw_name(i as usize).cmp(name))
            })
            .ok()
            .map(|i| self.children[i] as usize)
    }
    pub fn matches(&self, slot: usize, filters: &[Filter]) -> bool {
        let native = os_name(self.raw_name(slot));
        let name = native.to_string_lossy();
        filters.iter().all(|f| {
            f.matches(&ItemMeta {
                name: &name,
                is_dir: self.flags[slot] & Record::DIRECTORY != 0,
                size: if matches!(f, Filter::Size(_)) && self.flags[slot] & HAS_SIZE != 0 {
                    Some(self.sizes[slot])
                } else {
                    None
                },
                mtime: if matches!(f, Filter::Modified(_)) && self.flags[slot] & HAS_MTIME != 0 {
                    Some(self.mtimes[slot])
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
                .map(|&i| self.meta_postings.encoded_len(i))
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
            devices: Packed::load(&reader, image.devices)?,
            native_order: Packed::load(&reader, image.native_order)?,
            search: LiteralIndex::load(&reader, image.search)?,
            roots: image.roots,
            sequence: image.sequence,
            ids: Packed::load(&reader, image.ids)?,
            parents: Packed::load(&reader, image.parents)?,
            root_ids: Packed::load(&reader, image.root_ids)?,
            names: Packed::load(&reader, image.names)?,
            flags: Packed::load(&reader, image.flags)?,
            keys: Packed::load(&reader, image.keys)?,
            births: Packed::load(&reader, image.births)?,
            sizes: Packed::load(&reader, image.sizes)?,
            mtimes: Packed::load(&reader, image.mtimes)?,
            raw: Packed::load(&reader, image.raw)?,
            raw_offsets: Packed::load(&reader, image.raw_offsets)?,
            children: Packed::load(&reader, image.children)?,
            meta_keys: image.meta_keys,
            meta_postings: Postings::load(&reader, image.meta_postings)?,
        };
        for (&root, &parent) in segment.root_ids.iter().zip(segment.parents.iter()) {
            if parent != 0 {
                *segment.root_counts.entry(root).or_insert(0) += 1;
            }
        }
        segment.validate()?;
        // Validation touches every column. Fresh mappings let query-only
        // processes keep cold metadata pages out of their resident working set.
        segment.remap(&reader.fresh_mapping()?)?;

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
            self.ids.windows(2).all(|w| w[0] < w[1]) && self.ids.first() != Some(&0),
            "invalid file IDs"
        );
        ensure!(
            self.raw_offsets.first() == Some(&0)
                && self.raw_offsets.last().copied() == Some(self.raw.len() as u32)
                && self.raw_offsets.windows(2).all(|w| w[0] <= w[1]),
            "invalid raw names"
        );
        ensure!(
            self.names
                .iter()
                .all(|&n| (n as usize) + 1 < self.raw_offsets.len())
                && self
                    .children
                    .iter()
                    .chain(self.native_order.iter())
                    .all(|&i| (i as usize) < n),
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
                self.roots.iter().any(|r| r.id == self.root_ids[slot]),
                "unknown root"
            );
            let parent = self.parents[slot];
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
            self.root_files.iter().all(|(r, id)| self
                .slot(*id)
                .is_some_and(|i| self.parents[i] == 0 && self.root_ids[i] == *r)),
            "invalid root records"
        );
        let mut root_files = std::collections::HashSet::new();
        ensure!(
            self.root_files.len() == self.parents.iter().filter(|&&parent| parent == 0).count()
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
                if self.parents[at] == 0 {
                    break;
                }
                let parent = self.slot(self.parents[at]).unwrap();
                ensure!(
                    self.root_ids[parent] == self.root_ids[at],
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
        // Bounds alone are insufficient: duplicate or unsorted ordinals make
        // binary child/native lookups silently miss valid records.
        let mut seen = vec![false; self.len()];
        for (name, ordinals) in [("child", &self.children), ("native", &self.native_order)] {
            seen.fill(false);
            for &ordinal in ordinals.iter() {
                ensure!(
                    !std::mem::replace(&mut seen[ordinal as usize], true),
                    "duplicate {name} ordinal"
                );
            }
        }
        ensure!(
            self.children.windows(2).all(|pair| {
                let (left, right) = (pair[0] as usize, pair[1] as usize);
                (self.parents[left], self.raw_name(left))
                    <= (self.parents[right], self.raw_name(right))
            }),
            "unsorted child lookup"
        );
        let native_key = |slot: usize| {
            (
                self.root_ids[slot],
                self.devices[slot],
                self.keys[slot],
                self.births[slot],
            )
        };
        ensure!(
            self.native_order
                .windows(2)
                .all(|pair| { native_key(pair[0] as usize) <= native_key(pair[1] as usize) }),
            "unsorted native lookup"
        );
        Ok(())
    }
}
pub fn os_name(raw: &[u8]) -> OsString {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        OsString::from_vec(raw.to_vec())
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        if raw.starts_with(&[255, 254]) {
            OsString::from_wide(
                &raw[2..]
                    .chunks_exact(2)
                    .map(|b| u16::from_le_bytes([b[0], b[1]]))
                    .collect::<Vec<_>>(),
            )
        } else {
            OsString::from(String::from_utf8_lossy(raw).into_owned())
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        OsString::from(String::from_utf8_lossy(raw).into_owned())
    }
}
pub fn raw_name(name: &std::ffi::OsStr) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        name.as_bytes().to_vec()
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        if let Some(text) = name.to_str() {
            text.as_bytes().to_vec()
        } else {
            let mut bytes = vec![255, 254];
            for unit in name.encode_wide() {
                bytes.extend_from_slice(&unit.to_le_bytes());
            }
            bytes
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        name.to_string_lossy().as_bytes().to_vec()
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
}

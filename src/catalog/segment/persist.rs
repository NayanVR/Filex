//! On-disk segment image: save, memory-mapped open, and the validation
//! that rejects a corrupt or inconsistent image before it is served.

use super::*;

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
    tombstones: Option<Vec<u64>>,
}

impl Segment {
    pub fn save(&self, path: &Path) -> Result<()> {
        let mut w = Writer::create(path)?;
        let image = Image {
            root_files: self.root_files.clone(),
            normalization: crate::catalog::normalize::VERSION,
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
            tombstones: self.tombstones.clone(),
        };
        w.finish(&image)
    }
    /// # Safety
    /// Active generation files must remain immutable until all readers finish.
    pub unsafe fn open(path: &Path) -> Result<Self> {
        let reader = unsafe { Reader::open(path)? };
        let image: Image = reader.metadata()?;
        ensure!(
            image.normalization == crate::catalog::normalize::VERSION,
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
            tombstones: image.tombstones,
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
        crate::catalog::storage::release_builder_memory();

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
        ensure!(
            self.tombstones
                .as_ref()
                .is_none_or(|t| t.first() != Some(&0) && t.windows(2).all(|w| w[0] < w[1])),
            "invalid tombstones"
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
                    || self.tombstones.is_some()
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
                // A delta's chain may continue in an older level.
                let Some(parent) = self.slot(self.parents.get(at)) else {
                    ensure!(self.tombstones.is_some(), "missing parent");
                    break;
                };
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

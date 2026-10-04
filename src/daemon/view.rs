//! Query epochs share immutable base mappings and small overlay layers.
use crate::catalog::segment::{Identity, Record, Root, Segment, os_name, raw_name};
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::Arc,
};
#[derive(Clone, Default)]
pub struct Overlay {
    pub records: BTreeMap<u64, Option<Record>>,
    children: HashMap<(u64, Vec<u8>), u64>,
}

#[cfg(test)]
mod record_tests {
    use super::*;
    use crate::catalog::segment::{Identity, Root};

    #[test]
    fn appended_records_keep_base_paths_but_tombstones_hide_descendants() {
        let record = |id, parent, directory| Record {
            id,
            parent,
            root: 1,
            name: format!("entry-{id}").into_bytes(),
            flags: if directory { Record::DIRECTORY } else { 0 },
            identity: Identity::default(),
            size: Some(id),
            mtime: None,
        };
        let roots = vec![Root {
            id: 1,
            path: "/fixture".into(),
            device: 0,
        }];
        let base = Arc::new(
            Segment::build(
                [record(1, 0, true), record(2, 1, true), record(3, 2, false)],
                roots.clone(),
                0,
            )
            .unwrap(),
        );
        let mut changes = Overlay::default();
        changes.put(4, Some(record(4, 2, false)));
        changes.put(5, Some(record(5, 999, false)));
        let mut view = View {
            roots,
            base,
            layers: vec![Arc::new(changes.clone())],
            epoch: 1,
        };
        assert_eq!(
            view.records().map(|r| r.id).collect::<Vec<_>>(),
            [1, 2, 3, 4]
        );
        assert!(
            view.records_projected(false, false)
                .all(|r| r.id == 4 || r.size.is_none())
        );
        changes.put(2, None);
        view.layers = vec![Arc::new(changes)];
        assert_eq!(view.records().map(|r| r.id).collect::<Vec<_>>(), [1]);
        view.roots.clear();
        assert_eq!(view.records().count(), 0);
    }
}
impl Overlay {
    pub fn put(&mut self, id: u64, record: Option<Record>) {
        if let Some(Some(old)) = self.records.get(&id) {
            self.children.remove(&(old.parent, old.name.clone()));
        }
        if let Some(r) = &record {
            self.children.insert((r.parent, r.name.clone()), id);
        }
        self.records.insert(id, record);
    }
    pub fn bytes(&self) -> usize {
        self.records
            .values()
            .map(|r| r.as_ref().map_or(32, |r| 128 + r.name.len() * 2))
            .sum()
    }
}
#[derive(Clone)]
pub struct View {
    pub base: Arc<Segment>,
    pub layers: Vec<Arc<Overlay>>,
    pub roots: Vec<Root>,
    pub epoch: u64,
}
impl View {
    pub fn record(&self, id: u64) -> Option<Record> {
        self.record_projected(id, true, true)
    }
    pub fn record_projected(&self, id: u64, size: bool, mtime: bool) -> Option<Record> {
        for layer in self.layers.iter().rev() {
            if let Some(r) = layer.records.get(&id) {
                return r.clone();
            }
        }
        self.base
            .slot(id)
            .map(|slot| self.base.record_projected(slot, size, mtime))
    }
    pub fn child(&self, parent: u64, name: &[u8]) -> Option<u64> {
        for layer in self.layers.iter().rev() {
            if let Some(&id) = layer.children.get(&(parent, name.to_vec()))
                && self
                    .record(id)
                    .is_some_and(|r| r.parent == parent && r.name == name)
            {
                return Some(id);
            }
        }
        self.base.child(parent, name).and_then(|slot| {
            let id = self.base.id(slot);
            self.record(id)
                .filter(|r| r.parent == parent && r.name == name)
                .map(|_| id)
        })
    }
    pub fn root_record(&self, root: u32) -> Option<u64> {
        for layer in self.layers.iter().rev() {
            for r in layer.records.values().flatten() {
                if r.parent == 0 && r.root == root {
                    return Some(r.id);
                }
            }
        }
        self.base.root_file(root)
    }
    pub fn resolve(&self, path: &Path) -> Option<u64> {
        let root = self
            .roots
            .iter()
            .filter(|r| path.starts_with(&r.path))
            .max_by_key(|r| r.path.components().count())?;
        let mut id = self.root_record(root.id)?;
        for part in path.strip_prefix(&root.path).ok()?.components() {
            match part {
                std::path::Component::Normal(n) => id = self.child(id, &raw_name(n))?,
                std::path::Component::CurDir => {}
                _ => return None,
            }
        }
        Some(id)
    }
    pub fn path(&self, id: u64) -> Option<PathBuf> {
        let mut id = id;
        let mut parts = Vec::<Vec<u8>>::new();
        for _ in 0..4096 {
            let changed = self.layers.iter().rev().find_map(|l| l.records.get(&id));
            let (parent, root, name) = if let Some(record) = changed {
                let r = record.as_ref()?;
                (
                    r.parent,
                    r.root,
                    std::borrow::Cow::Borrowed(r.name.as_slice()),
                )
            } else {
                self.base.path_part(self.base.slot(id)?)
            };
            if parent == 0 {
                let mut path = self.roots.iter().find(|r| r.id == root)?.path.clone();
                for part in parts.into_iter().rev() {
                    path.push(os_name(&part));
                }
                return Some(path);
            }
            parts.push(name.into_owned());
            id = parent;
        }
        None
    }
    pub fn find_native(&self, root: u32, identity: Identity) -> Option<u64> {
        if identity.key == 0 {
            return None;
        }
        for layer in self.layers.iter().rev() {
            for r in layer.records.values().flatten() {
                if r.root == root && r.identity == identity {
                    return Some(r.id);
                }
            }
        }
        self.base.find_native(root, identity)
    }
    /// Enumerate a small scope by child ranges, bounded before fallback to text.
    pub fn scoped_ids(
        &self,
        path: &Path,
        limit: usize,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Vec<u64> {
        let Some(root) = self.resolve(path) else {
            return Vec::new();
        };
        let mut changes = HashMap::<u64, Option<Record>>::new();
        for layer in &self.layers {
            changes.extend(layer.records.iter().map(|(&id, r)| (id, r.clone())));
        }
        let mut children = HashMap::<u64, Vec<u64>>::new();
        for (&id, r) in &changes {
            if let Some(r) = r {
                children.entry(r.parent).or_default().push(id);
            }
        }
        let mut pending = vec![root];
        let mut result = Vec::new();
        while let Some(parent) = pending.pop() {
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            let ids = self
                .base
                .child_ids(parent)
                .filter(|id| !changes.contains_key(id))
                .chain(children.get(&parent).into_iter().flatten().copied());
            for id in ids {
                result.push(id);
                if result.len() >= limit {
                    return result;
                }
                if self
                    .record_projected(id, false, false)
                    .is_some_and(|r| r.is_dir())
                {
                    pending.push(id);
                }
            }
        }
        result
    }
    pub fn records(&self) -> impl Iterator<Item = Record> + '_ {
        self.records_projected(true, true)
    }
    pub fn records_projected(&self, size: bool, mtime: bool) -> impl Iterator<Item = Record> + '_ {
        let mut changed = std::collections::BTreeSet::new();
        for layer in &self.layers {
            changed.extend(layer.records.keys().copied());
        }
        // Validated base records retain valid paths when overlays only append
        // new IDs and roots are unchanged. Avoid millions of redundant parent
        // walks and ID lookups during compaction and exhaustive streaming.
        let base_unchanged =
            self.roots == self.base.roots && changed.iter().all(|id| self.base.slot(*id).is_none());
        let mut extra = changed.into_iter().peekable();
        let mut slot = 0;
        std::iter::from_fn(move || {
            loop {
                let base_slot = slot;
                let base = (slot < self.base.len()).then(|| self.base.id(slot));
                let id = match (base, extra.peek().copied()) {
                    (Some(a), Some(b)) if a == b => {
                        slot += 1;
                        extra.next();
                        a
                    }
                    (Some(a), Some(b)) if b < a => {
                        extra.next();
                        b
                    }
                    (Some(a), _) => {
                        slot += 1;
                        a
                    }
                    (None, Some(b)) => {
                        extra.next();
                        b
                    }
                    (None, None) => return None,
                };
                if base_unchanged && base == Some(id) {
                    return Some(self.base.record_projected(base_slot, size, mtime));
                }
                if let Some(record) = self.record_projected(id, size, mtime)
                    && self.path(id).is_some()
                {
                    return Some(record);
                }
            }
        })
    }
}

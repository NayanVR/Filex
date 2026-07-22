//! File tags (Phase 2d, block 8 — see `docs/design-tags.md`).
//!
//! A [`Tag`] is a name plus an optional [`TagColor`] drawn from Finder's
//! 0–7 palette (so a color set in filex round-trips into Finder on
//! macOS). [`TagStore`] is the read/write interface; [`SidecarTags`] is
//! the portable backend — a single JSON map from absolute path to tags,
//! kept in the data dir. It is the enumeration index on every platform;
//! a macOS xattr backend (added later) layers Finder interop on top for
//! single-file reads/writes.
//!
//! Pure I/O + map logic, no GPUI, so it is unit-tested in isolation. The
//! app calls it on a background executor (the I/O blocks) and migrates
//! path keys through the same [`filex::ops`](crate::ops) hooks that move
//! files, so tags follow a filex-side rename/move/copy/delete.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use crate::ops::AppliedOp;

/// Default location: `<data_local_dir>/filex/tags.json`.
pub fn default_tags_file() -> Option<PathBuf> {
    Some(dirs::data_local_dir()?.join("filex").join("tags.json"))
}

/// A tag color, mirroring Finder's user-tag palette so filex-created
/// colors survive into Finder (and vice-versa) on macOS. Finder's index
/// 0 means "no color", represented here as `Option<TagColor>::None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TagColor {
    Grey,
    Green,
    Purple,
    Blue,
    Yellow,
    Red,
    Orange,
}

impl TagColor {
    /// Finder's numeric label index (1–7) for this color.
    pub fn finder_index(self) -> u8 {
        match self {
            Self::Grey => 1,
            Self::Green => 2,
            Self::Purple => 3,
            Self::Blue => 4,
            Self::Yellow => 5,
            Self::Red => 6,
            Self::Orange => 7,
        }
    }

    /// The color for a Finder label index; `None` for 0 (no color) or an
    /// out-of-range value.
    pub fn from_finder_index(index: u8) -> Option<Self> {
        match index {
            1 => Some(Self::Grey),
            2 => Some(Self::Green),
            3 => Some(Self::Purple),
            4 => Some(Self::Blue),
            5 => Some(Self::Yellow),
            6 => Some(Self::Red),
            7 => Some(Self::Orange),
            _ => None,
        }
    }

    /// All colors, in palette order — for the picker UI.
    pub fn all() -> [Self; 7] {
        [Self::Grey, Self::Green, Self::Purple, Self::Blue, Self::Yellow, Self::Red, Self::Orange]
    }
}

/// A named, optionally-colored tag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tag {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<TagColor>,
}

impl Tag {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into(), color: None }
    }

    pub fn colored(name: impl Into<String>, color: TagColor) -> Self {
        Self { name: name.into(), color: Some(color) }
    }
}

/// Read/write interface for a file's tags. Backends: [`SidecarTags`]
/// (portable), plus a macOS xattr backend layered on later.
pub trait TagStore: Send + Sync {
    /// Tags on `path`, in stored order; empty when none or unreadable.
    fn tags(&self, path: &Path) -> Vec<Tag>;
    /// Replace the whole tag set on `path` (an empty set clears it).
    fn set_tags(&self, path: &Path, tags: &[Tag]) -> Result<()>;
    /// Every tagged (path, tags) the store knows — powers the sidebar
    /// TAGS section and the `tag:` filter.
    fn all(&self) -> Vec<(PathBuf, Vec<Tag>)>;
}

/// The portable sidecar backend: an in-memory `path → tags` map mirrored
/// to one JSON file. Mutations persist synchronously (tags change
/// rarely; the app calls off-thread).
pub struct SidecarTags {
    file: PathBuf,
    map: RwLock<BTreeMap<PathBuf, Vec<Tag>>>,
}

impl SidecarTags {
    /// Load from `file`; a missing or corrupt file starts empty (tags
    /// are a convenience, never worth failing over).
    pub fn load(file: PathBuf) -> Self {
        let map = std::fs::read_to_string(&file)
            .ok()
            .and_then(|contents| serde_json::from_str(&contents).ok())
            .unwrap_or_default();
        Self { file, map: RwLock::new(map) }
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, BTreeMap<PathBuf, Vec<Tag>>> {
        self.map.read().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, BTreeMap<PathBuf, Vec<Tag>>> {
        self.map.write().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Serialize the current map to `file` via temp-file + rename.
    fn persist(&self) -> Result<()> {
        let parent = self.file.parent().context("tags file has no parent dir")?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
        let json = serde_json::to_string_pretty(&*self.read()).context("serializing tags")?;
        let tmp = self.file.with_extension("json.tmp");
        std::fs::write(&tmp, json).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.file)
            .with_context(|| format!("replacing {}", self.file.display()))
    }

    /// Move a file's tags `from → to` (a filex-side rename/move). No-op
    /// when the source is untagged.
    pub fn rename_key(&self, from: &Path, to: &Path) -> Result<()> {
        let moved = {
            let mut map = self.write();
            match map.remove(from) {
                Some(tags) => {
                    map.insert(to.to_path_buf(), tags);
                    true
                }
                None => false,
            }
        };
        if moved { self.persist() } else { Ok(()) }
    }

    /// Copy a file's tags `from → to` (a filex-side copy mirrors Finder).
    pub fn copy_key(&self, from: &Path, to: &Path) -> Result<()> {
        let copied = {
            let mut map = self.write();
            match map.get(from).cloned() {
                Some(tags) => {
                    map.insert(to.to_path_buf(), tags);
                    true
                }
                None => false,
            }
        };
        if copied { self.persist() } else { Ok(()) }
    }

    /// Remove and return a file's tags (a filex-side delete). The caller
    /// keeps the returned tags on the undo journal to [`restore_key`]
    /// them if the delete is undone.
    ///
    /// [`restore_key`]: Self::restore_key
    pub fn remove_key(&self, path: &Path) -> Result<Vec<Tag>> {
        let removed = self.write().remove(path);
        match removed {
            Some(tags) => {
                self.persist()?;
                Ok(tags)
            }
            None => Ok(Vec::new()),
        }
    }

    /// Reinstate tags at `path` (undo of a delete). No-op for an empty
    /// set.
    pub fn restore_key(&self, path: &Path, tags: Vec<Tag>) -> Result<()> {
        if tags.is_empty() {
            return Ok(());
        }
        self.write().insert(path.to_path_buf(), tags);
        self.persist()
    }

    /// Drop keys whose path no longer exists (lazy cleanup for files
    /// moved/deleted outside filex). Returns how many were pruned.
    pub fn prune(&self, exists: impl Fn(&Path) -> bool) -> Result<usize> {
        let removed: Vec<PathBuf> =
            self.read().keys().filter(|p| !exists(p)).cloned().collect();
        if removed.is_empty() {
            return Ok(0);
        }
        {
            let mut map = self.write();
            for path in &removed {
                map.remove(path);
            }
        }
        self.persist()?;
        Ok(removed.len())
    }

    /// Migrate the sidecar index to reflect a just-completed file op,
    /// keeping tags attached to their file across a filex-side
    /// move/rename/copy/delete (see `docs/design-tags.md`). For a delete
    /// the removed tags are written back into `op` ([`AppliedOp::Deleted`]'s
    /// `removed_tags`) so [`undo_applied`] can reinstate them. Blocking I/O
    /// (persists) — call on a background executor, after the op succeeds.
    ///
    /// [`undo_applied`]: Self::undo_applied
    pub fn apply_applied(&self, op: &mut AppliedOp) -> Result<()> {
        match op {
            AppliedOp::Moved { from, to } | AppliedOp::Renamed { from, to } => {
                self.rename_key(from, to)
            }
            AppliedOp::Copied { from, to } => self.copy_key(from, to),
            AppliedOp::Deleted { original, removed_tags, .. } => {
                *removed_tags = self.remove_key(original)?;
                Ok(())
            }
        }
    }

    /// Reverse [`apply_applied`] for an op being undone: move/rename put
    /// the key back `to → from`, an undone copy drops the copy's key, and
    /// an undone delete restores the tags carried on the op. Blocking I/O
    /// — call on a background executor.
    ///
    /// [`apply_applied`]: Self::apply_applied
    pub fn undo_applied(&self, op: &AppliedOp) -> Result<()> {
        match op {
            AppliedOp::Moved { from, to } | AppliedOp::Renamed { from, to } => {
                self.rename_key(to, from)
            }
            AppliedOp::Copied { to, .. } => {
                self.remove_key(to)?;
                Ok(())
            }
            AppliedOp::Deleted { original, removed_tags, .. } => {
                self.restore_key(original, removed_tags.clone())
            }
        }
    }
}

impl TagStore for SidecarTags {
    fn tags(&self, path: &Path) -> Vec<Tag> {
        self.read().get(path).cloned().unwrap_or_default()
    }

    fn set_tags(&self, path: &Path, tags: &[Tag]) -> Result<()> {
        {
            let mut map = self.write();
            if tags.is_empty() {
                map.remove(path);
            } else {
                map.insert(path.to_path_buf(), tags.to_vec());
            }
        }
        self.persist()
    }

    fn all(&self) -> Vec<(PathBuf, Vec<Tag>)> {
        self.read().iter().map(|(path, tags)| (path.clone(), tags.clone())).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, SidecarTags) {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("nested").join("tags.json");
        let store = SidecarTags::load(file);
        (dir, store)
    }

    #[test]
    fn finder_color_index_round_trips() {
        for color in TagColor::all() {
            assert_eq!(TagColor::from_finder_index(color.finder_index()), Some(color));
        }
        assert_eq!(TagColor::from_finder_index(0), None); // "no color"
        assert_eq!(TagColor::from_finder_index(9), None);
    }

    #[test]
    fn set_get_and_clear() {
        let (_dir, store) = store();
        assert!(store.tags(Path::new("/a")).is_empty());
        store.set_tags(Path::new("/a"), &[Tag::new("Work"), Tag::colored("Hot", TagColor::Red)])
            .unwrap();
        assert_eq!(
            store.tags(Path::new("/a")),
            vec![Tag::new("Work"), Tag::colored("Hot", TagColor::Red)]
        );
        // Empty set clears the key.
        store.set_tags(Path::new("/a"), &[]).unwrap();
        assert!(store.tags(Path::new("/a")).is_empty());
        assert!(store.all().is_empty());
    }

    #[test]
    fn persists_across_reload() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("tags.json");
        let store = SidecarTags::load(file.clone());
        store.set_tags(Path::new("/a"), &[Tag::colored("Blue", TagColor::Blue)]).unwrap();
        // A fresh load sees the persisted tags.
        let reloaded = SidecarTags::load(file);
        assert_eq!(reloaded.tags(Path::new("/a")), vec![Tag::colored("Blue", TagColor::Blue)]);
    }

    #[test]
    fn rename_and_copy_and_remove_migrate_keys() {
        let (_dir, store) = store();
        store.set_tags(Path::new("/a"), &[Tag::new("X")]).unwrap();

        store.copy_key(Path::new("/a"), Path::new("/b")).unwrap();
        assert_eq!(store.tags(Path::new("/b")), vec![Tag::new("X")]);
        assert_eq!(store.tags(Path::new("/a")), vec![Tag::new("X")]); // source kept

        store.rename_key(Path::new("/a"), Path::new("/c")).unwrap();
        assert!(store.tags(Path::new("/a")).is_empty()); // source moved away
        assert_eq!(store.tags(Path::new("/c")), vec![Tag::new("X")]);

        let removed = store.remove_key(Path::new("/c")).unwrap();
        assert_eq!(removed, vec![Tag::new("X")]);
        assert!(store.tags(Path::new("/c")).is_empty());
        // Undo reinstates them.
        store.restore_key(Path::new("/c"), removed).unwrap();
        assert_eq!(store.tags(Path::new("/c")), vec![Tag::new("X")]);
    }

    #[test]
    fn prune_drops_vanished_paths_only() {
        let (_dir, store) = store();
        store.set_tags(Path::new("/keep"), &[Tag::new("K")]).unwrap();
        store.set_tags(Path::new("/gone"), &[Tag::new("G")]).unwrap();
        let pruned = store.prune(|p| p == Path::new("/keep")).unwrap();
        assert_eq!(pruned, 1);
        assert_eq!(store.tags(Path::new("/keep")), vec![Tag::new("K")]);
        assert!(store.tags(Path::new("/gone")).is_empty());
    }

    use crate::ops::{AppliedOp, TrashRestore};

    #[test]
    fn apply_applied_migrates_each_op_kind() {
        let (_dir, store) = store();
        store.set_tags(Path::new("/a"), &[Tag::new("T")]).unwrap();

        // Copy duplicates the source's tags onto the copy.
        store
            .apply_applied(&mut AppliedOp::Copied { from: "/a".into(), to: "/b".into() })
            .unwrap();
        assert_eq!(store.tags(Path::new("/a")), vec![Tag::new("T")]);
        assert_eq!(store.tags(Path::new("/b")), vec![Tag::new("T")]);

        // Move/rename migrates the key.
        store
            .apply_applied(&mut AppliedOp::Moved { from: "/a".into(), to: "/c".into() })
            .unwrap();
        assert!(store.tags(Path::new("/a")).is_empty());
        assert_eq!(store.tags(Path::new("/c")), vec![Tag::new("T")]);

        // Delete drops the key and records the removed tags on the op.
        let mut del = AppliedOp::Deleted {
            original: "/c".into(),
            restore: TrashRestore::Unknown,
            removed_tags: Vec::new(),
        };
        store.apply_applied(&mut del).unwrap();
        assert!(store.tags(Path::new("/c")).is_empty());
        let AppliedOp::Deleted { removed_tags, .. } = &del else { panic!() };
        assert_eq!(removed_tags, &vec![Tag::new("T")]);
    }

    #[test]
    fn undo_applied_reverses_migration() {
        let (_dir, store) = store();
        store.set_tags(Path::new("/a"), &[Tag::new("T")]).unwrap();

        // Undo of a rename puts the key back.
        let renamed = AppliedOp::Renamed { from: "/a".into(), to: "/z".into() };
        store.apply_applied(&mut renamed.clone()).unwrap();
        store.undo_applied(&renamed).unwrap();
        assert_eq!(store.tags(Path::new("/a")), vec![Tag::new("T")]);
        assert!(store.tags(Path::new("/z")).is_empty());

        // Undo of a copy removes the copy's key, leaving the source.
        let copied = AppliedOp::Copied { from: "/a".into(), to: "/b".into() };
        store.apply_applied(&mut copied.clone()).unwrap();
        store.undo_applied(&copied).unwrap();
        assert_eq!(store.tags(Path::new("/a")), vec![Tag::new("T")]);
        assert!(store.tags(Path::new("/b")).is_empty());

        // Undo of a delete reinstates the tags carried on the op.
        let mut deleted = AppliedOp::Deleted {
            original: "/a".into(),
            restore: TrashRestore::Unknown,
            removed_tags: Vec::new(),
        };
        store.apply_applied(&mut deleted).unwrap();
        assert!(store.tags(Path::new("/a")).is_empty());
        store.undo_applied(&deleted).unwrap();
        assert_eq!(store.tags(Path::new("/a")), vec![Tag::new("T")]);
    }

    #[test]
    fn corrupt_file_loads_empty() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("tags.json");
        std::fs::write(&file, "{ not json").unwrap();
        assert!(SidecarTags::load(file).all().is_empty());
    }
}

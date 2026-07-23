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

/// Encode tags into the payload of Finder's `_kMDItemUserTags` extended
/// attribute: a binary property list holding an array of `"Name\n<idx>"`
/// strings, where `<idx>` is the Finder color index (0 = none). This is
/// Finder's exact on-disk format, so a value written here appears in
/// Finder with its color, and vice-versa. Portable (no macOS APIs) so the
/// format is testable on CI; the actual xattr I/O is macOS-only.
pub fn encode_finder_tags(tags: &[Tag]) -> Result<Vec<u8>> {
    let array = tags
        .iter()
        .map(|tag| {
            let idx = tag.color.map(TagColor::finder_index).unwrap_or(0);
            plist::Value::String(format!("{}\n{idx}", tag.name))
        })
        .collect::<Vec<_>>();
    let mut buf = Vec::new();
    plist::Value::Array(array)
        .to_writer_binary(&mut buf)
        .context("serializing Finder tags plist")?;
    Ok(buf)
}

/// Decode the payload of Finder's `_kMDItemUserTags` xattr (see
/// [`encode_finder_tags`]). A malformed plist or unexpected shape yields
/// no tags rather than an error — a file's tags are never worth failing
/// over. Each element is `"Name"` or `"Name\n<idx>"`; a trailing
/// `\n<0-7>` is read as the color, anything else is part of the name.
pub fn decode_finder_tags(bytes: &[u8]) -> Vec<Tag> {
    let Ok(value) = plist::Value::from_reader(std::io::Cursor::new(bytes)) else {
        return Vec::new();
    };
    let Some(array) = value.as_array() else {
        return Vec::new();
    };
    array.iter().filter_map(|v| v.as_string()).map(decode_finder_tag).collect()
}

/// Fold `new` into a tag set for the details-panel editor. When
/// `replacing` names an existing tag it is swapped out **in place**
/// (preserving order); otherwise `new` is appended. Either way the result
/// holds at most one tag per name — a collision with `new.name` is
/// dropped so re-adding a name just updates its color. Pure so the
/// editor's add/rename/recolor rule is unit-tested off the GPUI layer.
pub fn upsert_tag(tags: &[Tag], replacing: Option<&str>, new: Tag) -> Vec<Tag> {
    let mut out = Vec::with_capacity(tags.len() + 1);
    let mut inserted = false;
    for tag in tags {
        if replacing == Some(tag.name.as_str()) {
            if !inserted {
                out.push(new.clone());
                inserted = true;
            }
        } else if tag.name == new.name {
            continue; // de-dup: the incoming tag's color wins
        } else {
            out.push(tag.clone());
        }
    }
    if !inserted {
        out.push(new);
    }
    out
}

/// A search query split into its filename text and its `tag:` filters.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct TagQuery {
    /// The filename text, with the `tag:` tokens removed (may be empty).
    pub text: String,
    /// Required tag names — lowercased, de-duplicated, first-seen order.
    /// A path must carry *all* of them (AND).
    pub tags: Vec<String>,
}

impl TagQuery {
    /// Nothing to search on — no filename text and no tag filters.
    pub fn is_empty(&self) -> bool {
        self.text.is_empty() && self.tags.is_empty()
    }
}

/// Split `tag:NAME` tokens out of a raw query. The remaining whitespace-
/// separated words form the filename [`text`](TagQuery::text); each
/// `tag:NAME` adds a required tag ([`tags`](TagQuery::tags)). The `tag:`
/// prefix and the names match case-insensitively, so names are lowercased
/// here; a bare `tag:` (no name) is ignored. This is the first
/// `key:value` filter — the search-chips block reuses the same splitter.
pub fn parse_tag_query(raw: &str) -> TagQuery {
    let mut text_words = Vec::new();
    let mut tags = Vec::new();
    for word in raw.split_whitespace() {
        // Lowercased copy just for the prefix test / tag name; the
        // filename text keeps the word's original case.
        let lowered = word.to_ascii_lowercase();
        if let Some(name) = lowered.strip_prefix("tag:") {
            if !name.is_empty() && !tags.iter().any(|t| t == name) {
                tags.push(name.to_string());
            }
        } else {
            text_words.push(word);
        }
    }
    TagQuery { text: text_words.join(" "), tags }
}

/// Does `tags` carry every name in `required`? `required` is assumed
/// lowercased (as [`parse_tag_query`] produces); comparison is
/// case-insensitive. Empty `required` trivially matches.
pub fn tags_match(tags: &[Tag], required: &[String]) -> bool {
    required
        .iter()
        .all(|req| tags.iter().any(|tag| tag.name.eq_ignore_ascii_case(req)))
}

/// One `"Name\n<idx>"` (or bare `"Name"`) entry → a [`Tag`].
fn decode_finder_tag(entry: &str) -> Tag {
    if let Some((name, idx)) = entry.rsplit_once('\n')
        && let Ok(index) = idx.parse::<u8>()
        && index <= 7
    {
        return Tag { name: name.to_string(), color: TagColor::from_finder_index(index) };
    }
    Tag::new(entry)
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

    /// Absolute paths carrying every tag in `required` (lowercased,
    /// matched case-insensitively) — the data source for the `tag:`
    /// filter. Scans the in-memory index under the read lock, cloning only
    /// the matching paths, so it stays cheap even at 100k tagged entries
    /// (see `benches/tag_bench.rs`). Empty `required` returns nothing.
    pub fn paths_with_all_tags(&self, required: &[String]) -> Vec<PathBuf> {
        if required.is_empty() {
            return Vec::new();
        }
        self.read()
            .iter()
            .filter(|(_, tags)| tags_match(tags, required))
            .map(|(path, _)| path.clone())
            .collect()
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

/// The tag store the app runs against: [`SidecarTags`] everywhere, with
/// macOS swapping in [`MacosTags`] to add Finder interop on top of the
/// same sidecar index. The app is written against this alias so it never
/// branches on platform.
#[cfg(target_os = "macos")]
pub type PlatformTags = macos::MacosTags;
#[cfg(not(target_os = "macos"))]
pub type PlatformTags = SidecarTags;

/// macOS backend: Finder-interop tag xattr on the file itself, layered
/// over a [`SidecarTags`] that stays the enumeration index. `set_tags`
/// writes both; `tags` prefers the xattr so Finder-side edits win;
/// `all`/`prune`/most migration delegate to the sidecar. Only a copy
/// needs extra work — the byte-level copy doesn't carry the xattr, so we
/// write it onto the new file ("Option B", `docs/design-tags.md`).
#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;
    use std::path::{Path, PathBuf};

    use anyhow::{Context as _, Result};

    use super::{SidecarTags, Tag, TagStore, decode_finder_tags, encode_finder_tags};
    use crate::ops::AppliedOp;

    /// Finder's user-tags xattr name (NUL-terminated for the libc calls).
    const TAGS_XATTR: &[u8] = b"com.apple.metadata:_kMDItemUserTags\0";

    pub struct MacosTags {
        inner: SidecarTags,
    }

    impl MacosTags {
        pub fn load(file: PathBuf) -> Self {
            Self { inner: SidecarTags::load(file) }
        }

        /// Lazy prune of the enumeration index — delegates to the sidecar
        /// (the xattr can't be enumerated without a disk walk).
        pub fn prune(&self, exists: impl Fn(&Path) -> bool) -> Result<usize> {
            self.inner.prune(exists)
        }

        /// Paths carrying all `required` tags — delegates to the sidecar
        /// (the enumeration index), same as [`SidecarTags`].
        pub fn paths_with_all_tags(&self, required: &[String]) -> Vec<PathBuf> {
            self.inner.paths_with_all_tags(required)
        }

        /// Migrate stores for a completed file op. Move/rename/delete only
        /// touch the sidecar key — the xattr rides along with the file (or
        /// is gone with a trashed one). A copy additionally gets the
        /// source's tags written onto the new file's xattr so Finder shows
        /// the copy tagged too.
        pub fn apply_applied(&self, op: &mut AppliedOp) -> Result<()> {
            if let AppliedOp::Copied { from, to } = op {
                let tags = self.tags(from);
                return if tags.is_empty() { Ok(()) } else { self.set_tags(to, &tags) };
            }
            self.inner.apply_applied(op)
        }

        /// Reverse [`apply_applied`] for an undone op. `ops::undo` moves
        /// files (and their xattrs) back or deletes the copy, so only the
        /// sidecar index needs reversing here.
        pub fn undo_applied(&self, op: &AppliedOp) -> Result<()> {
            self.inner.undo_applied(op)
        }
    }

    impl TagStore for MacosTags {
        fn tags(&self, path: &Path) -> Vec<Tag> {
            // The xattr is authoritative for a single file (Finder edits
            // win); the sidecar is the fallback for files tagged only via
            // filex on a filesystem that later lost the xattr.
            match read_tags_xattr(path) {
                Some(tags) => tags,
                None => self.inner.tags(path),
            }
        }

        fn set_tags(&self, path: &Path, tags: &[Tag]) -> Result<()> {
            write_tags_xattr(path, tags)?; // interop channel
            self.inner.set_tags(path, tags) // enumeration index
        }

        fn all(&self) -> Vec<(PathBuf, Vec<Tag>)> {
            self.inner.all()
        }
    }

    /// Read and decode the Finder tags xattr; `None` when the file has no
    /// such xattr or it can't be read (logged, never fatal).
    fn read_tags_xattr(path: &Path) -> Option<Vec<Tag>> {
        match get_xattr(path, TAGS_XATTR) {
            Ok(Some(bytes)) => Some(decode_finder_tags(&bytes)),
            Ok(None) => None,
            Err(err) => {
                tracing::warn!("reading tags xattr for {}: {err:#}", path.display());
                None
            }
        }
    }

    /// Write the Finder tags xattr, or remove it when clearing all tags.
    fn write_tags_xattr(path: &Path, tags: &[Tag]) -> Result<()> {
        if tags.is_empty() {
            remove_xattr(path, TAGS_XATTR)
        } else {
            set_xattr(path, TAGS_XATTR, &encode_finder_tags(tags)?)
        }
    }

    fn cstring_path(path: &Path) -> Result<CString> {
        CString::new(path.as_os_str().as_bytes()).context("path contains an interior NUL byte")
    }

    /// `getxattr(2)`: two calls (size probe, then read). `Ok(None)` when
    /// the attribute is absent (`ENOATTR`). Follows symlinks and reads
    /// from offset 0 — correct for a plain (non-resource-fork) xattr.
    fn get_xattr(path: &Path, name: &[u8]) -> Result<Option<Vec<u8>>> {
        let cpath = cstring_path(path)?;
        let name = name.as_ptr() as *const libc::c_char;
        // SAFETY: `cpath`/`name` are valid NUL-terminated C strings that
        // outlive the call; a null value pointer with size 0 is the
        // documented way to query the attribute length.
        let size =
            unsafe { libc::getxattr(cpath.as_ptr(), name, std::ptr::null_mut(), 0, 0, 0) };
        if size < 0 {
            return match std::io::Error::last_os_error() {
                err if err.raw_os_error() == Some(libc::ENOATTR) => Ok(None),
                err => Err(err).context("querying xattr size"),
            };
        }
        let mut buf = vec![0u8; size as usize];
        // SAFETY: `buf` has `size` bytes; the kernel writes at most that.
        let read = unsafe {
            libc::getxattr(cpath.as_ptr(), name, buf.as_mut_ptr().cast(), buf.len(), 0, 0)
        };
        if read < 0 {
            return match std::io::Error::last_os_error() {
                err if err.raw_os_error() == Some(libc::ENOATTR) => Ok(None),
                err => Err(err).context("reading xattr"),
            };
        }
        buf.truncate(read as usize);
        Ok(Some(buf))
    }

    /// `setxattr(2)` — writes `data` as the attribute value.
    fn set_xattr(path: &Path, name: &[u8], data: &[u8]) -> Result<()> {
        let cpath = cstring_path(path)?;
        // SAFETY: all pointers are valid for the call; `data.len()` bounds
        // the read from `data`.
        let rc = unsafe {
            libc::setxattr(
                cpath.as_ptr(),
                name.as_ptr() as *const libc::c_char,
                data.as_ptr().cast(),
                data.len(),
                0,
                0,
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error()).context("writing xattr");
        }
        Ok(())
    }

    /// `removexattr(2)` — a no-op when the attribute is already absent.
    fn remove_xattr(path: &Path, name: &[u8]) -> Result<()> {
        let cpath = cstring_path(path)?;
        // SAFETY: valid NUL-terminated C strings outliving the call.
        let rc = unsafe {
            libc::removexattr(cpath.as_ptr(), name.as_ptr() as *const libc::c_char, 0)
        };
        if rc != 0 {
            return match std::io::Error::last_os_error() {
                err if err.raw_os_error() == Some(libc::ENOATTR) => Ok(()),
                err => Err(err).context("removing xattr"),
            };
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn store() -> (tempfile::TempDir, MacosTags) {
            let dir = tempfile::tempdir().unwrap();
            let store = MacosTags::load(dir.path().join("tags.json"));
            (dir, store)
        }

        #[test]
        fn set_tags_writes_finder_xattr_and_reads_it_back() {
            use super::super::TagColor;
            let (dir, store) = store();
            let file = dir.path().join("doc.txt");
            std::fs::write(&file, "x").unwrap();

            let tags = vec![Tag::new("Work"), Tag::colored("Hot", TagColor::Red)];
            store.set_tags(&file, &tags).unwrap();

            // tags() reads through the xattr (Finder-authoritative).
            assert_eq!(store.tags(&file), tags);
            // The raw xattr is exactly Finder's format.
            let raw = get_xattr(&file, TAGS_XATTR).unwrap().unwrap();
            assert_eq!(decode_finder_tags(&raw), tags);
            // And the sidecar mirrored it for enumeration.
            assert_eq!(store.all().len(), 1);

            // Clearing removes the xattr entirely.
            store.set_tags(&file, &[]).unwrap();
            assert!(get_xattr(&file, TAGS_XATTR).unwrap().is_none());
            assert!(store.tags(&file).is_empty());
        }

        #[test]
        fn copy_carries_the_xattr_onto_the_new_file() {
            let (dir, store) = store();
            let from = dir.path().join("a.txt");
            let to = dir.path().join("b.txt");
            std::fs::write(&from, "x").unwrap();
            std::fs::write(&to, "x").unwrap(); // the byte-copy already ran
            store.set_tags(&from, &[Tag::new("Keep")]).unwrap();

            // Option B: migrating a Copied op writes the tags onto `to`'s
            // xattr, not just the sidecar.
            store
                .apply_applied(&mut AppliedOp::Copied { from: from.clone(), to: to.clone() })
                .unwrap();
            let raw = get_xattr(&to, TAGS_XATTR).unwrap().unwrap();
            assert_eq!(decode_finder_tags(&raw), vec![Tag::new("Keep")]);
            assert_eq!(store.tags(&to), vec![Tag::new("Keep")]);
        }
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
    fn upsert_adds_replaces_and_dedups() {
        let base = vec![Tag::new("A"), Tag::colored("B", TagColor::Blue), Tag::new("C")];

        // Add a fresh tag → appended.
        assert_eq!(
            upsert_tag(&base, None, Tag::colored("D", TagColor::Red)),
            vec![
                Tag::new("A"),
                Tag::colored("B", TagColor::Blue),
                Tag::new("C"),
                Tag::colored("D", TagColor::Red)
            ]
        );

        // Re-add an existing name (no `replacing`) → color updated, moved
        // to the end (its old occurrence dropped).
        assert_eq!(
            upsert_tag(&base, None, Tag::colored("B", TagColor::Green)),
            vec![Tag::new("A"), Tag::new("C"), Tag::colored("B", TagColor::Green)]
        );

        // Recolor in place (replacing == new.name) → order preserved.
        assert_eq!(
            upsert_tag(&base, Some("B"), Tag::colored("B", TagColor::Yellow)),
            vec![Tag::new("A"), Tag::colored("B", TagColor::Yellow), Tag::new("C")]
        );

        // Rename in place, colliding with another existing tag → the
        // renamed tag takes B's slot and the collided "C" is removed.
        assert_eq!(
            upsert_tag(&base, Some("B"), Tag::new("C")),
            vec![Tag::new("A"), Tag::new("C")]
        );

        // `replacing` a name that isn't present → appended.
        assert_eq!(
            upsert_tag(&base, Some("Z"), Tag::new("D")),
            vec![Tag::new("A"), Tag::colored("B", TagColor::Blue), Tag::new("C"), Tag::new("D")]
        );
    }

    #[test]
    fn parse_tag_query_splits_text_and_tags() {
        // Plain text, no tags.
        assert_eq!(
            parse_tag_query("annual report"),
            TagQuery { text: "annual report".into(), tags: vec![] }
        );
        // Embedded tag token, case-insensitive prefix + name, deduped.
        assert_eq!(
            parse_tag_query("Report TAG:Work tag:work tag:Urgent"),
            TagQuery { text: "Report".into(), tags: vec!["work".into(), "urgent".into()] }
        );
        // Tag-only query: empty text.
        assert_eq!(
            parse_tag_query("tag:blue"),
            TagQuery { text: String::new(), tags: vec!["blue".into()] }
        );
        // A bare `tag:` contributes nothing; text case is preserved.
        assert_eq!(
            parse_tag_query("  Foo   tag:   Bar "),
            TagQuery { text: "Foo Bar".into(), tags: vec![] }
        );
        assert!(parse_tag_query("   ").is_empty());
    }

    #[test]
    fn tags_match_requires_all_case_insensitively() {
        let tags = vec![Tag::new("Work"), Tag::colored("Urgent", TagColor::Red)];
        assert!(tags_match(&tags, &["work".into()]));
        assert!(tags_match(&tags, &["work".into(), "urgent".into()]));
        assert!(tags_match(&tags, &[])); // vacuous
        assert!(!tags_match(&tags, &["work".into(), "home".into()]));
    }

    #[test]
    fn paths_with_all_tags_intersects() {
        let (_dir, store) = store();
        store.set_tags(Path::new("/a"), &[Tag::new("Work"), Tag::new("Urgent")]).unwrap();
        store.set_tags(Path::new("/b"), &[Tag::new("Work")]).unwrap();
        store.set_tags(Path::new("/c"), &[Tag::new("Home")]).unwrap();

        let mut work = store.paths_with_all_tags(&["work".into()]);
        work.sort();
        assert_eq!(work, vec![PathBuf::from("/a"), PathBuf::from("/b")]);
        // AND across tags.
        assert_eq!(
            store.paths_with_all_tags(&["work".into(), "urgent".into()]),
            vec![PathBuf::from("/a")]
        );
        // No filter → nothing (callers treat "no tags" separately).
        assert!(store.paths_with_all_tags(&[]).is_empty());
    }

    #[test]
    fn finder_tags_encode_decode_round_trip() {
        let tags = vec![
            Tag::new("Work"),
            Tag::colored("Hot", TagColor::Red),
            Tag::colored("Blue", TagColor::Blue),
        ];
        let bytes = encode_finder_tags(&tags).unwrap();
        assert_eq!(decode_finder_tags(&bytes), tags);
        // Empty set is a valid, empty array.
        assert!(decode_finder_tags(&encode_finder_tags(&[]).unwrap()).is_empty());
    }

    #[test]
    fn decodes_a_real_finder_plist_fixture() {
        // A binary plist produced independently (Python `plistlib`) for
        // `["Work\n0", "Hot\n6"]` — Finder's exact on-disk shape, so this
        // proves the decoder reads the genuine format (CI has no macOS).
        // Work has no color (index 0); Hot is red (index 6).
        const FIXTURE: &[u8] = &[
            0x62, 0x70, 0x6c, 0x69, 0x73, 0x74, 0x30, 0x30, 0xa2, 0x01, 0x02, 0x56, 0x57, 0x6f,
            0x72, 0x6b, 0x0a, 0x30, 0x55, 0x48, 0x6f, 0x74, 0x0a, 0x36, 0x08, 0x0b, 0x12, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x18,
        ];
        assert_eq!(
            decode_finder_tags(FIXTURE),
            vec![Tag::new("Work"), Tag::colored("Hot", TagColor::Red)]
        );
    }

    #[test]
    fn decode_tolerates_garbage_and_bare_names() {
        assert!(decode_finder_tags(b"not a plist").is_empty());
        // A bare "Name" (no color suffix) decodes as an uncolored tag.
        let bytes = {
            let mut buf = Vec::new();
            plist::Value::Array(vec![plist::Value::String("Plain".into())])
                .to_writer_binary(&mut buf)
                .unwrap();
            buf
        };
        assert_eq!(decode_finder_tags(&bytes), vec![Tag::new("Plain")]);
    }

    #[test]
    fn corrupt_file_loads_empty() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("tags.json");
        std::fs::write(&file, "{ not json").unwrap();
        assert!(SidecarTags::load(file).all().is_empty());
    }
}

//! OS-agnostic volume index: the in-memory structure that answers
//! filename searches instantly (see docs/indexing-architecture.md §3).
//!
//! Layout follows the "Everything" model: entries hold a parent link and a
//! reference into one contiguous name pool; full paths are never stored and
//! are materialized on demand by chasing parent links. A second, case-folded
//! copy of every name backs case-insensitive substring search.

// Platform watcher modules are compiled on every OS: their event parsers
// and delta mappers are pure and fixture-tested everywhere; only the
// OS-call sections inside are target-gated.
pub mod ipc;
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;
pub mod manager;
pub mod persist;
pub mod usn;
pub mod walker;
pub mod watcher;
pub mod windows;

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

use anyhow::{Result, bail};
use memchr::memmem;
use rayon::prelude::*;

/// Stable handle to an entry; index into `VolumeIndex::entries`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EntryId(u32);

pub const ROOT: EntryId = EntryId(0);

/// Maximum parent-chain length tolerated when materializing a path,
/// guarding against cycles introduced by a buggy delta stream.
const MAX_PATH_DEPTH: usize = 4096;

const FLAG_DIR: u8 = 1 << 0;
const FLAG_TOMBSTONE: u8 = 1 << 1;
/// Set once `size`/`mtime` have been populated for this entry. Bootstrap
/// inserts names only (this bit clear); a background backfill and the live
/// watchers set it (search-chips phase 2, `docs/design-search-chips.md`).
/// Distinguishes a real 0-byte file from "size unknown".
const FLAG_HAS_META: u8 = 1 << 2;

/// (offset, len) into a name pool. Filenames are ≤255 bytes on every
/// filesystem we target, so u16 lengths are safe (enforced on insert).
#[derive(Debug, Clone, Copy)]
struct NameRef {
    offset: u32,
    len: u16,
}

#[derive(Debug, Clone, Copy)]
struct FileEntry {
    name: NameRef,
    name_lower: NameRef,
    parent: EntryId,
    flags: u8,
    /// Platform-native identity: NTFS file reference number (FRN) on
    /// Windows, 0 elsewhere / when unknown. Lets journal-based delta
    /// sources (USN) address entries without any path resolution.
    native_key: u64,
    /// File size in bytes; meaningful only when [`FLAG_HAS_META`] is set
    /// (0 otherwise, and always 0 for directories).
    size: u64,
    /// Last-modified time, unix seconds; meaningful only when
    /// [`FLAG_HAS_META`] is set.
    mtime: i64,
}

impl FileEntry {
    fn is_dir(&self) -> bool {
        self.flags & FLAG_DIR != 0
    }

    fn is_tombstone(&self) -> bool {
        self.flags & FLAG_TOMBSTONE != 0
    }

    /// Whether `size`/`mtime` have been populated (else they're unknown).
    fn has_meta(&self) -> bool {
        self.flags & FLAG_HAS_META != 0
    }
}

/// How a hit matched the query, in ranking order (lower is better).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MatchKind {
    Exact,
    Prefix,
    WordBoundary,
    Substring,
}

/// Compaction trigger: at least this many dead units *and* at least a
/// quarter of the arena dead. The floor keeps small indexes from
/// compacting constantly; the ratio keeps big ones from carrying
/// gigabytes of tombstones.
const COMPACTION_MIN_DEAD: usize = 4096;

fn compaction_due(dead: usize, total_entries: usize) -> bool {
    dead >= COMPACTION_MIN_DEAD && dead * 4 >= total_entries
}

/// Keeps the best `limit` ranked items seen so far: a max-heap where the
/// root is the *worst* kept item, evicted when something better arrives.
/// Rayon folds one per chunk, then merges — memory is O(limit) per task
/// instead of O(matches) total.
struct TopK {
    limit: usize,
    heap: std::collections::BinaryHeap<(MatchKind, u16, u32)>,
}

impl TopK {
    fn new(limit: usize) -> Self {
        Self { limit, heap: std::collections::BinaryHeap::with_capacity(limit + 1) }
    }

    fn push(mut self, item: (MatchKind, u16, u32)) -> Self {
        if self.heap.len() < self.limit {
            self.heap.push(item);
        } else if let Some(mut worst) = self.heap.peek_mut()
            && item < *worst
        {
            *worst = item;
        }
        self
    }

    fn merge(self, other: Self) -> Self {
        let (mut acc, source) = if self.heap.len() >= other.heap.len() {
            (self, other)
        } else {
            (other, self)
        };
        for item in source.heap {
            acc = acc.push(item);
        }
        acc
    }

    fn into_sorted(self) -> Vec<(MatchKind, u16, u32)> {
        self.heap.into_sorted_vec()
    }
}

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub id: EntryId,
    pub kind: MatchKind,
    /// Folded-name length — the ranking tiebreak, exposed so hits from
    /// multiple indexes can be merged without re-deriving it.
    pub name_len: u16,
}

#[derive(Debug)]
pub struct VolumeIndex {
    root_path: PathBuf,
    entries: Vec<FileEntry>,
    name_pool: Vec<u8>,
    name_pool_lower: Vec<u8>,
    /// Child lists, used for browse-style listing and path resolution.
    children: HashMap<EntryId, Vec<EntryId>>,
    /// Native key (FRN) → entry, for journal-based delta sources.
    by_native_key: HashMap<u64, EntryId>,
    /// Bumped on every mutation; lets async consumers detect staleness.
    generation: u64,
    /// Approximate count of dead arena units: tombstoned entries plus
    /// names leaked by renames. Drives compaction (see
    /// [`VolumeIndex::needs_compaction`]); recomputed as the tombstone
    /// count when loading a snapshot.
    dead_debt: usize,
}

impl VolumeIndex {
    /// Create an index whose root entry represents `root_path`.
    pub fn new(root_path: impl Into<PathBuf>) -> Self {
        Self::new_with_root_key(root_path, 0)
    }

    /// Like [`VolumeIndex::new`], additionally registering the root's
    /// native key (e.g. the FRN of the root directory on NTFS).
    pub fn new_with_root_key(root_path: impl Into<PathBuf>, root_key: u64) -> Self {
        let mut index = Self {
            root_path: root_path.into(),
            entries: Vec::new(),
            name_pool: Vec::new(),
            name_pool_lower: Vec::new(),
            children: HashMap::new(),
            by_native_key: HashMap::new(),
            generation: 0,
            dead_debt: 0,
        };
        // Root points at itself; its name is empty (path comes from root_path).
        let name = index.intern("");
        index.entries.push(FileEntry {
            name,
            name_lower: name,
            parent: ROOT,
            flags: FLAG_DIR,
            native_key: root_key,
            size: 0,
            mtime: 0,
        });
        if root_key != 0 {
            index.by_native_key.insert(root_key, ROOT);
        }
        index
    }

    pub fn root_path(&self) -> &Path {
        &self.root_path
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Number of live (non-tombstoned) entries, excluding the root.
    pub fn len(&self) -> usize {
        self.entries
            .iter()
            .skip(1)
            .filter(|e| !e.is_tombstone())
            .count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn intern(&mut self, name: &str) -> NameRef {
        let offset = self.name_pool.len() as u32;
        self.name_pool.extend_from_slice(name.as_bytes());
        NameRef {
            offset,
            len: name.len() as u16,
        }
    }

    fn intern_lower(&mut self, name: &str) -> NameRef {
        let lower = name.to_lowercase();
        let offset = self.name_pool_lower.len() as u32;
        self.name_pool_lower.extend_from_slice(lower.as_bytes());
        NameRef {
            offset,
            len: lower.len() as u16,
        }
    }

    fn name_bytes(&self, r: NameRef) -> &[u8] {
        &self.name_pool[r.offset as usize..r.offset as usize + r.len as usize]
    }

    fn name_lower_bytes(&self, r: NameRef) -> &[u8] {
        &self.name_pool_lower[r.offset as usize..r.offset as usize + r.len as usize]
    }

    fn entry(&self, id: EntryId) -> Option<&FileEntry> {
        self.entries.get(id.0 as usize)
    }

    /// Append a new entry under `parent`. Does not check for duplicate names —
    /// bootstrap feeds are already unique per directory; delta sources must
    /// resolve first (see `resolve_child`).
    pub fn insert(&mut self, parent: EntryId, name: &str, is_dir: bool) -> Result<EntryId> {
        self.insert_with_key(parent, name, is_dir, 0)
    }

    /// [`VolumeIndex::insert`] with a platform-native key (NTFS FRN).
    /// Key 0 means "no native identity". Duplicate keys are rejected —
    /// callers resolve via [`VolumeIndex::entry_by_native_key`] first.
    pub fn insert_with_key(
        &mut self,
        parent: EntryId,
        name: &str,
        is_dir: bool,
        native_key: u64,
    ) -> Result<EntryId> {
        if name.len() > u16::MAX as usize {
            bail!("file name longer than {} bytes: {name:?}", u16::MAX);
        }
        if native_key != 0 && self.by_native_key.contains_key(&native_key) {
            bail!("native key {native_key:#x} already present");
        }
        let parent_entry = self
            .entry(parent)
            .ok_or_else(|| anyhow::anyhow!("insert under unknown parent {parent:?}"))?;
        if !parent_entry.is_dir() {
            bail!("insert under non-directory parent {parent:?}");
        }

        let name_ref = self.intern(name);
        let lower_ref = self.intern_lower(name);
        let id = EntryId(self.entries.len() as u32);
        self.entries.push(FileEntry {
            name: name_ref,
            name_lower: lower_ref,
            parent,
            flags: if is_dir { FLAG_DIR } else { 0 },
            native_key,
            // Bootstrap inserts names only; size/mtime are filled in later
            // (backfill / live watchers) via `set_meta`.
            size: 0,
            mtime: 0,
        });
        self.children.entry(parent).or_default().push(id);
        if native_key != 0 {
            self.by_native_key.insert(native_key, id);
        }
        self.generation += 1;
        Ok(id)
    }

    /// Record an entry's size and modification time (unix seconds),
    /// marking it as having populated metadata. Used by the background
    /// backfill and the live watchers; a no-op for an unknown id.
    pub fn set_meta(&mut self, id: EntryId, size: u64, mtime: i64) {
        if let Some(entry) = self.entries.get_mut(id.0 as usize) {
            let size = if entry.is_dir() { 0 } else { size };
            // Skip when nothing changes, so a redundant refresh (a repeat
            // create/modify event for an unchanged file) is a true no-op —
            // no generation bump, no needless re-save.
            if entry.has_meta() && entry.size == size && entry.mtime == mtime {
                return;
            }
            entry.size = size;
            entry.mtime = mtime;
            entry.flags |= FLAG_HAS_META;
            self.generation += 1;
        }
    }

    /// Up to `limit` live entries still lacking populated metadata, as
    /// `(id, name, absolute path)` — the background backfill's work queue.
    /// Directories are included (they get an mtime; size stays 0).
    pub fn unpopulated_batch(&self, limit: usize) -> Vec<(EntryId, String, PathBuf)> {
        let mut out = Vec::new();
        for (ix, entry) in self.entries.iter().enumerate().skip(1) {
            if out.len() >= limit {
                break;
            }
            if entry.is_tombstone() || entry.has_meta() {
                continue;
            }
            let id = EntryId(ix as u32);
            let (Some(name), Some(path)) = (self.name_of(id), self.path_of(id)) else {
                continue;
            };
            out.push((id, name.to_string(), path));
        }
        out
    }

    /// Populate metadata for a backfilled entry, but only if `id` still
    /// names the same entry (`expected_name`) — guards against the id
    /// remap a compaction performs between the off-lock stat and this
    /// apply. Returns whether it was applied.
    pub fn backfill_meta(&mut self, id: EntryId, expected_name: &str, size: u64, mtime: i64) -> bool {
        let matches = self
            .entry(id)
            .is_some_and(|e| !e.is_tombstone() && self.name_bytes(e.name) == expected_name.as_bytes());
        if matches {
            self.set_meta(id, size, mtime);
        }
        matches
    }

    /// Look up a live entry by its platform-native key (NTFS FRN).
    pub fn entry_by_native_key(&self, native_key: u64) -> Option<EntryId> {
        if native_key == 0 {
            return None;
        }
        self.by_native_key.get(&native_key).copied()
    }

    /// The platform-native key recorded for an entry (0 if none).
    pub fn native_key_of(&self, id: EntryId) -> Option<u64> {
        self.entry(id).map(|e| e.native_key)
    }

    /// Tombstone an entry and all its descendants. Pool bytes and arena
    /// slots are leaked until the writer runs [`VolumeIndex::compacted`];
    /// tombstones are skipped by search and listing.
    pub fn remove(&mut self, id: EntryId) -> Result<()> {
        if id == ROOT {
            bail!("cannot remove the root entry");
        }
        let mut stack = vec![id];
        while let Some(current) = stack.pop() {
            let Some(entry) = self.entries.get_mut(current.0 as usize) else {
                bail!("remove of unknown entry {current:?}");
            };
            entry.flags |= FLAG_TOMBSTONE;
            if entry.native_key != 0 {
                self.by_native_key.remove(&entry.native_key);
            }
            self.dead_debt += 1;
            if let Some(children) = self.children.remove(&current) {
                stack.extend(children);
            }
        }
        self.generation += 1;
        Ok(())
    }

    /// Rename and/or move an entry. Old name bytes are leaked until compaction.
    pub fn rename(&mut self, id: EntryId, new_parent: EntryId, new_name: &str) -> Result<()> {
        if id == ROOT {
            bail!("cannot rename the root entry");
        }
        if self.entry(id).is_none_or(FileEntry::is_tombstone) {
            bail!("rename of unknown or removed entry {id:?}");
        }
        if self.entry(new_parent).is_none_or(|p| !p.is_dir()) {
            bail!("rename target parent {new_parent:?} is not a directory");
        }

        let name_ref = self.intern(new_name);
        let lower_ref = self.intern_lower(new_name);
        let old_parent = self.entries[id.0 as usize].parent;
        let entry = &mut self.entries[id.0 as usize];
        entry.name = name_ref;
        entry.name_lower = lower_ref;
        entry.parent = new_parent;

        if old_parent != new_parent {
            if let Some(siblings) = self.children.get_mut(&old_parent) {
                siblings.retain(|&c| c != id);
            }
            self.children.entry(new_parent).or_default().push(id);
        }
        self.dead_debt += 1; // the old name's pool bytes are now leaked
        self.generation += 1;
        Ok(())
    }

    pub fn name_of(&self, id: EntryId) -> Option<&str> {
        let entry = self.entry(id)?;
        std::str::from_utf8(self.name_bytes(entry.name)).ok()
    }

    pub fn is_dir(&self, id: EntryId) -> Option<bool> {
        self.entry(id).map(FileEntry::is_dir)
    }

    /// Materialize the full path of an entry by chasing parent links.
    pub fn path_of(&self, id: EntryId) -> Option<PathBuf> {
        let mut components: Vec<&str> = Vec::new();
        let mut current = id;
        for _ in 0..MAX_PATH_DEPTH {
            if current == ROOT {
                let mut path = self.root_path.clone();
                path.extend(components.iter().rev());
                return Some(path);
            }
            let entry = self.entry(current)?;
            components.push(std::str::from_utf8(self.name_bytes(entry.name)).ok()?);
            current = entry.parent;
        }
        None // cycle or absurd depth
    }

    /// Live (non-tombstoned) children of a directory entry.
    pub fn children_of(&self, id: EntryId) -> impl Iterator<Item = EntryId> + '_ {
        self.children
            .get(&id)
            .into_iter()
            .flatten()
            .copied()
            .filter(|&c| self.entry(c).is_some_and(|e| !e.is_tombstone()))
    }

    /// Find the live child of `parent` with the given name (exact match).
    pub fn resolve_child(&self, parent: EntryId, name: &str) -> Option<EntryId> {
        self.children.get(&parent)?.iter().copied().find(|&c| {
            self.entry(c).is_some_and(|e| {
                !e.is_tombstone() && self.name_bytes(e.name) == name.as_bytes()
            })
        })
    }

    /// Resolve a path relative to the index root.
    pub fn resolve(&self, relative: &Path) -> Option<EntryId> {
        let mut current = ROOT;
        for component in relative.components() {
            match component {
                Component::Normal(name) => {
                    current = self.resolve_child(current, name.to_str()?)?;
                }
                Component::CurDir => {}
                _ => return None,
            }
        }
        Some(current)
    }

    /// Whether enough of the arena is dead (tombstones, leaked rename
    /// names) that a compaction pass is worth its rebuild cost.
    pub fn needs_compaction(&self) -> bool {
        compaction_due(self.dead_debt, self.entries.len())
    }

    /// Rebuild a fresh index containing only live entries — tombstoned
    /// arena slots and orphaned name-pool bytes are left behind. Entry
    /// ids change; names, hierarchy, native keys, and the root are
    /// preserved. The generation continues (old + 1) so staleness checks
    /// remain monotonic across the swap.
    pub fn compacted(&self) -> VolumeIndex {
        let root_key = self.entries[0].native_key;
        let mut fresh = VolumeIndex::new_with_root_key(&self.root_path, root_key);
        // Depth-first copy; children_of yields live entries only.
        let mut stack: Vec<(EntryId, EntryId)> = vec![(ROOT, ROOT)];
        while let Some((old_parent, new_parent)) = stack.pop() {
            for old_child in self.children_of(old_parent) {
                let Some(name) = self.name_of(old_child) else { continue };
                let is_dir = self.is_dir(old_child).unwrap_or(false);
                let key = self.native_key_of(old_child).unwrap_or(0);
                let Ok(new_child) = fresh.insert_with_key(new_parent, name, is_dir, key) else {
                    continue; // unreachable on a consistent index
                };
                // Carry populated size/mtime across the rebuild.
                if let Some(old) = self.entry(old_child)
                    && old.has_meta()
                {
                    fresh.set_meta(new_child, old.size, old.mtime);
                }
                if is_dir {
                    stack.push((old_child, new_child));
                }
            }
        }
        fresh.generation = self.generation + 1;
        fresh
    }

    /// Case-insensitive substring search over all live entries, ranked
    /// exact > prefix > word-boundary > substring, then by name length.
    /// Runs as a rayon parallel scan over the entry arena with bounded
    /// per-chunk top-K heaps, so cost is scan-dominated even for
    /// single-character queries that match nearly everything — no
    /// all-matches allocation, no global sort (see benches/).
    pub fn search(&self, query: &str, limit: usize) -> Vec<SearchHit> {
        if query.is_empty() || limit == 0 {
            return Vec::new();
        }
        let needle = query.to_lowercase();
        let finder = memmem::Finder::new(needle.as_bytes());

        let top = self
            .entries
            .par_iter()
            .enumerate()
            .skip(1) // root
            .filter(|(_, e)| !e.is_tombstone())
            .filter_map(|(ix, entry)| {
                let haystack = self.name_lower_bytes(entry.name_lower);
                let pos = finder.find(haystack)?;
                let kind = if pos == 0 {
                    if haystack.len() == needle.len() {
                        MatchKind::Exact
                    } else {
                        MatchKind::Prefix
                    }
                } else if !haystack[pos - 1].is_ascii_alphanumeric() {
                    MatchKind::WordBoundary
                } else {
                    MatchKind::Substring
                };
                Some((kind, entry.name_lower.len, ix as u32))
            })
            .fold(|| TopK::new(limit), TopK::push)
            .reduce(|| TopK::new(limit), TopK::merge);

        top.into_sorted()
            .into_iter()
            .map(|(kind, name_len, id)| SearchHit { id: EntryId(id), kind, name_len })
            .collect()
    }

    /// Like [`search`](Self::search), but each candidate must also satisfy
    /// every index-evaluable `filter` (`kind:`/`ext:`, and `size:`/
    /// `modified:` once the arena carries those fields). With no filters
    /// this is exactly [`search`](Self::search) — the branch keeps the
    /// common no-filter keystroke on the untouched fast path. An empty
    /// `query` with filters present is a filter-only scan (rank by name
    /// length). `tag:` filters are a no-op here (tags live in the sidecar,
    /// intersected by the caller).
    ///
    /// Prototype for docs/design-search-chips.md "Option A"; benchmarked
    /// against post-filtering in `benches/filter_bench.rs`.
    pub fn search_filtered(
        &self,
        query: &str,
        filters: &[crate::search_filter::Filter],
        limit: usize,
    ) -> Vec<SearchHit> {
        if limit == 0 {
            return Vec::new();
        }
        if filters.is_empty() {
            return self.search(query, limit);
        }
        let needle = (!query.is_empty()).then(|| query.to_lowercase());
        let finder = needle.as_ref().map(|n| memmem::Finder::new(n.as_bytes()));

        let top = self
            .entries
            .par_iter()
            .enumerate()
            .skip(1)
            .filter(|(_, e)| !e.is_tombstone())
            .filter_map(|(ix, entry)| {
                // Name-match first (cheapest cut) when there's a needle;
                // filter-only queries rank every survivor by name length.
                let kind = match (&finder, &needle) {
                    (Some(finder), Some(needle)) => {
                        let haystack = self.name_lower_bytes(entry.name_lower);
                        let pos = finder.find(haystack)?;
                        if pos == 0 {
                            if haystack.len() == needle.len() {
                                MatchKind::Exact
                            } else {
                                MatchKind::Prefix
                            }
                        } else if !haystack[pos - 1].is_ascii_alphanumeric() {
                            MatchKind::WordBoundary
                        } else {
                            MatchKind::Substring
                        }
                    }
                    _ => MatchKind::Substring,
                };
                let name = std::str::from_utf8(self.name_bytes(entry.name)).ok()?;
                let has_meta = entry.has_meta();
                let item = crate::search_filter::ItemMeta {
                    name,
                    is_dir: entry.is_dir(),
                    // Size is meaningless for directories; mtime isn't.
                    size: (has_meta && !entry.is_dir()).then_some(entry.size),
                    mtime: has_meta.then_some(entry.mtime),
                };
                if filters.iter().all(|f| f.matches(&item)) {
                    Some((kind, entry.name_lower.len, ix as u32))
                } else {
                    None
                }
            })
            .fold(|| TopK::new(limit), TopK::push)
            .reduce(|| TopK::new(limit), TopK::merge);

        top.into_sorted()
            .into_iter()
            .map(|(kind, name_len, id)| SearchHit { id: EntryId(id), kind, name_len })
            .collect()
    }
}

/// The live-update source for the OS we're built for, feeding
/// [`watcher::FsDelta`]s into the shared channel.
#[cfg(target_os = "macos")]
type PlatformWatcher = macos::FsEventsWatcher;
#[cfg(target_os = "linux")]
type PlatformWatcher = linux::LinuxWatcher;
/// Windows has two sources: the USN journal (elevated fast path, volume
/// roots) and ReadDirectoryChangesW (unprivileged fallback, any subtree).
#[cfg(target_os = "windows")]
#[allow(dead_code)] // fields are RAII guards: dropping them stops the watcher
enum PlatformWatcher {
    Usn(windows::UsnJournalWatcher),
    Rdcw(windows::DirChangesWatcher),
}

/// How the shutdown snapshot learns its checkpoint.
#[derive(Clone)]
enum CheckpointSource {
    /// macOS with a live FSEvents stream: the shared latest-event-id.
    /// Read after the watcher is dropped and the writer joined, it is the
    /// exact id through which every event has been applied.
    #[cfg(target_os = "macos")]
    FsEventsId(std::sync::Arc<std::sync::atomic::AtomicU64>),
    /// Windows with a live USN journal watcher: journal identity plus the
    /// shared next-USN position (same read-after-join contract as
    /// FsEventsId).
    #[cfg(target_os = "windows")]
    UsnPos {
        journal_id: u64,
        next_usn: std::sync::Arc<std::sync::atomic::AtomicU64>,
    },
    /// Walk-based watchers (inotify/RDCW): record the save time; the next
    /// start reconciles with a rescan regardless.
    #[cfg(not(target_os = "macos"))]
    Reconcile,
    /// The watcher never started — the index may have silently missed
    /// changes; the next start must not trust any journal position.
    Untracked,
}

#[derive(Clone)]
struct Persistence {
    path: PathBuf,
    source: CheckpointSource,
}

/// How often a live index snapshots itself (via a PersistNow marker
/// through the delta channel — see [`watcher::FsDelta::PersistNow`] for
/// why that ordering makes the checkpoint safe). Bounds how much walk
/// work a crash can cost; a clean shutdown still saves precisely.
const SNAPSHOT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(300);

/// Ticks the snapshot interval and enqueues PersistNow markers. Owning a
/// delta sender, it must be stopped *before* the writer is joined.
struct SnapshotSaver {
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl SnapshotSaver {
    fn spawn(
        interval: std::time::Duration,
        persistence: Persistence,
        deltas: std::sync::mpsc::Sender<Vec<watcher::FsDelta>>,
    ) -> Result<Self> {
        let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let thread = std::thread::Builder::new().name("filex-snapshot".into()).spawn({
            let shutdown = shutdown.clone();
            move || {
                use std::sync::atomic::Ordering;
                let tick = std::time::Duration::from_millis(500);
                let mut elapsed = std::time::Duration::ZERO;
                while !shutdown.load(Ordering::Relaxed) {
                    std::thread::sleep(tick);
                    elapsed += tick;
                    if elapsed < interval {
                        continue;
                    }
                    elapsed = std::time::Duration::ZERO;
                    let marker = watcher::FsDelta::PersistNow {
                        checkpoint: persistence.checkpoint(),
                    };
                    if deltas.send(vec![marker]).is_err() {
                        break; // writer gone: shutting down
                    }
                }
            }
        })?;
        Ok(Self { shutdown, thread: Some(thread) })
    }
}

impl Drop for SnapshotSaver {
    fn drop(&mut self) {
        self.shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            thread.join().ok(); // wakes within one tick
        }
    }
}

/// How many entries the backfill stats per pass before writing them back.
const BACKFILL_BATCH: usize = 512;

/// Populates `size`/`mtime` on entries the bootstrap left name-only, off
/// the critical path (Option C, `docs/design-search-chips.md`). On a
/// low-priority thread it stats entries in batches and writes the results
/// back under short write locks, so `size:`/`modified:` filters converge
/// over the first seconds of a fresh index without slowing time-to-
/// searchable. Once caught up it polls slowly for entries added by live
/// updates (the freshness layer will populate those inline later).
struct MetaBackfiller {
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl MetaBackfiller {
    fn spawn(index: watcher::SharedIndex) -> Result<Self> {
        let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let thread = std::thread::Builder::new().name("filex-meta-backfill".into()).spawn({
            let shutdown = shutdown.clone();
            move || backfill_loop(&index, &shutdown)
        })?;
        Ok(Self { shutdown, thread: Some(thread) })
    }
}

impl Drop for MetaBackfiller {
    fn drop(&mut self) {
        self.shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            thread.join().ok();
        }
    }
}

/// Read the entry's modification time as unix seconds (0 if unavailable).
fn mtime_secs(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The backfill worker loop (see [`MetaBackfiller`]). Sleeps in short
/// ticks so `shutdown` is observed promptly.
fn backfill_loop(
    index: &watcher::SharedIndex,
    shutdown: &std::sync::atomic::AtomicBool,
) {
    use std::sync::atomic::Ordering;
    let tick = std::time::Duration::from_millis(500);
    while !shutdown.load(Ordering::Relaxed) {
        let batch = {
            let idx = index.read().unwrap_or_else(std::sync::PoisonError::into_inner);
            idx.unpopulated_batch(BACKFILL_BATCH)
        };
        if batch.is_empty() {
            // Caught up: poll slowly (~2s) for entries added by live
            // updates, in shutdown-observing ticks.
            for _ in 0..4 {
                if shutdown.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(tick);
            }
            continue;
        }
        // Stat off-lock. A vanished/unreadable entry is marked with 0/0 so
        // it isn't re-collected forever (the watcher will remove it).
        let mut updates = Vec::with_capacity(batch.len());
        for (id, name, path) in batch {
            if shutdown.load(Ordering::Relaxed) {
                return;
            }
            let (size, mtime) = std::fs::symlink_metadata(&path)
                .map(|meta| (meta.len(), mtime_secs(&meta)))
                .unwrap_or((0, 0));
            updates.push((id, name, size, mtime));
        }
        {
            let mut idx = index.write().unwrap_or_else(std::sync::PoisonError::into_inner);
            for (id, name, size, mtime) in updates {
                idx.backfill_meta(id, &name, size, mtime);
            }
        }
        // Yield between batches so the writer/readers aren't starved.
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

impl Persistence {
    fn checkpoint(&self) -> persist::Checkpoint {
        match &self.source {
            #[cfg(target_os = "macos")]
            CheckpointSource::FsEventsId(id) => persist::Checkpoint::FsEvents {
                last_event_id: id.load(std::sync::atomic::Ordering::Relaxed),
            },
            #[cfg(target_os = "windows")]
            CheckpointSource::UsnPos { journal_id, next_usn } => {
                persist::Checkpoint::UsnJournal {
                    journal_id: *journal_id,
                    next_usn: next_usn.load(std::sync::atomic::Ordering::Relaxed),
                }
            }
            #[cfg(not(target_os = "macos"))]
            CheckpointSource::Reconcile => persist::Checkpoint::WalkedAt {
                unix_seconds: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
            },
            CheckpointSource::Untracked => persist::Checkpoint::None,
        }
    }
}

/// A volume index kept up to date by a platform watcher feeding an
/// [`watcher::IndexWriter`]. Dropping this stops live updates and writes
/// the snapshot (watcher first, then writer join, then save — an order
/// that guarantees the persisted checkpoint covers exactly the applied
/// events, losing none).
pub struct LiveIndex {
    pub index: watcher::SharedIndex,
    saver: Option<SnapshotSaver>,
    backfiller: Option<MetaBackfiller>,
    watcher: Option<PlatformWatcher>,
    writer: Option<watcher::IndexWriter>,
    persistence: Option<Persistence>,
}

impl LiveIndex {
    /// Whether the live watcher's coverage is partial (Linux inotify with
    /// an exhausted watch budget). The index still converges via periodic
    /// reconcile rescans, but changes may lag — worth surfacing in the UI.
    pub fn coverage_degraded(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            self.watcher.as_ref().is_some_and(linux::LinuxWatcher::is_degraded)
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }
}

impl Drop for LiveIndex {
    fn drop(&mut self) {
        drop(self.backfiller.take()); // stop backfill writes before the save
        drop(self.saver.take()); // stop marker source; its sender closes
        drop(self.watcher.take()); // stop events; delta senders close
        drop(self.writer.take()); // join: every sent delta is now applied
        if let Some(persistence) = self.persistence.take() {
            let index = self
                .index
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Err(err) = persist::save(&index, persistence.checkpoint(), &persistence.path) {
                tracing::error!("failed to save index snapshot: {err:#}");
            }
        }
    }
}

/// [`start_live_index_with_snapshot`] with the default per-root snapshot
/// location in the platform data directory.
pub fn start_live_index(
    root: &Path,
    on_change: impl Fn() + Send + 'static,
) -> Result<LiveIndex> {
    start_live_index_cancellable(root, on_change, None)
}

/// [`start_live_index`] whose (walk) bootstrap can be cancelled by setting
/// `cancel` — a service shutdown abandons a long initial index promptly
/// instead of blocking the stop until the walk finishes. A snapshot-load
/// or USN-fast-path start has no long walk to cancel.
pub fn start_live_index_cancellable(
    root: &Path,
    on_change: impl Fn() + Send + 'static,
    cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
) -> Result<LiveIndex> {
    let snapshot_path = persist::default_snapshot_path(root);
    start_live_index_inner(root, snapshot_path, on_change, cancel)
}

/// Bootstrap an index for `root` and attach the platform's live watcher
/// (FSEvents / inotify / ReadDirectoryChangesW).
///
/// Startup strategy, fastest first:
/// - Valid snapshot + FSEvents checkpoint (macOS): load and *replay the
///   journal gap* — no walk at all. If the OS purged the history, the
///   stream reports `MustScanSubDirs` which becomes a root rescan.
/// - Valid snapshot, no replayable journal (Linux/Windows, or a dead
///   watcher): load for instant-but-stale search and queue a root rescan;
///   the writer rebuilds off-lock and swaps, so queries never block on it.
/// - No/invalid snapshot: full bootstrap walk.
///
/// If the watcher can't start (permissions, watch limits), the index still
/// works as a static snapshot — search is never hostage to live updates.
/// The watcher starts *before* any walk so no event is missed; replayed or
/// double-seen deltas are absorbed by idempotent application. `on_change`
/// fires from the writer thread after each applied batch.
pub fn start_live_index_with_snapshot(
    root: &Path,
    snapshot_path: Option<PathBuf>,
    on_change: impl Fn() + Send + 'static,
) -> Result<LiveIndex> {
    start_live_index_inner(root, snapshot_path, on_change, None)
}

fn start_live_index_inner(
    root: &Path,
    snapshot_path: Option<PathBuf>,
    on_change: impl Fn() + Send + 'static,
    cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
) -> Result<LiveIndex> {
    // Watchers report canonical paths; watch the same form we index.
    let canonical = root.canonicalize()?;
    let loaded = snapshot_path.as_ref().and_then(|path| {
        if !path.exists() {
            return None;
        }
        match persist::load(path, &canonical) {
            Ok(snapshot) => Some(snapshot),
            Err(err) => {
                tracing::warn!("ignoring unusable index snapshot: {err:#}");
                None
            }
        }
    });
    platform_start(canonical, snapshot_path, loaded, on_change, cancel)
}

/// Assemble the writer/persistence tail shared by every platform start.
fn assemble_live_index(
    canonical: PathBuf,
    snapshot_path: Option<PathBuf>,
    watcher: Option<PlatformWatcher>,
    index: VolumeIndex,
    needs_rescan: bool,
    source: CheckpointSource,
    delta_tx: std::sync::mpsc::Sender<Vec<watcher::FsDelta>>,
    delta_rx: std::sync::mpsc::Receiver<Vec<watcher::FsDelta>>,
    on_change: impl Fn() + Send + 'static,
) -> Result<LiveIndex> {
    if needs_rescan {
        // Loaded state is stale-but-searchable; reconcile off-lock.
        delta_tx
            .send(vec![watcher::FsDelta::Rescan { path: canonical }])
            .ok();
    }

    let persistence = snapshot_path.map(|path| Persistence { path, source });
    // Periodic saves: the writer saves when it processes a PersistNow
    // marker; the saver thread enqueues one every SNAPSHOT_INTERVAL.
    // Skipped when nothing changed since the last save.
    let save_hook: Option<watcher::SaveHook> = persistence.as_ref().map(|p| {
        let path = p.path.clone();
        let last_generation = std::sync::atomic::AtomicU64::new(u64::MAX);
        Box::new(move |index: &VolumeIndex, checkpoint: persist::Checkpoint| {
            use std::sync::atomic::Ordering;
            if last_generation.swap(index.generation(), Ordering::Relaxed)
                == index.generation()
            {
                return; // unchanged since the last periodic save
            }
            if let Err(err) = persist::save(index, checkpoint, &path) {
                tracing::error!("periodic snapshot save failed: {err:#}");
            }
        }) as watcher::SaveHook
    });
    let saver = match &persistence {
        Some(p) => Some(SnapshotSaver::spawn(
            SNAPSHOT_INTERVAL,
            p.clone(),
            delta_tx.clone(),
        )?),
        None => None,
    };
    drop(delta_tx); // watcher + saver hold the remaining senders

    let shared: watcher::SharedIndex = std::sync::Arc::new(std::sync::RwLock::new(index));
    let writer = watcher::IndexWriter::spawn(shared.clone(), delta_rx, on_change, save_hook)?;
    // Populate size/mtime that bootstrap left name-only, off the critical
    // path (Option C). Idle-polls once caught up.
    let backfiller = Some(MetaBackfiller::spawn(shared.clone())?);

    Ok(LiveIndex {
        index: shared,
        saver,
        backfiller,
        watcher,
        writer: Some(writer),
        persistence,
    })
}

#[cfg(not(target_os = "windows"))]
fn platform_start(
    canonical: PathBuf,
    snapshot_path: Option<PathBuf>,
    loaded: Option<persist::Snapshot>,
    on_change: impl Fn() + Send + 'static,
    cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
) -> Result<LiveIndex> {
    let (delta_tx, delta_rx) = std::sync::mpsc::channel();

    let resume_from = match loaded.as_ref().map(|s| s.checkpoint) {
        Some(persist::Checkpoint::FsEvents { last_event_id }) => Some(last_event_id),
        _ => None,
    };
    #[cfg(not(target_os = "macos"))]
    let _ = resume_from; // only FSEvents supports replay today

    let spawned = {
        #[cfg(target_os = "macos")]
        {
            macos::FsEventsWatcher::spawn(&canonical, resume_from, delta_tx.clone())
        }
        #[cfg(target_os = "linux")]
        {
            linux::LinuxWatcher::spawn(&canonical, delta_tx.clone())
        }
    };
    let fs_watcher = match spawned {
        Ok(watcher) => Some(watcher),
        Err(err) => {
            tracing::warn!("live index updates disabled: {err:#}");
            None
        }
    };

    let replaying = cfg!(target_os = "macos") && fs_watcher.is_some() && resume_from.is_some();
    let (index, needs_rescan) = match loaded {
        Some(snapshot) => (snapshot.index, !replaying),
        None => {
            use walker::IndexSource as _;
            let source = walker::FsWalkSource::default();
            (source.bootstrap_cancellable(&canonical, cancel.as_deref())?, false)
        }
    };

    let source = match &fs_watcher {
        #[cfg(target_os = "macos")]
        Some(watcher) => CheckpointSource::FsEventsId(watcher.latest_event_id_handle()),
        #[cfg(not(target_os = "macos"))]
        Some(_) => CheckpointSource::Reconcile,
        None => CheckpointSource::Untracked,
    };
    assemble_live_index(
        canonical, snapshot_path, fs_watcher, index, needs_rescan, source, delta_tx, delta_rx,
        on_change,
    )
}

/// Windows startup, fastest viable tier first:
/// 1. Snapshot + valid USN checkpoint (volume root, elevated): load and
///    tail the journal from the saved USN — replay, no walk, no MFT pass.
/// 2. Elevated on a volume root: MFT enumeration bootstrap + journal tail.
/// 3. Anything else (unelevated, subtree root, non-NTFS): RDCW watcher
///    with snapshot-plus-rescan or a fresh walk.
#[cfg(target_os = "windows")]
fn platform_start(
    canonical: PathBuf,
    snapshot_path: Option<PathBuf>,
    loaded: Option<persist::Snapshot>,
    on_change: impl Fn() + Send + 'static,
    cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
) -> Result<LiveIndex> {
    let (delta_tx, delta_rx) = std::sync::mpsc::channel();
    let mut loaded = loaded;

    if windows::volume_root_drive(&canonical).is_some() {
        // Tier 1: journal replay from the persisted checkpoint.
        if let Some(snapshot) = loaded.as_ref()
            && let persist::Checkpoint::UsnJournal { journal_id, next_usn } = snapshot.checkpoint
        {
            let valid = match windows::query_usn_journal(&canonical) {
                Ok(info) => {
                    info.journal_id == journal_id
                        && (next_usn as i64) >= info.first_usn
                        && (next_usn as i64) <= info.next_usn
                }
                Err(err) => {
                    tracing::warn!("USN journal query failed: {err:#}");
                    false
                }
            };
            if valid {
                match windows::UsnJournalWatcher::spawn(
                    &canonical,
                    journal_id,
                    next_usn as i64,
                    delta_tx.clone(),
                ) {
                    Ok(watcher) => {
                        let source = CheckpointSource::UsnPos {
                            journal_id: watcher.journal_id(),
                            next_usn: watcher.next_usn_handle(),
                        };
                        let index = loaded.take().expect("checked above").index;
                        return assemble_live_index(
                            canonical,
                            snapshot_path,
                            Some(PlatformWatcher::Usn(watcher)),
                            index,
                            false,
                            source,
                            delta_tx,
                            delta_rx,
                            on_change,
                        );
                    }
                    Err(err) => tracing::warn!("USN journal watcher failed: {err:#}"),
                }
            }
        }

        // Tier 2: fresh MFT-enumeration bootstrap.
        match windows::usn_bootstrap(&canonical) {
            Ok(boot) => {
                let (watcher, source) = match windows::UsnJournalWatcher::spawn(
                    &canonical,
                    boot.journal.journal_id,
                    boot.journal.next_usn,
                    delta_tx.clone(),
                ) {
                    Ok(watcher) => {
                        let source = CheckpointSource::UsnPos {
                            journal_id: watcher.journal_id(),
                            next_usn: watcher.next_usn_handle(),
                        };
                        (Some(PlatformWatcher::Usn(watcher)), source)
                    }
                    Err(err) => {
                        tracing::warn!("USN journal watcher failed: {err:#}");
                        (None, CheckpointSource::Untracked)
                    }
                };
                return assemble_live_index(
                    canonical,
                    snapshot_path,
                    watcher,
                    boot.index,
                    false,
                    source,
                    delta_tx,
                    delta_rx,
                    on_change,
                );
            }
            Err(err) => {
                // Expected without elevation: fall through to RDCW.
                tracing::info!("USN fast path unavailable ({err:#}); using directory watching");
            }
        }
    }

    // Tier 3: unprivileged fallback — RDCW plus snapshot-or-walk.
    let fs_watcher = match windows::DirChangesWatcher::spawn(&canonical, delta_tx.clone()) {
        Ok(watcher) => Some(PlatformWatcher::Rdcw(watcher)),
        Err(err) => {
            tracing::warn!("live index updates disabled: {err:#}");
            None
        }
    };
    let (index, needs_rescan) = match loaded {
        Some(snapshot) => (snapshot.index, true),
        None => {
            use walker::IndexSource as _;
            let source = walker::FsWalkSource::default();
            (source.bootstrap_cancellable(&canonical, cancel.as_deref())?, false)
        }
    };
    let source = match &fs_watcher {
        Some(_) => CheckpointSource::Reconcile,
        None => CheckpointSource::Untracked,
    };
    assemble_live_index(
        canonical, snapshot_path, fs_watcher, index, needs_rescan, source, delta_tx, delta_rx,
        on_change,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build:  root/{docs/{Report.pdf, notes.txt}, src/main.rs}
    fn sample_index() -> (VolumeIndex, EntryId, EntryId, EntryId, EntryId, EntryId) {
        let mut index = VolumeIndex::new("/vol");
        let docs = index.insert(ROOT, "docs", true).unwrap();
        let report = index.insert(docs, "Report.pdf", false).unwrap();
        let notes = index.insert(docs, "notes.txt", false).unwrap();
        let src = index.insert(ROOT, "src", true).unwrap();
        let main_rs = index.insert(src, "main.rs", false).unwrap();
        (index, docs, report, notes, src, main_rs)
    }

    #[test]
    fn search_filtered_matches_search_and_filters() {
        use crate::listing::FileKind;
        use crate::search_filter::Filter;
        let (index, ..) = sample_index();

        // No filters ⇒ identical to plain search.
        let plain = index.search("r", 100);
        let filtered = index.search_filtered("r", &[], 100);
        assert_eq!(plain.len(), filtered.len());
        assert_eq!(
            plain.iter().map(|h| h.id).collect::<Vec<_>>(),
            filtered.iter().map(|h| h.id).collect::<Vec<_>>()
        );

        // text + kind:code ⇒ only main.rs (not Report.pdf / notes.txt).
        let hits = index.search_filtered("", &[Filter::Kind(FileKind::Code)], 100);
        let names: Vec<_> = hits.iter().filter_map(|h| index.name_of(h.id)).collect();
        assert_eq!(names, vec!["main.rs"]);

        // filter-only ext:pdf ⇒ only Report.pdf.
        let hits = index.search_filtered("", &[Filter::Ext("pdf".into())], 100);
        let names: Vec<_> = hits.iter().filter_map(|h| index.name_of(h.id)).collect();
        assert_eq!(names, vec!["Report.pdf"]);
    }

    #[test]
    fn search_filtered_size_and_mtime_need_populated_meta() {
        use crate::search_filter::{Bound, Filter};
        let (mut index, _docs, report, notes, ..) = sample_index();

        // Before backfill, size:/modified: match nothing (meta unknown).
        assert!(index.search_filtered("", &[Filter::Size(Bound::Gt(0))], 100).is_empty());

        index.set_meta(report, 5000, 1_000); // Report.pdf: 5 KB, old
        index.set_meta(notes, 10, 9_000); // notes.txt: tiny, newer

        // size:>1kb ⇒ only Report.pdf.
        let big = index.search_filtered("", &[Filter::Size(Bound::Gt(1024))], 100);
        assert_eq!(
            big.iter().filter_map(|h| index.name_of(h.id)).collect::<Vec<_>>(),
            vec!["Report.pdf"]
        );
        // modified:>5000s ⇒ only notes.txt.
        let recent = index.search_filtered("", &[Filter::Modified(Bound::Gt(5_000))], 100);
        assert_eq!(
            recent.iter().filter_map(|h| index.name_of(h.id)).collect::<Vec<_>>(),
            vec!["notes.txt"]
        );
    }

    #[test]
    fn unpopulated_batch_shrinks_and_backfill_meta_guards() {
        let (mut index, _docs, report, ..) = sample_index();
        let before = index.unpopulated_batch(100).len();
        assert!(before >= 4); // docs, Report.pdf, notes.txt, src, main.rs

        // Guard: applies for the right name, rejects a mismatched one
        // (as a compaction id-remap would produce).
        assert!(index.backfill_meta(report, "Report.pdf", 100, 5));
        assert!(!index.backfill_meta(report, "Wrong.pdf", 999, 9));
        // One fewer unpopulated; the rejected update didn't stick.
        assert_eq!(index.unpopulated_batch(100).len(), before - 1);
        assert_eq!(
            index
                .search_filtered("", &[crate::search_filter::Filter::Size(
                    crate::search_filter::Bound::Ge(50),
                )], 10)
                .iter()
                .filter_map(|h| index.name_of(h.id))
                .collect::<Vec<_>>(),
            vec!["Report.pdf"]
        );
    }

    #[test]
    fn backfiller_thread_populates_size_for_search() {
        use crate::index::walker::IndexSource;
        use crate::search_filter::{Bound, Filter};
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("big.bin"), vec![0u8; 5000]).unwrap();
        std::fs::write(dir.path().join("small.txt"), b"hi").unwrap();
        let index = walker::FsWalkSource::default().bootstrap(dir.path()).unwrap();
        // Before backfill nothing has size metadata.
        assert!(index.search_filtered("", &[Filter::Size(Bound::Ge(5000))], 10).is_empty());

        let shared: watcher::SharedIndex = std::sync::Arc::new(std::sync::RwLock::new(index));
        let backfiller = MetaBackfiller::spawn(shared.clone()).unwrap();

        // Bounded wait for the background pass to fill size/mtime.
        let mut names: Vec<String> = Vec::new();
        for _ in 0..40 {
            std::thread::sleep(std::time::Duration::from_millis(50));
            let idx = shared.read().unwrap();
            names = idx
                .search_filtered("", &[Filter::Size(Bound::Ge(5000))], 10)
                .iter()
                .filter_map(|h| idx.name_of(h.id).map(str::to_string))
                .collect();
            if !names.is_empty() {
                break;
            }
        }
        drop(backfiller);
        assert_eq!(names, vec!["big.bin".to_string()]);
    }

    #[test]
    fn materializes_full_paths_from_parent_links() {
        let (index, docs, report, ..) = sample_index();
        assert_eq!(index.path_of(ROOT).unwrap(), PathBuf::from("/vol"));
        assert_eq!(index.path_of(docs).unwrap(), PathBuf::from("/vol/docs"));
        assert_eq!(
            index.path_of(report).unwrap(),
            PathBuf::from("/vol/docs/Report.pdf")
        );
    }

    #[test]
    fn resolves_relative_paths() {
        let (index, docs, report, ..) = sample_index();
        assert_eq!(index.resolve(Path::new("docs")), Some(docs));
        assert_eq!(index.resolve(Path::new("docs/Report.pdf")), Some(report));
        assert_eq!(index.resolve(Path::new("docs/missing")), None);
    }

    #[test]
    fn search_is_case_insensitive() {
        let (index, _, report, ..) = sample_index();
        let hits = index.search("REPORT", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, report);
    }

    #[test]
    fn search_ranks_exact_then_prefix_then_boundary_then_substring() {
        let mut index = VolumeIndex::new("/vol");
        let substring = index.insert(ROOT, "xmain.rs", false).unwrap();
        let boundary = index.insert(ROOT, "test_main.rs", false).unwrap();
        let exact = index.insert(ROOT, "main", false).unwrap();
        let prefix = index.insert(ROOT, "main.rs", false).unwrap();

        let ids: Vec<EntryId> = index.search("main", 10).into_iter().map(|h| h.id).collect();
        assert_eq!(ids, vec![exact, prefix, boundary, substring]);
    }

    #[test]
    fn search_respects_limit_and_skips_root() {
        let mut index = VolumeIndex::new("/match-in-root-name");
        for i in 0..20 {
            index.insert(ROOT, &format!("file{i}.txt"), false).unwrap();
        }
        assert_eq!(index.search("file", 5).len(), 5);
        // Root's own (empty) name never matches.
        assert!(index.search("match-in-root-name", 10).is_empty());
    }

    #[test]
    fn removed_entries_disappear_from_search_and_resolution() {
        let (mut index, docs, report, notes, ..) = sample_index();
        index.remove(docs).unwrap();

        assert!(index.search("report", 10).is_empty());
        assert!(index.search("notes", 10).is_empty());
        assert_eq!(index.resolve(Path::new("docs")), None);
        assert_eq!(index.len(), 2); // src + main.rs survive
        let _ = (report, notes);
    }

    #[test]
    fn rename_moves_entry_and_updates_search() {
        let (mut index, _docs, report, _notes, src, _main) = sample_index();
        index.rename(report, src, "summary.pdf").unwrap();

        assert!(index.search("report", 10).is_empty());
        let hits = index.search("summary", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(
            index.path_of(hits[0].id).unwrap(),
            PathBuf::from("/vol/src/summary.pdf")
        );
        assert_eq!(index.resolve(Path::new("docs/Report.pdf")), None);
        assert_eq!(index.resolve(Path::new("src/summary.pdf")), Some(report));
    }

    #[test]
    fn insert_rejects_bad_parents() {
        let (mut index, _docs, report, ..) = sample_index();
        assert!(index.insert(report, "child", false).is_err()); // file parent
        assert!(index.insert(EntryId(9999), "child", false).is_err());
    }

    #[test]
    fn mutations_bump_generation() {
        let (mut index, _docs, report, ..) = sample_index();
        let g0 = index.generation();
        index.rename(report, ROOT, "r.pdf").unwrap();
        assert!(index.generation() > g0);
        let g1 = index.generation();
        index.remove(report).unwrap();
        assert!(index.generation() > g1);
    }

    /// The saver ticks PersistNow markers into the delta channel at its
    /// interval and stops cleanly on drop.
    #[test]
    fn snapshot_saver_enqueues_markers_periodically() {
        use std::time::{Duration, Instant};

        let (delta_tx, delta_rx) = std::sync::mpsc::channel();
        let persistence = Persistence {
            path: PathBuf::from("/nowhere/test.fxidx"),
            source: CheckpointSource::Untracked,
        };
        let saver =
            SnapshotSaver::spawn(Duration::from_millis(500), persistence, delta_tx).unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut seen = 0;
        while seen < 2 && Instant::now() < deadline {
            if let Ok(batch) = delta_rx.recv_timeout(Duration::from_millis(250))
                && batch
                    .iter()
                    .any(|d| matches!(d, watcher::FsDelta::PersistNow { .. }))
            {
                seen += 1;
            }
        }
        assert_eq!(seen, 2, "saver never ticked twice");
        drop(saver); // must join promptly, not hang
    }

    /// Full persistence cycle: index, shut down (snapshot written), change
    /// the filesystem while "down", restart from the snapshot, converge —
    /// via FSEvents replay on macOS, via the startup rescan elsewhere.
    /// Runs on every OS with a live watcher.
    #[test]
    fn restart_from_snapshot_catches_up_with_offline_changes() {
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        let dir = tempfile::tempdir().unwrap();
        let snap_dir = tempfile::tempdir().unwrap();
        let snap = Some(snap_dir.path().join("test.fxidx"));
        std::fs::write(dir.path().join("keep.txt"), b"x").unwrap();
        std::fs::write(dir.path().join("doomed.txt"), b"x").unwrap();

        // First run: bootstrap walk, then clean shutdown -> snapshot.
        {
            let live =
                start_live_index_with_snapshot(dir.path(), snap.clone(), || {}).unwrap();
            let index = live.index.read().unwrap();
            assert_eq!(index.search("keep", 10).len(), 1);
            drop(index);
        }
        assert!(snap.as_ref().unwrap().exists(), "snapshot not written");

        // Changes while the index is down.
        std::fs::write(dir.path().join("offline-new.txt"), b"x").unwrap();
        std::fs::remove_file(dir.path().join("doomed.txt")).unwrap();

        // Second run: loads the snapshot (instant search on stale data),
        // then converges via replay or rescan.
        let (notify_tx, notify_rx) = mpsc::channel();
        let live = start_live_index_with_snapshot(dir.path(), snap, move || {
            notify_tx.send(()).ok();
        })
        .unwrap();

        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            {
                let index = live.index.read().unwrap();
                let converged = index.search("offline-new", 10).len() == 1
                    && index.search("doomed", 10).is_empty()
                    && index.search("keep", 10).len() == 1;
                if converged {
                    break;
                }
            }
            if Instant::now() > deadline {
                panic!("index never converged with offline changes");
            }
            notify_rx.recv_timeout(Duration::from_millis(250)).ok();
        }
    }

    /// End-to-end: bootstrap + FSEvents watcher + writer thread. A file
    /// created after startup becomes searchable without any rescan call.
    #[cfg(target_os = "macos")]
    #[test]
    fn live_index_picks_up_new_files_end_to_end() {
        use std::time::{Duration, Instant};

        let dir = tempfile::tempdir().unwrap();
        let (notify_tx, notify_rx) = std::sync::mpsc::channel();
        // No snapshot path: tests must not touch the real user data dir.
        let live = start_live_index_with_snapshot(dir.path(), None, move || {
            notify_tx.send(()).ok();
        })
        .unwrap();

        std::thread::sleep(Duration::from_millis(500)); // let the stream attach
        std::fs::write(dir.path().join("live-e2e.txt"), b"x").unwrap();

        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if notify_rx.recv_timeout(Duration::from_millis(250)).is_ok() {
                let index = live.index.read().unwrap();
                if index.search("live-e2e", 10).len() == 1 {
                    return;
                }
            }
        }
        panic!("file created after bootstrap never became searchable");
    }

    #[test]
    fn compaction_reclaims_dead_space_and_preserves_content() {
        let mut index = VolumeIndex::new_with_root_key("/vol", 5);
        let docs = index.insert_with_key(ROOT, "docs", true, 10).unwrap();
        let keep = index.insert_with_key(docs, "keep.txt", false, 11).unwrap();
        let doomed = index.insert_with_key(ROOT, "doomed", true, 20).unwrap();
        index.insert_with_key(doomed, "gone.txt", false, 21).unwrap();
        index.remove(doomed).unwrap();
        index.rename(keep, docs, "kept-longer-name.txt").unwrap();
        let generation = index.generation();
        let pool_before = index.name_pool.len();

        let fresh = index.compacted();

        // Same live content...
        assert_eq!(fresh.len(), index.len());
        assert_eq!(fresh.search("kept-longer", 10).len(), 1);
        assert!(fresh.search("gone", 10).is_empty());
        let hit = fresh.search("kept-longer", 10)[0].id;
        assert_eq!(fresh.path_of(hit).unwrap(), PathBuf::from("/vol/docs/kept-longer-name.txt"));
        assert_eq!(fresh.entry_by_native_key(5), Some(ROOT));
        assert_eq!(fresh.entry_by_native_key(11), Some(hit));
        assert_eq!(fresh.entry_by_native_key(21), None);
        // ...in a smaller arena, with debt cleared and generation advanced.
        assert_eq!(fresh.entries.len(), fresh.len() + 1);
        assert!(fresh.name_pool.len() < pool_before);
        assert_eq!(fresh.dead_debt, 0);
        assert_eq!(fresh.generation(), generation + 1);
    }

    #[test]
    fn compaction_trigger_needs_both_floor_and_ratio() {
        assert!(!compaction_due(100, 200)); // ratio met, below floor
        assert!(!compaction_due(5000, 1_000_000)); // floor met, ratio not
        assert!(compaction_due(5000, 20_000));
        assert!(!compaction_due(4095, 4)); // just under the floor
    }

    #[test]
    fn unicode_names_search_case_insensitively() {
        let mut index = VolumeIndex::new("/vol");
        let id = index.insert(ROOT, "Übersicht.md", false).unwrap();
        let hits = index.search("übersicht", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, id);
    }
}

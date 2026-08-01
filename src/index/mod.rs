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

use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, bail};
use memchr::memmem;
use rayon::prelude::*;

/// A cancel flag that is never set, for the non-cancellable public search
/// entry points. Threading a real flag is opt-in via the `*_cancellable`
/// methods; everything else (tests, benches, one-shot lookups) shares
/// this constant-false sentinel so their signatures stay unchanged.
pub(crate) static NEVER_CANCEL: AtomicBool = AtomicBool::new(false);

/// Stable handle to an entry; index into `VolumeIndex::entries`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EntryId(u32);

pub const ROOT: EntryId = EntryId(0);

/// Per-scan controls threaded through the parallel search.
///
/// Bundled into one struct rather than two parameters because they ride
/// together down every layer (`search_cancellable` → `finish` →
/// `fuzzy_pass`) and the list would otherwise only grow.
#[derive(Clone, Copy)]
pub struct ScanCtl<'a> {
    /// Polled once per candidate; when a newer query sets it, the scan
    /// winds down to a near-no-op. See [`VolumeIndex::search_cancellable`].
    pub cancel: &'a AtomicBool,
    /// When `Some(dir)`, only entries within `dir`'s subtree count — the
    /// "Current Dir" search scope. Others are skipped as if unmatched.
    pub scope: Option<EntryId>,
}

impl<'a> ScanCtl<'a> {
    /// Cancellable, unscoped — the shape every pre-scope caller wants.
    pub fn new(cancel: &'a AtomicBool) -> Self {
        Self { cancel, scope: None }
    }
}

/// A never-cancelled, unscoped control, for the plain public search
/// entry points and tests.
pub(crate) fn plain_scan() -> ScanCtl<'static> {
    ScanCtl { cancel: &NEVER_CANCEL, scope: None }
}

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

/// How few literal hits it takes to fall back to the fuzzy pass.
///
/// The fuzzy pass is a *second* full scan of the arena, and it is not
/// cheap: unlike the literal pass's SIMD substring find, it UTF-8-decodes
/// every surviving name and runs two alignment passes over it. Measured
/// at ~20-24 ms against a 1.2M-entry index, versus ~4-7 ms for the
/// literal pass alone.
///
/// So the gate has to mean "the result list is *empty enough to be
/// useless*", not "the result list isn't completely full". Fifty rows is
/// already more than fits on screen; below that, a `dsr` →
/// `Design System Review.pdf` acronym hit is worth a second scan, and
/// above it the user has plenty to look at and the scan is pure latency.
///
/// See `docs/design-search-ranking.md` decision 2 and [`Self::finish`].
pub const FUZZY_GATE: usize = 50;

/// How a hit matched the query, in ranking order (lower is better).
///
/// `Fuzzy` is last on purpose: subsequence hits are *filler* below every
/// literal hit, never a way for a loose match to outrank a real one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MatchKind {
    Exact,
    Prefix,
    WordBoundary,
    Substring,
    Fuzzy,
}

/// A hit's ranking key — lower sorts better, field order *is* the
/// precedence (derived `Ord` is lexicographic).
///
/// `penalty` is [`crate::fuzzy`]'s match-quality score, and is always `0`
/// for the four literal kinds, so their relative order is exactly what it
/// was before fuzzy matching existed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Score {
    pub kind: MatchKind,
    pub penalty: u16,
    /// Folded-name length — the historical tiebreak, kept so hits from
    /// multiple indexes merge without re-deriving it.
    pub name_len: u16,
}

/// Widest `name_len` the packed key can hold (12 bits). Every filesystem
/// filex targets caps a single name far below this (255 bytes is the
/// usual limit), so the clamp is unreachable in practice — and it only
/// ever affects a tiebreak, never whether an entry matches.
const PACKED_NAME_LEN_MAX: u16 = 0x0FFF;

impl Score {
    /// A literal (non-fuzzy) hit, which carries no quality penalty.
    fn literal(kind: MatchKind, name_len: u16) -> Self {
        Self { kind, penalty: 0, name_len }
    }

    /// Pack into one `u32` whose natural ordering *is* the ranking
    /// order: `kind` in the top 4 bits, then `penalty`, then `name_len`.
    ///
    /// This exists for a measured reason. Carrying the fields as a
    /// struct made the top-K heap element 12 bytes instead of 8 and cost
    /// ~12% on every keystroke — a regression the design doc forbids on
    /// the literal path. Packed, the heap element is the same 8 bytes it
    /// was before fuzzy matching existed.
    fn pack(self) -> u32 {
        ((self.kind as u32) << 28)
            | ((self.penalty as u32) << 12)
            | self.name_len.min(PACKED_NAME_LEN_MAX) as u32
    }

    fn unpack(bits: u32) -> Self {
        let kind = match bits >> 28 {
            0 => MatchKind::Exact,
            1 => MatchKind::Prefix,
            2 => MatchKind::WordBoundary,
            3 => MatchKind::Substring,
            _ => MatchKind::Fuzzy,
        };
        Self {
            kind,
            penalty: ((bits >> 12) & 0xFFFF) as u16,
            name_len: (bits & PACKED_NAME_LEN_MAX as u32) as u16,
        }
    }
}

/// A 256-bit set of the byte values present in `bytes`, packed into four
/// `u64` lanes. Used by the fuzzy prefilter ([`mask_covers`]) as a cheap
/// necessary condition for a subsequence match.
fn byte_mask(bytes: &[u8]) -> [u64; 4] {
    let mut mask = [0u64; 4];
    for &b in bytes {
        mask[(b >> 6) as usize] |= 1u64 << (b & 63);
    }
    mask
}

/// Whether `name` contains **every** byte in `needle_mask` — the necessary
/// condition for `name` to contain the needle as a subsequence. Building
/// the name's own byte mask is one linear pass with no allocation; the
/// four-lane AND is a handful of instructions. A superset test on folded
/// bytes, so a genuine (case-insensitive, possibly multi-byte) match is
/// never rejected: if a needle char occurs in the match, all its bytes are
/// present in the name.
fn mask_covers(name: &[u8], needle_mask: &[u64; 4]) -> bool {
    let nm = byte_mask(name);
    needle_mask[0] & nm[0] == needle_mask[0]
        && needle_mask[1] & nm[1] == needle_mask[1]
        && needle_mask[2] & nm[2] == needle_mask[2]
        && needle_mask[3] & nm[3] == needle_mask[3]
}

/// Classify a literal (substring) match of `needle` in the folded
/// `haystack`, or `None` if there is none. One SIMD find plus a byte
/// compare — this runs per live entry on every keystroke, so it stays
/// allocation-free.
fn literal_kind(
    finder: &memmem::Finder<'_>,
    haystack: &[u8],
    needle: &[u8],
) -> Option<MatchKind> {
    let pos = finder.find(haystack)?;
    Some(if pos == 0 {
        if haystack.len() == needle.len() {
            MatchKind::Exact
        } else {
            MatchKind::Prefix
        }
    } else if !haystack[pos - 1].is_ascii_alphanumeric() {
        MatchKind::WordBoundary
    } else {
        MatchKind::Substring
    })
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
    /// `(packed score, entry index)` — 8 bytes, see [`Score::pack`].
    heap: std::collections::BinaryHeap<(u32, u32)>,
}

impl TopK {
    fn new(limit: usize) -> Self {
        Self { limit, heap: std::collections::BinaryHeap::with_capacity(limit + 1) }
    }

    fn push(mut self, item: (u32, u32)) -> Self {
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

    fn into_sorted(self) -> Vec<(u32, u32)> {
        self.heap.into_sorted_vec()
    }

    fn len(&self) -> usize {
        self.heap.len()
    }
}

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub id: EntryId,
    pub score: Score,
}

// `Clone` is what makes a published snapshot independent of the writer's
// mutable head (optimization B): the writer clones its head into a fresh
// `Arc` to publish, then keeps mutating its own copy. All fields are
// owned/clonable, so this is a straight deep copy.
#[derive(Debug, Clone)]
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

    /// Whether `entry` lies within `ancestor`'s subtree (inclusive) — the
    /// membership test behind the "Current Dir" search scope. Walks parent
    /// links, bounded like [`path_of`](Self::path_of) against a cyclic
    /// index. `ancestor == ROOT` is trivially true (the whole index).
    pub fn is_within(&self, entry: EntryId, ancestor: EntryId) -> bool {
        if ancestor == ROOT {
            return true;
        }
        let mut current = entry;
        for _ in 0..MAX_PATH_DEPTH {
            if current == ancestor {
                return true;
            }
            if current == ROOT {
                return false;
            }
            match self.entry(current) {
                Some(e) => current = e.parent,
                None => return false,
            }
        }
        false
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
        self.search_cancellable(query, limit, &plain_scan())
    }

    /// [`search`](Self::search), abortable mid-scan and optionally scoped
    /// to a subtree (see [`ScanCtl`]).
    ///
    /// `ctl.cancel` is polled once per candidate: when a newer keystroke
    /// sets it, every remaining closure cheaply returns `None`, so the
    /// rayon fold winds down to a near-no-op instead of scoring and
    /// ranking two million entries whose results the generation check
    /// will discard anyway. `ctl.scope`, when set, restricts hits to that
    /// directory's subtree — the "Current Dir" scope — checked only after
    /// the cheap name match so unmatched entries never pay for the
    /// parent-chain walk.
    pub fn search_cancellable(
        &self,
        query: &str,
        limit: usize,
        ctl: &ScanCtl,
    ) -> Vec<SearchHit> {
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
                if ctl.cancel.load(Ordering::Relaxed) {
                    return None;
                }
                let haystack = self.name_lower_bytes(entry.name_lower);
                let kind = literal_kind(&finder, haystack, needle.as_bytes())?;
                if let Some(scope) = ctl.scope
                    && !self.is_within(EntryId(ix as u32), scope)
                {
                    return None;
                }
                Some((Score::literal(kind, entry.name_lower.len).pack(), ix as u32))
            })
            .fold(|| TopK::new(limit), TopK::push)
            .reduce(|| TopK::new(limit), TopK::merge);

        self.finish(top, &needle, &finder, &[], limit, ctl)
    }

    /// Turn a literal pass's heap into hits, running the gated fuzzy pass
    /// first if the literal pass came up short. See
    /// `docs/design-search-ranking.md` decision 2: this is what keeps the
    /// common keystroke on exactly the pre-fuzzy code path.
    ///
    /// The gate is [`FUZZY_GATE`], **not** `limit`. `limit` is the wrong
    /// number twice over: callers reaching this through
    /// [`manager::search_all`](crate::index::manager::search_all) pass an
    /// overfetched `limit * OVERFETCH`, and even the un-multiplied display
    /// limit is in the hundreds — so gating on it ran the second scan for
    /// any query specific enough to be useful, which is the opposite of
    /// decision 2's intent ("only when the user is staring at an empty
    /// result list"). `min` with `limit` keeps the original property that a
    /// caller asking for fewer hits than the gate never pays for a pass it
    /// has no room to show.
    fn finish(
        &self,
        literal: TopK,
        needle: &str,
        finder: &memmem::Finder<'_>,
        filters: &[crate::search_filter::Filter],
        limit: usize,
        ctl: &ScanCtl,
    ) -> Vec<SearchHit> {
        let top = if literal.len() < limit.min(FUZZY_GATE) {
            literal.merge(self.fuzzy_pass(needle, finder, filters, limit, ctl))
        } else {
            literal
        };
        top.into_sorted()
            .into_iter()
            .map(|(bits, id)| SearchHit { id: EntryId(id), score: Score::unpack(bits) })
            .collect()
    }

    /// Subsequence-match entries the literal pass did *not* match, scored
    /// by [`crate::fuzzy`] and ranked strictly below every literal hit.
    ///
    /// Only reached when the literal pass returned fewer than `limit`
    /// hits — i.e. when the user is looking at a near-empty result list —
    /// so its cost never lands on a normal keystroke. Entries that
    /// already matched literally are skipped with the same SIMD find the
    /// literal pass used, so hits are not duplicated.
    fn fuzzy_pass(
        &self,
        needle: &str,
        finder: &memmem::Finder<'_>,
        filters: &[crate::search_filter::Filter],
        limit: usize,
        ctl: &ScanCtl,
    ) -> TopK {
        // Cheap necessary-condition prefilter (docs/design-search-ranking.md
        // "trigram bitmap in front of the scan"): a name can only contain
        // the needle as a subsequence if it contains *every byte* of the
        // needle. Testing that on the folded bytes — a couple of 64-bit AND
        // s, no UTF-8 decode, no alignment — rejects the vast majority of a
        // 2M-entry arena before the expensive `fuzzy::penalty`. It is a
        // superset test on folded bytes, so it never rejects a real match
        // (see `byte_mask`).
        let needle_mask = byte_mask(needle.as_bytes());
        self.entries
            .par_iter()
            .enumerate()
            .skip(1)
            .filter(|(_, e)| !e.is_tombstone())
            .filter_map(|(ix, entry)| {
                if ctl.cancel.load(Ordering::Relaxed) {
                    return None;
                }
                let lower = self.name_lower_bytes(entry.name_lower);
                if finder.find(lower).is_some() {
                    return None; // already a literal hit
                }
                // Prefilter before the costly decode + alignment below.
                if !mask_covers(lower, &needle_mask) {
                    return None;
                }
                // Fuzzy scoring reads the *original* name: camelCase
                // humps are one of its word-boundary signals, and the
                // folded copy has thrown that away.
                let name = std::str::from_utf8(self.name_bytes(entry.name)).ok()?;
                let penalty = crate::fuzzy::penalty(name, needle)?;
                if !filters.is_empty() && !self.passes_filters(entry, name, filters) {
                    return None;
                }
                if let Some(scope) = ctl.scope
                    && !self.is_within(EntryId(ix as u32), scope)
                {
                    return None;
                }
                let score =
                    Score { kind: MatchKind::Fuzzy, penalty, name_len: entry.name_lower.len };
                Some((score.pack(), ix as u32))
            })
            .fold(|| TopK::new(limit), TopK::push)
            .reduce(|| TopK::new(limit), TopK::merge)
    }

    /// Does `entry` satisfy every index-evaluable filter?
    fn passes_filters(
        &self,
        entry: &FileEntry,
        name: &str,
        filters: &[crate::search_filter::Filter],
    ) -> bool {
        let has_meta = entry.has_meta();
        let item = crate::search_filter::ItemMeta {
            name,
            is_dir: entry.is_dir(),
            // Size is meaningless for directories; mtime isn't.
            size: (has_meta && !entry.is_dir()).then_some(entry.size),
            mtime: has_meta.then_some(entry.mtime),
        };
        filters.iter().all(|f| f.matches(&item))
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
        self.search_filtered_cancellable(query, filters, limit, &plain_scan())
    }

    /// [`search_filtered`](Self::search_filtered), abortable mid-scan and
    /// optionally subtree-scoped — see [`ScanCtl`].
    pub fn search_filtered_cancellable(
        &self,
        query: &str,
        filters: &[crate::search_filter::Filter],
        limit: usize,
        ctl: &ScanCtl,
    ) -> Vec<SearchHit> {
        if limit == 0 {
            return Vec::new();
        }
        if filters.is_empty() {
            return self.search_cancellable(query, limit, ctl);
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
                if ctl.cancel.load(Ordering::Relaxed) {
                    return None;
                }
                // Name-match first (cheapest cut) when there's a needle;
                // filter-only queries rank every survivor by name length.
                let kind = match (&finder, &needle) {
                    (Some(finder), Some(needle)) => {
                        let haystack = self.name_lower_bytes(entry.name_lower);
                        literal_kind(finder, haystack, needle.as_bytes())?
                    }
                    _ => MatchKind::Substring,
                };
                let name = std::str::from_utf8(self.name_bytes(entry.name)).ok()?;
                if !self.passes_filters(entry, name, filters) {
                    return None;
                }
                if let Some(scope) = ctl.scope
                    && !self.is_within(EntryId(ix as u32), scope)
                {
                    return None;
                }
                Some((Score::literal(kind, entry.name_lower.len).pack(), ix as u32))
            })
            .fold(|| TopK::new(limit), TopK::push)
            .reduce(|| TopK::new(limit), TopK::merge);

        match (&needle, &finder) {
            (Some(needle), Some(finder)) => {
                self.finish(top, needle, finder, filters, limit, ctl)
            }
            // Filter-only: there is no needle to fuzzy-match against.
            _ => top
                .into_sorted()
                .into_iter()
                .map(|(bits, id)| SearchHit { id: EntryId(id), score: Score::unpack(bits) })
                .collect(),
        }
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
            let idx = index.load();
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
            let mut idx = index.write();
            for (id, name, size, mtime) in updates {
                idx.backfill_meta(id, &name, size, mtime);
            }
        }
        // Publish the backfilled metadata so searches with `size:`/
        // `modified:` filters see it. The backfill is already paced (a
        // batch every ~10 ms+), so this publish rate is naturally bounded.
        index.publish();
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
            // The writer has joined (its senders are dropped), so the head
            // is quiescent and the snapshot reflects every applied event.
            let index = self.index.load();
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

    let shared = watcher::SharedIndex::new(index);
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

        let shared = watcher::SharedIndex::new(index);
        let backfiller = MetaBackfiller::spawn(shared.clone()).unwrap();

        // Bounded wait for the background pass to fill size/mtime.
        let mut names: Vec<String> = Vec::new();
        for _ in 0..40 {
            std::thread::sleep(std::time::Duration::from_millis(50));
            let idx = shared.load();
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
    fn packed_score_round_trips_and_preserves_ranking_order() {
        let scores = [
            Score { kind: MatchKind::Exact, penalty: 0, name_len: 4 },
            Score { kind: MatchKind::Prefix, penalty: 0, name_len: 0 },
            Score { kind: MatchKind::WordBoundary, penalty: 0, name_len: 9 },
            Score { kind: MatchKind::Substring, penalty: 0, name_len: 1 },
            Score { kind: MatchKind::Fuzzy, penalty: 3, name_len: 2 },
            Score { kind: MatchKind::Fuzzy, penalty: u16::MAX, name_len: 0 },
        ];
        for score in scores {
            assert_eq!(Score::unpack(score.pack()), score, "round trip");
        }
        // The packed ordering must match the struct ordering exactly —
        // that equivalence is what lets the heap sort on the u32.
        for a in scores {
            for b in scores {
                assert_eq!(
                    a.pack().cmp(&b.pack()),
                    a.cmp(&b),
                    "packed order disagrees for {a:?} vs {b:?}"
                );
            }
        }
    }

    #[test]
    fn packed_score_clamps_an_absurd_name_length() {
        let score = Score { kind: MatchKind::Exact, penalty: 0, name_len: u16::MAX };
        // Clamped, not wrapped — a long name must not alias into the
        // penalty field and score as a better match.
        let unpacked = Score::unpack(score.pack());
        assert_eq!(unpacked.name_len, PACKED_NAME_LEN_MAX);
        assert_eq!(unpacked.kind, MatchKind::Exact);
        assert_eq!(unpacked.penalty, 0);
    }

    #[test]
    fn an_already_cancelled_search_returns_nothing() {
        // A scan whose flag is set before it starts must produce no hits —
        // every candidate's closure short-circuits. This is the mechanism
        // that stops a superseded per-keystroke search from spending CPU
        // and holding the read lock; a stale generation would discard its
        // results anyway, so returning them is pure waste.
        let mut index = VolumeIndex::new("/vol");
        for i in 0..1000 {
            index.insert(ROOT, &format!("report_{i:04}.txt"), false).unwrap();
        }
        let flag = AtomicBool::new(true);
        let cancelled = ScanCtl::new(&flag);

        // Both scan paths (no-filter and filtered) honour the flag.
        assert!(index.search_cancellable("report", 500, &cancelled).is_empty());
        assert!(
            index
                .search_filtered_cancellable(
                    "report",
                    &[crate::search_filter::Filter::Ext("txt".into())],
                    500,
                    &cancelled,
                )
                .is_empty()
        );

        // And the fuzzy fallback path: a needle with no literal hits would
        // normally trigger the second scan — cancelled, it too yields none.
        assert!(index.search_cancellable("xqz", 500, &cancelled).is_empty());

        // Sanity: the same queries with a live (false) flag do find things,
        // so the emptiness above is the cancel, not a broken fixture.
        let live_flag = AtomicBool::new(false);
        let live = ScanCtl::new(&live_flag);
        assert!(!index.search_cancellable("report", 500, &live).is_empty());
    }

    #[test]
    fn a_scoped_search_only_returns_hits_within_the_directory() {
        // "Current Dir": resolve a subfolder to its entry and scope to it.
        let mut index = VolumeIndex::new("/vol");
        let inside = index.insert(ROOT, "project", true).unwrap();
        index.insert(inside, "report.txt", false).unwrap();
        let elsewhere = index.insert(ROOT, "other", true).unwrap();
        index.insert(elsewhere, "report.txt", false).unwrap();

        let flag = AtomicBool::new(false);
        let scoped = ScanCtl { cancel: &flag, scope: Some(inside) };

        let hits = index.search_cancellable("report", 500, &scoped);
        assert_eq!(hits.len(), 1, "only the report under `project`");
        assert_eq!(index.path_of(hits[0].id).unwrap(), PathBuf::from("/vol/project/report.txt"));

        // Unscoped finds both.
        assert_eq!(index.search("report", 500).len(), 2);
    }

    #[test]
    fn search_falls_back_to_fuzzy_when_nothing_matches_literally() {
        let mut index = VolumeIndex::new("/vol");
        let acronym = index.insert(ROOT, "Design System Review.pdf", false).unwrap();
        index.insert(ROOT, "unrelated.txt", false).unwrap();

        let hits = index.search("dsr", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, acronym);
        assert_eq!(hits[0].score.kind, MatchKind::Fuzzy);
    }

    #[test]
    fn the_fuzzy_prefilter_never_rejects_a_real_match() {
        // The prefilter is a necessary-condition cut; it must not drop a
        // genuine subsequence hit — including case-insensitively and across
        // word boundaries (the acronym case).
        let cases: &[(&str, &str)] = &[
            ("Design System Review.pdf", "dsr"),
            ("MyCamelCaseFile.rs", "mccf"),
            ("UPPER_snake.TXT", "usnake"),  // mixed case survives folding
            ("a-b-c-d.log", "abcd"),
        ];
        for (name, needle) in cases {
            let mut index = VolumeIndex::new("/vol");
            index.insert(ROOT, name, false).unwrap();
            assert_eq!(
                index.search(needle, 10).len(),
                1,
                "prefilter dropped {needle:?} against {name:?}"
            );
        }
    }

    #[test]
    fn byte_mask_prefilter_covers_and_rejects_correctly() {
        let needle = byte_mask(b"abc");
        assert!(mask_covers(b"xaybzc", &needle), "all three present (scattered)");
        assert!(mask_covers(b"cba", &needle), "order irrelevant to the prefilter");
        assert!(!mask_covers(b"ab", &needle), "missing 'c' -> rejected");
        assert!(mask_covers(b"anything", &byte_mask(b"")), "empty needle covers all");
    }

    #[test]
    fn fuzzy_hits_rank_below_every_literal_hit() {
        let mut index = VolumeIndex::new("/vol");
        let fuzzy = index.insert(ROOT, "Design System Review.pdf", false).unwrap();
        let literal = index.insert(ROOT, "zzz_dsr_zzz.txt", false).unwrap();

        let ids: Vec<EntryId> = index.search("dsr", 10).into_iter().map(|h| h.id).collect();
        assert_eq!(ids, vec![literal, fuzzy]);
    }

    /// Regression: the fuzzy gate was `literal.len() < limit`, and
    /// [`manager::search_all`] hands `finish` an *overfetched*
    /// `limit * OVERFETCH` (2000 for the UI's 500, 4004 for a Magic
    /// command). So any query with fewer than 2000 literal hits — i.e.
    /// every query specific enough to be worth typing — paid for a second
    /// full scan of the arena on every keystroke, ~20-24 ms against a
    /// 1.2M-entry index versus ~4-7 ms for the literal pass alone.
    ///
    /// The two tests below pin both sides of [`FUZZY_GATE`], because the
    /// bug is only visible as a boundary: the old code passed every test
    /// that used a small `limit`.
    #[test]
    fn overfetching_does_not_widen_the_fuzzy_gate() {
        let mut index = VolumeIndex::new("/vol");
        for i in 0..FUZZY_GATE {
            index.insert(ROOT, &format!("dsr_{i}.txt"), false).unwrap();
        }
        index.insert(ROOT, "Design System Review.pdf", false).unwrap();

        // Room for far more hits than exist — the shape `search_all` asks
        // for once OVERFETCH has multiplied the display limit.
        let hits = index.search("dsr", 2000);

        assert_eq!(hits.len(), FUZZY_GATE, "only the literal hits");
        assert!(
            hits.iter().all(|h| h.score.kind != MatchKind::Fuzzy),
            "{FUZZY_GATE} literal hits is already more than fills a screen; \
             the fuzzy pass must not run just because the caller overfetched"
        );
    }

    #[test]
    fn the_fuzzy_pass_still_runs_when_the_result_list_is_nearly_empty() {
        // The other side of the boundary: one hit fewer than the gate, and
        // the acronym fallback must still be there. This is the recall the
        // gate is spending latency to buy, so it needs its own guard.
        let mut index = VolumeIndex::new("/vol");
        for i in 0..FUZZY_GATE - 1 {
            index.insert(ROOT, &format!("dsr_{i}.txt"), false).unwrap();
        }
        let acronym = index.insert(ROOT, "Design System Review.pdf", false).unwrap();

        let hits = index.search("dsr", 2000);

        assert_eq!(hits.len(), FUZZY_GATE, "the literal hits plus the acronym");
        let fuzzy: Vec<EntryId> = hits
            .iter()
            .filter(|h| h.score.kind == MatchKind::Fuzzy)
            .map(|h| h.id)
            .collect();
        assert_eq!(fuzzy, vec![acronym], "below the gate the fallback still runs");
    }

    #[test]
    fn fuzzy_pass_does_not_run_when_literal_hits_fill_the_limit() {
        // The gate from docs/design-search-ranking.md decision 2: with
        // enough literal hits the second pass never runs, so a fuzzy-only
        // candidate cannot appear.
        let mut index = VolumeIndex::new("/vol");
        for i in 0..5 {
            index.insert(ROOT, &format!("dsr_{i}.txt"), false).unwrap();
        }
        index.insert(ROOT, "Design System Review.pdf", false).unwrap();

        let hits = index.search("dsr", 3);
        assert_eq!(hits.len(), 3);
        assert!(
            hits.iter().all(|h| h.score.kind != MatchKind::Fuzzy),
            "literal hits filled the limit, so no fuzzy pass"
        );
    }

    #[test]
    fn fuzzy_hits_are_not_duplicated_by_the_second_pass() {
        // A literal match is also a subsequence match; the fuzzy pass
        // must skip entries the literal pass already claimed.
        let mut index = VolumeIndex::new("/vol");
        index.insert(ROOT, "dsr.txt", false).unwrap();
        index.insert(ROOT, "Design System Review.pdf", false).unwrap();

        let hits = index.search("dsr", 10);
        let ids: std::collections::HashSet<EntryId> = hits.iter().map(|h| h.id).collect();
        assert_eq!(ids.len(), hits.len(), "no entry appears twice");
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn fuzzy_fallback_respects_filters() {
        let mut index = VolumeIndex::new("/vol");
        index.insert(ROOT, "Design System Review.pdf", false).unwrap();
        let dir = index.insert(ROOT, "Design System Review", true).unwrap();

        let filters =
            vec![crate::search_filter::Filter::Kind(crate::listing::FileKind::Directory)];
        let hits = index.search_filtered("dsr", &filters, 10);
        assert_eq!(hits.len(), 1, "the .pdf must be filtered out of the fuzzy pass");
        assert_eq!(hits[0].id, dir);
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
            let index = live.index.load();
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
                let index = live.index.load();
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
                let index = live.index.load();
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

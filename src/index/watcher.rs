//! OS-agnostic live-update pipeline (docs/indexing-architecture.md §2).
//!
//! Platform watchers normalize native events into [`FsDelta`]s and send
//! batches over a channel. A single writer thread ([`IndexWriter`]) drains
//! the channel, applies deltas under a short write lock, and fires a
//! change notification. The UI never touches this machinery directly.

use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Result, bail};
use arc_swap::ArcSwap;

use super::walker::index_subtree;
use super::{EntryId, ROOT, VolumeIndex};

/// Lock-free-read handle to a live index (optimization B,
/// docs/indexing-architecture.md §3).
///
/// Readers [`load`](SharedIndex::load) an immutable `Arc<VolumeIndex>` and
/// scan it with **no lock at all**, so writer and search can never stall
/// each other. Writers serialize on a `Mutex` readers never touch: each
/// mutates a private head via [`write`](SharedIndex::write) and
/// republishes on its own cadence via [`publish`](SharedIndex::publish).
/// Splitting the two lets a burst of tiny batches update the head cheaply
/// while the cloning publish happens rarely.
#[derive(Clone)]
pub struct SharedIndex(Arc<IndexCell>);

struct IndexCell {
    /// What readers load — replaced wholesale on publish, lock-free.
    snapshot: ArcSwap<VolumeIndex>,
    /// The writers' working copy. Guarded by a `Mutex` that is only ever
    /// taken by the (background) writer threads, never by a reader.
    head: Mutex<Arc<VolumeIndex>>,
}

impl SharedIndex {
    /// Wrap an index; its initial state is immediately loadable.
    pub fn new(index: VolumeIndex) -> Self {
        let arc = Arc::new(index);
        Self(Arc::new(IndexCell {
            snapshot: ArcSwap::new(arc.clone()),
            head: Mutex::new(arc),
        }))
    }

    /// A lock-free, point-in-time snapshot to read or search. The returned
    /// guard derefs to `&VolumeIndex`; hold it only as long as the scan.
    pub fn load(&self) -> arc_swap::Guard<Arc<VolumeIndex>> {
        self.0.snapshot.load()
    }

    /// Like [`load`](Self::load) but returns an owned `Arc` — which, unlike
    /// the load guard, is `Send`, so it can cross into a worker pool (the
    /// dedicated search rayon pool). One atomic refcount bump.
    pub fn load_full(&self) -> Arc<VolumeIndex> {
        self.0.snapshot.load_full()
    }

    /// Exclusive in-place write access to the head. Does **not** publish —
    /// call [`publish`](Self::publish) for that. The head stays uniquely
    /// owned (publish clones into a separate Arc), so this never clones.
    pub fn write(&self) -> IndexWrite<'_> {
        let mut head = self.0.head.lock().unwrap_or_else(PoisonError::into_inner);
        // No-op unless a `replace` just shared the Arc; then it clones once.
        Arc::make_mut(&mut head);
        IndexWrite { head }
    }

    /// Publish the current head as the snapshot readers load. This deep-
    /// clones the head — the price of an immutable snapshot — so callers
    /// publish on a bounded cadence (see the writer loop), not per batch.
    pub fn publish(&self) {
        let head = self.0.head.lock().unwrap_or_else(PoisonError::into_inner);
        self.0.snapshot.store(Arc::new((**head).clone()));
    }

    /// Replace the whole index (compaction, root rescan) and publish it in
    /// one step. Head and snapshot briefly share the Arc; the next
    /// [`write`](Self::write) re-uniques it.
    pub fn replace(&self, index: VolumeIndex) {
        let arc = Arc::new(index);
        let mut head = self.0.head.lock().unwrap_or_else(PoisonError::into_inner);
        *head = arc.clone();
        self.0.snapshot.store(arc);
    }
}

/// Exclusive write guard over the index head; see [`SharedIndex::write`].
pub struct IndexWrite<'a> {
    head: MutexGuard<'a, Arc<VolumeIndex>>,
}

impl std::ops::Deref for IndexWrite<'_> {
    type Target = VolumeIndex;
    fn deref(&self) -> &VolumeIndex {
        &self.head
    }
}

impl std::ops::DerefMut for IndexWrite<'_> {
    fn deref_mut(&mut self) -> &mut VolumeIndex {
        // `write()` made the head unique, so this cannot fail.
        Arc::get_mut(&mut self.head).expect("index head is uniquely owned during a write")
    }
}

/// A normalized filesystem change. Paths are absolute; anything outside the
/// index root is ignored at application time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsDelta {
    /// Something now exists at `path` (created, moved in, or type unknown
    /// but present). For directories this re-indexes the whole subtree,
    /// which also covers wholesale directory moves.
    Upsert { path: PathBuf, is_dir: bool },
    /// Nothing exists at `path` anymore (deleted or moved out).
    Remove { path: PathBuf },
    /// The watcher lost precision (event coalescing / queue overflow);
    /// reconcile the subtree at `path` against the real filesystem.
    Rescan { path: PathBuf },
    /// Journal-sourced upsert by platform-native key (NTFS FRN): `key` now
    /// exists under `parent_key` with `name`. Covers creates and moves —
    /// the entry is re-pointed and descendants follow via parent links. An
    /// unknown `parent_key` means the event is outside the indexed
    /// subtree, so it is dropped.
    NativeUpsert {
        key: u64,
        parent_key: u64,
        name: String,
        is_dir: bool,
    },
    /// Journal-sourced removal by native key. Unknown keys are no-ops
    /// (deletes outside the subtree, or replays of applied events).
    NativeRemove { key: u64 },
    /// Writer control marker (never produced by watchers): save a snapshot
    /// with this checkpoint once every delta queued *before* it has been
    /// applied. Watchers advance their checkpoint atomics before sending,
    /// so everything the checkpoint covers precedes the marker.
    PersistNow {
        checkpoint: super::persist::Checkpoint,
    },
}

/// Where a file's size/mtime comes from when [`apply`] upserts it — the
/// seam for optimization A (`docs/indexing-architecture.md` §3). The
/// writer pre-fetches a whole batch's metadata *off* the write lock
/// ([`MetaSource::prefetch`]), so a burst holds the lock only for
/// in-memory mutation rather than one `stat` per file, which serialized
/// hundreds of syscalls under it and stalled every search.
/// [`MetaSource::Inline`] keeps stat-on-demand for tests.
pub enum MetaSource {
    /// Stat each path on demand, under whatever lock the caller holds.
    Inline,
    /// Size/mtime gathered ahead of time, off the lock. A path mapping to
    /// `None` was unreadable/vanished when prefetched — treated exactly
    /// like a failed inline stat (left as-is).
    Prefetched(std::collections::HashMap<PathBuf, Option<(u64, i64)>>),
}

impl MetaSource {
    /// Gather size/mtime for every file upsert in `batch`, off the lock.
    /// Directories aren't prefetched: after the known-directory no-op in
    /// [`upsert`] they walk only for genuinely new subtrees, which is
    /// rare. A file `stat` per change is the hot path this targets.
    pub fn prefetch(batch: &[FsDelta]) -> Self {
        let mut map = std::collections::HashMap::new();
        for delta in batch {
            if let FsDelta::Upsert {
                path,
                is_dir: false,
            } = delta
            {
                map.entry(path.clone()).or_insert_with(|| stat_meta(path));
            }
        }
        Self::Prefetched(map)
    }

    fn meta_for(&self, path: &Path) -> Option<(u64, i64)> {
        match self {
            Self::Inline => stat_meta(path),
            Self::Prefetched(map) => map.get(path).copied().flatten(),
        }
    }
}

/// One `symlink_metadata` reduced to the two fields the index keeps.
fn stat_meta(path: &Path) -> Option<(u64, i64)> {
    std::fs::symlink_metadata(path)
        .ok()
        .map(|meta| (meta.len(), super::mtime_secs(&meta)))
}

/// Apply one delta to the index, stat-ing inline. Idempotent — replaying
/// an event already reflected (an Upsert raced by the bootstrap walk) is a
/// no-op, so watchers may start before bootstrap.
pub fn apply(index: &mut VolumeIndex, delta: &FsDelta) -> Result<()> {
    apply_prepared(index, delta, &MetaSource::Inline)
}

/// [`apply`], but file metadata comes from `meta` — see [`MetaSource`].
pub fn apply_prepared(index: &mut VolumeIndex, delta: &FsDelta, meta: &MetaSource) -> Result<()> {
    match delta {
        FsDelta::Upsert { path, is_dir } => upsert(index, path, *is_dir, meta),
        FsDelta::Remove { path } => remove(index, path),
        FsDelta::Rescan { path } => rescan(index, path, meta),
        FsDelta::NativeUpsert {
            key,
            parent_key,
            name,
            is_dir,
        } => native_upsert(index, *key, *parent_key, name, *is_dir),
        FsDelta::NativeRemove { key } => match index.entry_by_native_key(*key) {
            Some(id) => index.remove(id),
            None => Ok(()), // outside the subtree, or already applied
        },
        FsDelta::PersistNow { .. } => Ok(()), // handled by the writer loop
    }
}

fn native_upsert(
    index: &mut VolumeIndex,
    key: u64,
    parent_key: u64,
    name: &str,
    is_dir: bool,
) -> Result<()> {
    let parent = index.entry_by_native_key(parent_key);
    // A top-level OS/system dir (parent is the volume root): never insert it
    // when exclusion is on, so its whole subtree stays out — every child's
    // parent FRN then resolves to nothing and is dropped below.
    if index.exclude_system_dirs() && is_dir && parent == Some(ROOT) && super::is_system_top(name) {
        return Ok(());
    }
    let existing = index.entry_by_native_key(key);
    match (existing, parent) {
        // A move out of the indexed subtree: the entry leaves the index.
        (Some(id), None) => index.remove(id),
        // Rename/move within the subtree: re-point the entry, children
        // follow via parent links. A type flip means the FRN was recycled
        // mid-window, so rebuild instead. Either way refresh size/mtime —
        // a USN data-change reason also arrives as a NativeUpsert.
        (Some(id), Some(parent)) => {
            if index.is_dir(id) == Some(is_dir) {
                index.rename(id, parent, name)?;
                if !is_dir {
                    populate_meta_by_id(index, id);
                }
                Ok(())
            } else {
                index.remove(id)?;
                let new_id = index.insert_with_key(parent, name, is_dir, key)?;
                if !is_dir {
                    populate_meta_by_id(index, new_id);
                }
                Ok(())
            }
        }
        (None, Some(parent)) => {
            let id = index.insert_with_key(parent, name, is_dir, key)?;
            if !is_dir {
                populate_meta_by_id(index, id);
            }
            Ok(())
        }
        // Entirely outside the indexed subtree.
        (None, None) => Ok(()),
    }
}

fn relative_to_root(index: &VolumeIndex, path: &Path) -> Option<PathBuf> {
    path.strip_prefix(index.root_path())
        .ok()
        .map(Path::to_path_buf)
}

fn upsert(index: &mut VolumeIndex, path: &Path, is_dir: bool, meta: &MetaSource) -> Result<()> {
    // Excluded OS/system dir (C:\Windows, …): a create/modify event here is
    // a no-op, so the subtree the bootstrap skipped can't creep back in via
    // `ensure_dirs` vivifying a parent chain.
    if index.excludes_path(path) {
        return Ok(());
    }
    let Some(rel) = relative_to_root(index, path) else {
        return Ok(()); // outside the indexed root
    };
    let Some(name) = rel.file_name().and_then(|n| n.to_str()) else {
        return Ok(()); // event for the root itself, or non-UTF-8 name
    };
    let parent_rel = rel.parent().unwrap_or(Path::new(""));
    let parent = ensure_dirs(index, parent_rel)?;

    if let Some(existing) = index.resolve_child(parent, name) {
        if index.is_dir(existing) == Some(is_dir) {
            if !is_dir {
                // File already known and unchanged in name, but a modify
                // event lands here to refresh size/mtime — an in-place edit
                // changes neither name nor existence, so this is the only
                // signal `size:`/`modified:` get.
                populate_meta(index, existing, path, meta);
            }
            // A known directory is left alone, not dropped and re-walked.
            // Watchers report a directory whenever its contents change, so
            // re-walking meant every write anywhere triggered a recursive
            // `jwalk` of the parent subtree plus tombstoning everything
            // under it — 502 mutations per event on an unchanged 500-file
            // directory, pinning a core and growing the arena unbounded.
            //
            // Nothing is lost: contents arrive as their own per-file
            // events, and coalesced/dropped events are exactly what
            // `FsDelta::Rescan` exists for.
            return Ok(());
        }
        // A file/dir type flip: drop the stale entry (and any subtree under
        // it) and re-index from the filesystem below.
        index.remove(existing)?;
    }

    let id = index.insert(parent, name, is_dir)?;
    if is_dir {
        index_subtree(index, id, path, false, None)?;
    } else {
        // A freshly-created file gets its metadata now rather than waiting
        // for the background backfill.
        populate_meta(index, id, path, meta);
    }
    Ok(())
}

/// Record the entry's size/mtime from `meta` (prefetched off the lock, or
/// stat-ed inline). Best-effort — a vanished/unreadable file is left
/// as-is.
fn populate_meta(index: &mut VolumeIndex, id: EntryId, path: &Path, meta: &MetaSource) {
    if let Some((size, mtime)) = meta.meta_for(path) {
        index.set_meta(id, size, mtime);
    }
}

/// [`populate_meta`] for a native-key delta, which knows the entry but not
/// its path — reconstruct the path from the index and stat it inline.
/// (Windows USN path; not part of the batch prefetch, which keys on path.)
fn populate_meta_by_id(index: &mut VolumeIndex, id: EntryId) {
    if let Some(path) = index.path_of(id) {
        populate_meta(index, id, &path, &MetaSource::Inline);
    }
}

fn remove(index: &mut VolumeIndex, path: &Path) -> Result<()> {
    let Some(rel) = relative_to_root(index, path) else {
        return Ok(());
    };
    match index.resolve(&rel) {
        Some(id) if id != ROOT => index.remove(id),
        _ => Ok(()), // never indexed (or the root): nothing to do
    }
}

/// Reconcile a subtree against the real filesystem: drop what the index
/// believes and re-walk. Correctness-first; incremental diffing can come
/// later if rescans show up in profiles.
fn rescan(index: &mut VolumeIndex, path: &Path, meta: &MetaSource) -> Result<()> {
    if index.excludes_path(path) {
        return Ok(()); // excluded OS/system subtree: nothing to reconcile
    }
    let Some(rel) = relative_to_root(index, path) else {
        return Ok(());
    };
    if !std::fs::metadata(path).is_ok_and(|m| m.is_dir()) {
        // Gone (or not a directory): treat as a removal of whatever we had.
        return remove(index, path);
    }
    match index.resolve(&rel) {
        Some(id) if id == ROOT => {
            for child in index.children_of(ROOT).collect::<Vec<_>>() {
                index.remove(child)?;
            }
            index_subtree(index, ROOT, path, false, None)
        }
        // Drop what the index believes so the `upsert` below takes its
        // insert-and-walk path. Load-bearing: `upsert` leaves a known
        // directory untouched (the per-event hot path), so without this a
        // rescan of a known subtree would reconcile nothing. The expensive
        // rebuild belongs here — `Rescan` is the rare "lost precision,
        // resync from disk" signal.
        Some(id) => {
            index.remove(id)?;
            upsert(index, path, true, meta)
        }
        None => upsert(index, path, true, meta),
    }
}

/// Resolve `rel` under the root, creating missing intermediate directory
/// entries (events arrive for paths deeper than anything indexed yet). A
/// file where a directory is needed is replaced — the events say the
/// filesystem disagrees with the index.
fn ensure_dirs(index: &mut VolumeIndex, rel: &Path) -> Result<EntryId> {
    let mut current = ROOT;
    for component in rel.components() {
        let Component::Normal(os_name) = component else {
            bail!(
                "unexpected path component {component:?} in {}",
                rel.display()
            );
        };
        let Some(name) = os_name.to_str() else {
            bail!("non-UTF-8 path component in {}", rel.display());
        };
        current = match index.resolve_child(current, name) {
            Some(id) if index.is_dir(id) == Some(true) => id,
            Some(stale_file) => {
                index.remove(stale_file)?;
                index.insert(current, name, true)?
            }
            None => index.insert(current, name, true)?,
        };
    }
    Ok(current)
}

/// Owns the thread that applies deltas to the shared index. It exits when
/// every sender is dropped, and Drop *joins* it — so dropping watchers
/// then writer guarantees every delta sent has been applied, which is what
/// makes a checkpoint-then-save shutdown lose no events.
pub struct IndexWriter {
    handle: Option<JoinHandle<()>>,
}

/// Called by the writer thread (under a read lock) when a PersistNow
/// marker is processed — every delta enqueued before the marker has been
/// applied at that point.
pub type SaveHook = Box<dyn Fn(&VolumeIndex, super::persist::Checkpoint) + Send>;

/// How long the writer waits for more deltas before applying a batch —
/// coalesces event bursts (large copies, builds) into one lock + one
/// notification. Adds at most this much staleness, never search latency.
const BATCH_WINDOW: Duration = Duration::from_millis(30);
const MAX_BATCH: usize = 10_000;

/// Minimum gap between snapshot publishes (optimization B). Publishing
/// deep-clones the head, so under churn the writer coalesces many batches
/// into one publish per interval, bounding clone cost to
/// ~`1000/PUBLISH_INTERVAL_MS` per second; a quiescent tick flushes once
/// churn stops. Readers are at most this stale on top of `BATCH_WINDOW`.
const PUBLISH_INTERVAL: Duration = Duration::from_millis(150);

impl IndexWriter {
    pub fn spawn(
        index: SharedIndex,
        deltas: mpsc::Receiver<Vec<FsDelta>>,
        on_change: impl Fn() + Send + 'static,
        save_hook: Option<SaveHook>,
    ) -> Result<Self> {
        let handle = std::thread::Builder::new()
            .name("filex-index-writer".into())
            .spawn(move || {
                let root = index.load().root_path().to_path_buf();
                // Optimization B: apply mutates the private head cheaply,
                // while publishing *clones* it — so publish is rate-limited
                // to once per `PUBLISH_INTERVAL`. `dirty` tracks unpublished
                // changes; a quiescent tick flushes them so readers converge
                // shortly after churn stops.
                let mut last_publish = std::time::Instant::now();
                let mut dirty = false;
                loop {
                    let first = match deltas.recv_timeout(PUBLISH_INTERVAL) {
                        Ok(batch) => batch,
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            if dirty {
                                index.publish();
                                dirty = false;
                                last_publish = std::time::Instant::now();
                                on_change();
                            }
                            continue;
                        }
                        // Every sender dropped: drain done. Flush deferred
                        // changes so the final snapshot — which the shutdown
                        // save reads — reflects every applied event.
                        Err(mpsc::RecvTimeoutError::Disconnected) => {
                            if dirty {
                                index.publish();
                                on_change();
                            }
                            break;
                        }
                    };
                    let mut batch = first;
                    while batch.len() < MAX_BATCH {
                        match deltas.recv_timeout(BATCH_WINDOW) {
                            Ok(more) => batch.extend(more),
                            Err(_) => break,
                        }
                    }

                    // A whole-root rescan is rebuilt off to the side then
                    // swapped in via `replace`, so readers keep answering
                    // from the old snapshot during the walk. The rest of the
                    // batch is dropped — the fresh walk already covers it.
                    let root_rescan = batch
                        .iter()
                        .any(|d| matches!(d, FsDelta::Rescan { path } if *path == root));
                    if root_rescan {
                        use super::walker::IndexSource as _;
                        match super::walker::FsWalkSource::default().bootstrap(&root) {
                            Ok(rebuilt) => {
                                index.replace(rebuilt);
                                dirty = false;
                                last_publish = std::time::Instant::now();
                                on_change();
                            }
                            Err(err) => {
                                tracing::warn!("root rescan of {} failed: {err:#}", root.display());
                            }
                        }
                        continue;
                    }

                    // Save with the *last* PersistNow marker in the batch,
                    // after applying everything else — deltas past the marker
                    // just mean the next replay re-applies a few idempotently.
                    let mut pending_save = None;
                    let mut applied_any = false;
                    // Optimization A: stat every file upsert in the batch
                    // *before* taking the write lock, so the head is held for
                    // in-memory mutation only (see `MetaSource`).
                    let prep_started = std::time::Instant::now();
                    let meta = MetaSource::prefetch(&batch);
                    let prefetch_ms = prep_started.elapsed().as_millis() as u64;
                    let hold_started = std::time::Instant::now();
                    let (before, after);
                    {
                        let mut index = index.write();
                        before = index.generation();
                        for delta in &batch {
                            if let FsDelta::PersistNow { checkpoint } = delta {
                                pending_save = Some(*checkpoint);
                                continue;
                            }
                            match apply_prepared(&mut index, delta, &meta) {
                                Ok(()) => applied_any = true,
                                Err(err) => {
                                    tracing::warn!("failed to apply {delta:?}: {err:#}");
                                }
                            }
                        }
                        after = index.generation();
                    } // writer Mutex released; readers were never blocked by it
                    let hold_ms = hold_started.elapsed().as_millis() as u64;
                    let mutations = after - before;
                    dirty |= applied_any;
                    // `mutations` far exceeding `deltas` means events are
                    // triggering subtree rebuilds rather than point updates.
                    // `hold_ms` is pure in-memory mutation now, so warn only
                    // if it is surprisingly long.
                    if hold_ms >= 200 {
                        tracing::warn!(
                            deltas = batch.len(),
                            mutations,
                            prefetch_ms,
                            hold_ms,
                            "slow delta apply"
                        );
                    } else {
                        tracing::debug!(
                            deltas = batch.len(),
                            mutations,
                            prefetch_ms,
                            hold_ms,
                            "applied delta batch"
                        );
                    }

                    // Publish on a bounded cadence; a pending save forces it
                    // so the persisted snapshot matches the checkpoint.
                    let force = pending_save.is_some();
                    if dirty && (force || last_publish.elapsed() >= PUBLISH_INTERVAL) {
                        index.publish();
                        dirty = false;
                        last_publish = std::time::Instant::now();
                        on_change();
                    }

                    if let (Some(checkpoint), Some(hook)) = (pending_save, &save_hook) {
                        // `force` published above, so the snapshot now equals
                        // the head — persist it.
                        hook(&index.load(), checkpoint);
                    }

                    // Compact when enough of the arena is dead. Read the head
                    // (the authoritative copy), build a fresh index, swap it
                    // in via `replace`.
                    let compacted = {
                        let head = index.write();
                        head.needs_compaction().then(|| head.compacted())
                    };
                    if let Some(fresh) = compacted {
                        index.replace(fresh);
                        dirty = false;
                        last_publish = std::time::Instant::now();
                        on_change();
                    }
                }
            })?;
        Ok(Self {
            handle: Some(handle),
        })
    }
}

impl Drop for IndexWriter {
    fn drop(&mut self) {
        // Blocks until the delta channel is closed AND drained. Callers must
        // drop all senders (watchers) first or this deadlocks by design —
        // LiveIndex's Drop encodes the correct order.
        if let Some(handle) = self.handle.take() {
            handle.join().ok();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Real directory + bootstrapped index, so deltas can re-walk subtrees.
    fn indexed_tempdir() -> (tempfile::TempDir, VolumeIndex) {
        use crate::index::walker::{FsWalkSource, IndexSource};
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("existing")).unwrap();
        fs::write(dir.path().join("existing/old.txt"), b"x").unwrap();
        let index = FsWalkSource::default().bootstrap(dir.path()).unwrap();
        (dir, index)
    }

    fn root(index: &VolumeIndex) -> PathBuf {
        index.root_path().to_path_buf()
    }

    #[test]
    fn upsert_adds_a_new_file() {
        let (_dir, mut index) = indexed_tempdir();
        let path = root(&index).join("existing/new.txt");
        // (No need for the file to exist: file upserts don't touch the fs.)
        apply(
            &mut index,
            &FsDelta::Upsert {
                path,
                is_dir: false,
            },
        )
        .unwrap();

        assert_eq!(index.search("new.txt", 10).len(), 1);
        assert_eq!(index.len(), 3);
    }

    #[test]
    fn upsert_under_excluded_system_dir_is_a_noop() {
        // Regression: a live create/modify event under an excluded top dir
        // (C:\Windows, …) must not creep back in via `ensure_dirs`.
        let sys = crate::index::SYSTEM_TOP_DIRS[0];
        let (_dir, mut index) = indexed_tempdir();
        index.set_exclude_system_dirs(true);
        let before = index.len();

        let path = root(&index).join(sys).join("System32/evil.dll");
        apply(
            &mut index,
            &FsDelta::Upsert {
                path,
                is_dir: false,
            },
        )
        .unwrap();

        assert_eq!(index.len(), before, "excluded subtree must not be inserted");
        assert!(index.search("evil", 10).is_empty());
        assert!(
            index.resolve(Path::new(sys)).is_none(),
            "top dir not vivified"
        );
    }

    #[test]
    fn upsert_refreshes_size_of_a_known_file() {
        use crate::search_filter::{Bound, Filter};
        let (_dir, mut index) = indexed_tempdir();
        let path = root(&index).join("existing/old.txt");
        // Seed a small size, then grow the file and deliver a modify as an
        // Upsert — the freshness path must re-stat it.
        let id = index.resolve(Path::new("existing/old.txt")).unwrap();
        index.set_meta(id, 1, 1);
        assert!(
            index
                .search_filtered("", &[Filter::Size(Bound::Ge(500))], 10)
                .is_empty()
        );

        fs::write(&path, vec![0u8; 500]).unwrap();
        apply(
            &mut index,
            &FsDelta::Upsert {
                path,
                is_dir: false,
            },
        )
        .unwrap();

        let hits = index.search_filtered("", &[Filter::Size(Bound::Ge(500))], 10);
        assert_eq!(
            hits.iter()
                .filter_map(|h| index.name_of(h.id))
                .collect::<Vec<_>>(),
            vec!["old.txt"]
        );
    }

    #[test]
    fn prefetched_metadata_is_used_at_apply_time_not_re_statted() {
        use crate::search_filter::{Bound, Filter};
        let (dir, mut index) = indexed_tempdir();
        let path = root(&index).join("existing/big.txt");
        fs::write(&path, vec![0u8; 500]).unwrap();

        // Gather metadata off the lock…
        let batch = vec![FsDelta::Upsert {
            path: path.clone(),
            is_dir: false,
        }];
        let meta = MetaSource::prefetch(&batch);

        // …then the file vanishes before the (locked) apply. An inline
        // stat would now find nothing; the prefetched 500 bytes must still
        // be what lands, proving apply used the prefetched value and did
        // not re-stat under the lock.
        fs::remove_file(&path).unwrap();
        apply_prepared(&mut index, &batch[0], &meta).unwrap();

        let hits = index.search_filtered("", &[Filter::Size(Bound::Ge(500))], 10);
        assert_eq!(
            hits.iter()
                .filter_map(|h| index.name_of(h.id))
                .collect::<Vec<_>>(),
            vec!["big.txt"],
            "the size recorded is the prefetched one"
        );
        drop(dir);
    }

    #[test]
    fn prefetch_gathers_only_file_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        fs::write(&file, b"hi").unwrap();
        let subdir = dir.path().join("sub");
        fs::create_dir(&subdir).unwrap();

        let batch = vec![
            FsDelta::Upsert {
                path: file.clone(),
                is_dir: false,
            },
            FsDelta::Upsert {
                path: subdir.clone(),
                is_dir: true,
            },
            FsDelta::Remove {
                path: dir.path().join("gone"),
            },
        ];
        let MetaSource::Prefetched(map) = MetaSource::prefetch(&batch) else {
            panic!("prefetch returns Prefetched");
        };
        // Only the file upsert is prefetched; the dir upsert and remove are
        // not (dir walks are rare after the known-dir no-op; removes need
        // no filesystem access).
        assert_eq!(map.len(), 1);
        assert!(map.contains_key(&file));
        assert!(map[&file].is_some());
    }

    #[test]
    fn upsert_is_idempotent_once_meta_is_populated() {
        let (_dir, mut index) = indexed_tempdir();
        let path = root(&index).join("existing/old.txt");
        // First upsert populates size/mtime (generation moves once).
        apply(
            &mut index,
            &FsDelta::Upsert {
                path: path.clone(),
                is_dir: false,
            },
        )
        .unwrap();
        let generation = index.generation();
        // A repeat upsert of the unchanged file is a true no-op.
        apply(
            &mut index,
            &FsDelta::Upsert {
                path,
                is_dir: false,
            },
        )
        .unwrap();
        assert_eq!(index.generation(), generation); // untouched
        assert_eq!(index.search("old.txt", 10).len(), 1);
    }

    #[test]
    fn upsert_of_directory_indexes_its_contents() {
        let (dir, mut index) = indexed_tempdir();
        // A directory moved in from outside, with contents the watcher
        // never produced events for.
        fs::create_dir_all(dir.path().join("moved-in/nested")).unwrap();
        fs::write(dir.path().join("moved-in/nested/deep.rs"), b"x").unwrap();

        let path = root(&index).join("moved-in");
        apply(&mut index, &FsDelta::Upsert { path, is_dir: true }).unwrap();

        assert_eq!(index.search("deep.rs", 10).len(), 1);
        assert!(index.resolve(Path::new("moved-in/nested")).is_some());
    }

    /// Regression: an upsert naming a directory we already know must not
    /// rebuild it. Watchers report a directory whenever anything inside it
    /// changes, so the old drop-and-re-walk turned one file write into a
    /// full recursive walk of the parent subtree plus a tombstone per
    /// entry — 502 mutations here for a directory that did not change.
    /// On a 2.2M-file home directory that is a pinned core and an arena
    /// that grows on every event.
    #[test]
    fn upsert_of_a_known_directory_does_not_rebuild_it() {
        let (dir, mut index) = indexed_tempdir();
        for i in 0..20 {
            fs::write(dir.path().join(format!("existing/f{i}.txt")), b"x").unwrap();
        }
        let path = root(&index).join("existing");
        // Re-walk once so the index knows the files, then take a baseline.
        apply(&mut index, &FsDelta::Rescan { path: path.clone() }).unwrap();
        let live = index.len();
        let generation = index.generation();

        apply(&mut index, &FsDelta::Upsert { path, is_dir: true }).unwrap();

        assert_eq!(index.len(), live, "no entries added or dropped");
        assert_eq!(
            index.generation(),
            generation,
            "a known directory that did not change must cost zero mutations"
        );
    }

    #[test]
    fn upsert_creates_missing_ancestor_directories() {
        let (_dir, mut index) = indexed_tempdir();
        let path = root(&index).join("a/b/c/orphan.txt");
        apply(
            &mut index,
            &FsDelta::Upsert {
                path,
                is_dir: false,
            },
        )
        .unwrap();

        assert!(index.resolve(Path::new("a/b/c/orphan.txt")).is_some());
        assert_eq!(
            index.is_dir(index.resolve(Path::new("a/b")).unwrap()),
            Some(true)
        );
    }

    #[test]
    fn remove_drops_the_subtree() {
        let (_dir, mut index) = indexed_tempdir();
        let path = root(&index).join("existing");
        apply(&mut index, &FsDelta::Remove { path }).unwrap();

        assert!(index.search("old.txt", 10).is_empty());
        assert_eq!(index.len(), 0);
    }

    #[test]
    fn remove_of_unknown_path_is_a_noop() {
        let (_dir, mut index) = indexed_tempdir();
        let path = root(&index).join("never-indexed.txt");
        apply(&mut index, &FsDelta::Remove { path }).unwrap();
        assert_eq!(index.len(), 2);
    }

    #[test]
    fn events_outside_the_root_are_ignored() {
        let (_dir, mut index) = indexed_tempdir();
        apply(
            &mut index,
            &FsDelta::Upsert {
                path: PathBuf::from("/elsewhere/x.txt"),
                is_dir: false,
            },
        )
        .unwrap();
        assert_eq!(index.len(), 2);
    }

    #[test]
    fn rescan_reconciles_out_of_band_changes() {
        let (dir, mut index) = indexed_tempdir();
        // Changes the index never heard about:
        fs::write(dir.path().join("existing/surprise.txt"), b"x").unwrap();
        fs::remove_file(dir.path().join("existing/old.txt")).unwrap();

        let path = root(&index).join("existing");
        apply(&mut index, &FsDelta::Rescan { path }).unwrap();

        assert_eq!(index.search("surprise", 10).len(), 1);
        assert!(index.search("old.txt", 10).is_empty());
    }

    #[test]
    fn rescan_of_the_root_rebuilds_everything() {
        let (dir, mut index) = indexed_tempdir();
        fs::write(dir.path().join("root-level.txt"), b"x").unwrap();

        let path = root(&index);
        apply(&mut index, &FsDelta::Rescan { path }).unwrap();

        assert_eq!(index.search("root-level", 10).len(), 1);
        assert_eq!(index.search("old.txt", 10).len(), 1); // still there
        assert_eq!(index.len(), 3);
    }

    #[test]
    fn rescan_of_a_vanished_directory_removes_it() {
        let (dir, mut index) = indexed_tempdir();
        fs::remove_dir_all(dir.path().join("existing")).unwrap();

        let path = root(&index).join("existing");
        apply(&mut index, &FsDelta::Rescan { path }).unwrap();
        assert_eq!(index.len(), 0);
    }

    /// Keyed fixture: root(frn 5)/{docs(10)/{a.txt(11)}, b.txt(20)}
    fn keyed_index() -> VolumeIndex {
        let mut index = VolumeIndex::new_with_root_key("/vol", 5);
        let docs = index
            .insert_with_key(crate::index::ROOT, "docs", true, 10)
            .unwrap();
        index.insert_with_key(docs, "a.txt", false, 11).unwrap();
        index
            .insert_with_key(crate::index::ROOT, "b.txt", false, 20)
            .unwrap();
        index
    }

    #[test]
    fn native_upsert_inserts_under_keyed_parent() {
        let mut index = keyed_index();
        apply(
            &mut index,
            &FsDelta::NativeUpsert {
                key: 12,
                parent_key: 10,
                name: "new.txt".into(),
                is_dir: false,
            },
        )
        .unwrap();
        let hits = index.search("new.txt", 10);
        assert_eq!(
            index.path_of(hits[0].id).unwrap(),
            PathBuf::from("/vol/docs/new.txt")
        );
    }

    #[test]
    fn native_upsert_renames_existing_key_and_children_follow() {
        let mut index = keyed_index();
        // docs (frn 10) renamed to "archive" — a.txt must follow.
        apply(
            &mut index,
            &FsDelta::NativeUpsert {
                key: 10,
                parent_key: 5,
                name: "archive".into(),
                is_dir: true,
            },
        )
        .unwrap();
        let hits = index.search("a.txt", 10);
        assert_eq!(
            index.path_of(hits[0].id).unwrap(),
            PathBuf::from("/vol/archive/a.txt")
        );
        assert!(index.resolve(Path::new("docs")).is_none());
    }

    #[test]
    fn native_upsert_to_unknown_parent_removes_moved_out_entry() {
        let mut index = keyed_index();
        apply(
            &mut index,
            &FsDelta::NativeUpsert {
                key: 20,
                parent_key: 999,
                name: "b.txt".into(),
                is_dir: false,
            },
        )
        .unwrap();
        assert!(index.search("b.txt", 10).is_empty());

        // Entirely-unknown key + parent: silently ignored.
        apply(
            &mut index,
            &FsDelta::NativeUpsert {
                key: 777,
                parent_key: 999,
                name: "x".into(),
                is_dir: false,
            },
        )
        .unwrap();
        assert_eq!(index.len(), 2); // docs + a.txt remain
    }

    #[test]
    fn native_remove_drops_subtree_and_tolerates_unknown_keys() {
        let mut index = keyed_index();
        apply(&mut index, &FsDelta::NativeRemove { key: 10 }).unwrap();
        assert!(index.search("a.txt", 10).is_empty());
        assert!(index.entry_by_native_key(11).is_none()); // key map cleaned

        apply(&mut index, &FsDelta::NativeRemove { key: 10 }).unwrap(); // replay: no-op
        assert_eq!(index.len(), 1);
    }

    #[test]
    fn native_upsert_type_flip_rebuilds_recycled_frn() {
        let mut index = keyed_index();
        // FRN 20 was b.txt (file); journal now says it's a directory.
        apply(
            &mut index,
            &FsDelta::NativeUpsert {
                key: 20,
                parent_key: 5,
                name: "b".into(),
                is_dir: true,
            },
        )
        .unwrap();
        let id = index.entry_by_native_key(20).unwrap();
        assert_eq!(index.is_dir(id), Some(true));
        assert!(index.search("b.txt", 10).is_empty());
    }

    #[test]
    fn a_loaded_snapshot_is_immutable_across_writes_and_publishes() {
        // Optimization B's core contract: a reader scans a point-in-time
        // snapshot that never changes under it, and a publish is what makes
        // new state visible to *fresh* loads — no locks either way.
        let mut index = VolumeIndex::new("/vol");
        index.insert(ROOT, "before.txt", false).unwrap();
        let shared = SharedIndex::new(index);

        let snap = shared.load();
        assert_eq!(snap.search("before", 10).len(), 1);

        // Mutate the head and publish while the old snapshot is still held.
        {
            let mut w = shared.write();
            w.insert(ROOT, "after.txt", false).unwrap();
        }
        shared.publish();

        // The held snapshot is frozen at load time…
        assert_eq!(
            snap.search("after", 10).len(),
            0,
            "old snapshot must not mutate"
        );
        // …and a fresh load sees the published change.
        assert_eq!(shared.load().search("after", 10).len(), 1);
    }

    #[test]
    fn writes_are_invisible_until_published() {
        // Mutating the head does not touch what readers load; only
        // `publish` does. This is what lets the writer coalesce many small
        // batches into one (cloning) publish.
        let shared = SharedIndex::new(VolumeIndex::new("/vol"));
        {
            let mut w = shared.write();
            w.insert(ROOT, "pending.txt", false).unwrap();
        }
        assert_eq!(shared.load().search("pending", 10).len(), 0, "unpublished");
        shared.publish();
        assert_eq!(shared.load().search("pending", 10).len(), 1, "now visible");
    }

    #[test]
    fn writer_thread_applies_batches_and_notifies() {
        let (_dir, index) = indexed_tempdir();
        let shared = SharedIndex::new(index);
        let (delta_tx, delta_rx) = mpsc::channel();
        let (notify_tx, notify_rx) = mpsc::channel();

        let _writer = IndexWriter::spawn(
            shared.clone(),
            delta_rx,
            move || {
                notify_tx.send(()).ok();
            },
            None,
        )
        .unwrap();

        let path = shared.load().root_path().join("from-writer.txt");
        delta_tx
            .send(vec![FsDelta::Upsert {
                path,
                is_dir: false,
            }])
            .unwrap();

        notify_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("writer never signaled a change");
        assert_eq!(shared.load().search("from-writer", 10).len(), 1);

        // IndexWriter's Drop joins its thread, which only exits once every
        // sender is gone — drop the sender first or this test deadlocks
        // (LiveIndex encodes this order for the real wiring).
        drop(delta_tx);
    }

    /// Mass deletion pushing dead debt past the threshold must make the
    /// writer compact: same live content, shrunken arena.
    #[test]
    fn writer_compacts_after_mass_deletion() {
        let mut index = VolumeIndex::new("/vol");
        let doomed = index.insert(crate::index::ROOT, "doomed", true).unwrap();
        for i in 0..6000 {
            index.insert(doomed, &format!("f{i}.txt"), false).unwrap();
        }
        index
            .insert(crate::index::ROOT, "survivor.txt", false)
            .unwrap();
        let arena_before = 6003; // root + doomed + 6000 + survivor

        let shared = SharedIndex::new(index);
        let (delta_tx, delta_rx) = mpsc::channel();
        let (notify_tx, notify_rx) = mpsc::channel();
        let _writer = IndexWriter::spawn(
            shared.clone(),
            delta_rx,
            move || {
                notify_tx.send(()).ok();
            },
            None,
        )
        .unwrap();

        delta_tx
            .send(vec![FsDelta::Remove {
                path: PathBuf::from("/vol/doomed"),
            }])
            .unwrap();

        // Two notifications: the removal batch, then the compaction swap.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            notify_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("writer went quiet before compacting");
            let index = shared.load();
            if index.entries.len() < arena_before {
                assert_eq!(index.len(), 1); // survivor.txt
                assert_eq!(index.entries.len(), 2); // root + survivor
                assert_eq!(index.search("survivor", 10).len(), 1);
                break;
            }
            drop(index);
            assert!(std::time::Instant::now() < deadline, "never compacted");
        }
        drop(delta_tx);
    }

    /// The writer must apply everything queued before a PersistNow marker
    /// and then call the save hook with the marker's checkpoint.
    #[test]
    fn writer_saves_on_persist_marker_after_applying_prior_deltas() {
        use crate::index::persist::Checkpoint;

        let (_dir, index) = indexed_tempdir();
        let shared = SharedIndex::new(index);
        let (delta_tx, delta_rx) = mpsc::channel();
        let (save_tx, save_rx) = mpsc::channel();

        let hook: SaveHook = Box::new(move |index, checkpoint| {
            save_tx
                .send((index.search("queued-first", 10).len(), checkpoint))
                .ok();
        });
        let _writer = IndexWriter::spawn(shared.clone(), delta_rx, || {}, Some(hook)).unwrap();

        let path = shared.load().root_path().join("queued-first.txt");
        let checkpoint = Checkpoint::FsEvents { last_event_id: 7 };
        delta_tx
            .send(vec![
                FsDelta::Upsert {
                    path,
                    is_dir: false,
                },
                FsDelta::PersistNow { checkpoint },
            ])
            .unwrap();

        let (hits_at_save, saved_checkpoint) = save_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("save hook never ran");
        assert_eq!(
            hits_at_save, 1,
            "delta queued before the marker not applied"
        );
        assert_eq!(saved_checkpoint, checkpoint);
        drop(delta_tx);
    }
}

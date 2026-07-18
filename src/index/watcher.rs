//! OS-agnostic live-update pipeline (docs/indexing-architecture.md §2).
//!
//! Platform watchers normalize native events into [`FsDelta`]s and send
//! batches over a channel. A single writer thread ([`IndexWriter`]) drains
//! the channel, applies deltas under a short write lock, and fires a
//! change notification. The UI never touches this machinery directly.

use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, RwLock, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Result, bail};

use super::walker::index_subtree;
use super::{EntryId, ROOT, VolumeIndex};

/// The index shared between the writer thread and (background) readers.
/// Phase 1 uses a plain `RwLock` — writes are short, batched, and rare
/// relative to reads; swap for `ArcSwap` snapshots if profiling ever shows
/// readers stalling on writers.
pub type SharedIndex = Arc<RwLock<VolumeIndex>>;

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
    /// Journal-sourced upsert addressed by platform-native keys (NTFS
    /// FRNs): `key` now exists under `parent_key` with `name`. Covers
    /// creates and renames/moves — the index entry is re-pointed, and
    /// descendants follow automatically through parent links. A
    /// `parent_key` unknown to the index means the event is outside the
    /// indexed subtree; it (and, for moves, the entry) is dropped.
    NativeUpsert {
        key: u64,
        parent_key: u64,
        name: String,
        is_dir: bool,
    },
    /// Journal-sourced removal by native key. Unknown keys are no-ops
    /// (deletes outside the subtree, or replays of applied events).
    NativeRemove { key: u64 },
    /// Writer control marker (never produced by watchers): save a
    /// snapshot with this checkpoint once every delta queued *before* it
    /// has been applied. Enqueue-time capture makes the checkpoint safe:
    /// watchers advance their checkpoint atomics before sending, so all
    /// events the checkpoint covers precede the marker in the channel.
    PersistNow { checkpoint: super::persist::Checkpoint },
}

/// Apply one delta to the index. Idempotent: replaying an event whose
/// effect is already reflected (e.g. an Upsert raced by the bootstrap walk)
/// is a no-op, so watchers may start before bootstrap without double-counting.
pub fn apply(index: &mut VolumeIndex, delta: &FsDelta) -> Result<()> {
    match delta {
        FsDelta::Upsert { path, is_dir } => upsert(index, path, *is_dir),
        FsDelta::Remove { path } => remove(index, path),
        FsDelta::Rescan { path } => rescan(index, path),
        FsDelta::NativeUpsert { key, parent_key, name, is_dir } => {
            native_upsert(index, *key, *parent_key, name, *is_dir)
        }
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
    let existing = index.entry_by_native_key(key);
    match (existing, parent) {
        // A move out of the indexed subtree: the entry leaves the index.
        (Some(id), None) => index.remove(id),
        // Rename/move within the subtree: re-point the entry; children
        // follow via parent links. A type flip means the FRN was recycled
        // mid-window — rebuild the entry instead.
        (Some(id), Some(parent)) => {
            if index.is_dir(id) == Some(is_dir) {
                index.rename(id, parent, name)
            } else {
                index.remove(id)?;
                index.insert_with_key(parent, name, is_dir, key).map(|_| ())
            }
        }
        (None, Some(parent)) => index.insert_with_key(parent, name, is_dir, key).map(|_| ()),
        // Entirely outside the indexed subtree.
        (None, None) => Ok(()),
    }
}

fn relative_to_root(index: &VolumeIndex, path: &Path) -> Option<PathBuf> {
    path.strip_prefix(index.root_path())
        .ok()
        .map(Path::to_path_buf)
}

fn upsert(index: &mut VolumeIndex, path: &Path, is_dir: bool) -> Result<()> {
    let Some(rel) = relative_to_root(index, path) else {
        return Ok(()); // outside the indexed root
    };
    let Some(name) = rel.file_name().and_then(|n| n.to_str()) else {
        return Ok(()); // event for the root itself, or non-UTF-8 name
    };
    let parent_rel = rel.parent().unwrap_or(Path::new(""));
    let parent = ensure_dirs(index, parent_rel)?;

    if let Some(existing) = index.resolve_child(parent, name) {
        if index.is_dir(existing) == Some(is_dir) && !is_dir {
            return Ok(()); // file already known; content changes don't affect names
        }
        // Directory upsert (contents may have changed wholesale, e.g. a move
        // into the tree) or a file/dir type flip: drop the stale subtree and
        // re-index from the filesystem below.
        index.remove(existing)?;
    }

    let id = index.insert(parent, name, is_dir)?;
    if is_dir {
        index_subtree(index, id, path, false)?;
    }
    Ok(())
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
fn rescan(index: &mut VolumeIndex, path: &Path) -> Result<()> {
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
            index_subtree(index, ROOT, path, false)
        }
        _ => upsert(index, path, true), // re-indexes the subtree
    }
}

/// Resolve `rel` under the root, creating any missing intermediate
/// directory entries (events can arrive for paths deeper than anything
/// indexed yet). An entry that exists as a file where a directory is needed
/// is replaced — events told us the filesystem disagrees with the index.
fn ensure_dirs(index: &mut VolumeIndex, rel: &Path) -> Result<EntryId> {
    let mut current = ROOT;
    for component in rel.components() {
        let Component::Normal(os_name) = component else {
            bail!("unexpected path component {component:?} in {}", rel.display());
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

/// Owns the thread that applies deltas to the shared index. The thread
/// exits when every delta sender has been dropped, and Drop *joins* it:
/// dropping the watcher(s) and then the writer guarantees every delta the
/// watcher ever sent has been applied — which is what makes a
/// checkpoint-then-save shutdown lose no events.
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
                let root = match index.read() {
                    Ok(guard) => guard.root_path().to_path_buf(),
                    Err(poisoned) => poisoned.into_inner().root_path().to_path_buf(),
                };
                while let Ok(first) = deltas.recv() {
                    let mut batch = first;
                    while batch.len() < MAX_BATCH {
                        match deltas.recv_timeout(BATCH_WINDOW) {
                            Ok(more) => batch.extend(more),
                            Err(_) => break,
                        }
                    }

                    // A whole-root rescan (startup reconcile, queue overflow)
                    // is rebuilt *outside* the lock so searches keep answering
                    // from the old index during the walk, then swapped in.
                    // The rest of the batch is dropped: the fresh walk already
                    // reflects those events, and later batches apply on top.
                    let root_rescan = batch
                        .iter()
                        .any(|d| matches!(d, FsDelta::Rescan { path } if *path == root));
                    if root_rescan {
                        use super::walker::IndexSource as _;
                        match super::walker::FsWalkSource::default().bootstrap(&root) {
                            Ok(rebuilt) => {
                                let mut guard = match index.write() {
                                    Ok(guard) => guard,
                                    Err(poisoned) => poisoned.into_inner(),
                                };
                                *guard = rebuilt;
                                drop(guard);
                                on_change();
                            }
                            Err(err) => {
                                tracing::warn!("root rescan of {} failed: {err:#}", root.display());
                            }
                        }
                        continue;
                    }

                    // A batch may carry a PersistNow marker; save with the
                    // *last* one after applying everything else (deltas
                    // after the marker being included only means the next
                    // replay re-applies a few events idempotently).
                    let mut pending_save = None;
                    let mut applied_any = false;
                    {
                        let mut index = match index.write() {
                            Ok(guard) => guard,
                            Err(poisoned) => poisoned.into_inner(),
                        };
                        for delta in &batch {
                            if let FsDelta::PersistNow { checkpoint } = delta {
                                pending_save = Some(*checkpoint);
                                continue;
                            }
                            match apply(&mut index, delta) {
                                Ok(()) => applied_any = true,
                                Err(err) => {
                                    tracing::warn!("failed to apply {delta:?}: {err:#}");
                                }
                            }
                        }
                    } // write lock released before notifying
                    if let (Some(checkpoint), Some(hook)) = (pending_save, &save_hook) {
                        let index = match index.read() {
                            Ok(guard) => guard,
                            Err(poisoned) => poisoned.into_inner(),
                        };
                        hook(&index, checkpoint);
                    }
                    if applied_any {
                        on_change();
                    }

                    // Compact when enough of the arena is dead. Built from
                    // a read guard (searches keep answering), then swapped;
                    // this thread is the only mutator, so nothing can
                    // change between the build and the swap.
                    let compacted = {
                        let index = match index.read() {
                            Ok(guard) => guard,
                            Err(poisoned) => poisoned.into_inner(),
                        };
                        index.needs_compaction().then(|| index.compacted())
                    };
                    if let Some(fresh) = compacted {
                        let mut guard = match index.write() {
                            Ok(guard) => guard,
                            Err(poisoned) => poisoned.into_inner(),
                        };
                        *guard = fresh;
                        drop(guard);
                        on_change();
                    }
                }
            })?;
        Ok(Self { handle: Some(handle) })
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
        apply(&mut index, &FsDelta::Upsert { path, is_dir: false }).unwrap();

        assert_eq!(index.search("new.txt", 10).len(), 1);
        assert_eq!(index.len(), 3);
    }

    #[test]
    fn upsert_is_idempotent_for_known_files() {
        let (_dir, mut index) = indexed_tempdir();
        let path = root(&index).join("existing/old.txt");
        let generation = index.generation();
        apply(&mut index, &FsDelta::Upsert { path, is_dir: false }).unwrap();

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

    #[test]
    fn upsert_creates_missing_ancestor_directories() {
        let (_dir, mut index) = indexed_tempdir();
        let path = root(&index).join("a/b/c/orphan.txt");
        apply(&mut index, &FsDelta::Upsert { path, is_dir: false }).unwrap();

        assert!(index.resolve(Path::new("a/b/c/orphan.txt")).is_some());
        assert_eq!(index.is_dir(index.resolve(Path::new("a/b")).unwrap()), Some(true));
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
            &FsDelta::Upsert { path: PathBuf::from("/elsewhere/x.txt"), is_dir: false },
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
        let docs = index.insert_with_key(crate::index::ROOT, "docs", true, 10).unwrap();
        index.insert_with_key(docs, "a.txt", false, 11).unwrap();
        index.insert_with_key(crate::index::ROOT, "b.txt", false, 20).unwrap();
        index
    }

    #[test]
    fn native_upsert_inserts_under_keyed_parent() {
        let mut index = keyed_index();
        apply(
            &mut index,
            &FsDelta::NativeUpsert { key: 12, parent_key: 10, name: "new.txt".into(), is_dir: false },
        )
        .unwrap();
        let hits = index.search("new.txt", 10);
        assert_eq!(index.path_of(hits[0].id).unwrap(), PathBuf::from("/vol/docs/new.txt"));
    }

    #[test]
    fn native_upsert_renames_existing_key_and_children_follow() {
        let mut index = keyed_index();
        // docs (frn 10) renamed to "archive" — a.txt must follow.
        apply(
            &mut index,
            &FsDelta::NativeUpsert { key: 10, parent_key: 5, name: "archive".into(), is_dir: true },
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
            &FsDelta::NativeUpsert { key: 20, parent_key: 999, name: "b.txt".into(), is_dir: false },
        )
        .unwrap();
        assert!(index.search("b.txt", 10).is_empty());

        // Entirely-unknown key + parent: silently ignored.
        apply(
            &mut index,
            &FsDelta::NativeUpsert { key: 777, parent_key: 999, name: "x".into(), is_dir: false },
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
            &FsDelta::NativeUpsert { key: 20, parent_key: 5, name: "b".into(), is_dir: true },
        )
        .unwrap();
        let id = index.entry_by_native_key(20).unwrap();
        assert_eq!(index.is_dir(id), Some(true));
        assert!(index.search("b.txt", 10).is_empty());
    }

    #[test]
    fn writer_thread_applies_batches_and_notifies() {
        let (_dir, index) = indexed_tempdir();
        let shared: SharedIndex = Arc::new(RwLock::new(index));
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

        let path = shared.read().unwrap().root_path().join("from-writer.txt");
        delta_tx
            .send(vec![FsDelta::Upsert { path, is_dir: false }])
            .unwrap();

        notify_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("writer never signaled a change");
        assert_eq!(shared.read().unwrap().search("from-writer", 10).len(), 1);

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
        index.insert(crate::index::ROOT, "survivor.txt", false).unwrap();
        let arena_before = 6003; // root + doomed + 6000 + survivor

        let shared: SharedIndex = Arc::new(RwLock::new(index));
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
            .send(vec![FsDelta::Remove { path: PathBuf::from("/vol/doomed") }])
            .unwrap();

        // Two notifications: the removal batch, then the compaction swap.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            notify_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("writer went quiet before compacting");
            let index = shared.read().unwrap();
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
        let shared: SharedIndex = Arc::new(RwLock::new(index));
        let (delta_tx, delta_rx) = mpsc::channel();
        let (save_tx, save_rx) = mpsc::channel();

        let hook: SaveHook = Box::new(move |index, checkpoint| {
            save_tx
                .send((index.search("queued-first", 10).len(), checkpoint))
                .ok();
        });
        let _writer = IndexWriter::spawn(shared.clone(), delta_rx, || {}, Some(hook)).unwrap();

        let path = shared.read().unwrap().root_path().join("queued-first.txt");
        let checkpoint = Checkpoint::FsEvents { last_event_id: 7 };
        delta_tx
            .send(vec![
                FsDelta::Upsert { path, is_dir: false },
                FsDelta::PersistNow { checkpoint },
            ])
            .unwrap();

        let (hits_at_save, saved_checkpoint) = save_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("save hook never ran");
        assert_eq!(hits_at_save, 1, "delta queued before the marker not applied");
        assert_eq!(saved_checkpoint, checkpoint);
        drop(delta_tx);
    }
}

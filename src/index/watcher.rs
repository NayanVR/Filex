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
}

/// Apply one delta to the index. Idempotent: replaying an event whose
/// effect is already reflected (e.g. an Upsert raced by the bootstrap walk)
/// is a no-op, so watchers may start before bootstrap without double-counting.
pub fn apply(index: &mut VolumeIndex, delta: &FsDelta) -> Result<()> {
    match delta {
        FsDelta::Upsert { path, is_dir } => upsert(index, path, *is_dir),
        FsDelta::Remove { path } => remove(index, path),
        FsDelta::Rescan { path } => rescan(index, path),
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
/// exits when every delta sender has been dropped; it is detached rather
/// than joined on drop so tearing down the UI never blocks on it.
pub struct IndexWriter {
    _handle: JoinHandle<()>,
}

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
    ) -> Result<Self> {
        let handle = std::thread::Builder::new()
            .name("filex-index-writer".into())
            .spawn(move || {
                while let Ok(first) = deltas.recv() {
                    let mut batch = first;
                    while batch.len() < MAX_BATCH {
                        match deltas.recv_timeout(BATCH_WINDOW) {
                            Ok(more) => batch.extend(more),
                            Err(_) => break,
                        }
                    }
                    let mut applied_any = false;
                    {
                        let mut index = match index.write() {
                            Ok(guard) => guard,
                            Err(poisoned) => poisoned.into_inner(),
                        };
                        for delta in &batch {
                            match apply(&mut index, delta) {
                                Ok(()) => applied_any = true,
                                Err(err) => {
                                    eprintln!("filex: failed to apply {delta:?}: {err:#}");
                                }
                            }
                        }
                    } // write lock released before notifying
                    if applied_any {
                        on_change();
                    }
                }
            })?;
        Ok(Self { _handle: handle })
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

    #[test]
    fn writer_thread_applies_batches_and_notifies() {
        let (_dir, index) = indexed_tempdir();
        let shared: SharedIndex = Arc::new(RwLock::new(index));
        let (delta_tx, delta_rx) = mpsc::channel();
        let (notify_tx, notify_rx) = mpsc::channel();

        let _writer = IndexWriter::spawn(shared.clone(), delta_rx, move || {
            notify_tx.send(()).ok();
        })
        .unwrap();

        let path = shared.read().unwrap().root_path().join("from-writer.txt");
        delta_tx
            .send(vec![FsDelta::Upsert { path, is_dir: false }])
            .unwrap();

        notify_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("writer never signaled a change");
        assert_eq!(shared.read().unwrap().search("from-writer", 10).len(), 1);
    }
}

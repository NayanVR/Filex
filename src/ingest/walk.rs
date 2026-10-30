//! The enumeration walk. Lister threads read directories (`list` and, where
//! the listing doesn't carry it, a `stat` per entry), which is where a walk
//! spends its time waiting on the filesystem. The caller's thread allocates
//! IDs and emits records one at a time, so `allocate` and `emit` need no
//! synchronization and see each directory before its children.
//!
//! A directory's record is held back until its own listing arrives, so the
//! listing can correct its mtime (FIL-47): on Windows the copy in the
//! parent's listing can lag the directory's own.

use super::{IndexFilter, Listed, Stat, list, record, stat};
use crate::catalog::segment::{Identity, Record, Root};
use anyhow::Result;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
};

/// Directory reads overlap well on SSDs and network shares up to a few
/// in flight; beyond that they mostly contend.
const MAX_LISTERS: usize = 4;
/// Listed directories waiting for the emitting thread. Bounds memory when
/// listing outpaces ID allocation (a reconcile's lookups).
const PENDING_LISTINGS: usize = 64;

/// One directory's kept entries, or `None` when it couldn't be opened.
struct Listing {
    parent: u64,
    entries: Option<Entries>,
}

/// A directory's own mtime when its listing read one (see `DirListing`),
/// and its kept entries.
type Entries = (Option<i64>, Vec<std::io::Result<(PathBuf, Stat)>>);

/// Directories waiting to be listed, shared by the listers.
struct Queue {
    state: Mutex<(Vec<(PathBuf, u64)>, bool)>,
    ready: Condvar,
}

impl Queue {
    fn push(&self, dir: (PathBuf, u64)) {
        if let Ok(mut state) = self.state.lock() {
            state.0.push(dir);
            self.ready.notify_one();
        }
    }
    /// The next directory, blocking while none is queued; `None` once closed.
    fn pop(&self) -> Option<(PathBuf, u64)> {
        let mut state = self.state.lock().ok()?;
        loop {
            if state.1 {
                return None;
            }
            if let Some(dir) = state.0.pop() {
                return Some(dir);
            }
            state = self.ready.wait(state).ok()?;
        }
    }
    fn close(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.1 = true;
            self.ready.notify_all();
        }
    }
}

/// Walk `root` without following symlinks. Permission loss is reported
/// rather than silently claiming a complete generation: each directory or
/// entry that can't be read counts as skipped.
///
/// `allocate` receives each entry's path, the ID already allocated to its
/// parent directory (0 for the root) and its native identity. `cancel` is
/// checked before every entry, so a stop lands within one entry, not one
/// directory (FIL-27).
pub fn walk(
    root: &Root,
    excluded: &Path,
    filter: &IndexFilter,
    mut allocate: impl FnMut(&Path, u64, &Identity) -> u64,
    mut emit: impl FnMut(Record) -> Result<()>,
    cancel: &AtomicBool,
) -> Result<usize> {
    let root_stat = stat(&root.path)?;
    let root_id = allocate(&root.path, 0, &root_stat.identity);
    emit(record(&root.path, root_stat, root_id, 0, root.id))?;
    let queue = Queue {
        state: Mutex::new((vec![(root.path.clone(), root_id)], false)),
        ready: Condvar::new(),
    };
    let queue = &queue;
    let listers = std::thread::available_parallelism().map_or(1, |n| n.get().min(MAX_LISTERS));
    let (tx, rx) = mpsc::sync_channel::<Listing>(PENDING_LISTINGS);
    std::thread::scope(|scope| {
        for _ in 0..listers {
            let tx = tx.clone();
            scope.spawn(move || {
                while let Some((dir, parent)) = queue.pop() {
                    let listing = Listing {
                        parent,
                        entries: read_dir(&dir, root, excluded, filter, cancel),
                    };
                    if tx.send(listing).is_err() {
                        return;
                    }
                }
            });
        }
        drop(tx);
        // Owns `rx`, so an early return drops it and unblocks any lister
        // waiting to send; closing the queue then releases the rest.
        let result = (move || {
            let mut skipped = 0;
            // Directories queued or being listed whose listing hasn't
            // arrived; every popped directory sends exactly one.
            let mut outstanding = 1usize;
            // Their records, emitted when that listing arrives. The root
            // was emitted from its own `stat` and isn't here.
            let mut held = HashMap::<u64, Record>::new();
            while outstanding > 0 {
                anyhow::ensure!(!cancel.load(Ordering::Relaxed), "enumeration cancelled");
                let listing = rx.recv()?;
                outstanding -= 1;
                if let Some(mut dir) = held.remove(&listing.parent) {
                    if let Some((Some(mtime), _)) = listing.entries {
                        dir.mtime = Some(mtime);
                    }
                    emit(dir)?;
                }
                let Some((_, entries)) = listing.entries else {
                    skipped += 1;
                    continue;
                };
                for entry in entries {
                    anyhow::ensure!(!cancel.load(Ordering::Relaxed), "enumeration cancelled");
                    let Ok((path, entry_stat)) = entry else {
                        skipped += 1;
                        continue;
                    };
                    let id = allocate(&path, listing.parent, &entry_stat.identity);
                    let record = record(&path, entry_stat, id, listing.parent, root.id);
                    if record.is_dir() {
                        held.insert(id, record);
                        queue.push((path, id));
                        outstanding += 1;
                    } else {
                        emit(record)?;
                    }
                }
            }
            Ok(skipped)
        })();
        queue.close();
        result
    })
}

/// The entries of `dir` the walk keeps, each with its [`Stat`]: excluded
/// areas are dropped before any per-entry `stat`. `None` if the directory
/// can't be listed. A cancelled walk lists nothing more.
fn read_dir(
    dir: &Path,
    root: &Root,
    excluded: &Path,
    filter: &IndexFilter,
    cancel: &AtomicBool,
) -> Option<Entries> {
    if cancel.load(Ordering::Relaxed) {
        return Some((None, Vec::new()));
    }
    let listing = list(dir).ok()?;
    Some((
        listing.mtime,
        listing
            .entries
            .filter_map(|entry| {
                let Listed {
                    path,
                    is_dir,
                    stat: listed,
                } = match entry {
                    Ok(listed) => listed,
                    Err(e) => return Some(Err(e)),
                };
                let pruned = is_dir
                    && path
                        .file_name()
                        .is_some_and(|name| filter.excluded_name(name));
                if pruned || path.starts_with(excluded) || filter.excludes_area(&root.path, &path) {
                    return None;
                }
                Some(listed.map_or_else(|| stat(&path), Ok).map(|s| (path, s)))
            })
            .collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::walk;
    use crate::catalog::segment::Root;
    use std::{collections::HashSet, fs, sync::atomic::AtomicBool};

    /// With several listers in flight, every entry is still emitted exactly
    /// once, after its parent, under its parent's ID.
    #[test]
    fn parents_are_emitted_before_children_across_listers() {
        let tree = tempfile::tempdir().unwrap();
        let mut expected = 1; // the root
        for a in 0..6 {
            for b in 0..5 {
                let dir = tree.path().join(format!("a{a}/b{b}"));
                fs::create_dir_all(&dir).unwrap();
                for f in 0..7 {
                    fs::write(dir.join(format!("f{f}.txt")), b"x").unwrap();
                }
                expected += 1 + 7;
            }
            expected += 1;
        }
        let root = Root {
            id: 1,
            path: tree.path().to_path_buf(),
            device: 0,
        };
        let mut next = 0;
        let mut emitted = HashSet::new();
        let mut paths = HashSet::new();
        let skipped = walk(
            &root,
            &tree.path().join("not-the-database"),
            &Default::default(),
            |path, _, _| {
                assert!(paths.insert(path.to_path_buf()), "{path:?} twice");
                next += 1;
                next
            },
            |record| {
                assert!(
                    record.parent == 0 || emitted.contains(&record.parent),
                    "child before its parent"
                );
                emitted.insert(record.id);
                Ok(())
            },
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(skipped, 0);
        assert_eq!(emitted.len(), expected);
    }
}

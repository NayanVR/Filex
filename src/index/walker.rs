//! Generic bootstrap indexer: a parallel filesystem walk. Works on every OS
//! with no special privileges — the lowest-common-denominator `IndexSource`.
//! Platform-specific sources (Windows USN enumeration, macOS getattrlistbulk)
//! will implement the same trait (see docs/indexing-architecture.md §1).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

use super::{EntryId, ROOT, VolumeIndex};

/// A source that can produce a fully-populated index for a root directory.
/// Implementations must yield parent directories before their contents.
pub trait IndexSource {
    fn bootstrap(&self, root: &Path) -> Result<VolumeIndex> {
        self.bootstrap_cancellable(root, None)
    }

    /// Like [`bootstrap`](Self::bootstrap), but abandons the walk (with an
    /// error, so no partial index is used) once `cancel` is set — lets a
    /// service shut down promptly mid-bootstrap. `None` never cancels.
    fn bootstrap_cancellable(
        &self,
        root: &Path,
        cancel: Option<&std::sync::atomic::AtomicBool>,
    ) -> Result<VolumeIndex>;
}

/// Bootstrap by walking the filesystem with parallel directory reads
/// (`jwalk`). Symlinks are recorded as leaf entries, never followed, so
/// cycles cannot occur. Unreadable subtrees are skipped, not fatal.
pub struct FsWalkSource {
    /// Skip entries whose name starts with a dot (default: false — an index
    /// should see everything; filtering is a query-time concern).
    pub skip_hidden: bool,
    /// Skip top-level OS/system dirs (`C:\Windows`, …) — see
    /// [`super::SYSTEM_TOP_DIRS`]. Default true: the whole-machine index
    /// would otherwise be dominated by files no one searches.
    pub skip_system_dirs: bool,
}

impl Default for FsWalkSource {
    fn default() -> Self {
        Self {
            skip_hidden: false,
            skip_system_dirs: true,
        }
    }
}

impl IndexSource for FsWalkSource {
    /// Assumes only POSIX/Win32 directory semantics (readdir + file type);
    /// no OS-specific metadata structures are touched.
    fn bootstrap_cancellable(
        &self,
        root: &Path,
        cancel: Option<&std::sync::atomic::AtomicBool>,
    ) -> Result<VolumeIndex> {
        let root = root
            .canonicalize()
            .with_context(|| format!("canonicalizing index root {}", root.display()))?;
        let mut index = VolumeIndex::new(&root);
        index.set_exclude_system_dirs(self.skip_system_dirs);
        index_subtree(&mut index, ROOT, &root, self.skip_hidden, cancel)?;
        // A cancelled walk leaves a partial index; discard it rather than
        // persist an incomplete tree as if it were complete.
        if cancel.is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed)) {
            anyhow::bail!("bootstrap of {} was cancelled", root.display());
        }
        Ok(index)
    }
}

/// Walk the filesystem at `path` and insert everything found under the
/// existing entry `under`. Used both for bootstrap (under = ROOT) and for
/// re-indexing a subtree after a directory-level filesystem event.
/// Symlinks become leaves; unreadable entries are skipped.
pub fn index_subtree(
    index: &mut VolumeIndex,
    under: EntryId,
    path: &Path,
    skip_hidden: bool,
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> Result<()> {
    // Maps each *directory's* path to its entry, so children can find
    // their parent. jwalk yields a directory before its contents.
    let mut dir_ids: HashMap<PathBuf, EntryId> = HashMap::new();
    dir_ids.insert(path.to_path_buf(), under);

    let walk = jwalk::WalkDir::new(path)
        .skip_hidden(skip_hidden)
        .follow_links(false);

    for dirent in walk {
        // A whole-volume bootstrap can take a while; let a shutdown stop it
        // promptly (the partial index is discarded by the caller).
        if cancel.is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed)) {
            break;
        }
        let Ok(dirent) = dirent else {
            continue; // unreadable entry: skip, keep indexing
        };
        if dirent.depth() == 0 {
            continue; // `path` itself is represented by `under`
        }
        let Some(name) = dirent.file_name().to_str() else {
            // Non-UTF-8 names are deliberately excluded from the *index*:
            // the name pools, case folding, search, and persistence are
            // UTF-8 throughout, and a lossy copy would produce paths that
            // don't exist. Browse mode still shows and opens such files
            // (listing keeps the raw OsString path); only search misses
            // them. Full support would mean byte-based pools — not worth
            // it for names that are illegal on macOS and rare elsewhere.
            continue;
        };
        let parent_path = dirent.parent_path();
        let Some(&parent) = dir_ids.get(parent_path) else {
            continue; // parent was skipped (unreadable / non-UTF-8 / excluded)
        };
        // Top-level OS/system dir (C:\Windows, …): skip it, and its whole
        // subtree follows since it's never entered into `dir_ids`.
        // ponytail: jwalk still *reads* the excluded subtree from disk here
        // (no index cost, but wasted I/O on the unelevated walk fallback);
        // prune with jwalk's process_read_dir if bootstrap time regresses.
        // The elevated Windows path (USN/MFT) skips it at build time instead.
        if index.excludes_path(&dirent.path()) {
            continue;
        }
        // Symlinks (even to directories) are indexed as leaves.
        let is_dir = dirent.file_type().is_dir();
        let id = index.insert(parent, name, is_dir)?;
        if is_dir {
            dir_ids.insert(dirent.path(), id);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn build(root: &Path) -> VolumeIndex {
        FsWalkSource::default().bootstrap(root).unwrap()
    }

    #[test]
    fn indexes_nested_tree_with_correct_paths() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("a/b")).unwrap();
        fs::write(dir.path().join("a/b/deep.txt"), b"x").unwrap();
        fs::write(dir.path().join("top.txt"), b"x").unwrap();

        let index = build(dir.path());
        assert_eq!(index.len(), 4); // a, a/b, deep.txt, top.txt

        let hits = index.search("deep", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(
            index.path_of(hits[0].id).unwrap(),
            dir.path().canonicalize().unwrap().join("a/b/deep.txt")
        );
        assert!(
            index
                .is_dir(index.resolve(Path::new("a/b")).unwrap())
                .unwrap()
        );
    }

    #[test]
    fn bootstrap_cancellable_aborts_when_flagged() {
        use std::sync::atomic::AtomicBool;
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("a/b")).unwrap();
        fs::write(dir.path().join("a/b/deep.txt"), b"x").unwrap();

        // A pre-set flag aborts the walk with an error (no partial index).
        let err = FsWalkSource::default()
            .bootstrap_cancellable(dir.path(), Some(&AtomicBool::new(true)))
            .unwrap_err();
        assert!(err.to_string().contains("cancelled"), "{err}");

        // A clear flag indexes normally, same as `bootstrap`.
        let index = FsWalkSource::default()
            .bootstrap_cancellable(dir.path(), Some(&AtomicBool::new(false)))
            .unwrap();
        assert_eq!(index.len(), 3); // a, a/b, deep.txt
    }

    #[test]
    fn empty_root_yields_empty_index() {
        let dir = tempfile::tempdir().unwrap();
        let index = build(dir.path());
        assert!(index.is_empty());
    }

    #[test]
    fn missing_root_is_an_error() {
        let err = FsWalkSource::default()
            .bootstrap(Path::new("/nonexistent/filex-walker-test"))
            .unwrap_err();
        assert!(err.to_string().contains("filex-walker-test"));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_directories_are_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("real")).unwrap();
        fs::write(dir.path().join("real/inner.txt"), b"x").unwrap();
        // Cycle: link inside the tree pointing back at the tree root.
        std::os::unix::fs::symlink(dir.path(), dir.path().join("real/loop")).unwrap();

        let index = build(dir.path());
        // real, inner.txt, loop (as a leaf) — and nothing beyond the link.
        assert_eq!(index.len(), 3);
        let loop_id = index.resolve(Path::new("real/loop")).unwrap();
        assert_eq!(index.is_dir(loop_id), Some(false));
    }

    #[test]
    fn skip_system_dirs_excludes_top_level_os_dirs() {
        // The current platform's first excluded top dir (e.g. "System" on
        // macOS); "user" matches no platform's system list.
        let sys = crate::index::SYSTEM_TOP_DIRS[0];
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(sys).join("deep")).unwrap();
        fs::write(dir.path().join(sys).join("deep/lib.so"), b"x").unwrap();
        fs::write(dir.path().join("user-file.txt"), b"x").unwrap();

        // Default (skip_system_dirs = true): the whole system subtree is out.
        let index = FsWalkSource::default().bootstrap(dir.path()).unwrap();
        assert_eq!(index.len(), 1, "only user-file.txt");
        assert!(index.search("lib", 10).is_empty());
        assert_eq!(index.search("user-file", 10).len(), 1);

        // Opt back in: everything indexed again.
        let index = FsWalkSource {
            skip_hidden: false,
            skip_system_dirs: false,
        }
        .bootstrap(dir.path())
        .unwrap();
        assert_eq!(index.len(), 4); // sys, deep, lib.so, user-file.txt
        assert_eq!(index.search("lib", 10).len(), 1);
    }

    #[test]
    fn skip_hidden_excludes_dotfiles() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".hidden"), b"x").unwrap();
        fs::write(dir.path().join("visible.txt"), b"x").unwrap();

        let index = FsWalkSource {
            skip_hidden: true,
            skip_system_dirs: false,
        }
        .bootstrap(dir.path())
        .unwrap();
        assert_eq!(index.len(), 1);
        assert!(index.search("hidden", 10).is_empty());

        let index = build(dir.path());
        assert_eq!(index.len(), 2);
    }
}

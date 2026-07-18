//! Reversible file operations (Phase 2a block 3).
//!
//! Architecture (docs/roadmap.md): operations touch only the
//! filesystem, never the index — the platform watchers observe the
//! changes and feed the index as ordinary deltas, so there is nothing
//! to keep consistent here.
//!
//! [`apply`] executes a [`FileOp`] and returns the [`AppliedOp`]
//! carrying exactly what [`undo`] needs to reverse it; the [`Journal`]
//! is the bounded undo stack the app records into. Everything here is
//! blocking I/O — call it on a background executor, never the UI
//! thread.
//!
//! Not yet here (later block-3 slices): conflict resolution (a
//! destination that exists is an error, not a prompt) and
//! progress/cancellation for long copies.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};

/// A file operation as requested by the UI. Destinations are full
/// paths (not parent directories).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileOp {
    /// Move `from` to `to` (rename when possible, copy+delete across
    /// filesystems).
    Move { from: PathBuf, to: PathBuf },
    /// Copy `from` (file or directory tree) to `to`.
    Copy { from: PathBuf, to: PathBuf },
    /// Rename `path` to `new_name` within its parent directory.
    Rename { path: PathBuf, new_name: String },
    /// Move `path` (absolute) to the OS trash.
    Delete { path: PathBuf },
}

/// A completed operation, carrying what undo needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppliedOp {
    Moved { from: PathBuf, to: PathBuf },
    /// Undo removes the copy. The original is never touched.
    Copied { to: PathBuf },
    Renamed { from: PathBuf, to: PathBuf },
    Deleted { original: PathBuf, restore: TrashRestore },
}

/// What undo needs to bring a trashed item back — shaped by what each
/// OS reports (see the `trash_backend` modules).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrashRestore {
    /// The item's exact location inside the trash (macOS:
    /// NSFileManager reports it); restore is a rename back.
    TrashedAt(PathBuf),
    /// The item's identity in the OS trash database (Windows Recycle
    /// Bin / freedesktop trash); restore goes through the OS.
    Item {
        id: std::ffi::OsString,
        name: std::ffi::OsString,
        original_parent: PathBuf,
        time_deleted: i64,
    },
    /// Trashed, but the OS didn't identify the item for restore; undo
    /// reports that instead of guessing.
    Unknown,
}

impl AppliedOp {
    /// Short human label for notices ("renamed x", "undid move").
    pub fn describe(&self) -> String {
        match self {
            Self::Moved { to, .. } => format!("moved to {}", to.display()),
            Self::Copied { to } => format!("copied to {}", to.display()),
            Self::Renamed { from, to } => format!(
                "renamed {} to {}",
                from.file_name().unwrap_or_default().to_string_lossy(),
                to.file_name().unwrap_or_default().to_string_lossy()
            ),
            Self::Deleted { original, .. } => format!(
                "moved {} to the trash",
                original.file_name().unwrap_or_default().to_string_lossy()
            ),
        }
    }
}

/// Execute `op`. Blocking — run on a background executor.
pub fn apply(op: &FileOp) -> Result<AppliedOp> {
    match op {
        FileOp::Move { from, to } => {
            ensure_target_free(to)?;
            move_path(from, to)?;
            Ok(AppliedOp::Moved { from: from.clone(), to: to.clone() })
        }
        FileOp::Copy { from, to } => {
            ensure_target_free(to)?;
            copy_recursively(from, to)?;
            Ok(AppliedOp::Copied { to: to.clone() })
        }
        FileOp::Rename { path, new_name } => {
            let to = rename_target(path, new_name)?;
            ensure_target_free(&to)?;
            std::fs::rename(path, &to)
                .with_context(|| format!("renaming {}", path.display()))?;
            Ok(AppliedOp::Renamed { from: path.clone(), to })
        }
        FileOp::Delete { path } => {
            let restore = trash_backend::delete_to_trash(path)?;
            Ok(AppliedOp::Deleted { original: path.clone(), restore })
        }
    }
}

/// Reverse a completed operation. Blocking — run on a background
/// executor. Undoing a copy removes the copy (delete-to-trash will
/// soften this in a later slice).
pub fn undo(applied: &AppliedOp) -> Result<()> {
    match applied {
        AppliedOp::Moved { from, to } => {
            ensure_target_free(from)?;
            move_path(to, from)
        }
        AppliedOp::Copied { to } => {
            let meta = std::fs::symlink_metadata(to)
                .with_context(|| format!("inspecting {}", to.display()))?;
            if meta.is_dir() {
                std::fs::remove_dir_all(to)
                    .with_context(|| format!("removing copied dir {}", to.display()))
            } else {
                std::fs::remove_file(to)
                    .with_context(|| format!("removing copied file {}", to.display()))
            }
        }
        AppliedOp::Renamed { from, to } => {
            ensure_target_free(from)?;
            std::fs::rename(to, from)
                .with_context(|| format!("renaming {} back", to.display()))
        }
        AppliedOp::Deleted { original, restore } => {
            trash_backend::restore_from_trash(restore, original)
        }
    }
}

/// Bounded undo stack. Owned by the app (UI thread); the disk work of
/// [`undo`] happens elsewhere, so recording and popping are plain,
/// instant list operations.
#[derive(Debug, Default)]
pub struct Journal {
    applied: Vec<AppliedOp>,
}

/// Oldest entries fall off past this; unbounded growth serves nobody.
const JOURNAL_CAP: usize = 100;

impl Journal {
    pub fn record(&mut self, op: AppliedOp) {
        if self.applied.len() == JOURNAL_CAP {
            self.applied.remove(0);
        }
        self.applied.push(op);
    }

    /// The most recent operation, removed. The caller runs [`undo`] on
    /// it and should [`Journal::record`]-like push it back (via
    /// [`Journal::restore`]) if the disk undo fails.
    pub fn pop(&mut self) -> Option<AppliedOp> {
        self.applied.pop()
    }

    /// Put a popped entry back (its disk undo failed; the user can
    /// retry after fixing the cause).
    pub fn restore(&mut self, op: AppliedOp) {
        self.applied.push(op);
    }

    pub fn is_empty(&self) -> bool {
        self.applied.is_empty()
    }
}

/// The destination of a rename: same parent, new file name. Rejects
/// names that are empty or contain path separators.
fn rename_target(path: &Path, new_name: &str) -> Result<PathBuf> {
    if new_name.is_empty() {
        bail!("name can't be empty");
    }
    if new_name.contains(['/', '\\']) || new_name == "." || new_name == ".." {
        bail!("{new_name:?} is not a valid file name");
    }
    let parent = path.parent().context("path has no parent directory")?;
    Ok(parent.join(new_name))
}

fn ensure_target_free(to: &Path) -> Result<()> {
    // symlink_metadata: a dangling symlink at the target still blocks.
    if std::fs::symlink_metadata(to).is_ok() {
        bail!("{} already exists", to.display());
    }
    Ok(())
}

/// Rename when the OS allows it; fall back to copy+delete when the
/// destination is on another filesystem (EXDEV).
fn move_path(from: &Path, to: &Path) -> Result<()> {
    match std::fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::CrossesDevices => {
            copy_recursively(from, to)?;
            let meta = std::fs::symlink_metadata(from)
                .with_context(|| format!("inspecting {}", from.display()))?;
            if meta.is_dir() {
                std::fs::remove_dir_all(from)
            } else {
                std::fs::remove_file(from)
            }
            .with_context(|| format!("removing {} after cross-device move", from.display()))
        }
        Err(err) => {
            Err(err).with_context(|| format!("moving {} to {}", from.display(), to.display()))
        }
    }
}

/// Copy a file or directory tree. Symlinks are followed (their targets
/// are copied) — same as `std::fs::copy`; preserving links is a later
/// refinement if it ever matters in practice.
fn copy_recursively(from: &Path, to: &Path) -> Result<()> {
    let meta =
        std::fs::metadata(from).with_context(|| format!("inspecting {}", from.display()))?;
    if meta.is_dir() {
        std::fs::create_dir(to).with_context(|| format!("creating {}", to.display()))?;
        for dirent in
            std::fs::read_dir(from).with_context(|| format!("reading {}", from.display()))?
        {
            let dirent = dirent.with_context(|| format!("reading {}", from.display()))?;
            copy_recursively(&dirent.path(), &to.join(dirent.file_name()))?;
        }
        Ok(())
    } else {
        std::fs::copy(from, to)
            .map(drop)
            .with_context(|| format!("copying {} to {}", from.display(), to.display()))
    }
}

/// macOS trash backend. Calls NSFileManager's `trashItemAtURL` with
/// the `resultingItemURL` out-parameter — the OS reports exactly where
/// the item landed in the Trash, so restore is a plain rename back.
/// (The `trash` crate was evaluated first, per the roadmap: its
/// NSFileManager path discards that URL and its Finder path shells out
/// to osascript with permission prompts — neither can undo.)
#[cfg(target_os = "macos")]
mod trash_backend {
    use super::*;
    use objc2_foundation::{NSFileManager, NSString, NSURL};

    pub fn delete_to_trash(path: &Path) -> Result<TrashRestore> {
        let Some(path_str) = path.to_str() else {
            // Mirrors the index's documented non-UTF-8 stance.
            bail!("{} has a non-UTF-8 name; trashing it isn't supported yet", path.display());
        };
        let manager = NSFileManager::defaultManager();
        let url = NSURL::fileURLWithPath(&NSString::from_str(path_str));
        let mut resulting = None;
        manager
            .trashItemAtURL_resultingItemURL_error(&url, Some(&mut resulting))
            .map_err(|err| {
                anyhow::anyhow!("moving {} to the Trash: {err}", path.display())
            })?;
        Ok(resulting
            .and_then(|url| url.path())
            .map(|s| TrashRestore::TrashedAt(PathBuf::from(s.to_string())))
            .unwrap_or(TrashRestore::Unknown))
    }

    pub fn restore_from_trash(restore: &TrashRestore, original: &Path) -> Result<()> {
        match restore {
            TrashRestore::TrashedAt(trashed) => {
                ensure_target_free(original)?;
                std::fs::rename(trashed, original).with_context(|| {
                    format!("restoring {} from the Trash", original.display())
                })
            }
            TrashRestore::Item { .. } => {
                bail!("this restore handle is from another platform's trash")
            }
            TrashRestore::Unknown => {
                bail!("the Trash didn't report where this item went; restore it manually")
            }
        }
    }
}

/// Windows / Linux trash backend via the `trash` crate: Recycle Bin
/// (IFileOperation) and the freedesktop trash spec respectively. After
/// deleting, the item is looked up in the OS trash listing (newest
/// entry whose original path matches) so undo can restore it through
/// the OS. Paths are assumed absolute — the app always browses
/// absolute paths, and canonicalizing here would wrongly resolve a
/// symlink to its target before trashing.
#[cfg(not(target_os = "macos"))]
mod trash_backend {
    use super::*;

    pub fn delete_to_trash(path: &Path) -> Result<TrashRestore> {
        trash::delete(path)
            .with_context(|| format!("moving {} to the trash", path.display()))?;
        let newest_match = trash::os_limited::list().ok().and_then(|items| {
            items
                .into_iter()
                .filter(|item| item.original_path() == path)
                .max_by_key(|item| item.time_deleted)
        });
        Ok(newest_match
            .map(|item| TrashRestore::Item {
                id: item.id,
                name: item.name,
                original_parent: item.original_parent,
                time_deleted: item.time_deleted,
            })
            .unwrap_or(TrashRestore::Unknown))
    }

    pub fn restore_from_trash(restore: &TrashRestore, original: &Path) -> Result<()> {
        match restore {
            TrashRestore::Item { id, name, original_parent, time_deleted } => {
                ensure_target_free(original)?;
                let item = trash::TrashItem {
                    id: id.clone(),
                    name: name.clone(),
                    original_parent: original_parent.clone(),
                    time_deleted: *time_deleted,
                };
                trash::os_limited::restore_all([item]).with_context(|| {
                    format!("restoring {} from the trash", original.display())
                })
            }
            TrashRestore::TrashedAt(_) => {
                bail!("this restore handle is from another platform's trash")
            }
            TrashRestore::Unknown => {
                bail!("the trash didn't identify this item; restore it manually")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(path: &Path, contents: &str) {
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn move_then_undo_restores_the_original_path() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("a.txt");
        let to = dir.path().join("b.txt");
        write(&from, "hello");

        let applied = apply(&FileOp::Move { from: from.clone(), to: to.clone() }).unwrap();
        assert!(!from.exists() && to.exists());

        undo(&applied).unwrap();
        assert!(from.exists() && !to.exists());
        assert_eq!(fs::read_to_string(&from).unwrap(), "hello");
    }

    #[test]
    fn move_refuses_to_clobber() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("a.txt");
        let to = dir.path().join("b.txt");
        write(&from, "a");
        write(&to, "b");

        let err = apply(&FileOp::Move { from, to: to.clone() }).unwrap_err();
        assert!(err.to_string().contains("already exists"));
        assert_eq!(fs::read_to_string(&to).unwrap(), "b");
    }

    #[test]
    fn copy_directory_tree_then_undo_removes_only_the_copy() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs::create_dir_all(src.join("nested")).unwrap();
        write(&src.join("top.txt"), "top");
        write(&src.join("nested/deep.txt"), "deep");
        let dst = dir.path().join("dst");

        let applied = apply(&FileOp::Copy { from: src.clone(), to: dst.clone() }).unwrap();
        assert_eq!(fs::read_to_string(dst.join("nested/deep.txt")).unwrap(), "deep");

        undo(&applied).unwrap();
        assert!(!dst.exists());
        assert_eq!(fs::read_to_string(src.join("top.txt")).unwrap(), "top");
    }

    #[test]
    fn rename_then_undo_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.txt");
        write(&path, "x");

        let applied =
            apply(&FileOp::Rename { path: path.clone(), new_name: "new.txt".into() }).unwrap();
        assert_eq!(applied, AppliedOp::Renamed {
            from: path.clone(),
            to: dir.path().join("new.txt")
        });
        assert!(dir.path().join("new.txt").exists());

        undo(&applied).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn rename_rejects_bad_names_and_collisions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        write(&path, "a");
        write(&dir.path().join("b.txt"), "b");

        for bad in ["", "x/y", "x\\y", ".", ".."] {
            let err = apply(&FileOp::Rename { path: path.clone(), new_name: bad.into() })
                .unwrap_err();
            assert!(!err.to_string().is_empty(), "{bad:?} should be rejected");
        }
        let err = apply(&FileOp::Rename { path: path.clone(), new_name: "b.txt".into() })
            .unwrap_err();
        assert!(err.to_string().contains("already exists"));
        assert!(path.exists());
    }

    #[test]
    fn undo_move_refuses_when_the_original_path_is_reoccupied() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("a.txt");
        let to = dir.path().join("b.txt");
        write(&from, "moved");
        let applied = apply(&FileOp::Move { from: from.clone(), to }).unwrap();
        write(&from, "squatter");

        let err = undo(&applied).unwrap_err();
        assert!(err.to_string().contains("already exists"));
        assert_eq!(fs::read_to_string(&from).unwrap(), "squatter");
    }

    /// Exercises the real OS trash. Environments without a usable
    /// trash (containerized CI, tmpfs test dirs on another mount than
    /// the home trash) skip gracefully, same pattern as the platform
    /// watcher tests.
    #[test]
    fn delete_to_trash_then_undo_restores() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("filex-trash-roundtrip.txt");
        write(&victim, "bye");

        let applied = match apply(&FileOp::Delete { path: victim.clone() }) {
            Ok(applied) => applied,
            Err(err) => {
                eprintln!("skipping trash test (no usable trash here): {err:#}");
                return;
            }
        };
        assert!(!victim.exists(), "delete left the file in place");
        if matches!(&applied, AppliedOp::Deleted { restore: TrashRestore::Unknown, .. }) {
            eprintln!("skipping restore assertion (trash didn't identify the item)");
            return;
        }

        undo(&applied).unwrap();
        assert!(victim.exists(), "undo didn't restore the file");
        assert_eq!(fs::read_to_string(&victim).unwrap(), "bye");
    }

    #[test]
    fn journal_caps_and_pops_in_lifo_order() {
        let mut journal = Journal::default();
        assert!(journal.is_empty());
        for ix in 0..(JOURNAL_CAP + 10) {
            journal.record(AppliedOp::Copied { to: PathBuf::from(format!("f{ix}")) });
        }
        // Newest first...
        let top = journal.pop().unwrap();
        assert_eq!(top, AppliedOp::Copied { to: PathBuf::from(format!("f{}", JOURNAL_CAP + 9)) });
        // ...and the oldest 10 fell off the bottom.
        let mut count = 1;
        while let Some(op) = journal.pop() {
            count += 1;
            let AppliedOp::Copied { to } = &op else { panic!() };
            assert_ne!(to, &PathBuf::from("f9"), "f0..f9 should have been evicted");
        }
        assert_eq!(count, JOURNAL_CAP);
    }

    #[test]
    fn journal_restore_puts_a_failed_undo_back_on_top() {
        let mut journal = Journal::default();
        journal.record(AppliedOp::Copied { to: PathBuf::from("a") });
        let popped = journal.pop().unwrap();
        journal.restore(popped.clone());
        assert_eq!(journal.pop(), Some(popped));
    }
}

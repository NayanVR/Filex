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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use anyhow::{Context as _, Result, bail};

/// Shared handle between a running operation and the UI: byte-level
/// progress plus a cancellation flag, all atomics so the background
/// worker and the render loop never lock.
#[derive(Debug, Default)]
pub struct OpProgress {
    copied: AtomicU64,
    total: AtomicU64,
    cancel: AtomicBool,
}

impl OpProgress {
    /// Completed fraction in 0..=1; `None` until the total is known
    /// (the pre-copy size walk hasn't finished).
    pub fn fraction(&self) -> Option<f32> {
        let total = self.total.load(Ordering::Relaxed);
        if total == 0 {
            return None;
        }
        Some((self.copied.load(Ordering::Relaxed) as f32 / total as f32).min(1.))
    }

    pub fn request_cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    fn canceled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

/// Marker error for a user-requested cancellation, so callers can
/// report "canceled" instead of a failure. Detect with
/// `err.is::<OpCanceled>()`.
#[derive(Debug)]
pub struct OpCanceled;

impl std::fmt::Display for OpCanceled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "operation canceled")
    }
}

impl std::error::Error for OpCanceled {}

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

impl FileOp {
    /// Where this operation wants to create something — the path a
    /// conflict check should probe. Deletes create nothing.
    pub fn destination(&self) -> Option<PathBuf> {
        match self {
            Self::Move { to, .. } | Self::Copy { to, .. } => Some(to.clone()),
            Self::Rename { path, new_name } => rename_target(path, new_name).ok(),
            Self::Delete { .. } => None,
        }
    }

    /// The same operation aimed at a different destination (conflict
    /// resolution's "keep both"). No-op for deletes.
    pub fn with_destination(self, dest: PathBuf) -> Self {
        match self {
            Self::Move { from, .. } => Self::Move { from, to: dest },
            Self::Copy { from, .. } => Self::Copy { from, to: dest },
            Self::Rename { path, .. } => {
                let new_name =
                    dest.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                Self::Rename { path, new_name }
            }
            op @ Self::Delete { .. } => op,
        }
    }
}

/// First free "name 2.ext"-style variant of an occupied destination
/// (Finder's convention). Counts up from 2; multi-part extensions
/// split at the last dot ("x.tar.gz" → "x.tar 2.gz"), same as Finder.
pub fn next_free_name(dest: &Path) -> Result<PathBuf> {
    let parent = dest.parent().context("destination has no parent directory")?;
    let stem = dest.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = dest.extension().map(|e| e.to_string_lossy().into_owned());
    for n in 2..10_000u32 {
        let name = match &ext {
            Some(ext) => format!("{stem} {n}.{ext}"),
            None => format!("{stem} {n}"),
        };
        let candidate = parent.join(name);
        if std::fs::symlink_metadata(&candidate).is_err() {
            return Ok(candidate);
        }
    }
    bail!("no free name near {}", dest.display())
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
    apply_with_progress(op, &OpProgress::default())
}

/// Execute `op`, reporting byte progress and honoring cancellation for
/// the copying paths (copy, and cross-device move). A canceled or
/// failed copy removes its partial destination; a canceled cross-
/// device move additionally leaves the source untouched. Blocking —
/// run on a background executor.
pub fn apply_with_progress(op: &FileOp, progress: &OpProgress) -> Result<AppliedOp> {
    match op {
        FileOp::Move { from, to } => {
            ensure_target_free(to)?;
            move_path(from, to, progress)?;
            Ok(AppliedOp::Moved { from: from.clone(), to: to.clone() })
        }
        FileOp::Copy { from, to } => {
            ensure_target_free(to)?;
            progress.total.store(tree_size(from)?, Ordering::Relaxed);
            if let Err(err) = copy_recursively(from, to, progress) {
                remove_any(to);
                return Err(err);
            }
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

/// Reverse a whole batch (one user action — e.g. a multi-select delete)
/// in reverse order. Stops at the first failure and reports it; anything
/// already reversed stays reversed, so the caller should surface the
/// error rather than blindly retrying. Blocking — run off-thread.
pub fn undo_batch(batch: &[AppliedOp]) -> Result<()> {
    for applied in batch.iter().rev() {
        undo(applied)?;
    }
    Ok(())
}

/// Reverse a completed operation. Blocking — run on a background
/// executor. Undoing a copy removes the copy (delete-to-trash will
/// soften this in a later slice).
pub fn undo(applied: &AppliedOp) -> Result<()> {
    match applied {
        AppliedOp::Moved { from, to } => {
            ensure_target_free(from)?;
            move_path(to, from, &OpProgress::default())
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
    /// Each entry is one user action. A single op (rename, one paste) is
    /// a one-element batch; a multi-select action (delete N, paste N)
    /// records the whole set so one undo reverses all of it in order.
    batches: Vec<Vec<AppliedOp>>,
}

/// Oldest entries fall off past this; unbounded growth serves nobody.
const JOURNAL_CAP: usize = 100;

impl Journal {
    /// Record one user action's completed ops as a single undo unit.
    /// An empty batch (nothing succeeded) is not recorded.
    pub fn record(&mut self, batch: Vec<AppliedOp>) {
        if batch.is_empty() {
            return;
        }
        if self.batches.len() == JOURNAL_CAP {
            self.batches.remove(0);
        }
        self.batches.push(batch);
    }

    /// The most recent action's batch, removed. The caller runs
    /// [`undo_batch`] on it and should push it back (via
    /// [`Journal::restore`]) if the disk undo fails.
    pub fn pop(&mut self) -> Option<Vec<AppliedOp>> {
        self.batches.pop()
    }

    /// Put a popped batch back (its disk undo failed; the user can retry
    /// after fixing the cause).
    pub fn restore(&mut self, batch: Vec<AppliedOp>) {
        self.batches.push(batch);
    }

    pub fn is_empty(&self) -> bool {
        self.batches.is_empty()
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
/// destination is on another filesystem (EXDEV). Only the fallback
/// reports progress — a same-device rename is instant.
fn move_path(from: &Path, to: &Path, progress: &OpProgress) -> Result<()> {
    match std::fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::CrossesDevices => {
            progress.total.store(tree_size(from)?, Ordering::Relaxed);
            if let Err(err) = copy_recursively(from, to, progress) {
                remove_any(to);
                return Err(err);
            }
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

/// Total bytes a copy of `path` will write (files only; directory
/// entries themselves count as zero).
fn tree_size(path: &Path) -> Result<u64> {
    let meta = std::fs::metadata(path).with_context(|| format!("inspecting {}", path.display()))?;
    if !meta.is_dir() {
        return Ok(meta.len());
    }
    let mut sum = 0;
    for dirent in std::fs::read_dir(path).with_context(|| format!("reading {}", path.display()))? {
        let dirent = dirent.with_context(|| format!("reading {}", path.display()))?;
        sum += tree_size(&dirent.path())?;
    }
    Ok(sum)
}

/// Copy a file or directory tree, counting bytes into `progress` and
/// stopping (with [`OpCanceled`]) when cancellation is requested.
/// Symlinks are followed (their targets are copied) — same as
/// `std::fs::copy`; preserving links is a later refinement if it ever
/// matters in practice.
fn copy_recursively(from: &Path, to: &Path, progress: &OpProgress) -> Result<()> {
    if progress.canceled() {
        return Err(anyhow::Error::new(OpCanceled));
    }
    let meta =
        std::fs::metadata(from).with_context(|| format!("inspecting {}", from.display()))?;
    if meta.is_dir() {
        std::fs::create_dir(to).with_context(|| format!("creating {}", to.display()))?;
        for dirent in
            std::fs::read_dir(from).with_context(|| format!("reading {}", from.display()))?
        {
            let dirent = dirent.with_context(|| format!("reading {}", from.display()))?;
            copy_recursively(&dirent.path(), &to.join(dirent.file_name()), progress)?;
        }
        Ok(())
    } else {
        copy_file_chunked(from, to, progress)
    }
}

/// Chunk size for cancellable file copies: big enough to stream fast,
/// small enough that cancellation lands promptly mid-file.
const COPY_CHUNK: usize = 1 << 20;

fn copy_file_chunked(from: &Path, to: &Path, progress: &OpProgress) -> Result<()> {
    use std::io::{Read as _, Write as _};
    let mut src =
        std::fs::File::open(from).with_context(|| format!("opening {}", from.display()))?;
    let mut dst =
        std::fs::File::create(to).with_context(|| format!("creating {}", to.display()))?;
    let mut buf = vec![0u8; COPY_CHUNK];
    loop {
        if progress.canceled() {
            return Err(anyhow::Error::new(OpCanceled));
        }
        let read = src.read(&mut buf).with_context(|| format!("reading {}", from.display()))?;
        if read == 0 {
            break;
        }
        dst.write_all(&buf[..read]).with_context(|| format!("writing {}", to.display()))?;
        progress.copied.fetch_add(read as u64, Ordering::Relaxed);
    }
    // `fs::copy` preserves permissions; the chunked path must too.
    let perms = std::fs::metadata(from)
        .with_context(|| format!("inspecting {}", from.display()))?
        .permissions();
    std::fs::set_permissions(to, perms)
        .with_context(|| format!("setting permissions on {}", to.display()))
}

/// Best-effort removal of a partial destination after a canceled or
/// failed copy. The copy error is what the user needs to see, so
/// cleanup problems are only logged.
fn remove_any(path: &Path) {
    let result = match std::fs::symlink_metadata(path) {
        Err(_) => return, // never created
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
    };
    if let Err(err) = result {
        tracing::warn!("couldn't clean up partial copy at {}: {err}", path.display());
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

    #[test]
    fn copy_reports_byte_progress() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs::create_dir(&src).unwrap();
        write(&src.join("a.txt"), "12345");
        write(&src.join("b.txt"), "123");
        let progress = OpProgress::default();

        assert_eq!(progress.fraction(), None, "no total before the copy starts");
        apply_with_progress(
            &FileOp::Copy { from: src, to: dir.path().join("dst") },
            &progress,
        )
        .unwrap();
        assert_eq!(progress.total.load(Ordering::Relaxed), 8);
        assert_eq!(progress.copied.load(Ordering::Relaxed), 8);
        assert_eq!(progress.fraction(), Some(1.0));
    }

    #[test]
    fn canceled_copy_reports_cancel_and_leaves_no_partial() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs::create_dir(&src).unwrap();
        write(&src.join("a.txt"), "data");
        let dst = dir.path().join("dst");
        let progress = OpProgress::default();
        progress.request_cancel();

        let err = apply_with_progress(
            &FileOp::Copy { from: src.clone(), to: dst.clone() },
            &progress,
        )
        .unwrap_err();
        assert!(err.is::<OpCanceled>(), "expected OpCanceled, got: {err:#}");
        assert!(!dst.exists(), "partial destination should be cleaned up");
        assert!(src.join("a.txt").exists(), "source must be untouched");
    }

    #[test]
    fn next_free_name_counts_past_occupied_variants() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("doc.txt");
        write(&dest, "");
        assert_eq!(next_free_name(&dest).unwrap(), dir.path().join("doc 2.txt"));
        write(&dir.path().join("doc 2.txt"), "");
        assert_eq!(next_free_name(&dest).unwrap(), dir.path().join("doc 3.txt"));
        // Extensionless names and dotfiles.
        let bare = dir.path().join("Makefile");
        write(&bare, "");
        assert_eq!(next_free_name(&bare).unwrap(), dir.path().join("Makefile 2"));
    }

    #[test]
    fn destination_and_retarget_line_up() {
        let op = FileOp::Copy { from: "/a/x.txt".into(), to: "/b/x.txt".into() };
        assert_eq!(op.destination(), Some(PathBuf::from("/b/x.txt")));
        let retargeted = op.with_destination("/b/x 2.txt".into());
        assert_eq!(retargeted.destination(), Some(PathBuf::from("/b/x 2.txt")));

        let rename = FileOp::Rename { path: "/a/x.txt".into(), new_name: "y.txt".into() };
        assert_eq!(rename.destination(), Some(PathBuf::from("/a/y.txt")));
        let retargeted = rename.with_destination("/a/y 2.txt".into());
        assert_eq!(retargeted, FileOp::Rename {
            path: "/a/x.txt".into(),
            new_name: "y 2.txt".into()
        });
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

    fn one(name: &str) -> Vec<AppliedOp> {
        vec![AppliedOp::Copied { to: PathBuf::from(name) }]
    }

    #[test]
    fn journal_caps_and_pops_in_lifo_order() {
        let mut journal = Journal::default();
        assert!(journal.is_empty());
        // An empty batch (nothing succeeded) is never recorded.
        journal.record(Vec::new());
        assert!(journal.is_empty());
        for ix in 0..(JOURNAL_CAP + 10) {
            journal.record(one(&format!("f{ix}")));
        }
        // Newest first...
        let top = journal.pop().unwrap();
        assert_eq!(top, one(&format!("f{}", JOURNAL_CAP + 9)));
        // ...and the oldest 10 fell off the bottom.
        let mut count = 1;
        while let Some(batch) = journal.pop() {
            count += 1;
            let [AppliedOp::Copied { to }] = &batch[..] else { panic!() };
            assert_ne!(to, &PathBuf::from("f9"), "f0..f9 should have been evicted");
        }
        assert_eq!(count, JOURNAL_CAP);
    }

    #[test]
    fn journal_records_a_multi_op_batch_as_one_unit() {
        let mut journal = Journal::default();
        let batch =
            vec![AppliedOp::Copied { to: "a".into() }, AppliedOp::Copied { to: "b".into() }];
        journal.record(batch.clone());
        // One user action = one pop, carrying both ops.
        assert_eq!(journal.pop(), Some(batch));
        assert!(journal.is_empty());
    }

    #[test]
    fn journal_restore_puts_a_failed_undo_back_on_top() {
        let mut journal = Journal::default();
        journal.record(one("a"));
        let popped = journal.pop().unwrap();
        journal.restore(popped.clone());
        assert_eq!(journal.pop(), Some(popped));
    }
}

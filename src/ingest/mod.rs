//! Read-only filesystem inspection. Notification adapters never mutate catalogs.
mod platform;

use crate::catalog::segment::{Identity, Record, Root, raw_name};
use anyhow::Result;
use platform::SYSTEM_DIRS;
pub use platform::identity;
#[cfg(target_os = "macos")]
pub use platform::{has_full_disk_access, open_full_disk_access_settings};
use std::{
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

pub fn inspect(path: &Path, id: u64, parent: u64, root: u32) -> Result<Record> {
    let meta = std::fs::symlink_metadata(path)?;
    Ok(Record {
        id,
        parent,
        root,
        name: raw_name(path.file_name().unwrap_or(path.as_os_str())),
        flags: if meta.is_dir() { Record::DIRECTORY } else { 0 }
            | if meta.file_type().is_symlink() {
                Record::SYMLINK
            } else {
                0
            },
        identity: identity(path, &meta),
        size: Some(meta.len()),
        mtime: meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64),
    })
}
/// Iterative walk: bounded directory stack, no full descendant path arena and no
/// symlink traversal. Permission loss is reported rather than silently claiming
/// a complete generation.
///
/// `allocate` receives each entry's path, the ID already allocated to its
/// parent directory (0 for the root) and its native identity. `cancel` is
/// checked before every entry, so a stop lands within one `stat`, not one
/// directory (FIL-27).
pub fn walk(
    root: &Root,
    excluded: &Path,
    mut allocate: impl FnMut(&Path, u64, &Identity) -> u64,
    mut emit: impl FnMut(Record) -> Result<()>,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<usize> {
    let mut skipped = 0;
    let include_system = include_system_files();
    let meta = std::fs::symlink_metadata(&root.path)?;
    let root_identity = identity(&root.path, &meta);
    let root_id = allocate(&root.path, 0, &root_identity);
    emit(inspect(&root.path, root_id, 0, root.id)?)?;
    let mut stack = vec![(root.path.clone(), root_id)];
    let cancelled = || cancel.load(std::sync::atomic::Ordering::Relaxed);
    while let Some((path, parent)) = stack.pop() {
        anyhow::ensure!(!cancelled(), "enumeration cancelled");
        let entries = match std::fs::read_dir(path) {
            Ok(e) => e,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        for entry in entries {
            anyhow::ensure!(!cancelled(), "enumeration cancelled");
            let entry = match entry {
                Ok(e) => e,
                Err(_) => {
                    skipped += 1;
                    continue;
                }
            };
            let path = entry.path();
            if path.starts_with(excluded) || (!include_system && excluded_system(&root.path, &path))
            {
                continue;
            }
            let meta = match std::fs::symlink_metadata(&path) {
                Ok(m) => m,
                Err(_) => {
                    skipped += 1;
                    continue;
                }
            };
            let key = identity(&path, &meta);
            let id = allocate(&path, parent, &key);
            let record = match inspect(&path, id, parent, root.id) {
                Ok(r) => r,
                Err(_) => {
                    skipped += 1;
                    continue;
                }
            };
            let dir = record.is_dir();
            emit(record)?;
            if dir {
                stack.push((path, id));
            }
        }
        std::thread::yield_now();
    }
    Ok(skipped)
}
pub fn validate_new_root(existing: &[PathBuf], path: &Path) -> Result<PathBuf> {
    let canonical = path.canonicalize()?;
    anyhow::ensure!(canonical.is_dir(), "root is not a directory");
    anyhow::ensure!(
        !existing
            .iter()
            .any(|p| canonical.starts_with(p) || p.starts_with(&canonical)),
        "root overlaps an existing root"
    );
    Ok(canonical)
}
/// Normalize existing ancestors without following the final symlink itself.
pub fn canonical_event_path(path: &Path) -> PathBuf {
    if let Some(parent) = path.parent()
        && let (Ok(parent), Some(name)) = (parent.canonicalize(), path.file_name())
    {
        return parent.join(name);
    }
    path.to_path_buf()
}

pub fn include_system_files() -> bool {
    crate::settings::default_settings_file()
        .and_then(|p| crate::settings::Settings::load(&p).ok())
        .is_some_and(|s| s.index_system_files)
}
pub fn excluded_system(root: &Path, path: &Path) -> bool {
    path.strip_prefix(root)
        .ok()
        .and_then(|p| p.components().next())
        .is_some_and(|part| {
            SYSTEM_DIRS
                .iter()
                .any(|s| part.as_os_str().to_string_lossy().eq_ignore_ascii_case(s))
        })
}

#[cfg(test)]
mod tests {
    /// FIL-27 regression: a stop raised mid-directory ends the walk at the
    /// next entry. It used to be checked once per directory, so a stopping
    /// daemon still inspected the rest of a large directory first.
    #[test]
    fn walk_stops_within_a_directory_once_cancelled() {
        use super::{Root, walk};
        use std::sync::atomic::{AtomicBool, Ordering};

        let tree = tempfile::tempdir().unwrap();
        for i in 0..50 {
            std::fs::write(tree.path().join(format!("{i}.txt")), b"x").unwrap();
        }
        let root = Root {
            id: 1,
            path: tree.path().to_path_buf(),
            device: 0,
        };
        let cancel = AtomicBool::new(false);
        let mut emitted = 0;
        let mut parents = Vec::new();
        let result = walk(
            &root,
            &tree.path().join("not-the-database"),
            |_, parent, _| {
                parents.push(parent);
                parents.len() as u64
            },
            |_| {
                emitted += 1;
                if emitted == 2 {
                    cancel.store(true, Ordering::Relaxed);
                }
                Ok(())
            },
            &cancel,
        );
        assert!(result.is_err(), "a cancelled walk is never complete");
        assert_eq!(emitted, 2, "no entry is inspected after the stop");
        assert_eq!(parents, [0, 1], "entries receive their parent's ID");
    }

    /// A walk whose directory permissions were revoked must degrade rather than
    /// fail: skip and count that directory, keep every readable sibling, and
    /// pick the contents up once the grant returns. Only the `chmod` setup is
    /// Unix-specific — Windows denies reads through ACLs and needs its own
    /// setup before these same assertions can run there.
    #[cfg(unix)]
    #[test]
    fn walk_skips_an_unreadable_directory_until_the_grant_returns() {
        use super::{Root, walk};
        use std::{fs, os::unix::fs::PermissionsExt, sync::atomic::AtomicBool};

        let tree = tempfile::tempdir().unwrap();
        fs::create_dir(tree.path().join("open")).unwrap();
        fs::write(tree.path().join("open/readable.txt"), b"a").unwrap();
        let locked = tree.path().join("locked");
        fs::create_dir(&locked).unwrap();
        fs::write(locked.join("hidden.txt"), b"b").unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        // Root bypasses the mode bits outright, so there is nothing to observe.
        if fs::read_dir(&locked).is_ok() {
            return;
        }

        let root = Root {
            id: 1,
            path: tree.path().to_path_buf(),
            device: 0,
        };
        let excluded = tree.path().join("not-the-database");
        let collect = || {
            let mut next = 1u64;
            let mut seen = Vec::new();
            let skipped = walk(
                &root,
                &excluded,
                |_, _, _| {
                    next += 1;
                    next
                },
                |record| {
                    seen.push(String::from_utf8_lossy(&record.name).into_owned());
                    Ok(())
                },
                &AtomicBool::new(false),
            )
            .unwrap();
            (seen, skipped)
        };

        let (seen, skipped) = collect();
        assert_eq!(skipped, 1, "the unreadable directory is counted, not fatal");
        assert!(
            seen.iter().any(|name| name == "readable.txt"),
            "readable siblings are still indexed: {seen:?}"
        );
        assert!(
            seen.iter().any(|name| name == "locked"),
            "the directory entry itself stays visible: {seen:?}"
        );
        assert!(
            !seen.iter().any(|name| name == "hidden.txt"),
            "contents behind the denied grant stay out: {seen:?}"
        );

        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        let (seen, skipped) = collect();
        assert_eq!(skipped, 0, "nothing is skipped once the grant is restored");
        assert!(
            seen.iter().any(|name| name == "hidden.txt"),
            "restored contents are picked up by the next walk: {seen:?}"
        );
    }
}

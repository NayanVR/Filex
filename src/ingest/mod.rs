//! Read-only filesystem inspection. Notification adapters never mutate catalogs.
mod platform;

use crate::catalog::segment::{Identity, Record, Root, raw_name};
use anyhow::Result;
pub use platform::identity;
use platform::{CASE_INSENSITIVE_NAMES, SYSTEM_DIRS};
#[cfg(target_os = "macos")]
pub use platform::{has_full_disk_access, open_full_disk_access_settings};
use std::{
    collections::HashSet,
    ffi::OsStr,
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
    filter: &IndexFilter,
    mut allocate: impl FnMut(&Path, u64, &Identity) -> u64,
    mut emit: impl FnMut(Record) -> Result<()>,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<usize> {
    let mut skipped = 0;
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
            // `file_type` comes from the directory entry itself on most
            // platforms, so the name check costs no extra stat.
            let pruned = !filter.excluded_names.is_empty()
                && entry.file_type().is_ok_and(|t| t.is_dir())
                && filter.excluded_name(&entry.file_name());
            if pruned
                || path.starts_with(excluded)
                || (!filter.include_system && excluded_system(&root.path, &path))
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

/// What the indexer leaves out, resolved from settings: OS folders directly
/// under a root, and (FIL-28) user-listed folder names at any depth.
#[derive(Debug, Clone, Default)]
pub struct IndexFilter {
    include_system: bool,
    /// Case-folded per [`CASE_INSENSITIVE_NAMES`]; empty while the
    /// setting is off.
    excluded_names: HashSet<String>,
}
impl IndexFilter {
    pub fn from_settings(settings: &crate::settings::Settings) -> Self {
        let excluded_names = if settings.exclude_dev_folders {
            settings
                .excluded_folder_names
                .iter()
                .map(|n| fold(n))
                .collect()
        } else {
            HashSet::new()
        };
        Self {
            include_system: settings.index_system_files,
            excluded_names,
        }
    }
    /// The filter from the settings file on disk; defaults when it's
    /// missing or unreadable.
    pub fn load() -> Self {
        crate::settings::default_settings_file()
            .and_then(|p| crate::settings::Settings::load(&p).ok())
            .map(|s| Self::from_settings(&s))
            .unwrap_or_else(|| Self::from_settings(&Default::default()))
    }
    fn excluded_name(&self, name: &OsStr) -> bool {
        !self.excluded_names.is_empty()
            && self.excluded_names.contains(&fold(&name.to_string_lossy()))
    }
    /// Whether a live event path under `root` falls in an excluded area.
    /// Ancestors are matched by name alone; the last component only when
    /// it is a directory, so a *file* named `target` is still indexed.
    pub fn excludes(&self, root: &Path, path: &Path) -> bool {
        if !self.include_system && excluded_system(root, path) {
            return true;
        }
        let Ok(relative) = path.strip_prefix(root) else {
            return false;
        };
        let mut names = relative.iter().peekable();
        while let Some(name) = names.next() {
            if self.excluded_name(name) && (names.peek().is_some() || path.is_dir()) {
                return true;
            }
        }
        false
    }
}
fn fold(name: &str) -> String {
    if CASE_INSENSITIVE_NAMES {
        name.to_lowercase()
    } else {
        name.to_owned()
    }
}
fn excluded_system(root: &Path, path: &Path) -> bool {
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
    fn dev_filter(names: &[&str]) -> super::IndexFilter {
        super::IndexFilter::from_settings(&crate::settings::Settings {
            exclude_dev_folders: true,
            excluded_folder_names: names.iter().map(|n| n.to_string()).collect(),
            ..Default::default()
        })
    }

    /// FIL-28: listed folder names are pruned at any depth (contents
    /// included), files with the same name are kept, and nothing is
    /// pruned while the setting is off.
    #[test]
    fn walk_prunes_excluded_folder_names_at_any_depth() {
        use super::{Root, walk};
        use std::{fs, sync::atomic::AtomicBool};

        let tree = tempfile::tempdir().unwrap();
        fs::create_dir_all(tree.path().join("node_modules/pkg")).unwrap();
        fs::create_dir_all(tree.path().join("app/web/node_modules/lib")).unwrap();
        fs::write(tree.path().join("app/web/node_modules/lib/index.js"), b"x").unwrap();
        fs::write(tree.path().join("app/web/main.js"), b"x").unwrap();
        fs::write(
            tree.path().join("app/node_modules"),
            b"a file, not a folder",
        )
        .unwrap();
        let root = Root {
            id: 1,
            path: tree.path().to_path_buf(),
            device: 0,
        };
        let names = |filter| {
            let mut next = 0;
            let mut seen = Vec::new();
            walk(
                &root,
                &tree.path().join("not-the-database"),
                &filter,
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
            seen
        };

        let seen = names(dev_filter(&["node_modules"]));
        assert!(seen.iter().any(|n| n == "main.js"), "{seen:?}");
        for gone in ["pkg", "lib", "index.js"] {
            assert!(!seen.iter().any(|n| n == gone), "{gone} leaked: {seen:?}");
        }
        assert_eq!(
            seen.iter().filter(|n| *n == "node_modules").count(),
            1,
            "only the same-named file survives: {seen:?}"
        );

        let seen = names(super::IndexFilter::from_settings(
            &crate::settings::Settings {
                exclude_dev_folders: false,
                ..Default::default()
            },
        ));
        assert!(seen.iter().any(|n| n == "index.js"), "off means unchanged");
    }

    #[test]
    fn live_event_paths_under_an_excluded_folder_are_dropped() {
        let tree = tempfile::tempdir().unwrap();
        let root = tree.path();
        std::fs::create_dir_all(root.join("web/target")).unwrap();
        let filter = dev_filter(&["target"]);
        assert!(filter.excludes(root, &root.join("web/target/debug/app")));
        assert!(
            filter.excludes(root, &root.join("web/target")),
            "the folder itself"
        );
        assert!(!filter.excludes(root, &root.join("web/target.rs")));
        assert!(
            !filter.excludes(root, &root.join("web/notes/target")),
            "a missing or file path named target is kept"
        );
        assert!(!super::IndexFilter::default().excludes(root, &root.join("web/target/x")));
    }

    #[test]
    fn folder_names_match_case_per_platform() {
        let tree = tempfile::tempdir().unwrap();
        let path = tree.path().join("App/Node_Modules/x");
        let excluded = dev_filter(&["node_modules"]).excludes(tree.path(), &path);
        assert_eq!(excluded, cfg!(any(windows, target_os = "macos")));
    }

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
            &Default::default(),
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
                &Default::default(),
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

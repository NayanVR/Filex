//! Read-only filesystem inspection. Notification adapters never mutate catalogs.
mod platform;
mod walk;
#[cfg_attr(not(windows), cfg(test))]
mod windows;

use crate::catalog::segment::{Identity, Record, raw_name};
use anyhow::Result;
pub use platform::identity;
use platform::{CASE_INSENSITIVE_NAMES, OTHER_VOLUME_DIRS, SYSTEM_DIRS};
#[cfg(target_os = "macos")]
pub use platform::{has_full_disk_access, open_full_disk_access_settings};
use std::{
    collections::HashSet,
    ffi::OsStr,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};
pub use walk::walk;

pub fn inspect(path: &Path, id: u64, parent: u64, root: u32) -> Result<Record> {
    Ok(record(path, stat(path)?, id, parent, root))
}

/// What a record needs from the filesystem about one entry, symlinks not
/// followed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Stat {
    pub identity: Identity,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub size: u64,
    pub mtime: Option<i64>,
}

/// The [`Stat`] of one path: `lstat` plus its native identity.
fn stat(path: &Path) -> std::io::Result<Stat> {
    let meta = std::fs::symlink_metadata(path)?;
    Ok(Stat {
        identity: identity(path, &meta),
        is_dir: meta.is_dir(),
        is_symlink: meta.file_type().is_symlink(),
        size: meta.len(),
        mtime: meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64),
    })
}

fn record(path: &Path, stat: Stat, id: u64, parent: u64, root: u32) -> Record {
    Record {
        id,
        parent,
        root,
        name: raw_name(path.file_name().unwrap_or(path.as_os_str())),
        flags: if stat.is_dir { Record::DIRECTORY } else { 0 }
            | if stat.is_symlink { Record::SYMLINK } else { 0 },
        identity: stat.identity,
        size: Some(stat.size),
        mtime: stat.mtime,
    }
}

/// One directory entry from [`list`]: its path, whether the directory entry
/// itself says it's a directory (no extra syscall), and its [`Stat`] when
/// the listing already carries it.
struct Listed {
    path: PathBuf,
    is_dir: bool,
    stat: Option<Stat>,
}

/// A directory's entries from [`list`], and the directory's own mtime when
/// the listing read it from the directory itself rather than from its
/// parent's listing (Windows, FIL-47); `None` keeps the parent's value.
struct DirListing {
    mtime: Option<i64>,
    entries: Box<dyn Iterator<Item = std::io::Result<Listed>>>,
}

/// The entries of `dir`. Windows reads each entry's [`Stat`] from the
/// directory handle in batches; elsewhere `readdir` plus a later `lstat`
/// per kept entry is already cheap.
fn list(dir: &Path) -> std::io::Result<DirListing> {
    #[cfg(windows)]
    {
        let entries = windows::list(dir)?;
        Ok(DirListing {
            mtime: entries.mtime,
            entries: Box::new(entries.map(|entry| {
                entry.map(|(path, stat)| Listed {
                    path,
                    is_dir: stat.is_dir,
                    stat: Some(stat),
                })
            })),
        })
    }
    #[cfg(not(windows))]
    {
        Ok(DirListing {
            // The parent's per-entry `lstat` already read it from the inode.
            mtime: None,
            entries: Box::new(std::fs::read_dir(dir)?.map(|entry| {
                entry.map(|entry| Listed {
                    path: entry.path(),
                    // `file_type` comes from the directory entry itself on
                    // most platforms, so the name check costs no extra stat.
                    is_dir: entry.file_type().is_ok_and(|t| t.is_dir()),
                    stat: None,
                })
            })),
        })
    }
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
        if self.excludes_top_level(root, path) {
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
impl IndexFilter {
    /// OS folders (unless the setting includes them) and other-volume
    /// mount folders, matched by the first component under `root`.
    fn excludes_top_level(&self, root: &Path, path: &Path) -> bool {
        (!self.include_system && first_component_in(root, path, SYSTEM_DIRS))
            || first_component_in(root, path, OTHER_VOLUME_DIRS)
    }
}
fn first_component_in(root: &Path, path: &Path, names: &[&str]) -> bool {
    path.strip_prefix(root)
        .ok()
        .and_then(|p| p.components().next())
        .is_some_and(|part| {
            names
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

    /// A whole-disk root (the macOS default) leaves out other volumes even
    /// when OS folders are included, so a `/` root never wanders onto
    /// external or network drives.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_whole_disk_root_skips_os_folders_and_other_volumes() {
        use std::path::Path;
        let root = Path::new("/");
        let default = super::IndexFilter::from_settings(&Default::default());
        assert!(default.excludes(root, Path::new("/System/Library/x")));
        assert!(default.excludes(root, Path::new("/Volumes/Backup/a.txt")));
        assert!(!default.excludes(root, Path::new("/Users/me/notes.txt")));
        assert!(!default.excludes(root, Path::new("/Applications/Safari.app")));

        let with_system = super::IndexFilter::from_settings(&crate::settings::Settings {
            index_system_files: true,
            ..Default::default()
        });
        assert!(!with_system.excludes(root, Path::new("/Library/Fonts")));
        assert!(with_system.excludes(root, Path::new("/Volumes/Backup/a.txt")));
        // An external drive added as its own root is indexed normally.
        assert!(!default.excludes(
            Path::new("/Volumes/Backup"),
            Path::new("/Volumes/Backup/a.txt")
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_indexes_the_whole_startup_disk_by_default() {
        assert_eq!(
            crate::drives::default_index_roots(),
            [std::path::PathBuf::from("/")]
        );
    }

    /// FIL-28: listed folder names are pruned at any depth (contents
    /// included), files with the same name are kept, and nothing is
    /// pruned while the setting is off.
    #[test]
    fn walk_prunes_excluded_folder_names_at_any_depth() {
        use super::walk;
        use crate::catalog::segment::Root;
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
        use super::walk;
        use crate::catalog::segment::Root;
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
        use super::walk;
        use crate::catalog::segment::Root;
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

    /// A walk reuses the metadata and identity it already read for each
    /// entry; its records must match a fresh per-path `inspect`.
    #[test]
    fn walk_records_match_inspect() {
        use super::{inspect, walk};
        use crate::catalog::segment::Root;
        use std::{fs, path::PathBuf, sync::atomic::AtomicBool};

        let tree = tempfile::tempdir().unwrap();
        fs::create_dir_all(tree.path().join("docs/old")).unwrap();
        fs::write(tree.path().join("docs/resume.pdf"), b"pdf").unwrap();
        fs::write(tree.path().join("docs/old/notes.txt"), b"some notes").unwrap();
        let root = Root {
            id: 3,
            path: tree.path().to_path_buf(),
            device: 0,
        };
        let mut paths = std::collections::HashMap::<u64, PathBuf>::new();
        let mut records = Vec::new();
        let mut next = 0;
        walk(
            &root,
            &tree.path().join("not-the-database"),
            &super::IndexFilter::from_settings(&Default::default()),
            |path, _, _| {
                next += 1;
                paths.insert(next, path.to_path_buf());
                next
            },
            |record| {
                records.push(record);
                Ok(())
            },
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(records.len(), 5, "root, 2 dirs, 2 files");
        for record in records {
            let path = &paths[&record.id];
            let fresh = inspect(path, record.id, record.parent, root.id).unwrap();
            assert_eq!(record, fresh, "{}", path.display());
        }
    }

    /// Regression (FIL-47): NTFS refreshes the copy of a directory's
    /// timestamps kept in its parent's index lazily, so a walk that took a
    /// subdirectory's mtime from its parent's listing lagged a write into it
    /// that crossed a second boundary (a folder or file created or removed;
    /// on Windows 11 each of those reproduced it).
    #[test]
    fn directory_mtime_follows_changes_inside_it() {
        use super::{inspect, walk};
        use crate::catalog::segment::Root;
        use std::{
            fs,
            path::Path,
            sync::atomic::AtomicBool,
            time::{Duration, SystemTime, UNIX_EPOCH},
        };

        type Change = (&'static str, fn(&Path));
        let changes: [Change; 3] = [
            ("folder created", |docs| {
                fs::create_dir(docs.join("new")).unwrap()
            }),
            ("file created", |docs| {
                fs::write(docs.join("new.txt"), b"x").unwrap()
            }),
            ("file removed", |docs| {
                fs::remove_file(docs.join("old.txt")).unwrap()
            }),
        ];
        for (change, apply) in changes {
            let tree = tempfile::tempdir().unwrap();
            let docs = tree.path().join("docs");
            fs::create_dir(&docs).unwrap();
            fs::write(docs.join("old.txt"), b"x").unwrap();
            let root = Root {
                id: 3,
                path: tree.path().to_path_buf(),
                device: 0,
            };
            let into_second = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
            std::thread::sleep(
                Duration::from_millis(1050)
                    - Duration::from_nanos(into_second.subsec_nanos().into()),
            );
            apply(&docs);
            let mut records = Vec::new();
            let (mut next, mut docs_id) = (0, 0);
            walk(
                &root,
                &tree.path().join("not-the-database"),
                &super::IndexFilter::from_settings(&Default::default()),
                |path, _, _| {
                    next += 1;
                    if path == docs {
                        docs_id = next;
                    }
                    next
                },
                |record| {
                    records.push(record);
                    Ok(())
                },
                &AtomicBool::new(false),
            )
            .unwrap();
            let walked = records
                .into_iter()
                .find(|r| r.id == docs_id)
                .expect("docs is walked");
            let fresh = inspect(&docs, walked.id, walked.parent, root.id).unwrap();
            assert_eq!(walked.mtime, fresh.mtime, "{change}");
        }
    }

    /// Regression: identity opened files for reading, which made Defender
    /// scan every file a walk touched and failed on files held open without
    /// sharing, leaving them a zero key. An attribute-only open is exempt
    /// from share modes, so an exclusively held file still gets its key.
    #[cfg(windows)]
    #[test]
    fn identity_reads_files_held_open_without_sharing() {
        use std::os::windows::fs::OpenOptionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("locked.dat");
        std::fs::write(&path, b"x").unwrap();
        let meta = std::fs::symlink_metadata(&path).unwrap();
        let _held = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(0)
            .open(&path)
            .unwrap();
        assert!(
            std::fs::File::open(&path).is_err(),
            "the file should refuse data opens while held"
        );
        let identity = super::identity(&path, &meta);
        assert_ne!(identity.key, 0, "{identity:?}");
        assert_ne!(identity.device, 0, "{identity:?}");
    }
}

//! Live-change watches on index roots (FIL-66). Linux registers one inotify
//! watch per directory itself, so excluded areas (folder names, FIL-28;
//! nested network mounts, FIL-62) are never walked or watched: notify's
//! recursive mode walks the whole tree on its own to add its watches.
//! macOS (FSEvents) and Windows (ReadDirectoryChangesW) watch a subtree
//! natively, so they keep the recursive watch and only filter events.
#[cfg(any(target_os = "linux", test))]
mod per_dir;
#[cfg(not(target_os = "linux"))]
mod recursive;

#[cfg(target_os = "linux")]
pub(super) use per_dir::RootWatches;
#[cfg(not(target_os = "linux"))]
pub(super) use recursive::RootWatches;

#[cfg(any(target_os = "linux", test))]
use crate::ingest::IndexFilter;
#[cfg(any(target_os = "linux", test))]
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Receives every watch event and watch error, on notify's thread or (for
/// errors registering watches) the watch registrar's.
pub(super) type Handler = Arc<dyn Fn(notify::Result<notify::Event>) + Send + Sync>;

/// The directories a per-directory watch on `root` needs from `start`
/// down, `start` included: pruned with the same [`IndexFilter`] the index
/// walk uses, symlinks not followed. A directory that can't be read is
/// kept (its own watch may still succeed) but not descended into.
#[cfg(any(target_os = "linux", test))]
pub(super) fn watch_dirs(root: &Path, start: &Path, filter: &IndexFilter) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![start.to_path_buf()];
    while let Some(dir) = pending.pop() {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                // The directory entry's own type: no extra stat, and a
                // symlink to a directory isn't a directory here.
                if entry.file_type().is_ok_and(|t| t.is_dir()) {
                    let path = entry.path();
                    if !filter.excludes_dir(root, &path) {
                        pending.push(path);
                    }
                }
            }
        }
        found.push(dir);
    }
    found
}

#[cfg(test)]
mod tests {
    use super::watch_dirs;
    use crate::ingest::IndexFilter;
    use std::{collections::BTreeSet, fs, path::PathBuf};

    fn names(root: &std::path::Path, dirs: Vec<PathBuf>) -> BTreeSet<String> {
        dirs.iter()
            .map(|d| {
                d.strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect()
    }

    /// FIL-66: the watch walk prunes excluded folder names and nested
    /// network mounts, the way the index walk does, and skips files.
    #[test]
    fn watch_dirs_skip_excluded_folders_and_network_mounts() {
        let tree = tempfile::tempdir().unwrap();
        let root = tree.path();
        for dir in ["src/app", "web/node_modules/pkg", "remote/deep"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        fs::write(root.join("src/main.rs"), b"x").unwrap();
        let filter = IndexFilter::from_settings(&crate::settings::Settings {
            exclude_dev_folders: true,
            excluded_folder_names: vec!["node_modules".into()],
            ..Default::default()
        })
        .with_network_mounts(vec![root.join("remote")]);

        assert_eq!(
            names(root, watch_dirs(root, root, &filter)),
            ["", "src", "src/app", "web"].map(String::from).into()
        );
        assert_eq!(
            names(root, watch_dirs(root, &root.join("src"), &filter)),
            ["src", "src/app"].map(String::from).into(),
            "a walk from a new directory covers just its subtree"
        );
    }

    #[cfg(unix)]
    #[test]
    fn watch_dirs_do_not_follow_symlinks() {
        let tree = tempfile::tempdir().unwrap();
        let root = tree.path();
        fs::create_dir_all(root.join("real/inner")).unwrap();
        std::os::unix::fs::symlink(root.join("real"), root.join("link")).unwrap();
        assert_eq!(
            names(root, watch_dirs(root, root, &IndexFilter::default())),
            ["", "real", "real/inner"].map(String::from).into()
        );
    }
}

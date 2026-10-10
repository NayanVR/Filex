//! Convert advisory filesystem paths into identity-preserving catalog changes.
//! A staging overlay makes later paths in a batch see earlier parent creations.
use super::view::{Overlay, View};
use crate::{
    catalog::{segment::Root, wal::Delta},
    ingest,
};
use anyhow::{Result, ensure};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

fn normalize_path(view: &View, path: &Path, next: &AtomicU64, depth: usize) -> Result<Vec<Delta>> {
    ensure!(depth < 128, "parent depth exceeded");
    let root = view
        .roots
        .iter()
        .find(|r| path.starts_with(&r.path))
        .ok_or_else(|| anyhow::anyhow!("path outside indexed roots"))?;
    let mut changes = Vec::new();
    let parent = if path == root.path {
        0
    } else {
        let parent = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("missing parent"))?;
        if let Some(id) = view.resolve(parent) {
            id
        } else {
            let preceding = normalize_path(view, parent, next, depth + 1)?;
            let id = preceding
                .iter()
                .rev()
                .find_map(|d| {
                    if let Delta::Upsert(r) = d {
                        Some(r.id)
                    } else {
                        None
                    }
                })
                .ok_or_else(|| anyhow::anyhow!("missing parent record"))?;
            changes.extend(preceding);
            id
        }
    };
    let mut record = ingest::inspect(path, 0, parent, root.id)?;
    let existing = view.resolve(path).and_then(|id| view.record(id));
    if let Some(old) = &existing {
        if old.identity == record.identity {
            record.id = old.id;
        } else {
            changes.push(Delta::Delete(old.id));
        }
    }
    if record.id == 0
        && record.identity.birth != 0
        && let Some(id) = view.find_native(root.id, record.identity)
        && view.path(id).is_none_or(|old| old == path || !old.exists())
    {
        record.id = id;
    }
    if record.id == 0 {
        record.id = next.fetch_add(1, Ordering::Relaxed);
    }
    if existing.as_ref() != Some(&record) {
        changes.push(Delta::Upsert(record));
    }
    Ok(changes)
}
pub(super) fn normalize_batch(
    view: &View,
    paths: impl IntoIterator<Item = (PathBuf, bool)>,
    next: &AtomicU64,
) -> (Vec<Delta>, bool) {
    let mut degraded = false;
    let mut working = view.clone();
    working.layers.push(Arc::new(Overlay::default()));
    let mut deltas = Vec::new();
    for (path, remove) in paths {
        if !working.roots.iter().any(|r| path.starts_with(&r.path)) {
            continue;
        }
        let path = ingest::canonical_event_path(&path);
        if remove && let Some(id) = working.resolve(&path) {
            Arc::make_mut(working.layers.last_mut().unwrap()).put(id, None);
            deltas.push(Delta::Delete(id));
        }
        match normalize_path(&working, &path, next, 0) {
            Ok(changes) => {
                for delta in changes {
                    match &delta {
                        Delta::Upsert(r) => Arc::make_mut(working.layers.last_mut().unwrap())
                            .put(r.id, Some(r.clone())),
                        Delta::Delete(id) => {
                            Arc::make_mut(working.layers.last_mut().unwrap()).put(*id, None)
                        }
                        _ => {}
                    }
                    deltas.push(delta);
                }
            }
            Err(_) => {
                match std::fs::symlink_metadata(&path) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        if let Some(id) = working.resolve(&path) {
                            Arc::make_mut(working.layers.last_mut().unwrap()).put(id, None);
                            deltas.push(Delta::Delete(id));
                        }
                    }
                    // An existing but unresolvable file, or an inaccessible
                    // path, needs reconciliation rather than a silent omission.
                    _ => degraded = true,
                }
            }
        }
    }
    (deltas, degraded)
}

/// Descendants of the directories in `paths` that the catalog has never seen.
///
/// A directory moved in from outside the roots produces one notification, not
/// one per descendant, so its contents are listed here. This used to request a
/// reconcile, which re-walks every root: any `mkdir` under a home-directory
/// root cost a full rebuild (FIL-27). Returns `None` once more than `limit`
/// entries are found or a directory cannot be read; the caller reconciles.
/// Symlinks are not followed, matching `ingest::walk`; a directory that has
/// already disappeared again contributes nothing.
pub(super) fn new_trees(
    view: &View,
    paths: &[PathBuf],
    excluded: &Path,
    limit: usize,
) -> Option<Vec<PathBuf>> {
    let is_dir = |p: &Path| std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir());
    let mut pending: Vec<PathBuf> = paths
        .iter()
        .filter(|p| is_dir(p) && view.resolve(&ingest::canonical_event_path(p)).is_none())
        .cloned()
        .collect();
    let mut found = Vec::new();
    while let Some(directory) = pending.pop() {
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return None,
        };
        for entry in entries {
            let path = entry.ok()?.path();
            if path.starts_with(excluded) {
                continue;
            }
            if is_dir(&path) {
                pending.push(path.clone());
            }
            found.push(path);
            if found.len() > limit {
                return None;
            }
        }
    }
    Some(found)
}

pub(super) fn apply(overlay: &mut Overlay, roots: &mut Vec<Root>, delta: Delta) {
    match delta {
        Delta::Upsert(r) => overlay.put(r.id, Some(r)),
        Delta::Delete(id) => overlay.put(id, None),
        Delta::Roots(r) => *roots = r,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::segment::{Record, Segment};

    fn view_of(root: &Path) -> View {
        let roots = vec![Root {
            id: 1,
            path: root.to_path_buf(),
            device: 0,
        }];
        let mut known = ingest::inspect(root, 1, 0, 1).unwrap();
        known.flags = Record::DIRECTORY;
        View {
            base: Arc::new(Segment::build([known], roots.clone(), 0).unwrap()),
            deltas: vec![],
            layers: vec![],
            roots,
            epoch: 0,
        }
    }

    /// FIL-27 regression: a directory the catalog has not seen is listed in
    /// place instead of forcing a full reconcile of every root.
    #[test]
    fn new_directories_are_listed_without_a_reconcile() {
        let tree = tempfile::tempdir().unwrap();
        let root = tree.path().canonicalize().unwrap();
        let view = view_of(&root);
        std::fs::create_dir_all(root.join("moved/inner")).unwrap();
        std::fs::write(root.join("moved/inner/deep.txt"), b"x").unwrap();
        std::fs::write(root.join("moved/top.txt"), b"x").unwrap();
        let excluded = root.join("moved/index");
        std::fs::create_dir(&excluded).unwrap();
        let paths = [
            root.join("moved"),
            root.join("gone"),
            root.join("moved/top.txt"),
        ];
        let mut found = new_trees(&view, &paths, &excluded, 100).unwrap();
        found.sort();
        assert_eq!(
            found,
            [
                root.join("moved/inner"),
                root.join("moved/inner/deep.txt"),
                root.join("moved/top.txt"),
            ]
        );
        assert!(
            new_trees(&view, std::slice::from_ref(&root), &excluded, 100)
                .unwrap()
                .is_empty(),
            "a known directory is not listed"
        );
        assert!(
            new_trees(&view, &paths, &excluded, 2).is_none(),
            "too large: reconcile"
        );
    }

    #[cfg(unix)]
    #[test]
    fn new_tree_listing_does_not_follow_symlinks() {
        let tree = tempfile::tempdir().unwrap();
        let root = tree.path().canonicalize().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("elsewhere.txt"), b"x").unwrap();
        std::fs::create_dir(root.join("new")).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("new/link")).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("link")).unwrap();
        let view = view_of(&root);
        let paths = [root.join("new"), root.join("link")];
        let found = new_trees(&view, &paths, &root.join("index"), 100).unwrap();
        assert_eq!(found, [root.join("new/link")]);
    }
}

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

pub(super) fn apply(overlay: &mut Overlay, roots: &mut Vec<Root>, delta: Delta) {
    match delta {
        Delta::Upsert(r) => overlay.put(r.id, Some(r)),
        Delta::Delete(id) => overlay.put(id, None),
        Delta::Roots(r) => *roots = r,
    }
}

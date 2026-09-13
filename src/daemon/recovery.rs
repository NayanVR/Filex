//! Select a durable generation, replay its WAL tail, and initialize configured roots.
use super::{changes::apply, view::Overlay};
use crate::{
    catalog::{
        manifest,
        segment::{Root, Segment},
        wal::Wal,
    },
    ingest,
};
use anyhow::{Result, ensure};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

pub(super) struct Recovered {
    pub wal: Wal,
    pub roots: Vec<Root>,
    pub base: Arc<Segment>,
    pub sequence: u64,
    pub next_id: u64,
    pub manifest_epoch: u64,
    pub active: Overlay,
    pub current_segment: Option<(PathBuf, u64)>,
}

pub(super) fn recover(directory: &Path, configured: Vec<PathBuf>) -> Result<Recovered> {
    // The owner lock is held by server::run. A previous worker exits when its
    // owner's lifetime pipe closes; none of its scratch files can be published.
    for entry in std::fs::read_dir(directory)?.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.ends_with(".tmp")
            && (name.starts_with("build-")
                || name.starts_with("enumerate-")
                || name.starts_with("manifest-"))
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    let mut selected = None;
    for (path, m) in manifest::candidates(directory)? {
        match (|| -> Result<Segment> {
            let segment = unsafe { Segment::open(&directory.join(&m.segment))? };
            ensure!(
                m.sequence == segment.sequence && m.roots == segment.roots,
                "manifest/segment mismatch"
            );
            Ok(segment)
        })() {
            Ok(segment) => {
                selected = Some((m, segment));
                break;
            }
            Err(e) => {
                tracing::warn!("invalid generation, trying fallback: {e}");
                let _ = std::fs::rename(&path, path.with_extension("invalid"));
            }
        }
    }
    let selected_path = selected
        .as_ref()
        .map(|(m, _)| (directory.join(&m.segment), m.epoch));
    let (mut roots, base, sequence, next_id, manifest_epoch) = if let Some((m, s)) = selected {
        (m.roots, Arc::new(s), m.sequence, m.next_id, m.epoch)
    } else {
        (
            Vec::new(),
            Arc::new(Segment::build([], Vec::new(), 0)?),
            0,
            1,
            0,
        )
    };
    let (mut wal, mut transactions) = match Wal::open(&directory.join("updates.wal")) {
        Ok(w) => w,
        Err(error) => {
            tracing::error!("WAL recovery failed; quarantining and reconciling: {error}");
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos();
            std::fs::rename(
                directory.join("updates.wal"),
                directory.join(format!("corrupt-{stamp}.wal")),
            )?;
            Wal::open(&directory.join("updates.wal"))?
        }
    };
    if transactions
        .iter()
        .find(|tx| tx.sequence > sequence)
        .is_some_and(|tx| tx.sequence != sequence + 1)
    {
        tracing::error!("no recoverable manifest for WAL tail; reconciling configured roots");
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        drop(wal);
        std::fs::rename(
            directory.join("updates.wal"),
            directory.join(format!("orphan-{stamp}.wal")),
        )?;
        (wal, transactions) = Wal::open(&directory.join("updates.wal"))?;
    }
    let mut active = Overlay::default();
    let mut seq = sequence;
    let mut next_id = next_id;
    for tx in transactions.into_iter().filter(|t| t.sequence > sequence) {
        ensure!(tx.sequence == seq + 1, "recovery sequence gap");
        for delta in tx.deltas {
            apply(&mut active, &mut roots, delta);
        }
        seq = tx.sequence;
        next_id = next_id.max(tx.next_id);
    }
    if roots.is_empty() && manifest_epoch == 0 {
        for path in configured {
            if let Ok(path) = path.canonicalize()
                && path.is_dir()
                && !roots
                    .iter()
                    .any(|r: &Root| path.starts_with(&r.path) || r.path.starts_with(&path))
            {
                let meta = std::fs::symlink_metadata(&path)?;
                roots.push(Root {
                    id: roots.len() as u32 + 1,
                    device: ingest::identity(&path, &meta).device,
                    path,
                });
            }
        }
    }
    Ok(Recovered {
        wal,
        roots,
        base,
        sequence: seq,
        next_id,
        manifest_epoch,
        active,
        current_segment: selected_path,
    })
}

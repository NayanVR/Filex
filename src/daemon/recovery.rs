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
    pub deltas: Vec<Arc<Segment>>,
    pub sequence: u64,
    pub next_id: u64,
    pub manifest_epoch: u64,
    pub active: Overlay,
    /// The selected generation's segment files, base first.
    pub files: Vec<PathBuf>,
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
        match open_generation(directory, &m) {
            Ok(segments) => {
                selected = Some((m, segments));
                break;
            }
            Err(e) => {
                tracing::warn!("invalid generation, trying fallback: {e}");
                let _ = std::fs::rename(&path, path.with_extension("invalid"));
            }
        }
    }
    let files = selected.as_ref().map_or_else(Vec::new, |(m, _)| {
        m.files().map(|f| directory.join(f)).collect()
    });
    let (mut roots, base, deltas, sequence, next_id, manifest_epoch) =
        if let Some((m, (base, deltas))) = selected {
            (m.roots, base, deltas, m.sequence, m.next_id, m.epoch)
        } else {
            (
                Vec::new(),
                Arc::new(Segment::build([], Vec::new(), 0)?),
                Vec::new(),
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
        deltas,
        sequence: seq,
        next_id,
        manifest_epoch,
        active,
        files,
    })
}

/// Open a manifest's base and deltas, checking they form one generation:
/// same roots, a base without tombstones, deltas with them, and sequences
/// that increase up to the manifest's.
fn open_generation(
    directory: &Path,
    m: &manifest::Manifest,
) -> Result<(Arc<Segment>, Vec<Arc<Segment>>)> {
    // SAFETY: published generation files are immutable until retired.
    let base = unsafe { Segment::open(&directory.join(&m.segment))? };
    ensure!(
        base.tombstones.is_none() && m.roots == base.roots,
        "manifest/segment mismatch"
    );
    let mut sequence = base.sequence;
    let mut deltas = Vec::new();
    for name in &m.deltas {
        let delta = unsafe { Segment::open(&directory.join(name))? };
        ensure!(
            delta.tombstones.is_some() && delta.roots == m.roots && delta.sequence >= sequence,
            "manifest/delta mismatch"
        );
        sequence = delta.sequence;
        deltas.push(Arc::new(delta));
    }
    ensure!(sequence == m.sequence, "manifest/segment mismatch");
    Ok((Arc::new(base), deltas))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{manifest::Manifest, segment::Record};

    fn entry(id: u64, parent: u64, name: &str, directory: bool) -> Record {
        Record {
            id,
            parent,
            root: 1,
            name: name.as_bytes().to_vec(),
            flags: if directory { Record::DIRECTORY } else { 0 },
            identity: Default::default(),
            size: None,
            mtime: None,
        }
    }

    fn write(
        directory: &Path,
        name: &str,
        records: Vec<Record>,
        sequence: u64,
        tombstones: Option<Vec<u64>>,
    ) {
        let mut segment = Segment::build(records, roots(), sequence).unwrap();
        segment.tombstones = tombstones;
        segment.save(&directory.join(name)).unwrap();
    }

    fn roots() -> Vec<Root> {
        vec![Root {
            id: 1,
            path: "/fixture".into(),
            device: 1,
        }]
    }

    fn manifest(epoch: u64, sequence: u64, segment: &str, deltas: &[&str]) -> Manifest {
        Manifest {
            version: manifest::VERSION,
            epoch,
            sequence,
            next_id: 10,
            segment: segment.into(),
            roots: roots(),
            deltas: deltas.iter().map(|d| d.to_string()).collect(),
        }
    }

    /// FIL-30: a generation is a base plus its deltas; recovery maps all of
    /// them and replays the WAL from the newest delta's sequence.
    #[test]
    fn recovers_a_base_with_its_deltas() {
        let directory = tempfile::tempdir().unwrap();
        let directory = directory.path();
        let base = vec![entry(1, 0, "fixture", true), entry(2, 1, "old.txt", false)];
        write(directory, "segment-1.fx2", base, 3, None);
        // The delta's record has its parent in the base.
        let delta = vec![entry(3, 1, "new.txt", false)];
        write(directory, "segment-2.fx2", delta, 5, Some(vec![2]));
        manifest::publish(
            directory,
            &manifest(2, 5, "segment-1.fx2", &["segment-2.fx2"]),
        )
        .unwrap();

        let recovered = recover(directory, vec![]).unwrap();
        assert_eq!(recovered.sequence, 5);
        assert_eq!(recovered.deltas.len(), 1);
        assert_eq!(
            recovered.files,
            [
                directory.join("segment-1.fx2"),
                directory.join("segment-2.fx2")
            ]
        );
        let view = super::super::view::View {
            base: recovered.base,
            deltas: recovered.deltas,
            layers: vec![],
            roots: recovered.roots,
            epoch: recovered.sequence,
        };
        assert_eq!(view.resolve(Path::new("/fixture/new.txt")), Some(3));
        assert_eq!(view.resolve(Path::new("/fixture/old.txt")), None);
    }

    #[test]
    fn an_inconsistent_delta_falls_back_to_the_previous_generation() {
        let directory = tempfile::tempdir().unwrap();
        let directory = directory.path();
        let base = vec![entry(1, 0, "fixture", true), entry(2, 1, "old.txt", false)];
        write(directory, "segment-1.fx2", base, 3, None);
        write(directory, "segment-2.fx2", vec![], 4, Some(vec![]));
        manifest::publish(directory, &manifest(1, 3, "segment-1.fx2", &[])).unwrap();
        // Claims a newer sequence than its delta holds.
        manifest::publish(
            directory,
            &manifest(2, 5, "segment-1.fx2", &["segment-2.fx2"]),
        )
        .unwrap();
        // A base written as a delta, or a delta as a base, is rejected too.
        manifest::publish(directory, &manifest(3, 4, "segment-2.fx2", &[])).unwrap();

        let recovered = recover(directory, vec![]).unwrap();
        assert_eq!(recovered.manifest_epoch, 1);
        assert_eq!(recovered.sequence, 3);
        assert!(recovered.deltas.is_empty());
        assert_eq!(recovered.files, [directory.join("segment-1.fx2")]);
    }
}

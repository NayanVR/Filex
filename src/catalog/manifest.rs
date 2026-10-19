//! Numbered generation publication: retain the latest two valid manifests.
//! Windows and POSIX both support durable exclusive creation of a new slot.
use super::segment::Root;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::Write,
    path::{Path, PathBuf},
};
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub epoch: u64,
    /// The WAL sequence the newest level covers; replay starts after it.
    pub sequence: u64,
    pub next_id: u64,
    /// The base segment.
    pub segment: String,
    pub roots: Vec<Root>,
    /// Delta segments over the base, oldest first.
    pub deltas: Vec<String>,
}
pub const VERSION: u32 = 3;
impl Manifest {
    /// Every segment file this generation needs.
    pub fn files(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.segment.as_str()).chain(self.deltas.iter().map(String::as_str))
    }
}
#[derive(Serialize, Deserialize)]
struct Envelope {
    payload: Vec<u8>,
    checksum: Vec<u8>,
}
pub fn publish(dir: &Path, manifest: &Manifest) -> Result<()> {
    let payload = serde_json::to_vec(manifest)?;
    let checksum = Sha256::digest(&payload).to_vec();
    let path = dir.join(format!("manifest-{:020}.json", manifest.epoch));
    let tmp = path.with_extension("tmp");
    let mut f = File::options().write(true).create_new(true).open(&tmp)?;
    f.write_all(&serde_json::to_vec(&Envelope { payload, checksum })?)?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(tmp, path)?;
    sync_dir(dir)?;
    Ok(())
}
pub fn candidates(dir: &Path) -> Result<Vec<(PathBuf, Manifest)>> {
    let mut candidates = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("manifest-") || !name.ends_with(".json") {
            continue;
        }
        let parsed = (|| -> Result<Manifest> {
            let env: Envelope = serde_json::from_slice(&std::fs::read(entry.path())?)?;
            ensure!(
                Sha256::digest(&env.payload).as_slice() == env.checksum,
                "manifest checksum mismatch"
            );
            let m: Manifest = serde_json::from_slice(&env.payload)?;
            ensure!(
                m.version == VERSION && m.files().all(|f| !f.contains('/') && !f.contains('\\')),
                "invalid manifest"
            );
            Ok(m)
        })();
        if let Ok(manifest) = parsed {
            candidates.push((entry.path(), manifest));
        }
    }
    candidates.sort_by_key(|(_, m)| std::cmp::Reverse(m.epoch));
    Ok(candidates)
}
/// Whether a `manifest-*.json` was written by an older index format —
/// unreadable to this build and never coming back. A *newer* version is
/// left alone: a daemon briefly older than the binary on disk (FIL-21)
/// must not delete the index its successor wrote.
pub fn is_older_format(path: &Path) -> bool {
    #[derive(Deserialize)]
    struct Versioned {
        version: u32,
    }
    let parsed = (|| -> Result<u32> {
        let env: Envelope = serde_json::from_slice(&std::fs::read(path)?)?;
        Ok(serde_json::from_slice::<Versioned>(&env.payload)?.version)
    })();
    parsed.is_ok_and(|version| version < VERSION)
}
pub fn sync_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(dir)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}
/// Replace a closed durable temporary file without a delete/rename crash gap.
pub fn replace_file(from: &Path, to: &Path) -> Result<()> {
    #[cfg(not(windows))]
    std::fs::rename(from, to)?;
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows::{
            Win32::Storage::FileSystem::{
                MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
            },
            core::PCWSTR,
        };
        let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
        let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
        unsafe {
            MoveFileExW(
                PCWSTR(from.as_ptr()),
                PCWSTR(to.as_ptr()),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        }?;
    }
    if let Some(parent) = to.parent() {
        sync_dir(parent)?;
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    fn manifest(epoch: u64) -> Manifest {
        Manifest {
            version: VERSION,
            epoch,
            sequence: epoch * 10,
            next_id: epoch * 100,
            segment: "base.fxseg".into(),
            roots: Vec::new(),
            deltas: Vec::new(),
        }
    }
    fn json(manifest: &Manifest) -> serde_json::Value {
        serde_json::to_value(manifest).unwrap()
    }
    /// Writes an envelope by hand so tests can forge checksums and payloads.
    fn forge(dir: &Path, name: &str, payload: Vec<u8>, checksum: Vec<u8>) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(
            &path,
            serde_json::to_vec(&Envelope { payload, checksum }).unwrap(),
        )
        .unwrap();
        path
    }
    fn forge_valid(dir: &Path, manifest: &Manifest) -> PathBuf {
        let payload = serde_json::to_vec(manifest).unwrap();
        let checksum = Sha256::digest(&payload).to_vec();
        forge(
            dir,
            &format!("manifest-{:020}.json", manifest.epoch),
            payload,
            checksum,
        )
    }
    fn epochs(dir: &Path) -> Vec<u64> {
        candidates(dir)
            .unwrap()
            .iter()
            .map(|(_, m)| m.epoch)
            .collect()
    }
    #[test]
    fn publish_then_candidates_round_trips_every_field() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = manifest(7);
        m.roots = vec![Root {
            id: 3,
            path: PathBuf::from("some/root"),
            device: 42,
        }];
        m.deltas = vec!["d1.fxseg".into(), "d2.fxseg".into()];
        publish(dir.path(), &m).unwrap();
        let found = candidates(dir.path()).unwrap();
        assert_eq!(found.len(), 1);
        let (path, loaded) = &found[0];
        assert_eq!(path, &dir.path().join(format!("manifest-{:020}.json", 7)));
        assert_eq!(json(loaded), json(&m));
        assert_eq!(
            loaded.files().collect::<Vec<_>>(),
            ["base.fxseg", "d1.fxseg", "d2.fxseg"]
        );
    }
    #[test]
    fn candidates_are_sorted_newest_epoch_first() {
        let dir = tempfile::tempdir().unwrap();
        for epoch in [2, 9, 5] {
            publish(dir.path(), &manifest(epoch)).unwrap();
        }
        assert_eq!(epochs(dir.path()), [9, 5, 2]);
    }
    #[test]
    fn checksum_mismatch_is_skipped_but_valid_sibling_survives() {
        let dir = tempfile::tempdir().unwrap();
        publish(dir.path(), &manifest(1)).unwrap();
        // A well-formed manifest payload carrying another payload's checksum.
        let checksum = Sha256::digest(serde_json::to_vec(&manifest(2)).unwrap()).to_vec();
        let mut tampered = manifest(2);
        tampered.next_id += 1;
        forge(
            dir.path(),
            "manifest-00000000000000000002.json",
            serde_json::to_vec(&tampered).unwrap(),
            checksum,
        );
        assert_eq!(epochs(dir.path()), [1]);
    }
    #[test]
    fn wrong_version_bad_names_garbage_and_unrelated_files_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        publish(d, &manifest(1)).unwrap();
        for version in [VERSION - 1, VERSION + 1] {
            let mut m = manifest(u64::from(version) + 10);
            m.version = version;
            forge_valid(d, &m);
        }
        let mut slash = manifest(20);
        slash.segment = "../base.fxseg".into();
        forge_valid(d, &slash);
        let mut backslash = manifest(21);
        backslash.deltas = vec!["..\\delta.fxseg".into()];
        forge_valid(d, &backslash);
        std::fs::write(d.join("manifest-00000000000000000030.json"), b"not json").unwrap();
        std::fs::write(d.join("other.json"), b"{}").unwrap();
        std::fs::write(d.join("manifest-1.tmp"), b"{}").unwrap();
        assert_eq!(epochs(d), [1]);
    }
    #[test]
    fn publish_refuses_an_existing_epoch_and_leaves_no_tmp() {
        let dir = tempfile::tempdir().unwrap();
        publish(dir.path(), &manifest(4)).unwrap();
        let names = || -> Vec<String> {
            std::fs::read_dir(dir.path())
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect()
        };
        assert_eq!(names(), ["manifest-00000000000000000004.json"]);
        // A leftover temporary from an interrupted publish blocks the slot.
        let tmp = dir.path().join("manifest-00000000000000000004.tmp");
        std::fs::write(&tmp, b"partial").unwrap();
        let mut other = manifest(4);
        other.next_id += 1;
        assert!(publish(dir.path(), &other).is_err());
        assert_eq!(std::fs::read(&tmp).unwrap(), b"partial");
        let found = candidates(dir.path()).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(json(&found[0].1), json(&manifest(4)));
    }
    #[test]
    fn older_format_is_strictly_below_current_version() {
        let dir = tempfile::tempdir().unwrap();
        let at = |version: u32| {
            let mut m = manifest(u64::from(version));
            m.version = version;
            forge_valid(dir.path(), &m)
        };
        assert!(is_older_format(&at(1)));
        assert!(is_older_format(&at(VERSION - 1)));
        assert!(!is_older_format(&at(VERSION)));
        // FIL-21: a newer daemon's index must never be deleted by an older one.
        assert!(!is_older_format(&at(VERSION + 1)));
    }
    #[test]
    fn older_format_is_false_for_unreadable_files() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        assert!(!is_older_format(&d.join("missing.json")));
        let garbage = d.join("garbage.json");
        std::fs::write(&garbage, b"not json").unwrap();
        assert!(!is_older_format(&garbage));
        let no_version = forge(d, "no-version.json", b"{}".to_vec(), Vec::new());
        assert!(!is_older_format(&no_version));
        assert!(!is_older_format(d));
    }
    #[test]
    fn replace_file_overwrites_destination_and_consumes_source() {
        let dir = tempfile::tempdir().unwrap();
        let (from, to) = (dir.path().join("new.tmp"), dir.path().join("current"));
        std::fs::write(&from, b"new").unwrap();
        std::fs::write(&to, b"old").unwrap();
        replace_file(&from, &to).unwrap();
        assert_eq!(std::fs::read(&to).unwrap(), b"new");
        assert!(!from.exists());
    }
}

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

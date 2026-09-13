//! Checksum-framed, sequenced write-ahead log. Only a torn trailing frame is
//! discarded; a complete frame with a bad checksum is never silently accepted.
use super::segment::{Record, Root};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Delta {
    Upsert(Record),
    Delete(u64),
    Roots(Vec<Root>),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Transaction {
    pub sequence: u64,
    pub deltas: Vec<Delta>,
    pub next_id: u64,
}
pub struct Wal {
    file: File,
    path: PathBuf,
}
impl Wal {
    pub fn open(path: &Path) -> Result<(Self, Vec<Transaction>)> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        let mut entries = Vec::new();
        let mut valid = 0u64;
        loop {
            let mut header = [0; 4];
            let n = file.read(&mut header)?;
            if n == 0 {
                break;
            }
            if n != 4 {
                file.set_len(valid)?;
                break;
            }
            let len = u32::from_le_bytes(header) as usize;
            ensure!(len <= 32 * 1024 * 1024, "WAL frame exceeds limit");
            let mut payload = vec![0; len];
            let mut checksum = [0; 32];
            if file.read_exact(&mut payload).is_err() || file.read_exact(&mut checksum).is_err() {
                file.set_len(valid)?;
                break;
            }
            ensure!(
                Sha256::digest(&payload).as_slice() == checksum,
                "WAL checksum mismatch"
            );
            let tx: Transaction = serde_json::from_slice(&payload)?;
            ensure!(
                entries
                    .last()
                    .is_none_or(|prev: &Transaction| tx.sequence == prev.sequence + 1),
                "WAL sequence gap"
            );
            entries.push(tx);
            valid = file.stream_position()?;
        }
        file.seek(SeekFrom::End(0))?;
        Ok((
            Self {
                file,
                path: path.to_path_buf(),
            },
            entries,
        ))
    }
    /// Keep the tail needed by the oldest retained manifest. Replacing the WAL
    /// occurs only after the replacement is durable; either file replays safely.
    pub fn checkpoint(&mut self, through: u64) -> Result<()> {
        let (_, entries) = Self::open(&self.path)?;
        let tmp = self.path.with_extension("checkpoint");
        let _ = std::fs::remove_file(&tmp);
        let (mut replacement, _) = Self::open(&tmp)?;
        for tx in entries.iter().filter(|tx| tx.sequence > through) {
            replacement.append(tx)?;
        }
        replacement.file.sync_all()?;
        drop(replacement);
        super::manifest::replace_file(&tmp, &self.path)?;
        let (new, _) = Self::open(&self.path)?;
        *self = new;
        Ok(())
    }
    pub fn append(&mut self, tx: &Transaction) -> Result<()> {
        let payload = serde_json::to_vec(tx)?;
        ensure!(
            payload.len() <= 32 * 1024 * 1024,
            "WAL transaction too large"
        );
        self.file.write_all(&(payload.len() as u32).to_le_bytes())?;
        self.file.write_all(&payload)?;
        self.file.write_all(&Sha256::digest(&payload))?;
        self.file.sync_data()?;
        Ok(())
    }
}

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
        // Windows refuses to replace a file that has open handles, so release
        // ours first. std opens with FILE_SHARE_DELETE, so the source may stay open.
        self.file = replacement.file;
        let replaced = super::manifest::replace_file(&tmp, &self.path);
        // Reopen either way so appends never land in the temporary file.
        let (new, _) = Self::open(&self.path)?;
        *self = new;
        replaced
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
#[cfg(test)]
mod tests {
    use super::*;
    const LIMIT: usize = 32 * 1024 * 1024;
    fn tx(sequence: u64) -> Transaction {
        Transaction {
            sequence,
            deltas: vec![Delta::Delete(sequence)],
            next_id: sequence + 1,
        }
    }
    fn sequences(entries: &[Transaction]) -> Vec<u64> {
        entries.iter().map(|tx| tx.sequence).collect()
    }
    fn wal_with(path: &Path, seqs: impl IntoIterator<Item = u64>) -> Wal {
        let (mut wal, entries) = Wal::open(path).unwrap();
        assert!(entries.is_empty());
        for seq in seqs {
            wal.append(&tx(seq)).unwrap();
        }
        wal
    }
    #[test]
    fn open_rejects_sequence_gap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wal");
        drop(wal_with(&path, [1, 3]));
        let err = Wal::open(&path).err().unwrap();
        assert!(err.to_string().contains("WAL sequence gap"), "{err}");
    }
    #[test]
    fn open_rejects_oversized_frame_header() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wal");
        drop(wal_with(&path, [1]));
        let mut bytes = std::fs::read(&path).unwrap();
        // A complete header claiming more than the limit must error before
        // any payload buffer is allocated, even with no payload following it.
        bytes.extend_from_slice(&(LIMIT as u32 + 1).to_le_bytes());
        std::fs::write(&path, &bytes).unwrap();
        let err = Wal::open(&path).err().unwrap();
        assert!(err.to_string().contains("WAL frame exceeds limit"), "{err}");
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
    #[test]
    fn append_rejects_oversized_transaction() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wal");
        let mut wal = wal_with(&path, [1]);
        let before = std::fs::read(&path).unwrap();
        let huge = Transaction {
            sequence: 2,
            deltas: vec![Delta::Roots(vec![Root {
                id: 1,
                path: "a".repeat(12 * 1024 * 1024).into(),
                device: 1,
            }])],
            next_id: 3,
        };
        assert!(serde_json::to_vec(&huge).unwrap().len() > LIMIT);
        let err = wal.append(&huge).err().unwrap();
        assert!(
            err.to_string().contains("WAL transaction too large"),
            "{err}"
        );
        drop(wal);
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let (_, entries) = Wal::open(&path).unwrap();
        assert_eq!(sequences(&entries), [1]);
    }
    #[test]
    fn append_after_checkpoint_lands_in_replaced_log() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wal");
        let mut wal = wal_with(&path, 1..=5);
        wal.checkpoint(3).unwrap();
        wal.append(&tx(6)).unwrap();
        drop(wal);
        let (_, entries) = Wal::open(&path).unwrap();
        assert_eq!(sequences(&entries), [4, 5, 6]);
        assert!(!path.with_extension("checkpoint").exists());
    }
    #[test]
    fn checkpoint_past_last_sequence_empties_log_and_accepts_next() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wal");
        let mut wal = wal_with(&path, 1..=3);
        wal.checkpoint(u64::MAX).unwrap();
        drop(wal);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
        let (mut wal, entries) = Wal::open(&path).unwrap();
        assert!(entries.is_empty());
        wal.append(&tx(4)).unwrap();
        drop(wal);
        let (_, entries) = Wal::open(&path).unwrap();
        assert_eq!(sequences(&entries), [4]);
    }
    #[test]
    fn open_creates_missing_file_and_accepts_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wal");
        let (wal, entries) = Wal::open(&path).unwrap();
        assert!(entries.is_empty());
        drop(wal);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
        let (_, entries) = Wal::open(&path).unwrap();
        assert!(entries.is_empty());
    }
}

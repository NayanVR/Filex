//! On-disk index snapshots with journal checkpoints
//! (docs/indexing-architecture.md §3, "On-disk persistence").
//!
//! The format mirrors the in-memory layout: the name pools and the entry
//! arena are dumped near-verbatim (little-endian, fixed-width fields);
//! the children map is rebuilt from parent links on load. A checkpoint
//! records where the platform journal was at save time so startup can
//! replay the gap (FSEvents/USN) or know it must reconcile (walk-based
//! platforms). Every reference is validated on load — a corrupt or
//! mismatched snapshot yields an error and the caller falls back to a
//! fresh walk; it must never panic or produce a lying index.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail, ensure};

use super::{EntryId, FLAG_TOMBSTONE, FileEntry, NameRef, ROOT, VolumeIndex};

const MAGIC: &[u8; 8] = b"FXIDX\0\0\0";
/// v2: entries carry a native key (NTFS FRN); older snapshots are
/// rejected and rebuilt by a fresh walk.
pub const FORMAT_VERSION: u32 = 3;

/// Where the platform journal stood when the snapshot was written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Checkpoint {
    /// No replay possible; reconcile with a rescan.
    None,
    /// macOS: replay FSEvents with `sinceWhen = last_event_id`.
    FsEvents { last_event_id: u64 },
    /// Windows USN journal (used once the elevated fast path lands).
    UsnJournal { journal_id: u64, next_usn: u64 },
    /// Walk-based platforms: when the index was last known good.
    WalkedAt { unix_seconds: u64 },
}

impl Checkpoint {
    fn encode(self) -> (u8, u64, u64) {
        match self {
            Checkpoint::None => (0, 0, 0),
            Checkpoint::FsEvents { last_event_id } => (1, last_event_id, 0),
            Checkpoint::UsnJournal { journal_id, next_usn } => (2, journal_id, next_usn),
            Checkpoint::WalkedAt { unix_seconds } => (3, unix_seconds, 0),
        }
    }

    fn decode(kind: u8, a: u64, b: u64) -> Result<Self> {
        Ok(match kind {
            0 => Checkpoint::None,
            1 => Checkpoint::FsEvents { last_event_id: a },
            2 => Checkpoint::UsnJournal { journal_id: a, next_usn: b },
            3 => Checkpoint::WalkedAt { unix_seconds: a },
            other => bail!("unknown checkpoint kind {other}"),
        })
    }
}

/// A loaded snapshot: the index plus the checkpoint to resume from.
#[derive(Debug)]
pub struct Snapshot {
    pub index: VolumeIndex,
    pub checkpoint: Checkpoint,
}

/// Default snapshot location for a root: `<data_local_dir>/filex/index/`,
/// one file per root keyed by a stable hash of its path.
pub fn default_snapshot_path(root: &Path) -> Option<PathBuf> {
    let dir = dirs::data_local_dir()?.join("filex").join("index");
    Some(dir.join(format!("{:016x}.fxidx", fnv1a(root.to_string_lossy().as_bytes()))))
}

/// FNV-1a: stable across runs and platforms (std's DefaultHasher is not).
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

const ENTRY_ENCODED_LEN: usize = 41; // 4+2 + 4+2 + 4 + 1 + 8 + 8(size) + 8(mtime)

/// Write a snapshot atomically (temp file + rename in the same directory).
pub fn save(index: &VolumeIndex, checkpoint: Checkpoint, path: &Path) -> Result<()> {
    let Some(root_str) = index.root_path.to_str() else {
        bail!("index root {} is not UTF-8", index.root_path.display());
    };

    let mut buf = Vec::with_capacity(
        64 + root_str.len()
            + index.name_pool.len()
            + index.name_pool_lower.len()
            + index.entries.len() * ENTRY_ENCODED_LEN,
    );
    buf.extend_from_slice(MAGIC);
    buf.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    let (kind, a, b) = checkpoint.encode();
    buf.push(kind);
    buf.extend_from_slice(&a.to_le_bytes());
    buf.extend_from_slice(&b.to_le_bytes());

    buf.extend_from_slice(&(root_str.len() as u64).to_le_bytes());
    buf.extend_from_slice(root_str.as_bytes());
    buf.extend_from_slice(&(index.name_pool.len() as u64).to_le_bytes());
    buf.extend_from_slice(&index.name_pool);
    buf.extend_from_slice(&(index.name_pool_lower.len() as u64).to_le_bytes());
    buf.extend_from_slice(&index.name_pool_lower);

    buf.extend_from_slice(&(index.entries.len() as u64).to_le_bytes());
    for entry in &index.entries {
        buf.extend_from_slice(&entry.name.offset.to_le_bytes());
        buf.extend_from_slice(&entry.name.len.to_le_bytes());
        buf.extend_from_slice(&entry.name_lower.offset.to_le_bytes());
        buf.extend_from_slice(&entry.name_lower.len.to_le_bytes());
        buf.extend_from_slice(&entry.parent.0.to_le_bytes());
        buf.push(entry.flags);
        buf.extend_from_slice(&entry.native_key.to_le_bytes());
        buf.extend_from_slice(&entry.size.to_le_bytes());
        buf.extend_from_slice(&entry.mtime.to_le_bytes());
    }

    let parent = path.parent().context("snapshot path has no parent dir")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("creating snapshot dir {}", parent.display()))?;
    let tmp = path.with_extension("fxidx.tmp");
    let mut file = std::fs::File::create(&tmp)
        .with_context(|| format!("creating {}", tmp.display()))?;
    file.write_all(&buf)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path)
        .with_context(|| format!("moving snapshot into place at {}", path.display()))?;
    Ok(())
}

/// Reads little-endian primitives off the front of a byte slice.
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        ensure!(self.pos + n <= self.buf.len(), "snapshot truncated");
        let slice = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().expect("2 bytes")))
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().expect("4 bytes")))
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().expect("8 bytes")))
    }

    fn len_prefixed(&mut self) -> Result<&'a [u8]> {
        let len = usize::try_from(self.u64()?).context("length overflows usize")?;
        self.take(len)
    }
}

/// Load and fully validate a snapshot. `expected_root` guards against a
/// hash collision or a copied file: the stored root must match exactly.
pub fn load(path: &Path, expected_root: &Path) -> Result<Snapshot> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("reading snapshot {}", path.display()))?;
    let mut r = Reader { buf: &bytes, pos: 0 };

    ensure!(r.take(MAGIC.len())? == MAGIC, "not a filex index snapshot");
    let version = r.u32()?;
    ensure!(
        version == FORMAT_VERSION,
        "snapshot format v{version}, expected v{FORMAT_VERSION}"
    );
    let checkpoint = {
        let (kind, a, b) = (r.u8()?, r.u64()?, r.u64()?);
        Checkpoint::decode(kind, a, b)?
    };

    let root = std::str::from_utf8(r.len_prefixed()?).context("root path not UTF-8")?;
    ensure!(
        Path::new(root) == expected_root,
        "snapshot is for {root}, not {}",
        expected_root.display()
    );
    let name_pool = r.len_prefixed()?.to_vec();
    let name_pool_lower = r.len_prefixed()?.to_vec();

    let entry_count = usize::try_from(r.u64()?).context("entry count overflows usize")?;
    ensure!(entry_count >= 1, "snapshot has no root entry");
    ensure!(
        entry_count <= u32::MAX as usize,
        "entry count exceeds EntryId range"
    );
    // Guard against a length claiming more than the file holds.
    ensure!(
        r.buf.len() - r.pos >= entry_count * ENTRY_ENCODED_LEN,
        "snapshot truncated (entry table)"
    );

    let mut entries = Vec::with_capacity(entry_count);
    for i in 0..entry_count {
        let name = NameRef { offset: r.u32()?, len: r.u16()? };
        let name_lower = NameRef { offset: r.u32()?, len: r.u16()? };
        let parent = EntryId(r.u32()?);
        let flags = r.u8()?;
        let native_key = r.u64()?;
        let size = r.u64()?;
        let mtime = r.u64()? as i64; // stored as raw le bytes of the i64

        let name_end = name.offset as usize + name.len as usize;
        let lower_end = name_lower.offset as usize + name_lower.len as usize;
        ensure!(name_end <= name_pool.len(), "entry {i}: name out of pool");
        ensure!(
            lower_end <= name_pool_lower.len(),
            "entry {i}: folded name out of pool"
        );
        ensure!(
            (parent.0 as usize) < entry_count,
            "entry {i}: parent {} out of range",
            parent.0
        );
        entries.push(FileEntry { name, name_lower, parent, flags, native_key, size, mtime });
    }

    // Rebuild the children and native-key maps from the entry table;
    // validate the hierarchy and key uniqueness.
    let mut children: HashMap<EntryId, Vec<EntryId>> = HashMap::new();
    let mut by_native_key: HashMap<u64, EntryId> = HashMap::new();
    if entries[0].native_key != 0 {
        by_native_key.insert(entries[0].native_key, ROOT);
    }
    // Rename-leaked pool bytes aren't recoverable from the file; the
    // tombstone count is the compaction-debt approximation after a load.
    let mut dead_debt = 0usize;
    for (i, entry) in entries.iter().enumerate().skip(1) {
        if entry.is_tombstone() {
            dead_debt += 1;
            continue;
        }
        let parent = &entries[entry.parent.0 as usize];
        ensure!(
            parent.is_dir() && !parent.is_tombstone(),
            "entry {i}: live entry under non-directory or removed parent"
        );
        children
            .entry(entry.parent)
            .or_default()
            .push(EntryId(i as u32));
        if entry.native_key != 0 {
            ensure!(
                by_native_key.insert(entry.native_key, EntryId(i as u32)).is_none(),
                "entry {i}: duplicate native key {:#x}",
                entry.native_key
            );
        }
    }
    ensure!(entries[0].parent == ROOT, "root entry has a parent");
    ensure!(
        entries[0].flags & FLAG_TOMBSTONE == 0,
        "root entry is tombstoned"
    );

    let index = VolumeIndex {
        root_path: PathBuf::from(root),
        entries,
        name_pool,
        name_pool_lower,
        children,
        by_native_key,
        generation: 0,
        dead_debt,
    };
    Ok(Snapshot { index, checkpoint })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::ROOT;

    fn sample() -> VolumeIndex {
        let mut index = VolumeIndex::new("/vol");
        let docs = index.insert(ROOT, "docs", true).unwrap();
        let report = index.insert(docs, "Report.pdf", false).unwrap();
        let uber = index.insert(ROOT, "Übersicht.md", false).unwrap();
        // Populated metadata must survive the round-trip (format v3).
        index.set_meta(uber, 4096, 1_700_000_000);
        // Exercise tombstones and renames in the persisted state.
        let temp = index.insert(ROOT, "temp.bin", false).unwrap();
        index.remove(temp).unwrap();
        index.rename(report, ROOT, "Summary.pdf").unwrap();
        index
    }

    fn tmp_snapshot_path(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("test.fxidx")
    }

    #[test]
    fn entry_footprint_grew_by_size_and_mtime() {
        // v3 (search-chips phase 2) added size:u64 + mtime:i64 = 16 bytes
        // per entry over v2's 25 — ~+32 MB at 2M files. In-memory the two
        // fields cost the same 16 bytes on top of the arena entry.
        assert_eq!(ENTRY_ENCODED_LEN, 25 + 16);
    }

    #[test]
    fn roundtrip_preserves_search_paths_and_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let path = tmp_snapshot_path(&dir);
        let original = sample();
        save(
            &original,
            Checkpoint::FsEvents { last_event_id: 42 },
            &path,
        )
        .unwrap();

        let snapshot = load(&path, Path::new("/vol")).unwrap();
        assert_eq!(
            snapshot.checkpoint,
            Checkpoint::FsEvents { last_event_id: 42 }
        );
        let index = snapshot.index;
        assert_eq!(index.len(), original.len());

        let hits = index.search("summary", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(
            index.path_of(hits[0].id).unwrap(),
            PathBuf::from("/vol/Summary.pdf")
        );
        assert_eq!(index.search("übersicht", 10).len(), 1);
        assert!(index.search("temp.bin", 10).is_empty()); // tombstone survives
        assert!(index.resolve(Path::new("docs")).is_some()); // children rebuilt

        // size/mtime (format v3) survive: Übersicht.md is the only 4 KB
        // file and the only one modified after 2023.
        use crate::search_filter::{Bound, Filter};
        let by_size = index.search_filtered("", &[Filter::Size(Bound::Ge(4096))], 10);
        assert_eq!(
            by_size.iter().filter_map(|h| index.name_of(h.id)).collect::<Vec<_>>(),
            vec!["Übersicht.md"]
        );
        let by_mtime = index.search_filtered("", &[Filter::Modified(Bound::Gt(1_600_000_000))], 10);
        assert_eq!(by_mtime.len(), 1);
    }

    #[test]
    fn loaded_index_accepts_further_mutations() {
        let dir = tempfile::tempdir().unwrap();
        let path = tmp_snapshot_path(&dir);
        save(&sample(), Checkpoint::None, &path).unwrap();

        let mut index = load(&path, Path::new("/vol")).unwrap().index;
        let docs = index.resolve(Path::new("docs")).unwrap();
        index.insert(docs, "post-load.txt", false).unwrap();
        assert_eq!(index.search("post-load", 10).len(), 1);
    }

    #[test]
    fn checkpoint_variants_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = tmp_snapshot_path(&dir);
        for checkpoint in [
            Checkpoint::None,
            Checkpoint::UsnJournal { journal_id: 7, next_usn: 900 },
            Checkpoint::WalkedAt { unix_seconds: 1_752_400_000 },
        ] {
            save(&sample(), checkpoint, &path).unwrap();
            assert_eq!(load(&path, Path::new("/vol")).unwrap().checkpoint, checkpoint);
        }
    }

    #[test]
    fn load_recomputes_compaction_debt_from_tombstones() {
        let dir = tempfile::tempdir().unwrap();
        let path = tmp_snapshot_path(&dir);
        // sample() removes one entry (temp.bin) and renames another.
        save(&sample(), Checkpoint::None, &path).unwrap();
        let index = load(&path, Path::new("/vol")).unwrap().index;
        assert_eq!(index.dead_debt, 1); // the tombstone; rename leak not recoverable
    }

    #[test]
    fn rejects_wrong_root() {
        let dir = tempfile::tempdir().unwrap();
        let path = tmp_snapshot_path(&dir);
        save(&sample(), Checkpoint::None, &path).unwrap();
        let err = load(&path, Path::new("/other")).unwrap_err();
        assert!(err.to_string().contains("/vol"));
    }

    #[test]
    fn rejects_bad_magic_and_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = tmp_snapshot_path(&dir);
        save(&sample(), Checkpoint::None, &path).unwrap();

        let mut bytes = std::fs::read(&path).unwrap();
        bytes[0] = b'X';
        std::fs::write(&path, &bytes).unwrap();
        assert!(load(&path, Path::new("/vol")).is_err());

        let mut bytes = {
            save(&sample(), Checkpoint::None, &path).unwrap();
            std::fs::read(&path).unwrap()
        };
        bytes[8..12].copy_from_slice(&99u32.to_le_bytes());
        std::fs::write(&path, &bytes).unwrap();
        assert!(load(&path, Path::new("/vol")).is_err());
    }

    #[test]
    fn rejects_truncated_and_corrupt_references() {
        let dir = tempfile::tempdir().unwrap();
        let path = tmp_snapshot_path(&dir);
        save(&sample(), Checkpoint::None, &path).unwrap();
        let bytes = std::fs::read(&path).unwrap();

        // Truncation at every prefix must error, never panic.
        for cut in [10, 30, bytes.len() / 2, bytes.len() - 1] {
            std::fs::write(&path, &bytes[..cut]).unwrap();
            assert!(load(&path, Path::new("/vol")).is_err(), "cut at {cut}");
        }

        // Flip the last entry's parent to an out-of-range id.
        // Entry tail layout: parent u32, flags u8, native_key u64, size
        // u64, mtime i64 → parent starts 29 bytes from the end.
        let mut corrupt = bytes.clone();
        let parent_field = corrupt.len() - 29;
        corrupt[parent_field..parent_field + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        std::fs::write(&path, &corrupt).unwrap();
        assert!(load(&path, Path::new("/vol")).is_err());
    }

    #[test]
    fn native_keys_roundtrip_and_duplicates_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = tmp_snapshot_path(&dir);
        let mut index = VolumeIndex::new_with_root_key("/vol", 5);
        let docs = index.insert_with_key(ROOT, "docs", true, 10).unwrap();
        index.insert_with_key(docs, "a.txt", false, 11).unwrap();
        save(&index, Checkpoint::UsnJournal { journal_id: 1, next_usn: 2 }, &path).unwrap();

        let loaded = load(&path, Path::new("/vol")).unwrap().index;
        assert_eq!(loaded.entry_by_native_key(5), Some(ROOT));
        assert_eq!(loaded.entry_by_native_key(10), loaded.resolve(Path::new("docs")));
        assert_eq!(loaded.entry_by_native_key(11), loaded.resolve(Path::new("docs/a.txt")));

        // Corrupt: give the last entry the same key as the root (5).
        // native_key sits before size(u64)+mtime(i64), i.e. 24 from the end.
        let mut bytes = std::fs::read(&path).unwrap();
        let key_field = bytes.len() - 24;
        bytes[key_field..key_field + 8].copy_from_slice(&5u64.to_le_bytes());
        std::fs::write(&path, &bytes).unwrap();
        assert!(load(&path, Path::new("/vol")).unwrap_err().to_string().contains("duplicate"));
    }

    #[test]
    fn missing_snapshot_is_an_error_not_a_panic() {
        assert!(load(Path::new("/nonexistent/x.fxidx"), Path::new("/vol")).is_err());
    }

    #[test]
    fn default_snapshot_paths_differ_per_root_and_are_stable() {
        let a1 = default_snapshot_path(Path::new("/vol/a")).unwrap();
        let a2 = default_snapshot_path(Path::new("/vol/a")).unwrap();
        let b = default_snapshot_path(Path::new("/vol/b")).unwrap();
        assert_eq!(a1, a2);
        assert_ne!(a1, b);
        assert!(a1.to_string_lossy().contains("filex"));
    }
}

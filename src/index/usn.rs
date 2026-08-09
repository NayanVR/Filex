//! NTFS USN structures: record parsing, MFT-enumeration index building,
//! and journal-record → delta mapping (docs/indexing-architecture.md §1–2).
//!
//! Everything here is pure and compiled on every OS so it can be tested
//! against fixture buffers anywhere; the `DeviceIoControl` plumbing that
//! produces these buffers lives in `index::windows` behind cfg(windows).
//!
//! Assumptions about the OS/filesystem:
//! - `USN_RECORD_V2` layout (winioctl.h) is stable ABI; names are UTF-16
//!   and *unqualified* — a record carries only (FRN, parent FRN, name),
//!   which is exactly why the index keys entries by FRN.
//! - Both `FSCTL_ENUM_USN_DATA` and `FSCTL_READ_USN_JOURNAL` output a
//!   u64 continuation value (next FRN / next USN) followed by a packed
//!   run of records.
//! - V3+ records (128-bit ReFS ids) are skipped, not errors: filex's
//!   fast path targets NTFS; ReFS volumes fall back to walk+RDCW.
//! - Reason flags are sticky until the file close record, so a single
//!   record can carry e.g. CREATE|DELETE; DELETE wins (final state).

use std::collections::HashMap;
use std::path::Path;

use super::watcher::FsDelta;
use super::{MAX_PATH_DEPTH, VolumeIndex};

// Reason flags from winioctl.h (stable ABI).
pub const USN_REASON_DATA_OVERWRITE: u32 = 0x0000_0001;
pub const USN_REASON_DATA_EXTEND: u32 = 0x0000_0002;
pub const USN_REASON_DATA_TRUNCATION: u32 = 0x0000_0004;
pub const USN_REASON_FILE_CREATE: u32 = 0x0000_0100;
pub const USN_REASON_FILE_DELETE: u32 = 0x0000_0200;
pub const USN_REASON_RENAME_OLD_NAME: u32 = 0x0000_1000;
pub const USN_REASON_RENAME_NEW_NAME: u32 = 0x0000_2000;
pub const USN_REASON_BASIC_INFO_CHANGE: u32 = 0x0000_8000;

/// Data-change reasons: a file's contents or basic info (size/timestamps)
/// changed in place — the Windows freshness signal for `size:`/`modified:`.
pub const USN_REASON_DATA_CHANGE: u32 = USN_REASON_DATA_OVERWRITE
    | USN_REASON_DATA_EXTEND
    | USN_REASON_DATA_TRUNCATION
    | USN_REASON_BASIC_INFO_CHANGE;

/// The reason mask the journal watcher subscribes to: name-affecting
/// changes plus in-place data/metadata changes (the latter re-stat the
/// entry for size/mtime freshness). (OLD_NAME is requested so sticky-flag
/// records still match, but mapping acts on the NEW_NAME record.)
pub const JOURNAL_REASON_MASK: u32 = USN_REASON_FILE_CREATE
    | USN_REASON_FILE_DELETE
    | USN_REASON_RENAME_OLD_NAME
    | USN_REASON_RENAME_NEW_NAME
    | USN_REASON_DATA_CHANGE;

pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;

/// One decoded `USN_RECORD_V2`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsnRecord {
    pub frn: u64,
    pub parent_frn: u64,
    pub usn: i64,
    pub reason: u32,
    pub attributes: u32,
    pub name: String,
}

impl UsnRecord {
    pub fn is_dir(&self) -> bool {
        self.attributes & FILE_ATTRIBUTE_DIRECTORY != 0
    }
}

/// Fixed-size prefix of USN_RECORD_V2 up to the (variable) file name.
const RECORD_HEADER_LEN: usize = 60;

/// Parse a `FSCTL_ENUM_USN_DATA` / `FSCTL_READ_USN_JOURNAL` output buffer:
/// a u64 continuation value followed by packed records. Returns the
/// continuation and the decoded V2 records; records of other versions or
/// with non-UTF-16 names are skipped. Malformed lengths terminate parsing
/// (kernel output is well-formed; anything else is a broken fixture).
pub fn parse_usn_output(buf: &[u8]) -> (u64, Vec<UsnRecord>) {
    if buf.len() < 8 {
        return (0, Vec::new());
    }
    let continuation = u64::from_le_bytes(buf[0..8].try_into().expect("8 bytes"));
    let mut records = Vec::new();
    let mut offset = 8usize;

    while offset + RECORD_HEADER_LEN <= buf.len() {
        let rec = &buf[offset..];
        let record_length =
            u32::from_le_bytes(rec[0..4].try_into().expect("4 bytes")) as usize;
        if record_length < RECORD_HEADER_LEN || offset + record_length > buf.len() {
            break;
        }
        let major = u16::from_le_bytes(rec[4..6].try_into().expect("2 bytes"));
        if major != 2 {
            offset += record_length; // V3/V4 (ReFS etc.): skip
            continue;
        }

        let name_len = u16::from_le_bytes(rec[56..58].try_into().expect("2 bytes")) as usize;
        let name_offset = u16::from_le_bytes(rec[58..60].try_into().expect("2 bytes")) as usize;
        if name_offset + name_len > record_length || name_len % 2 != 0 {
            break;
        }
        let name_utf16: Vec<u16> = rec[name_offset..name_offset + name_len]
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        if let Ok(name) = String::from_utf16(&name_utf16) {
            records.push(UsnRecord {
                frn: u64::from_le_bytes(rec[8..16].try_into().expect("8 bytes")),
                parent_frn: u64::from_le_bytes(rec[16..24].try_into().expect("8 bytes")),
                usn: i64::from_le_bytes(rec[24..32].try_into().expect("8 bytes")),
                reason: u32::from_le_bytes(rec[40..44].try_into().expect("4 bytes")),
                attributes: u32::from_le_bytes(rec[52..56].try_into().expect("4 bytes")),
                name,
            });
        }
        offset += record_length;
    }
    (continuation, records)
}

/// A record from MFT enumeration, reduced to what index building needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MftEntry {
    pub frn: u64,
    pub parent_frn: u64,
    pub name: String,
    pub is_dir: bool,
}

impl From<&UsnRecord> for MftEntry {
    fn from(record: &UsnRecord) -> Self {
        Self {
            frn: record.frn,
            parent_frn: record.parent_frn,
            name: record.name.clone(),
            is_dir: record.is_dir(),
        }
    }
}

/// Build an index from an MFT enumeration (records arrive in FRN order,
/// so parents may come *after* children). `root_frn` anchors the tree:
/// only records whose parent chain reaches it are indexed — this both
/// scopes a subtree root and drops orphans (entries of deleted dirs that
/// still had MFT records). Returns the index and the orphan count.
///
/// Hard links share an FRN across names; the first name encountered wins.
pub fn build_index_from_mft(
    root_path: &Path,
    root_frn: u64,
    records: &[MftEntry],
    exclude_system: bool,
) -> (VolumeIndex, usize) {
    let mut index = VolumeIndex::new_with_root_key(root_path, root_frn);
    index.set_exclude_system_dirs(exclude_system);
    // First name wins for hard links (duplicate FRNs), so insert-if-absent.
    let mut by_frn: HashMap<u64, &MftEntry> = HashMap::with_capacity(records.len());
    for record in records {
        by_frn.entry(record.frn).or_insert(record);
    }
    let mut orphans = 0usize;
    // FRNs of top-level OS/system dirs (C:\Windows, …) to exclude. Small
    // (~one per SYSTEM_TOP_DIRS entry); descendants short-circuit against it
    // on the way up, so each excluded record costs one bounded parent walk.
    let mut excluded_frns: std::collections::HashSet<u64> = std::collections::HashSet::new();

    let mut chain: Vec<&MftEntry> = Vec::new();
    for record in records {
        // Walk up until we hit something already indexed (or give up).
        chain.clear();
        let mut current = record.frn;
        let mut excluded = false;
        let reachable = loop {
            if exclude_system && excluded_frns.contains(&current) {
                excluded = true; // under an already-known excluded top dir
                break false;
            }
            if index.entry_by_native_key(current).is_some() {
                break true;
            }
            let Some(rec) = by_frn.get(&current) else {
                break false; // parent chain leaves the record set: orphan
            };
            // A top-level system dir sits directly under the volume root.
            if exclude_system
                && rec.is_dir
                && rec.parent_frn == root_frn
                && super::is_system_top(&rec.name)
            {
                excluded_frns.insert(rec.frn);
                excluded = true;
                break false;
            }
            chain.push(rec);
            current = rec.parent_frn;
            if chain.len() > MAX_PATH_DEPTH {
                break false; // cycle in corrupt input
            }
        };
        if excluded {
            continue; // silently dropped — not an orphan
        }
        if !reachable {
            orphans += 1;
            continue;
        }
        // Insert the chain top-down.
        for rec in chain.iter().rev() {
            let Some(parent) = index.entry_by_native_key(rec.parent_frn) else {
                break; // an ancestor insert failed below; count and stop
            };
            // Hard link with an already-indexed FRN, or a parent that is a
            // file (corrupt input): skip the subtree, don't fail the build.
            if index.entry_by_native_key(rec.frn).is_some()
                || index
                    .insert_with_key(parent, &rec.name, rec.is_dir, rec.frn)
                    .is_err()
            {
                break;
            }
        }
    }
    (index, orphans)
}

/// Translate one USN journal record into a normalized delta.
/// Reason flags are sticky, so precedence encodes the *final* state:
/// DELETE wins over everything; CREATE/RENAME_NEW/data-change upsert at the
/// record's (parent FRN, name) — a data-change re-points to the same name,
/// and the writer re-stats it for size/mtime; RENAME_OLD alone is ignored
/// (NEW carries the destination, and cross-volume moves surface as DELETE).
pub fn journal_record_to_delta(record: &UsnRecord) -> Option<FsDelta> {
    if record.reason & USN_REASON_FILE_DELETE != 0 {
        return Some(FsDelta::NativeRemove { key: record.frn });
    }
    const UPSERT_REASONS: u32 =
        USN_REASON_FILE_CREATE | USN_REASON_RENAME_NEW_NAME | USN_REASON_DATA_CHANGE;
    if record.reason & UPSERT_REASONS != 0 {
        return Some(FsDelta::NativeUpsert {
            key: record.frn,
            parent_key: record.parent_frn,
            name: record.name.clone(),
            is_dir: record.is_dir(),
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Encode a USN_RECORD_V2 the way the kernel lays it out.
    fn encode_record(frn: u64, parent: u64, usn: i64, reason: u32, attrs: u32, name: &str) -> Vec<u8> {
        let name_utf16: Vec<u16> = name.encode_utf16().collect();
        let name_bytes = name_utf16.len() * 2;
        let record_len = (RECORD_HEADER_LEN + name_bytes + 7) & !7; // 8-aligned
        let mut buf = vec![0u8; record_len];
        buf[0..4].copy_from_slice(&(record_len as u32).to_le_bytes());
        buf[4..6].copy_from_slice(&2u16.to_le_bytes()); // major
        buf[8..16].copy_from_slice(&frn.to_le_bytes());
        buf[16..24].copy_from_slice(&parent.to_le_bytes());
        buf[24..32].copy_from_slice(&usn.to_le_bytes());
        buf[40..44].copy_from_slice(&reason.to_le_bytes());
        buf[52..56].copy_from_slice(&attrs.to_le_bytes());
        buf[56..58].copy_from_slice(&(name_bytes as u16).to_le_bytes());
        buf[58..60].copy_from_slice(&(RECORD_HEADER_LEN as u16).to_le_bytes());
        for (i, unit) in name_utf16.iter().enumerate() {
            buf[RECORD_HEADER_LEN + i * 2..RECORD_HEADER_LEN + i * 2 + 2]
                .copy_from_slice(&unit.to_le_bytes());
        }
        buf
    }

    const ROOT_FRN: u64 = 5;

    #[test]
    fn parses_continuation_and_packed_records() {
        let mut buf = 777u64.to_le_bytes().to_vec();
        buf.extend(encode_record(10, ROOT_FRN, 100, USN_REASON_FILE_CREATE, 0, "a.txt"));
        buf.extend(encode_record(
            11,
            ROOT_FRN,
            101,
            USN_REASON_FILE_CREATE,
            FILE_ATTRIBUTE_DIRECTORY,
            "Ünterordner",
        ));

        let (next, records) = parse_usn_output(&buf);
        assert_eq!(next, 777);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].name, "a.txt");
        assert_eq!(records[0].usn, 100);
        assert!(!records[0].is_dir());
        assert_eq!(records[1].name, "Ünterordner");
        assert!(records[1].is_dir());
    }

    #[test]
    fn skips_non_v2_records_and_stops_on_garbage() {
        let mut buf = 0u64.to_le_bytes().to_vec();
        let mut v3 = encode_record(10, ROOT_FRN, 100, USN_REASON_FILE_CREATE, 0, "refs");
        v3[4..6].copy_from_slice(&3u16.to_le_bytes());
        buf.extend(v3);
        buf.extend(encode_record(11, ROOT_FRN, 101, USN_REASON_FILE_CREATE, 0, "keep.txt"));

        let (_, records) = parse_usn_output(&buf);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].name, "keep.txt");

        // A record claiming a length beyond the buffer terminates cleanly.
        let mut torn = 0u64.to_le_bytes().to_vec();
        let mut record = encode_record(12, ROOT_FRN, 102, 0, 0, "x");
        record[0..4].copy_from_slice(&4096u32.to_le_bytes());
        torn.extend(record);
        assert!(parse_usn_output(&torn).1.is_empty());
    }

    fn mft(frn: u64, parent: u64, name: &str, is_dir: bool) -> MftEntry {
        MftEntry { frn, parent_frn: parent, name: name.into(), is_dir }
    }

    #[test]
    fn builds_tree_from_out_of_order_records() {
        // Child records BEFORE their parent dirs — the MFT order case.
        let records = vec![
            mft(30, 20, "deep.txt", false),
            mft(20, 10, "sub", true),
            mft(10, ROOT_FRN, "top", true),
            mft(40, ROOT_FRN, "root-file.rs", false),
        ];
        let (index, orphans) = build_index_from_mft(Path::new(r"C:\"), ROOT_FRN, &records, false);

        assert_eq!(orphans, 0);
        assert_eq!(index.len(), 4);
        let hit = &index.search("deep", 10)[0];
        assert_eq!(
            index.path_of(hit.id).unwrap(),
            PathBuf::from(r"C:\").join("top").join("sub").join("deep.txt")
        );
        assert_eq!(index.entry_by_native_key(20), index.resolve(&PathBuf::from("top").join("sub")));
    }

    #[test]
    fn drops_orphans_and_records_outside_the_root() {
        let records = vec![
            mft(10, ROOT_FRN, "in-tree.txt", false),
            mft(30, 99, "orphaned.txt", false), // parent 99 has no record
            // A chain rooted elsewhere (e.g. subtree indexing of C:\Users\x
            // while the volume enumeration also returned C:\Windows).
            mft(50, 40, "outside.dll", false),
            mft(40, 41, "system", true),
        ];
        let (index, orphans) = build_index_from_mft(Path::new(r"C:\data"), ROOT_FRN, &records, false);

        assert_eq!(index.len(), 1);
        assert_eq!(orphans, 3);
        assert_eq!(index.search("outside", 10).len(), 0);
        assert_eq!(index.search("in-tree", 10).len(), 1);
    }

    #[test]
    fn excludes_system_top_dir_subtree_when_asked() {
        // Use the current platform's first excluded dir so is_system_top
        // matches; "Users" is a system dir on none of them.
        let sys = crate::index::SYSTEM_TOP_DIRS[0];
        let records = vec![
            mft(10, ROOT_FRN, sys, true),          // e.g. C:\Windows
            mft(20, 10, "System32", true),         //   \System32
            mft(30, 20, "kernel32.dll", false),    //     \kernel32.dll
            mft(40, ROOT_FRN, "Users", true),      // a user dir, kept
            mft(50, 40, "notes.txt", false),
        ];

        // Exclusion on: the whole system subtree is gone, not counted as
        // orphans; the user subtree survives.
        let (index, orphans) = build_index_from_mft(Path::new(r"C:\"), ROOT_FRN, &records, true);
        assert_eq!(orphans, 0);
        assert_eq!(index.len(), 2, "only Users + notes.txt");
        assert!(index.search("kernel32", 10).is_empty());
        assert!(index.search(&sys.to_lowercase(), 10).is_empty());
        assert_eq!(index.search("notes", 10).len(), 1);

        // Exclusion off: everything indexed (the pre-existing behavior).
        let (index, _) = build_index_from_mft(Path::new(r"C:\"), ROOT_FRN, &records, false);
        assert_eq!(index.len(), 5);
        assert_eq!(index.search("kernel32", 10).len(), 1);
    }

    #[test]
    fn hard_links_keep_first_name_without_failing() {
        let records = vec![
            mft(10, ROOT_FRN, "original.txt", false),
            mft(10, ROOT_FRN, "hardlink.txt", false), // same FRN
        ];
        let (index, _) = build_index_from_mft(Path::new(r"C:\"), ROOT_FRN, &records, false);
        assert_eq!(index.len(), 1);
        assert_eq!(index.search("original", 10).len(), 1);
    }

    #[test]
    fn cyclic_parent_chains_are_dropped_not_looped() {
        let records = vec![
            mft(10, 11, "a", true),
            mft(11, 10, "b", true), // cycle
        ];
        let (index, orphans) = build_index_from_mft(Path::new(r"C:\"), ROOT_FRN, &records, false);
        assert_eq!(index.len(), 0);
        assert_eq!(orphans, 2);
    }

    fn record(frn: u64, parent: u64, reason: u32, attrs: u32, name: &str) -> UsnRecord {
        UsnRecord { frn, parent_frn: parent, usn: 0, reason, attributes: attrs, name: name.into() }
    }

    #[test]
    fn journal_delete_wins_over_sticky_create() {
        let delta = journal_record_to_delta(&record(
            10,
            ROOT_FRN,
            USN_REASON_FILE_CREATE | USN_REASON_FILE_DELETE,
            0,
            "temp.txt",
        ));
        assert_eq!(delta, Some(FsDelta::NativeRemove { key: 10 }));
    }

    #[test]
    fn journal_create_and_rename_new_upsert_at_parent() {
        for reason in [USN_REASON_FILE_CREATE, USN_REASON_RENAME_NEW_NAME] {
            let delta = journal_record_to_delta(&record(
                10,
                20,
                reason,
                FILE_ATTRIBUTE_DIRECTORY,
                "moved",
            ));
            assert_eq!(
                delta,
                Some(FsDelta::NativeUpsert {
                    key: 10,
                    parent_key: 20,
                    name: "moved".into(),
                    is_dir: true,
                })
            );
        }
    }

    #[test]
    fn journal_data_change_upserts_for_freshness() {
        // In-place content/size/timestamp changes re-point the entry to its
        // own name so the writer re-stats it (size:/modified: freshness).
        for reason in [
            USN_REASON_DATA_EXTEND,
            USN_REASON_DATA_TRUNCATION,
            USN_REASON_DATA_OVERWRITE,
            USN_REASON_BASIC_INFO_CHANGE,
            // A close record accumulates the sticky data-change flag.
            USN_REASON_DATA_EXTEND | 0x8000_0000,
        ] {
            assert_eq!(
                journal_record_to_delta(&record(10, 20, reason, 0, "edited.txt")),
                Some(FsDelta::NativeUpsert {
                    key: 10,
                    parent_key: 20,
                    name: "edited.txt".into(),
                    is_dir: false,
                })
            );
        }
    }

    #[test]
    fn journal_rename_old_and_bare_close_map_to_nothing() {
        assert_eq!(
            journal_record_to_delta(&record(10, 20, USN_REASON_RENAME_OLD_NAME, 0, "old")),
            None
        );
        // A bare CLOSE with no data/name reason is nothing to do.
        assert_eq!(journal_record_to_delta(&record(10, 20, 0x8000_0000, 0, "closed")), None);
    }
}

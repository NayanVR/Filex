//! Windows directory enumeration: one handle per directory, not per file.
//!
//! `std::fs::read_dir` plus a per-entry `symlink_metadata` and identity
//! lookup opens every file twice (each a `CreateFileW`, a query and a
//! `CloseHandle`), which dominated a fresh walk of `C:\`. Asking the open
//! directory for `FileIdBothDirectoryInfo` returns each entry's attributes,
//! times, size and NTFS file ID in batches, so a walk opens only the
//! directories it lists.
//!
//! The decoder is plain byte parsing so fixture tests run on every OS.

use super::Stat;
use crate::catalog::segment::Identity;

/// Byte offsets into `FILE_ID_BOTH_DIR_INFO` (x86/x64/ARM64 layout alike;
/// every field is naturally aligned and the struct is 8-aligned).
const NEXT_ENTRY_OFFSET: usize = 0;
const CREATION_TIME: usize = 8;
const LAST_WRITE_TIME: usize = 24;
const END_OF_FILE: usize = 40;
const FILE_ATTRIBUTES: usize = 56;
const FILE_NAME_LENGTH: usize = 60;
const EA_SIZE: usize = 64;
const FILE_ID: usize = 96;
const FILE_NAME: usize = 104;

const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
/// Reparse tags with this bit (symlinks, junctions, mount points) name
/// another file; std reports those as symlinks and a walk never follows them.
const NAME_SURROGATE: u32 = 0x2000_0000;
/// 100 ns intervals between 1601-01-01 (FILETIME) and 1970-01-01.
const UNIX_EPOCH_INTERVALS: i64 = 116_444_736_000_000_000;

/// One directory entry decoded from a `FILE_ID_BOTH_DIR_INFO` buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RawEntry {
    pub name: Vec<u16>,
    pub attributes: u32,
    /// The reparse tag when [`FILE_ATTRIBUTE_REPARSE_POINT`] is set: the
    /// directory-info classes return it in `EaSize`.
    pub reparse_tag: u32,
    pub creation_time: i64,
    pub last_write_time: i64,
    pub end_of_file: i64,
    pub file_id: i64,
}

impl RawEntry {
    /// `.` and `..` are listed by NTFS but are not entries of the directory.
    pub fn is_dot(&self) -> bool {
        self.name == [b'.' as u16] || self.name == [b'.' as u16, b'.' as u16]
    }

    /// The entry's [`Stat`], given the volume serial of its directory.
    ///
    /// Assumes NTFS semantics, matching `ingest::identity`'s
    /// `GetFileInformationByHandle` route field for field: the 64-bit
    /// `FileId` is the file reference number that call splits into
    /// `nFileIndexHigh/Low`, and `birth` is the raw creation FILETIME. A
    /// filesystem without stable IDs (FAT) reports what that call would.
    pub fn stat(&self, volume_serial: u32) -> Stat {
        let is_symlink = self.attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
            && self.reparse_tag & NAME_SURROGATE != 0;
        Stat {
            identity: Identity {
                device: volume_serial as u64,
                key: self.file_id as u64,
                birth: self.creation_time as u64,
            },
            is_dir: !is_symlink && self.attributes & FILE_ATTRIBUTE_DIRECTORY != 0,
            is_symlink,
            size: self.end_of_file as u64,
            mtime: unix_seconds(self.last_write_time),
        }
    }
}

/// Seconds since the Unix epoch for a FILETIME, `None` before 1970 —
/// what `Metadata::modified()` then `duration_since(UNIX_EPOCH)` yields.
fn unix_seconds(filetime: i64) -> Option<i64> {
    let intervals = filetime.checked_sub(UNIX_EPOCH_INTERVALS)?;
    (intervals >= 0).then_some(intervals / 10_000_000)
}

/// Decode a buffer filled by `GetFileInformationByHandleEx` with a
/// `FileIdBothDirectory*Info` class: entries chained by `NextEntryOffset`,
/// the last one holding 0. A truncated or out-of-bounds entry ends the
/// batch rather than reading past the buffer.
pub(super) fn decode(buffer: &[u8]) -> Vec<RawEntry> {
    let mut entries = Vec::new();
    let mut offset = 0;
    while let Some(entry) = buffer.get(offset..) {
        let Some(decoded) = decode_one(entry) else {
            break;
        };
        entries.push(decoded);
        let next = u32_at(entry, NEXT_ENTRY_OFFSET).unwrap_or(0) as usize;
        if next == 0 {
            break;
        }
        offset += next;
    }
    entries
}

fn decode_one(entry: &[u8]) -> Option<RawEntry> {
    let name_bytes = u32_at(entry, FILE_NAME_LENGTH)? as usize;
    let name = entry.get(FILE_NAME..FILE_NAME.checked_add(name_bytes)?)?;
    let attributes = u32_at(entry, FILE_ATTRIBUTES)?;
    Some(RawEntry {
        name: name
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&c| u16::from_le_bytes(c))
            .collect(),
        attributes,
        reparse_tag: if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            u32_at(entry, EA_SIZE)?
        } else {
            0
        },
        creation_time: i64_at(entry, CREATION_TIME)?,
        last_write_time: i64_at(entry, LAST_WRITE_TIME)?,
        end_of_file: i64_at(entry, END_OF_FILE)?,
        file_id: i64_at(entry, FILE_ID)?,
    })
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

fn i64_at(bytes: &[u8], at: usize) -> Option<i64> {
    Some(i64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?))
}

#[cfg(windows)]
pub(super) use list::list;

#[cfg(windows)]
mod list {
    use super::{RawEntry, decode};
    use crate::ingest::Stat;
    use std::{
        ffi::OsString,
        fs::File,
        io,
        os::windows::{ffi::OsStringExt, fs::OpenOptionsExt, io::AsRawHandle},
        path::{Path, PathBuf},
    };
    use windows::Win32::{
        Foundation::{ERROR_NO_MORE_FILES, HANDLE},
        Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
            FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FileIdBothDirectoryInfo,
            FileIdBothDirectoryRestartInfo, GetFileInformationByHandle,
            GetFileInformationByHandleEx, SYNCHRONIZE,
        },
    };

    /// 64 KiB holds a few hundred entries per call; 8-aligned as the
    /// info structs require.
    const BUFFER_WORDS: usize = 8 * 1024;

    /// Every entry of `dir` with its [`Stat`], from one directory handle.
    ///
    /// Assumes a local NTFS-like volume: the directory is opened for listing
    /// only (no data access, so antivirus doesn't scan it), without
    /// following a reparse point, and the volume serial comes from that same
    /// handle. Each entry is `Err` only once the listing itself fails
    /// part-way; the caller counts it as skipped.
    pub fn list(
        dir: &Path,
    ) -> io::Result<impl Iterator<Item = io::Result<(PathBuf, Stat)>> + use<>> {
        let handle = std::fs::OpenOptions::new()
            .access_mode(FILE_LIST_DIRECTORY.0 | FILE_READ_ATTRIBUTES.0 | SYNCHRONIZE.0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(dir)?;
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        unsafe { GetFileInformationByHandle(HANDLE(handle.as_raw_handle()), &mut info) }?;
        Ok(Listing {
            dir: dir.to_path_buf(),
            handle,
            volume_serial: info.dwVolumeSerialNumber,
            buffer: vec![0; BUFFER_WORDS],
            batch: Vec::new().into_iter(),
            started: false,
            done: false,
        })
    }

    struct Listing {
        dir: PathBuf,
        handle: File,
        volume_serial: u32,
        buffer: Vec<u64>,
        batch: std::vec::IntoIter<RawEntry>,
        started: bool,
        done: bool,
    }

    impl Listing {
        /// The next batch from the handle; `Ok(false)` once it's exhausted.
        fn refill(&mut self) -> io::Result<bool> {
            let class = if self.started {
                FileIdBothDirectoryInfo
            } else {
                FileIdBothDirectoryRestartInfo
            };
            self.started = true;
            let bytes = self.buffer.len() * size_of::<u64>();
            let result = unsafe {
                GetFileInformationByHandleEx(
                    HANDLE(self.handle.as_raw_handle()),
                    class,
                    self.buffer.as_mut_ptr().cast(),
                    bytes as u32,
                )
            };
            match result {
                Ok(()) => {
                    // SAFETY: a u64 buffer viewed as its own bytes.
                    let raw = unsafe {
                        std::slice::from_raw_parts(self.buffer.as_ptr().cast::<u8>(), bytes)
                    };
                    self.batch = decode(raw).into_iter();
                    Ok(true)
                }
                Err(e) if e.code() == ERROR_NO_MORE_FILES.to_hresult() => Ok(false),
                Err(e) => Err(e.into()),
            }
        }
    }

    impl Iterator for Listing {
        type Item = io::Result<(PathBuf, Stat)>;

        fn next(&mut self) -> Option<Self::Item> {
            loop {
                if let Some(entry) = self.batch.next() {
                    if entry.is_dot() {
                        continue;
                    }
                    let path = self.dir.join(OsString::from_wide(&entry.name));
                    return Some(Ok((path, entry.stat(self.volume_serial))));
                }
                if self.done {
                    return None;
                }
                match self.refill() {
                    Ok(true) => {}
                    Ok(false) => {
                        self.done = true;
                        return None;
                    }
                    Err(e) => {
                        self.done = true;
                        return Some(Err(e));
                    }
                }
            }
        }
    }

    /// The decoder's hand-written offsets must match the SDK struct.
    #[test]
    fn offsets_match_the_sdk_layout() {
        use std::mem::offset_of;
        use windows::Win32::Storage::FileSystem::FILE_ID_BOTH_DIR_INFO as I;
        assert_eq!(offset_of!(I, NextEntryOffset), super::NEXT_ENTRY_OFFSET);
        assert_eq!(offset_of!(I, CreationTime), super::CREATION_TIME);
        assert_eq!(offset_of!(I, LastWriteTime), super::LAST_WRITE_TIME);
        assert_eq!(offset_of!(I, EndOfFile), super::END_OF_FILE);
        assert_eq!(offset_of!(I, FileAttributes), super::FILE_ATTRIBUTES);
        assert_eq!(offset_of!(I, FileNameLength), super::FILE_NAME_LENGTH);
        assert_eq!(offset_of!(I, EaSize), super::EA_SIZE);
        assert_eq!(offset_of!(I, FileId), super::FILE_ID);
        assert_eq!(offset_of!(I, FileName), super::FILE_NAME);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One `FILE_ID_BOTH_DIR_INFO` entry, padded to 8 bytes as NTFS packs them.
    fn entry(name: &str, attributes: u32, ea: u32, id: i64, last: bool) -> Vec<u8> {
        let name: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let len = (FILE_NAME + name.len()).next_multiple_of(8);
        let mut b = vec![0u8; len];
        let next = if last { 0 } else { len as u32 };
        b[NEXT_ENTRY_OFFSET..][..4].copy_from_slice(&next.to_le_bytes());
        b[CREATION_TIME..][..8].copy_from_slice(&(UNIX_EPOCH_INTERVALS + 7).to_le_bytes());
        b[LAST_WRITE_TIME..][..8]
            .copy_from_slice(&(UNIX_EPOCH_INTERVALS + 42 * 10_000_000 + 9).to_le_bytes());
        b[END_OF_FILE..][..8].copy_from_slice(&1234i64.to_le_bytes());
        b[FILE_ATTRIBUTES..][..4].copy_from_slice(&attributes.to_le_bytes());
        b[FILE_NAME_LENGTH..][..4].copy_from_slice(&(name.len() as u32).to_le_bytes());
        b[EA_SIZE..][..4].copy_from_slice(&ea.to_le_bytes());
        b[FILE_ID..][..8].copy_from_slice(&id.to_le_bytes());
        b[FILE_NAME..][..name.len()].copy_from_slice(&name);
        b
    }

    fn name(e: &RawEntry) -> String {
        String::from_utf16_lossy(&e.name)
    }

    #[test]
    fn decodes_a_chained_batch() {
        let mut buf = entry(".", FILE_ATTRIBUTE_DIRECTORY, 0, 5, false);
        buf.extend(entry("..", FILE_ATTRIBUTE_DIRECTORY, 0, 5, false));
        buf.extend(entry("Résumé.pdf", 0x20, 0, 0x0001_0000_0000_002a, false));
        buf.extend(entry("Projects", FILE_ATTRIBUTE_DIRECTORY, 0, 77, true));
        buf.extend(vec![0xAA; 64]); // stale bytes past the last entry
        let entries = decode(&buf);
        assert_eq!(
            entries.iter().map(name).collect::<Vec<_>>(),
            [".", "..", "Résumé.pdf", "Projects"]
        );
        assert!(entries[0].is_dot() && entries[1].is_dot() && !entries[2].is_dot());

        let file = entries[2].stat(0xBEEF);
        assert_eq!(file.identity.device, 0xBEEF);
        assert_eq!(file.identity.key, 0x0001_0000_0000_002a, "full 64-bit FRN");
        assert_eq!(file.identity.birth, (UNIX_EPOCH_INTERVALS + 7) as u64);
        assert_eq!((file.size, file.mtime), (1234, Some(42)));
        assert!(!file.is_dir && !file.is_symlink);
        assert!(entries[3].stat(1).is_dir);
    }

    /// Junctions and symlinks are reported as symlinks, not directories, so
    /// a walk never descends through them; other reparse points (OneDrive
    /// placeholders, dedup) keep their type. The tag comes from `EaSize`
    /// only when the reparse attribute is set.
    #[test]
    fn reparse_points_follow_std_file_types() {
        const JUNCTION: u32 = 0xA000_0003;
        const CLOUD: u32 = 0x9000_001A;
        let dir_reparse = FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT;
        let mut buf = entry("Application Data", dir_reparse, JUNCTION, 1, false);
        buf.extend(entry("OneDrive", dir_reparse, CLOUD, 2, false));
        buf.extend(entry(
            "plain",
            FILE_ATTRIBUTE_DIRECTORY,
            0xDEAD_BEEF,
            3,
            true,
        ));
        let stats: Vec<_> = decode(&buf).iter().map(|e| e.stat(1)).collect();
        assert!(stats[0].is_symlink && !stats[0].is_dir, "junction");
        assert!(!stats[1].is_symlink && stats[1].is_dir, "cloud folder");
        assert!(
            !stats[2].is_symlink && stats[2].is_dir,
            "EaSize ignored without the reparse attribute"
        );
    }

    #[test]
    fn a_truncated_entry_ends_the_batch() {
        let mut buf = entry("a.txt", 0x20, 0, 1, false);
        let second = entry("b-with-a-long-name.txt", 0x20, 0, 2, true);
        buf.extend(&second[..FILE_NAME + 4]);
        assert_eq!(decode(&buf).iter().map(name).collect::<Vec<_>>(), ["a.txt"]);
        assert!(decode(&[0u8; 16]).is_empty());
    }

    #[test]
    fn modification_times_before_1970_have_no_unix_seconds() {
        assert_eq!(unix_seconds(UNIX_EPOCH_INTERVALS - 1), None);
        assert_eq!(unix_seconds(0), None);
        assert_eq!(unix_seconds(UNIX_EPOCH_INTERVALS + 19_999_999), Some(1));
    }
}

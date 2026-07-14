//! Windows live watcher built on `ReadDirectoryChangesW` (RDCW).
//!
//! RDCW watches a whole subtree with one handle and — unlike the USN
//! Journal — requires **no elevation**: it works for any directory the
//! user can read, on any filesystem (NTFS, FAT, network shares). It is
//! therefore the universal/unprivileged watcher. The admin-only USN
//! Journal watcher is future work, paired with the FSCTL_ENUM_USN_DATA
//! bootstrap, because USN records identify files by FRN and need the
//! FRN-keyed index that bootstrap builds (docs/indexing-architecture.md §1–2).
//!
//! Assumptions about the OS:
//! - `FILE_NOTIFY_INFORMATION` records are a packed chain (NextEntryOffset
//!   links, 0 terminates) of UTF-16 names *relative to the watched root*,
//!   with `\` separators.
//! - A zero-byte completion means the OS buffer overflowed and changes
//!   were dropped — only a rescan restores truth.
//! - Renames arrive as OLD_NAME/NEW_NAME pairs; they are treated as
//!   independent Remove + Upsert, which idempotent delta application
//!   absorbs (same policy as the macOS and Linux watchers).
//!
//! The buffer parser and delta mapping are pure and compiled on every OS
//! so fixture tests run in any CI; only the RDCW loop is Windows-gated.

use std::path::{Path, PathBuf};

use super::watcher::FsDelta;

// Action codes from winnt.h (stable ABI).
pub const FILE_ACTION_ADDED: u32 = 1;
pub const FILE_ACTION_REMOVED: u32 = 2;
pub const FILE_ACTION_MODIFIED: u32 = 3;
pub const FILE_ACTION_RENAMED_OLD_NAME: u32 = 4;
pub const FILE_ACTION_RENAMED_NEW_NAME: u32 = 5;

/// One decoded `FILE_NOTIFY_INFORMATION` record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileNotifyEvent {
    pub action: u32,
    /// Path relative to the watched root, already split on `\`.
    pub relative_path: PathBuf,
}

/// Parse a `FILE_NOTIFY_INFORMATION` chain. Records with non-UTF-16 names
/// or malformed offsets terminate parsing of the chain (kernel output is
/// well-formed; anything else is a broken fixture).
pub fn parse_notify_buffer(buf: &[u8]) -> Vec<FileNotifyEvent> {
    let mut events = Vec::new();
    let mut offset = 0usize;
    loop {
        if offset + 12 > buf.len() {
            break;
        }
        let rec = &buf[offset..];
        let next = u32::from_ne_bytes(rec[0..4].try_into().expect("4 bytes")) as usize;
        let action = u32::from_ne_bytes(rec[4..8].try_into().expect("4 bytes"));
        let name_bytes = u32::from_ne_bytes(rec[8..12].try_into().expect("4 bytes")) as usize;

        if offset + 12 + name_bytes > buf.len() || name_bytes % 2 != 0 {
            break;
        }
        let name_utf16: Vec<u16> = rec[12..12 + name_bytes]
            .chunks_exact(2)
            .map(|pair| u16::from_ne_bytes([pair[0], pair[1]]))
            .collect();
        if let Ok(name) = String::from_utf16(&name_utf16) {
            // Backslash-separated, relative to the watch root. Build the
            // path component-wise so fixtures behave identically on
            // non-Windows hosts (where `\` is not a separator).
            let relative_path: PathBuf = name.split('\\').collect();
            events.push(FileNotifyEvent { action, relative_path });
        }

        if next == 0 {
            break;
        }
        offset += next;
    }
    events
}

/// Translate one RDCW record into a normalized delta. `presence` is the
/// current filesystem truth for the absolute path — `Some(is_dir)` if it
/// exists, `None` if not — because RDCW actions don't carry a file/dir
/// bit. Pure — fixture-tested on every OS.
pub fn event_to_delta(
    root: &Path,
    event: &FileNotifyEvent,
    presence: Option<bool>,
) -> Option<FsDelta> {
    let path = root.join(&event.relative_path);
    match event.action {
        FILE_ACTION_ADDED | FILE_ACTION_RENAMED_NEW_NAME => match presence {
            Some(is_dir) => Some(FsDelta::Upsert { path, is_dir }),
            // Already gone again (create+delete inside one poll window).
            None => Some(FsDelta::Remove { path }),
        },
        FILE_ACTION_REMOVED | FILE_ACTION_RENAMED_OLD_NAME => Some(FsDelta::Remove { path }),
        _ => None, // MODIFIED and unknown actions: names unaffected
    }
}

#[cfg(target_os = "windows")]
pub use imp::DirChangesWatcher;

#[cfg(target_os = "windows")]
mod imp {
    use super::*;
    use std::sync::mpsc::Sender;
    use std::thread::JoinHandle;

    use anyhow::{Context as _, Result};
    use windows::Win32::Foundation::{CloseHandle, ERROR_NOTIFY_ENUM_DIR, HANDLE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_DIR_NAME,
        FILE_NOTIFY_CHANGE_FILE_NAME, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        OPEN_EXISTING, ReadDirectoryChangesW,
    };
    use windows::Win32::System::IO::CancelIoEx;
    use windows::core::HSTRING;

    /// Sendable wrapper: HANDLEs are agile; this one is only used for
    /// ReadDirectoryChangesW on the reader thread and CancelIoEx/CloseHandle
    /// on drop.
    struct DirHandle(HANDLE);
    unsafe impl Send for DirHandle {}
    unsafe impl Sync for DirHandle {}

    /// Watches a directory subtree via a blocking RDCW loop on a dedicated
    /// thread. Dropping cancels the pending read and closes the handle.
    pub struct DirChangesWatcher {
        handle: std::sync::Arc<DirHandle>,
        thread: Option<JoinHandle<()>>,
    }

    impl DirChangesWatcher {
        /// Start watching `root` (must be canonicalized). Works without
        /// elevation for any directory the user can read.
        pub fn spawn(root: &Path, deltas: Sender<Vec<FsDelta>>) -> Result<Self> {
            // SAFETY: standard directory-handle open for change notification.
            let raw = unsafe {
                CreateFileW(
                    &HSTRING::from(root.as_os_str()),
                    FILE_LIST_DIRECTORY.0,
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                    None,
                    OPEN_EXISTING,
                    FILE_FLAG_BACKUP_SEMANTICS,
                    None,
                )
            }
            .with_context(|| format!("opening {} for change notification", root.display()))?;

            let handle = std::sync::Arc::new(DirHandle(raw));
            let thread = std::thread::Builder::new().name("filex-rdcw".into()).spawn({
                let handle = handle.clone();
                let root = root.to_path_buf();
                move || reader_loop(&handle, &root, &deltas)
            })?;

            Ok(Self { handle, thread: Some(thread) })
        }
    }

    impl Drop for DirChangesWatcher {
        fn drop(&mut self) {
            // SAFETY: cancelling/closing our own handle; the reader thread's
            // pending RDCW completes with an error and the loop exits.
            unsafe {
                CancelIoEx(self.handle.0, None).ok();
                CloseHandle(self.handle.0).ok();
            }
            if let Some(thread) = self.thread.take() {
                thread.join().ok();
            }
        }
    }

    fn reader_loop(handle: &DirHandle, root: &Path, deltas: &Sender<Vec<FsDelta>>) {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let mut bytes_returned = 0u32;
            // SAFETY: buf outlives the synchronous call; no OVERLAPPED means
            // the call blocks until changes arrive or the handle dies.
            let result = unsafe {
                ReadDirectoryChangesW(
                    handle.0,
                    buf.as_mut_ptr() as *mut _,
                    buf.len() as u32,
                    true, // recursive
                    FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_DIR_NAME,
                    Some(&mut bytes_returned),
                    None,
                    None,
                )
            };

            let batch = match result {
                Err(err) if err.code() == ERROR_NOTIFY_ENUM_DIR.to_hresult() => {
                    // Too many changes for the OS to enumerate: rescan.
                    vec![FsDelta::Rescan { path: root.to_path_buf() }]
                }
                Err(_) => break, // cancelled (drop) or handle closed
                Ok(()) if bytes_returned == 0 => {
                    // Our buffer overflowed; changes were dropped.
                    vec![FsDelta::Rescan { path: root.to_path_buf() }]
                }
                Ok(()) => parse_notify_buffer(&buf[..bytes_returned as usize])
                    .iter()
                    .filter_map(|event| {
                        let presence = std::fs::symlink_metadata(root.join(&event.relative_path))
                            .ok()
                            .map(|m| m.is_dir());
                        event_to_delta(root, event, presence)
                    })
                    .collect(),
            };

            if !batch.is_empty() && deltas.send(batch).is_err() {
                break; // receiver gone: shutting down
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a FILE_NOTIFY_INFORMATION chain from (action, name) pairs.
    fn encode_chain(records: &[(u32, &str)]) -> Vec<u8> {
        let mut buf = Vec::new();
        for (i, (action, name)) in records.iter().enumerate() {
            let name_utf16: Vec<u16> = name.encode_utf16().collect();
            let name_bytes = name_utf16.len() * 2;
            // Records are DWORD-aligned; the kernel pads NextEntryOffset.
            let record_len = 12 + name_bytes;
            let padded = (record_len + 3) & !3;
            let next = if i + 1 == records.len() { 0 } else { padded as u32 };

            buf.extend_from_slice(&next.to_ne_bytes());
            buf.extend_from_slice(&action.to_ne_bytes());
            buf.extend_from_slice(&(name_bytes as u32).to_ne_bytes());
            for unit in &name_utf16 {
                buf.extend_from_slice(&unit.to_ne_bytes());
            }
            buf.resize(buf.len() + (padded - record_len), 0);
        }
        buf
    }

    #[test]
    fn parses_chained_records_with_utf16_names() {
        let buf = encode_chain(&[
            (FILE_ACTION_ADDED, r"docs\new file.txt"),
            (FILE_ACTION_REMOVED, "übersicht.md"),
        ]);
        let events = parse_notify_buffer(&buf);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].action, FILE_ACTION_ADDED);
        assert_eq!(
            events[0].relative_path,
            PathBuf::from("docs").join("new file.txt")
        );
        assert_eq!(events[1].relative_path, PathBuf::from("übersicht.md"));
    }

    #[test]
    fn parses_single_record_with_zero_next_offset() {
        let buf = encode_chain(&[(FILE_ACTION_MODIFIED, "x.txt")]);
        assert_eq!(parse_notify_buffer(&buf).len(), 1);
    }

    #[test]
    fn parser_stops_on_malformed_length() {
        let mut buf = encode_chain(&[(FILE_ACTION_ADDED, "ok.txt")]);
        // Claim a name longer than the buffer.
        buf[8..12].copy_from_slice(&1000u32.to_ne_bytes());
        assert!(parse_notify_buffer(&buf).is_empty());
    }

    #[test]
    fn maps_added_and_renamed_new_using_presence() {
        let root = Path::new("/root");
        let added = FileNotifyEvent {
            action: FILE_ACTION_ADDED,
            relative_path: PathBuf::from("a.txt"),
        };
        assert_eq!(
            event_to_delta(root, &added, Some(false)),
            Some(FsDelta::Upsert { path: root.join("a.txt"), is_dir: false })
        );

        let renamed = FileNotifyEvent {
            action: FILE_ACTION_RENAMED_NEW_NAME,
            relative_path: PathBuf::from("dir"),
        };
        assert_eq!(
            event_to_delta(root, &renamed, Some(true)),
            Some(FsDelta::Upsert { path: root.join("dir"), is_dir: true })
        );
        // Vanished before we could stat it: safe to treat as removal.
        assert_eq!(
            event_to_delta(root, &added, None),
            Some(FsDelta::Remove { path: root.join("a.txt") })
        );
    }

    #[test]
    fn maps_removed_and_renamed_old_as_removes() {
        let root = Path::new("/root");
        for action in [FILE_ACTION_REMOVED, FILE_ACTION_RENAMED_OLD_NAME] {
            let event = FileNotifyEvent { action, relative_path: PathBuf::from("gone.txt") };
            assert_eq!(
                event_to_delta(root, &event, None),
                Some(FsDelta::Remove { path: root.join("gone.txt") })
            );
        }
    }

    #[test]
    fn modified_and_unknown_actions_map_to_nothing() {
        let root = Path::new("/root");
        for action in [FILE_ACTION_MODIFIED, 99] {
            let event = FileNotifyEvent { action, relative_path: PathBuf::from("x") };
            assert_eq!(event_to_delta(root, &event, Some(false)), None);
        }
    }

    #[test]
    fn nested_relative_paths_join_with_native_separators() {
        let root = Path::new("/root");
        let event = FileNotifyEvent {
            action: FILE_ACTION_ADDED,
            relative_path: PathBuf::from("a").join("b").join("c.txt"),
        };
        let Some(FsDelta::Upsert { path, .. }) = event_to_delta(root, &event, Some(false)) else {
            panic!("expected upsert");
        };
        assert_eq!(path, root.join("a").join("b").join("c.txt"));
    }
}

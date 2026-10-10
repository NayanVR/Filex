//! Per-OS filesystem inspection for ingest: file identity, Full Disk
//! Access (macOS), and the system directories excluded from indexing.

use crate::catalog::segment::Identity;
use std::{path::Path, time::UNIX_EPOCH};

/// Stable identity of a file across renames. Unix: `(st_dev, st_ino)`.
/// Windows: volume serial + NTFS file index from
/// `GetFileInformationByHandle`, opened without following reparse points;
/// if the handle can't be opened, falls back to a zero device/key.
/// `birth` is the creation time where the filesystem reports one, else 0.
pub fn identity(path: &Path, metadata: &std::fs::Metadata) -> Identity {
    let birth = metadata
        .created()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos() as u64);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let _ = path;
        Identity {
            device: metadata.dev(),
            key: metadata.ino(),
            birth,
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
        use windows::Win32::{
            Foundation::HANDLE,
            Storage::FileSystem::{
                BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS,
                FILE_FLAG_OPEN_REPARSE_POINT, GetFileInformationByHandle,
            },
        };
        if let Ok(file) = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(path)
        {
            let mut info = BY_HANDLE_FILE_INFORMATION::default();
            if unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info) }
                .is_ok()
            {
                return Identity {
                    device: info.dwVolumeSerialNumber as u64,
                    key: ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64,
                    birth: ((info.ftCreationTime.dwHighDateTime as u64) << 32)
                        | info.ftCreationTime.dwLowDateTime as u64,
                };
            }
        }
        Identity {
            device: 0,
            key: 0,
            birth,
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Identity {
            device: 0,
            key: 0,
            birth,
        }
    }
}

/// macOS: Full Disk Access is inferred from whether `~/Library/Mail`
/// (TCC-protected) is readable; there is no public API to query it.
#[cfg(target_os = "macos")]
pub fn has_full_disk_access() -> bool {
    dirs::home_dir().is_some_and(|home| std::fs::read_dir(home.join("Library/Mail")).is_ok())
}
#[cfg(target_os = "macos")]
pub fn open_full_disk_access_settings() {
    let _ = std::process::Command::new("open")
        .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles")
        .spawn();
}

#[cfg(windows)]
pub(super) const SYSTEM_DIRS: &[&str] = &[
    "Windows",
    "Program Files",
    "Program Files (x86)",
    "ProgramData",
    "$Recycle.Bin",
    "System Volume Information",
    "$WinREAgent",
    "Recovery",
    "PerfLogs",
];
#[cfg(target_os = "macos")]
pub(super) const SYSTEM_DIRS: &[&str] = &[
    "System", "Library", "private", "usr", "bin", "sbin", "cores", "opt", "dev",
];
#[cfg(not(any(windows, target_os = "macos")))]
pub(super) const SYSTEM_DIRS: &[&str] = &[
    "proc", "sys", "dev", "run", "boot", "usr", "bin", "sbin", "lib", "lib64", "etc", "var", "opt",
    "srv",
];

/// Whether folder names compare case-insensitively: NTFS and APFS/HFS+
/// default to case-insensitive, Linux filesystems don't. A case-sensitive
/// APFS volume would over-match here, which only excludes more.
pub(super) const CASE_INSENSITIVE_NAMES: bool = cfg!(any(windows, target_os = "macos"));

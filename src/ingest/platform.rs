//! Per-OS filesystem inspection for ingest: file identity, Full Disk
//! Access (macOS), and the system directories excluded from indexing.

use crate::catalog::segment::Identity;
use std::{path::Path, time::UNIX_EPOCH};

/// Stable identity of a file across renames. Unix: `(st_dev, st_ino)`.
/// Windows: volume serial + NTFS file index from
/// `GetFileInformationByHandle`, opened without following reparse points;
/// if the handle can't be opened, falls back to a zero device/key.
///
/// The Windows handle asks for `FILE_READ_ATTRIBUTES` only, the same kind of
/// open `std::fs::symlink_metadata` makes. Assumes NTFS-like semantics: such
/// an open is exempt from share modes, so files held open without sharing
/// (registry hives, page file) still yield a key, and it requests no data
/// access, so antivirus on-access scanning (Defender) skips it. A read open
/// here made Defender scan every file a walk touched.
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
                FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, GetFileInformationByHandle,
            },
        };
        if let Ok(file) = std::fs::OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES.0)
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

/// Top-level folders that hold *other* volumes or network mounts, left out
/// of a whole-disk root (`/`) whatever the system-files setting says. A
/// root indexes one disk, as Windows indexes each fixed drive; external
/// and network volumes are added as their own roots.
#[cfg(target_os = "macos")]
pub(super) const OTHER_VOLUME_DIRS: &[&str] = &["Volumes", "Network", "home"];
#[cfg(not(target_os = "macos"))]
pub(super) const OTHER_VOLUME_DIRS: &[&str] = &[];

/// Mount points of network filesystems (cifs, nfs, sshfs, …) that a root
/// may contain (FIL-62). Linux only: macOS keeps network volumes under
/// `/Volumes`, which [`OTHER_VOLUME_DIRS`] already prunes, and the Windows
/// default roots are fixed drives. Assumes `/proc/mounts` is the kernel's
/// view of this process's mount namespace; reading it never touches the
/// mounts themselves, so a dead share can't stall it.
#[cfg(target_os = "linux")]
pub(super) fn network_mounts() -> Vec<std::path::PathBuf> {
    std::fs::read_to_string("/proc/mounts")
        .map(|contents| network_mount_points(&contents))
        .unwrap_or_default()
}
#[cfg(not(target_os = "linux"))]
pub(super) fn network_mounts() -> Vec<std::path::PathBuf> {
    Vec::new()
}

/// The network mount points in `/proc/mounts` contents. Pure so tests can
/// use fixture mount tables.
#[cfg(any(target_os = "linux", test))]
pub(super) fn network_mount_points(proc_mounts: &str) -> Vec<std::path::PathBuf> {
    crate::drives::parse_proc_mounts(proc_mounts)
        .into_iter()
        .filter(|entry| crate::drives::is_network_fstype(&entry.fstype))
        .map(|entry| entry.mount)
        .collect()
}

/// Whether folder names compare case-insensitively: NTFS and APFS/HFS+
/// default to case-insensitive, Linux filesystems don't. A case-sensitive
/// APFS volume would over-match here, which only excludes more.
pub(super) const CASE_INSENSITIVE_NAMES: bool = cfg!(any(windows, target_os = "macos"));

/// Gated to the platforms with a real identity source: elsewhere every file
/// gets the zero device/key fallback and distinctness can't hold.
#[cfg(all(test, any(unix, windows)))]
mod tests {
    use super::identity;
    use crate::catalog::segment::Identity;
    use std::{fs, io::Write, path::Path};

    fn identity_of(path: &Path) -> Identity {
        identity(path, &fs::symlink_metadata(path).unwrap())
    }

    /// `(device, key)` is what the index matches on; `birth` is left out
    /// because tmpfs and some Linux filesystems report no creation time.
    fn device_key(path: &Path) -> (u64, u64) {
        let id = identity_of(path);
        (id.device, id.key)
    }

    #[test]
    fn distinct_files_in_one_dir_share_device_but_not_key() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a.txt"), dir.path().join("b.txt"));
        fs::write(&a, b"a").unwrap();
        fs::write(&b, b"b").unwrap();
        let (a, b) = (identity_of(&a), identity_of(&b));
        assert_eq!(a.device, b.device);
        assert_ne!(a.key, b.key);
    }

    #[test]
    fn rename_within_dir_keeps_identity() {
        let dir = tempfile::tempdir().unwrap();
        let (before, after) = (dir.path().join("old.txt"), dir.path().join("new.txt"));
        fs::write(&before, b"x").unwrap();
        let original = device_key(&before);
        fs::rename(&before, &after).unwrap();
        assert_eq!(device_key(&after), original);
    }

    #[test]
    fn directory_and_file_have_distinct_identities() {
        let dir = tempfile::tempdir().unwrap();
        let (sub, file) = (dir.path().join("sub"), dir.path().join("file.txt"));
        fs::create_dir(&sub).unwrap();
        fs::write(&file, b"x").unwrap();
        assert_ne!(device_key(&sub), device_key(&file));
    }

    /// A zero key is the Windows "handle couldn't open" fallback; a normal
    /// file must never produce it.
    #[test]
    fn normal_file_has_nonzero_key() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file.txt");
        fs::write(&file, b"x").unwrap();
        assert_ne!(identity_of(&file).key, 0);
    }

    #[test]
    fn content_change_keeps_identity() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file.txt");
        fs::write(&file, b"x").unwrap();
        let original = device_key(&file);
        fs::OpenOptions::new()
            .append(true)
            .open(&file)
            .unwrap()
            .write_all(b"more bytes")
            .unwrap();
        assert_eq!(device_key(&file), original);
    }
}

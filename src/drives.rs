//! Mounted-volume enumeration with capacity, behind a per-OS boundary.
//!
//! Each platform reports its mount points differently — macOS lists
//! `/Volumes/*`, Linux parses `/proc/mounts` for real block devices,
//! Windows walks the logical drive letters — but they converge on the
//! same [`Drive`] shape (name, mount path, total/free bytes) so the
//! sidebar renders one way everywhere. The syscalls block, so the app
//! calls [`list_drives`] on a background executor, not per frame.
//!
//! Platform assumptions live next to each implementation. The pure
//! parsing/derivation bits (`/proc/mounts` filtering, the used
//! fraction) are factored out and unit-tested; the raw `statvfs` /
//! Win32 calls are exercised on the dev machines and CI runners.

use std::path::PathBuf;

/// One mounted volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Drive {
    /// Display name (volume label, mount-point leaf, or drive letter).
    pub name: String,
    /// Mount point to navigate to when clicked.
    pub path: PathBuf,
    pub total_bytes: u64,
    pub free_bytes: u64,
}

impl Drive {
    /// Fraction of the volume in use, 0.0..=1.0 (0 when the total is
    /// unknown, so an empty bar rather than a divide-by-zero).
    pub fn used_fraction(&self) -> f32 {
        if self.total_bytes == 0 {
            return 0.0;
        }
        let used = self.total_bytes.saturating_sub(self.free_bytes);
        (used as f64 / self.total_bytes as f64) as f32
    }
}

/// Enumerate mounted volumes with their capacity. Blocking — call on a
/// background executor.
pub fn list_drives() -> Vec<Drive> {
    platform::list_drives()
}

/// The roots to index on a fresh install, before the user has configured
/// any. Windows returns every *fixed* (non-removable, non-network) drive —
/// the Everything-style whole-machine default. macOS/Linux stay scoped to
/// the home directory: indexing all of `/` there needs elevated/full-disk
/// access and would pull in the OS itself, which is not an expected
/// default. Each platform's indexer sits behind the same trait, so the
/// rest of the app treats these roots identically.
pub fn default_index_roots() -> Vec<PathBuf> {
    platform::default_index_roots()
}

/// Mount points worth showing from `/proc/mounts` contents: real block
/// devices (device path starts with `/dev/`), de-duplicated by mount
/// point, in file order. Pure so it's testable without a real `/proc`.
#[cfg(any(target_os = "linux", test))]
pub fn parse_proc_mounts(contents: &str) -> Vec<PathBuf> {
    let mut seen = std::collections::HashSet::new();
    let mut mounts = Vec::new();
    for line in contents.lines() {
        let mut fields = line.split_whitespace();
        let (Some(device), Some(mount)) = (fields.next(), fields.next()) else {
            continue;
        };
        if !device.starts_with("/dev/") {
            continue;
        }
        // /proc/mounts octal-escapes spaces etc. as \040 — good enough to
        // handle the common space case for display/navigation.
        let mount = mount.replace("\\040", " ");
        let path = PathBuf::from(mount);
        if seen.insert(path.clone()) {
            mounts.push(path);
        }
    }
    mounts
}

/// The label for a mount point: its leaf name, or the path itself for
/// the root (`/`).
#[cfg(any(unix, test))]
pub fn mount_label(path: &std::path::Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

#[cfg(unix)]
mod platform {
    use std::path::Path;

    use super::{Drive, mount_label};

    /// Free/total bytes for the filesystem containing `path`, via
    /// `statvfs`. `f_bavail` (not `f_bfree`) is used for free space —
    /// what an unprivileged user can actually write. Returns `None` if
    /// the call fails (an unmounted or vanished volume).
    fn capacity(path: &Path) -> Option<(u64, u64)> {
        use std::os::unix::ffi::OsStrExt;
        let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
        // SAFETY: statvfs writes into a zeroed struct we own; c_path is a
        // valid NUL-terminated C string for the duration of the call.
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } != 0 {
            return None;
        }
        let block = stat.f_frsize as u64;
        let total = (stat.f_blocks as u64).saturating_mul(block);
        let free = (stat.f_bavail as u64).saturating_mul(block);
        Some((total, free))
    }

    fn drive_at(path: std::path::PathBuf, name: String) -> Option<Drive> {
        let (total_bytes, free_bytes) = capacity(&path)?;
        Some(Drive { name, path, total_bytes, free_bytes })
    }

    /// Fresh-install default on Unix: the home directory. Unlike Windows
    /// (whole fixed drives), indexing all of `/` here needs elevated or
    /// full-disk access and includes the OS itself, so the default stays
    /// scoped to the user.
    pub fn default_index_roots() -> Vec<std::path::PathBuf> {
        std::env::home_dir().into_iter().collect()
    }

    /// macOS: the user-visible volumes are the mounts under `/Volumes`.
    #[cfg(target_os = "macos")]
    pub fn list_drives() -> Vec<Drive> {
        let Ok(entries) = std::fs::read_dir("/Volumes") else {
            return Vec::new();
        };
        let mut drives: Vec<Drive> = entries
            .filter_map(|entry| {
                let path = entry.ok()?.path();
                let name = mount_label(&path);
                drive_at(path, name)
            })
            .collect();
        drives.sort_by(|a, b| a.name.cmp(&b.name));
        drives
    }

    /// Linux: real block-device mounts from `/proc/mounts`.
    #[cfg(target_os = "linux")]
    pub fn list_drives() -> Vec<Drive> {
        let Ok(contents) = std::fs::read_to_string("/proc/mounts") else {
            return Vec::new();
        };
        super::parse_proc_mounts(&contents)
            .into_iter()
            .filter_map(|path| {
                let name = mount_label(&path);
                drive_at(path, name)
            })
            .collect()
    }
}

#[cfg(windows)]
mod platform {
    use std::path::PathBuf;

    use windows::Win32::Storage::FileSystem::{
        GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives,
    };
    use windows::core::HSTRING;

    use super::Drive;

    /// `DRIVE_FIXED` from `GetDriveTypeW` — a local, non-removable disk.
    /// Defined here rather than imported: the constant lives in the
    /// `Win32_System_WindowsProgramming` feature we don't otherwise pull
    /// in, and its value (3) is stable Win32 ABI.
    const DRIVE_FIXED: u32 = 3;

    /// Fresh-install default on Windows: every *fixed* drive (internal
    /// disks), skipping removable and network drives so plugging in a USB
    /// stick or mapping a share doesn't kick off a whole-volume re-index.
    /// `GetDriveTypeW` classifies each letter; `DRIVE_FIXED` is the local
    /// hard-drive case.
    pub fn default_index_roots() -> Vec<PathBuf> {
        let mask = unsafe { GetLogicalDrives() };
        let mut roots = Vec::new();
        for i in 0..26u32 {
            if mask & (1 << i) == 0 {
                continue;
            }
            let letter = (b'A' + i as u8) as char;
            let root = format!("{letter}:\\");
            let wide = HSTRING::from(root.as_str());
            // SAFETY: `wide` is a valid NUL-terminated wide path for the
            // duration of the call.
            if unsafe { GetDriveTypeW(&wide) } == DRIVE_FIXED {
                roots.push(PathBuf::from(&root));
            }
        }
        roots
    }

    /// Windows: each set bit of the logical-drives mask is a letter A–Z.
    pub fn list_drives() -> Vec<Drive> {
        let mask = unsafe { GetLogicalDrives() };
        let mut drives = Vec::new();
        for i in 0..26u32 {
            if mask & (1 << i) == 0 {
                continue;
            }
            let letter = (b'A' + i as u8) as char;
            let root = format!("{letter}:\\");
            let (mut total, mut free) = (0u64, 0u64);
            let wide = HSTRING::from(root.as_str());
            // SAFETY: valid NUL-terminated wide path; the out-params are
            // owned locals. Skip drives the call rejects (empty optical
            // drives, disconnected network mounts).
            let ok = unsafe {
                GetDiskFreeSpaceExW(&wide, None, Some(&mut total), Some(&mut free)).is_ok()
            };
            if ok {
                drives.push(Drive {
                    name: format!("{letter}:"),
                    path: PathBuf::from(&root),
                    total_bytes: total,
                    free_bytes: free,
                });
            }
        }
        drives
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn used_fraction_is_clamped_and_safe() {
        let d = Drive { name: "x".into(), path: "/".into(), total_bytes: 0, free_bytes: 0 };
        assert_eq!(d.used_fraction(), 0.0); // no divide-by-zero
        let d = Drive { name: "x".into(), path: "/".into(), total_bytes: 100, free_bytes: 25 };
        assert!((d.used_fraction() - 0.75).abs() < 1e-6);
        // Free somehow exceeding total must not underflow.
        let d = Drive { name: "x".into(), path: "/".into(), total_bytes: 100, free_bytes: 200 };
        assert_eq!(d.used_fraction(), 0.0);
    }

    #[test]
    fn proc_mounts_keeps_only_block_devices_deduped() {
        let contents = "\
sysfs /sys sysfs rw 0 0
/dev/sda1 / ext4 rw 0 0
proc /proc proc rw 0 0
/dev/sdb1 /mnt/data ext4 rw 0 0
tmpfs /run tmpfs rw 0 0
/dev/sda1 / ext4 rw,remount 0 0
/dev/sdc1 /media/My\\040Disk vfat rw 0 0
";
        let mounts = parse_proc_mounts(contents);
        assert_eq!(
            mounts,
            vec![
                PathBuf::from("/"),
                PathBuf::from("/mnt/data"),
                PathBuf::from("/media/My Disk"),
            ]
        );
    }

    #[test]
    fn mount_label_uses_leaf_or_root() {
        assert_eq!(mount_label(std::path::Path::new("/mnt/data")), "data");
        assert_eq!(mount_label(std::path::Path::new("/")), "/");
    }
}

//! Mounted-volume enumeration with capacity, behind a per-OS boundary.
//!
//! Each platform reports mounts differently — macOS lists `/Volumes/*`,
//! Linux parses `/proc/mounts` for real block devices, Windows walks the
//! drive letters — converging on one [`Drive`] shape so the sidebar
//! renders identically. The syscalls block, so [`list_drives`] runs on a
//! background executor, not per frame.
//!
//! Platform assumptions live next to each implementation; the pure
//! parsing bits are factored out and unit-tested.

use std::path::{Component, Path, PathBuf, Prefix};

/// The "This PC" location: every drive on one page, as in Explorer.
/// Windows has no single filesystem root, so this stands in for `/`.
/// It isn't a real path but a reserved spelling (Explorer's own shell
/// name for the folder). `:` is illegal in a Windows file name, so a path
/// accidentally joined onto it fails to resolve instead of landing in the
/// process's working directory.
pub const THIS_PC: &str = "::{20D04FE0-3AEA-1069-A2D8-08002B30309D}";

/// What [`THIS_PC`] is called on screen.
pub const THIS_PC_LABEL: &str = "This PC";

/// The [`THIS_PC`] location as a navigable path.
pub fn this_pc() -> PathBuf {
    PathBuf::from(THIS_PC)
}

/// Whether `path` is the [`THIS_PC`] location rather than a folder.
pub fn is_this_pc(path: &Path) -> bool {
    path.as_os_str() == THIS_PC
}

/// Where "up" goes from `path`: its parent folder, or This PC from a
/// drive root on Windows (Explorer's behavior). `None` at the top.
pub fn parent_location(path: &Path) -> Option<PathBuf> {
    parent_or_this_pc(path, cfg!(windows))
}

/// [`parent_location`] with the platform choice passed in, so both
/// behaviors are tested everywhere. Only drive-letter roots (`C:\`,
/// `\\?\C:\`) step up to This PC; a UNC share root stays the top.
fn parent_or_this_pc(path: &Path, has_this_pc: bool) -> Option<PathBuf> {
    // `Path::parent` would hand back "" for the reserved spelling.
    if is_this_pc(path) {
        return None;
    }
    if let Some(parent) = path.parent() {
        return Some(parent.to_path_buf());
    }
    let drive_root = matches!(
        path.components().next(),
        Some(Component::Prefix(p)) if matches!(p.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_))
    );
    (has_this_pc && drive_root && path.has_root()).then(this_pc)
}

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

/// The roots to index on a fresh install. Windows returns every *fixed*
/// drive — the Everything-style whole-machine default. macOS/Linux stay
/// scoped to home: indexing all of `/` needs full-disk access and would
/// pull in the OS itself. Every platform's indexer is behind the same
/// trait, so the rest of the app treats these roots identically.
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
        Some(Drive {
            name,
            path,
            total_bytes,
            free_bytes,
        })
    }

    /// Fresh-install default on macOS: the whole startup disk, like
    /// Windows' fixed drives. The OS folders, other volumes and the user's
    /// excluded folder names are pruned by `ingest::IndexFilter`; FSEvents
    /// covers `/` with one stream. Without Full Disk Access the protected
    /// folders are skipped and counted, not fatal.
    #[cfg(target_os = "macos")]
    pub fn default_index_roots() -> Vec<std::path::PathBuf> {
        vec![std::path::PathBuf::from("/")]
    }

    /// Fresh-install default on Linux: the home directory. Watching `/`
    /// needs fanotify (`CAP_SYS_ADMIN`); the inotify fallback costs one
    /// watch per directory and would exhaust `max_user_watches` long before
    /// a whole disk is covered (docs/indexing-architecture.md §2).
    #[cfg(not(target_os = "macos"))]
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
        GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
    };
    use windows::core::HSTRING;

    use super::Drive;

    /// `DRIVE_FIXED` from `GetDriveTypeW` — a local, non-removable disk.
    /// Defined here rather than imported: the constant lives in the
    /// `Win32_System_WindowsProgramming` feature we don't otherwise pull
    /// in, and its value (3) is stable Win32 ABI.
    const DRIVE_FIXED: u32 = 3;

    /// Fresh-install default on Windows: every *fixed* drive, skipping
    /// removable and network ones so a USB stick or mapped share doesn't
    /// kick off a whole-volume re-index. `GetDriveTypeW` classifies each
    /// letter; `DRIVE_FIXED` is the local hard-drive case.
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
                    name: super::drive_label(volume_label(&wide).as_deref(), letter),
                    path: PathBuf::from(&root),
                    total_bytes: total,
                    free_bytes: free,
                });
            }
        }
        drives
    }

    /// The volume label for a drive root, `None` when it has none or the
    /// call fails. Assumes `root` is a mounted drive root (`C:\`) that
    /// `GetDiskFreeSpaceExW` has just accepted, so the query won't spin up
    /// an empty optical drive.
    fn volume_label(root: &HSTRING) -> Option<String> {
        // MAX_PATH + 1 is the documented maximum for the label buffer.
        let mut buf = [0u16; 261];
        // SAFETY: valid NUL-terminated wide path; `buf` is a local the
        // call writes a NUL-terminated label into.
        unsafe { GetVolumeInformationW(root, Some(&mut buf), None, None, None, None) }.ok()?;
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..len]))
    }
}

/// Explorer's name for a Windows drive: "Data (D:)", or "Local Disk (C:)"
/// when the volume has no label.
#[cfg(any(windows, test))]
fn drive_label(volume: Option<&str>, letter: char) -> String {
    let volume = volume.map(str::trim).filter(|v| !v.is_empty());
    format!("{} ({letter}:)", volume.unwrap_or("Local Disk"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn used_fraction_is_clamped_and_safe() {
        let d = Drive {
            name: "x".into(),
            path: "/".into(),
            total_bytes: 0,
            free_bytes: 0,
        };
        assert_eq!(d.used_fraction(), 0.0); // no divide-by-zero
        let d = Drive {
            name: "x".into(),
            path: "/".into(),
            total_bytes: 100,
            free_bytes: 25,
        };
        assert!((d.used_fraction() - 0.75).abs() < 1e-6);
        // Free somehow exceeding total must not underflow.
        let d = Drive {
            name: "x".into(),
            path: "/".into(),
            total_bytes: 100,
            free_bytes: 200,
        };
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
    fn this_pc_is_recognized_only_by_its_exact_spelling() {
        assert!(is_this_pc(&this_pc()));
        assert!(!is_this_pc(Path::new("")));
        assert!(!is_this_pc(Path::new("This PC")));
        assert!(!is_this_pc(&this_pc().join("x")));
    }

    #[test]
    fn up_from_a_folder_is_its_parent_everywhere() {
        let dir = Path::new("/home/me/docs");
        assert_eq!(
            parent_or_this_pc(dir, true),
            Some(PathBuf::from("/home/me"))
        );
        assert_eq!(
            parent_or_this_pc(dir, false),
            Some(PathBuf::from("/home/me"))
        );
    }

    #[test]
    fn this_pc_has_nothing_above_it() {
        assert_eq!(parent_or_this_pc(&this_pc(), true), None);
    }

    #[cfg(windows)]
    #[test]
    fn up_from_a_drive_root_is_this_pc_on_windows() {
        assert_eq!(parent_or_this_pc(Path::new(r"C:\"), true), Some(this_pc()));
        assert_eq!(
            parent_or_this_pc(Path::new(r"\\?\D:\"), true),
            Some(this_pc())
        );
        // A drive-relative "C:" or a share root isn't a drive's top.
        assert_eq!(parent_or_this_pc(Path::new("C:"), true), None);
        assert_eq!(parent_or_this_pc(Path::new(r"\\server\share\"), true), None);
        assert_eq!(parent_or_this_pc(Path::new(r"C:\"), false), None);
    }

    #[cfg(unix)]
    #[test]
    fn unix_root_has_nothing_above_it() {
        assert_eq!(parent_location(Path::new("/")), None);
    }

    #[test]
    fn drives_are_named_like_explorer() {
        assert_eq!(drive_label(Some("Data"), 'D'), "Data (D:)");
        assert_eq!(drive_label(Some("  "), 'C'), "Local Disk (C:)");
        assert_eq!(drive_label(None, 'C'), "Local Disk (C:)");
    }

    #[test]
    fn mount_label_uses_leaf_or_root() {
        assert_eq!(mount_label(std::path::Path::new("/mnt/data")), "data");
        assert_eq!(mount_label(std::path::Path::new("/")), "/");
    }
}

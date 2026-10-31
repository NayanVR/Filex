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

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::Duration;

use crate::network::ShareAddress;

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

/// What kind of storage a [`Drive`] is. Decides its icon and whether its
/// capacity probe runs under a time limit (network only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriveKind {
    /// An internal disk or partition.
    Fixed,
    /// A USB stick, SD card, optical disc or other pluggable media.
    Removable,
    /// A mounted network share (SMB, NFS, sshfs, WebDAV, …).
    Network,
}

/// A volume's size and free space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capacity {
    pub total_bytes: u64,
    pub free_bytes: u64,
}

impl Capacity {
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

/// One mounted volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Drive {
    /// Display name (volume label, share name, mount-point leaf, or drive
    /// letter).
    pub name: String,
    /// Mount point to navigate to when clicked.
    pub path: PathBuf,
    pub kind: DriveKind,
    /// `None` when the volume didn't answer: a disconnected mapped drive or
    /// a network mount that's stalled. Local drives that fail the query
    /// aren't listed at all (an empty card reader).
    pub capacity: Option<Capacity>,
    /// Where a network drive points, when the OS says so in a form
    /// [`ShareAddress`] understands. Lets a typed `smb://` or UNC address
    /// open the folder it's already mounted at.
    pub share: Option<ShareAddress>,
}

impl Drive {
    /// Fraction of the volume in use; 0 when the capacity is unknown.
    pub fn used_fraction(&self) -> f32 {
        self.capacity.map_or(0.0, |c| c.used_fraction())
    }
}

/// The folder `address` is mounted at among `drives`, if any. See
/// [`ShareAddress::locate`].
pub fn locate_share(address: &ShareAddress, drives: &[Drive]) -> Option<PathBuf> {
    address.locate(
        drives
            .iter()
            .filter_map(|d| Some((d.share.as_ref()?, d.path.as_path()))),
    )
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

/// How long a network drive gets to report its capacity before it's shown
/// as not responding.
const NETWORK_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Probes that outlived their timeout and haven't returned yet, by key.
type StuckProbes = Arc<Mutex<HashSet<PathBuf>>>;

/// The process-wide [`StuckProbes`] the real drive refresh uses.
#[cfg(any(unix, windows))]
fn stuck_probes() -> &'static StuckProbes {
    static STUCK: OnceLock<StuckProbes> = OnceLock::new();
    STUCK.get_or_init(StuckProbes::default)
}

/// Lock that shrugs off poisoning: the set only ever holds plain paths.
fn lock(stuck: &StuckProbes) -> std::sync::MutexGuard<'_, HashSet<PathBuf>> {
    stuck
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Run `probe` on its own thread and wait at most `timeout` for it.
///
/// A network filesystem that stops answering blocks `statvfs` /
/// `GetDiskFreeSpaceExW` for a long time — an NFS hard mount, until the
/// server comes back. The drive refresh runs on the shared background
/// executor every 30 s, so waiting inline would park one executor thread
/// per refresh until listing and search starve. Instead the probe gets a
/// thread of its own; if it times out, `key` is remembered and no new
/// probe starts for it until the stuck one returns. That bounds the cost
/// of a dead mount at one parked thread.
fn bounded_probe<T: Send + 'static>(
    key: &Path,
    timeout: Duration,
    stuck: &StuckProbes,
    probe: impl FnOnce() -> Option<T> + Send + 'static,
) -> Option<T> {
    if lock(stuck).contains(key) {
        return None;
    }
    let (tx, rx) = mpsc::sync_channel(1);
    let finished = Arc::new(AtomicBool::new(false));
    let (thread_key, thread_stuck, thread_finished) =
        (key.to_path_buf(), Arc::clone(stuck), Arc::clone(&finished));
    let spawned = std::thread::Builder::new()
        .name("filex-drive-probe".into())
        .spawn(move || {
            let result = probe();
            // Under the lock, so the waiter either sees `finished` or its
            // insert below is undone here.
            let mut stuck = lock(&thread_stuck);
            thread_finished.store(true, Ordering::Relaxed);
            stuck.remove(&thread_key);
            drop(stuck);
            let _ = tx.send(result);
        });
    if let Err(error) = spawned {
        tracing::warn!("couldn't start a drive probe: {error}");
        return None;
    }
    match rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(_) => {
            let mut stuck = lock(stuck);
            if !finished.load(Ordering::Relaxed) {
                stuck.insert(key.to_path_buf());
            }
            None
        }
    }
}

/// [`bounded_probe`] with the real timeout and stuck set.
#[cfg(any(unix, windows))]
fn network_capacity(
    key: &Path,
    probe: impl FnOnce() -> Option<Capacity> + Send + 'static,
) -> Option<Capacity> {
    bounded_probe(key, NETWORK_PROBE_TIMEOUT, stuck_probes(), probe)
}

/// Filesystem types that reach over the network. The SMB and NFS ones
/// also parse into a [`ShareAddress`]; the rest are only classified.
pub const NETWORK_FSTYPES: &[&str] = &[
    "cifs",
    "smb3",
    "smbfs",
    "nfs",
    "nfs4",
    "afpfs",
    "webdav",
    "davfs",
    "fuse.sshfs",
    "fuse.rclone",
    "fuse.s3fs",
    "ceph",
    "glusterfs",
    "fuse.glusterfs",
    "afs",
];

/// Whether `fstype` (a mount table's type field) is a network filesystem.
pub fn is_network_fstype(fstype: &str) -> bool {
    NETWORK_FSTYPES.contains(&fstype)
}

/// One line of a Unix mount table, decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountEntry {
    /// What's mounted: `/dev/sda1`, `//nas/share`, `fs1:/srv`.
    pub source: String,
    pub mount: PathBuf,
    pub fstype: String,
}

/// Mounts worth showing from `/proc/mounts` contents: real block devices
/// (source starts with `/dev/`) and network filesystems
/// ([`is_network_fstype`]), de-duplicated by mount point, in file order.
/// Pure so it's testable without a real `/proc`.
#[cfg(any(target_os = "linux", test))]
pub fn parse_proc_mounts(contents: &str) -> Vec<MountEntry> {
    let mut seen = std::collections::HashSet::new();
    let mut mounts = Vec::new();
    for line in contents.lines() {
        let mut fields = line.split_whitespace();
        let (Some(source), Some(mount), Some(fstype)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if !source.starts_with("/dev/") && !is_network_fstype(fstype) {
            continue;
        }
        let path = PathBuf::from(unescape_mount_field(mount));
        if seen.insert(path.clone()) {
            mounts.push(MountEntry {
                source: unescape_mount_field(source),
                mount: path,
                fstype: fstype.to_owned(),
            });
        }
    }
    mounts
}

/// Undo `/proc/mounts`' octal escapes (`\040` space, `\011` tab, `\012`
/// newline, `\134` backslash). Anything that isn't a valid three-digit
/// escape is kept as written.
#[cfg(any(target_os = "linux", test))]
fn unescape_mount_field(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let octal = bytes
            .get(i + 1..i + 4)
            .filter(|d| bytes[i] == b'\\' && d.iter().all(|b| (b'0'..=b'7').contains(b)))
            .map(|d| d.iter().fold(0u32, |n, b| n * 8 + u32::from(b - b'0')))
            .and_then(|n| u8::try_from(n).ok());
        match octal {
            Some(byte) => {
                out.push(byte);
                i += 4;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The GVfs FUSE mount for this user's session in `/proc/mounts`
/// contents: the `fuse.gvfsd-fuse` mount at `<runtime_dir>/gvfs`. GVfs
/// puts every share it mounts in a subfolder of that one mount, so
/// [`parse_proc_mounts`] can't list them itself; other users' sessions
/// have their own, which aren't ours to show.
#[cfg(any(target_os = "linux", test))]
pub fn gvfs_mount_point(contents: &str, runtime_dir: &Path) -> Option<PathBuf> {
    let wanted = runtime_dir.join("gvfs");
    contents.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let (Some(_), Some(mount), Some("fuse.gvfsd-fuse")) =
            (fields.next(), fields.next(), fields.next())
        else {
            return None;
        };
        let mount = PathBuf::from(unescape_mount_field(mount));
        (mount == wanted).then_some(mount)
    })
}

/// The kind of a Linux mount: network by filesystem type, removable when
/// udisks put it under `/media` or `/run/media` (where desktop
/// auto-mounting puts USB sticks and SD cards), fixed otherwise. A
/// heuristic — a disk mounted there by hand reads as removable — but it
/// needs no sysfs walk.
#[cfg(any(target_os = "linux", test))]
pub fn linux_drive_kind(entry: &MountEntry) -> DriveKind {
    if is_network_fstype(&entry.fstype) {
        DriveKind::Network
    } else if entry.mount.starts_with("/media") || entry.mount.starts_with("/run/media") {
        DriveKind::Removable
    } else {
        DriveKind::Fixed
    }
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
    use std::path::{Path, PathBuf};

    use super::{Capacity, Drive, DriveKind, mount_label};
    use crate::network::ShareAddress;

    /// Free/total bytes for the filesystem containing `path`, via
    /// `statvfs`. `f_bavail` (not `f_bfree`) is used for free space —
    /// what an unprivileged user can actually write. Returns `None` if
    /// the call fails (an unmounted or vanished volume). On a network
    /// mount this can block for as long as the server is unreachable, so
    /// network drives go through [`super::network_capacity`].
    fn capacity(path: &Path) -> Option<Capacity> {
        use std::os::unix::ffi::OsStrExt;
        let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
        // SAFETY: statvfs writes into a zeroed struct we own; c_path is a
        // valid NUL-terminated C string for the duration of the call.
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } != 0 {
            return None;
        }
        let block = stat.f_frsize as u64;
        Some(Capacity {
            total_bytes: (stat.f_blocks as u64).saturating_mul(block),
            free_bytes: (stat.f_bavail as u64).saturating_mul(block),
        })
    }

    /// A drive at `path`. Local volumes that fail the capacity query are
    /// left out; network ones stay, with unknown capacity, so a stalled
    /// share is shown as such rather than vanishing.
    fn drive_at(
        path: PathBuf,
        name: String,
        kind: DriveKind,
        share: Option<ShareAddress>,
    ) -> Option<Drive> {
        let capacity = if kind == DriveKind::Network {
            let probe_path = path.clone();
            super::network_capacity(&path, move || capacity(&probe_path))
        } else {
            Some(capacity(&path)?)
        };
        Some(Drive {
            name,
            path,
            kind,
            capacity,
            share,
        })
    }

    /// A network mount's name: `share (server)` when its source parses,
    /// else the mount point's leaf.
    fn name_for(path: &Path, share: Option<&ShareAddress>) -> String {
        share.map_or_else(|| mount_label(path), ShareAddress::display_name)
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

    /// One `getfsstat` row: where it's mounted, what from, which
    /// filesystem type, and whether the kernel calls it local.
    #[cfg(target_os = "macos")]
    struct MacMount {
        source: String,
        fstype: String,
        local: bool,
    }

    /// The kernel's mount table, keyed by mount point.
    ///
    /// Assumes `getfsstat(MNT_NOWAIT)` returns the cached `statfs` for
    /// every mount *without* contacting file servers (that's what
    /// `MNT_NOWAIT` buys over `MNT_WAIT`), so a dead SMB share can't stall
    /// it. `getfsstat` rather than `getmntinfo`: the latter returns a
    /// shared static buffer, and two refreshes can overlap.
    #[cfg(target_os = "macos")]
    fn mount_table() -> std::collections::HashMap<PathBuf, MacMount> {
        use std::ffi::CStr;
        let mut table = std::collections::HashMap::new();
        // SAFETY: a null buffer asks only for the count.
        let count = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
        let Ok(count) = usize::try_from(count) else {
            return table;
        };
        // Room for a few mounts appearing between the two calls.
        let capacity = count + 8;
        // SAFETY: statfs is plain old data; zeroed is a valid value.
        let mut buf: Vec<libc::statfs> = vec![unsafe { std::mem::zeroed() }; capacity];
        let Ok(bytes) = libc::c_int::try_from(capacity * std::mem::size_of::<libc::statfs>())
        else {
            return table;
        };
        // SAFETY: `buf` holds `capacity` structs and `bytes` says so.
        let filled = unsafe { libc::getfsstat(buf.as_mut_ptr(), bytes, libc::MNT_NOWAIT) };
        let Ok(filled) = usize::try_from(filled) else {
            return table;
        };
        for stat in buf.iter().take(filled.min(capacity)) {
            // SAFETY: the kernel NUL-terminates each fixed-size name field.
            let text = |field: &[libc::c_char]| unsafe {
                CStr::from_ptr(field.as_ptr())
                    .to_string_lossy()
                    .into_owned()
            };
            table.insert(
                PathBuf::from(text(&stat.f_mntonname)),
                MacMount {
                    source: text(&stat.f_mntfromname),
                    fstype: text(&stat.f_fstypename),
                    local: stat.f_flags & (libc::MNT_LOCAL as u32) != 0,
                },
            );
        }
        table
    }

    /// macOS: the user-visible volumes are the mounts under `/Volumes`.
    /// Network shares are told apart by the mount table's local flag; an
    /// entry the table doesn't have (the startup disk's `/Volumes`
    /// symlink) is a local disk.
    #[cfg(target_os = "macos")]
    pub fn list_drives() -> Vec<Drive> {
        let Ok(entries) = std::fs::read_dir("/Volumes") else {
            return Vec::new();
        };
        let table = mount_table();
        let mut drives: Vec<Drive> = entries
            .filter_map(|entry| {
                let path = entry.ok()?.path();
                match table.get(&path) {
                    Some(mount) if !mount.local => {
                        let share = ShareAddress::from_mount_source(&mount.source, &mount.fstype);
                        let name = name_for(&path, share.as_ref());
                        drive_at(path, name, DriveKind::Network, share)
                    }
                    _ => {
                        let name = mount_label(&path);
                        drive_at(path, name, DriveKind::Fixed, None)
                    }
                }
            })
            .collect();
        drives.sort_by(|a, b| a.name.cmp(&b.name));
        drives
    }

    /// Linux: block-device and network mounts from `/proc/mounts`.
    /// Reading `/proc/mounts` never touches the mounts themselves.
    #[cfg(target_os = "linux")]
    pub fn list_drives() -> Vec<Drive> {
        let Ok(contents) = std::fs::read_to_string("/proc/mounts") else {
            return Vec::new();
        };
        super::parse_proc_mounts(&contents)
            .into_iter()
            .filter_map(|entry| {
                let kind = super::linux_drive_kind(&entry);
                let share = ShareAddress::from_mount_source(&entry.source, &entry.fstype);
                let name = match kind {
                    DriveKind::Network => name_for(&entry.mount, share.as_ref()),
                    _ => mount_label(&entry.mount),
                };
                drive_at(entry.mount, name, kind, share)
            })
            .chain(gvfs_drives(&contents))
            .collect()
    }

    /// The shares GVfs has mounted, one drive each (FIL-61). Listing the
    /// GVfs folder is answered by the local `gvfsd-fuse` daemon, but a
    /// wedged daemon blocks it, so it runs under the same time limit as a
    /// network capacity probe; each share's own capacity probe is bounded
    /// as for any network drive.
    #[cfg(target_os = "linux")]
    fn gvfs_drives(contents: &str) -> Vec<Drive> {
        let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            // SAFETY: getuid has no preconditions and can't fail.
            .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() })));
        let Some(gvfs) = super::gvfs_mount_point(contents, &runtime_dir) else {
            return Vec::new();
        };
        let dir = gvfs.clone();
        let names = super::bounded_probe(
            &gvfs,
            super::NETWORK_PROBE_TIMEOUT,
            super::stuck_probes(),
            move || {
                let entries = std::fs::read_dir(&dir).ok()?;
                Some(entries.flatten().map(|e| e.file_name()).collect::<Vec<_>>())
            },
        );
        let mut drives: Vec<Drive> = names
            .unwrap_or_default()
            .into_iter()
            .filter_map(|name| {
                let mount = crate::network::GvfsMount::parse(name.to_str()?)?;
                drive_at(
                    gvfs.join(&name),
                    mount.label,
                    DriveKind::Network,
                    mount.share,
                )
            })
            .collect();
        drives.sort_by(|a, b| a.name.cmp(&b.name));
        drives
    }
}

#[cfg(windows)]
mod platform {
    use std::path::PathBuf;

    use windows::Win32::Foundation::{ERROR_CONNECTION_UNAVAIL, NO_ERROR};
    use windows::Win32::NetworkManagement::WNet::WNetGetConnectionW;
    use windows::Win32::Storage::FileSystem::{
        GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
    };
    use windows::core::{HSTRING, PWSTR};

    use super::{Capacity, Drive, DriveKind};
    use crate::network::ShareAddress;

    /// Fresh-install default on Windows: every *fixed* drive, skipping
    /// removable and network ones so a USB stick or mapped share doesn't
    /// kick off a whole-volume re-index. `GetDriveTypeW` classifies each
    /// letter; `DRIVE_FIXED` is the local hard-drive case.
    pub fn default_index_roots() -> Vec<PathBuf> {
        letters()
            .filter(|&letter| drive_type(letter) == super::DRIVE_FIXED)
            .map(|letter| PathBuf::from(format!("{letter}:\\")))
            .collect()
    }

    /// The drive letters in use, A–Z, from the logical-drives mask.
    fn letters() -> impl Iterator<Item = char> {
        // SAFETY: no arguments; returns a bitmask.
        let mask = unsafe { GetLogicalDrives() };
        (0..26u32)
            .filter(move |i| mask & (1 << i) != 0)
            .map(|i| (b'A' + i as u8) as char)
    }

    /// `GetDriveTypeW` for a letter's root. Reads the device type the I/O
    /// manager already knows; it doesn't contact a mapped drive's server.
    fn drive_type(letter: char) -> u32 {
        let wide = HSTRING::from(format!("{letter}:\\"));
        // SAFETY: `wide` is a valid NUL-terminated wide path for the
        // duration of the call.
        unsafe { GetDriveTypeW(&wide) }
    }

    /// Windows: each letter in use, classified by `GetDriveTypeW`. Local
    /// drives the capacity query rejects (an empty optical drive) are
    /// skipped; network drives are always listed — a disconnected mapped
    /// drive shows as such, as in Explorer.
    pub fn list_drives() -> Vec<Drive> {
        letters()
            .filter_map(
                |letter| match super::kind_for_drive_type(drive_type(letter)) {
                    DriveKind::Network => Some(network_drive(letter)),
                    kind => local_drive(letter, kind),
                },
            )
            .collect()
    }

    fn local_drive(letter: char, kind: DriveKind) -> Option<Drive> {
        let root = format!("{letter}:\\");
        let capacity = free_space(&root)?;
        Some(Drive {
            name: super::drive_label(volume_label(&HSTRING::from(&root)).as_deref(), letter),
            path: PathBuf::from(root),
            kind,
            capacity: Some(capacity),
            share: None,
        })
    }

    /// A mapped drive. Its UNC target comes from the WNet connection
    /// table (no server round-trip); its capacity from a bounded probe,
    /// since `GetDiskFreeSpaceExW` on an unreachable server can block for
    /// the SMB timeout. The volume label is skipped for the same reason.
    fn network_drive(letter: char) -> Drive {
        let root = format!("{letter}:\\");
        let share = remote_name(letter).and_then(|unc| ShareAddress::parse(&unc));
        let probe_root = root.clone();
        let capacity =
            super::network_capacity(std::path::Path::new(&root), move || free_space(&probe_root));
        Drive {
            name: super::network_drive_label(share.as_ref(), letter),
            path: PathBuf::from(root),
            kind: DriveKind::Network,
            capacity,
            share,
        }
    }

    /// Size and free space for a drive root; `None` when the call fails.
    fn free_space(root: &str) -> Option<Capacity> {
        let (mut total, mut free) = (0u64, 0u64);
        let wide = HSTRING::from(root);
        // SAFETY: valid NUL-terminated wide path; the out-params are
        // owned locals.
        unsafe { GetDiskFreeSpaceExW(&wide, None, Some(&mut total), Some(&mut free)) }.ok()?;
        Some(Capacity {
            total_bytes: total,
            free_bytes: free,
        })
    }

    /// The UNC path a mapped drive letter points at (`\\nas\Media`), from
    /// the WNet connection table. Assumes `WNetGetConnectionW` answers from
    /// the local table, and that it still fills in the name for a
    /// remembered drive whose server is unreachable
    /// (`ERROR_CONNECTION_UNAVAIL`).
    fn remote_name(letter: char) -> Option<String> {
        let local = HSTRING::from(format!("{letter}:"));
        // Long enough for any UNC path WNet hands back.
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        // SAFETY: `local` is NUL-terminated; `buf` holds `len` u16s.
        let status = unsafe { WNetGetConnectionW(&local, Some(PWSTR(buf.as_mut_ptr())), &mut len) };
        if status != NO_ERROR && status != ERROR_CONNECTION_UNAVAIL {
            return None;
        }
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..end]))
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

// `GetDriveTypeW` results. Defined here rather than imported: the
// constants live in the `Win32_System_WindowsProgramming` feature we don't
// otherwise pull in, and their values are stable Win32 ABI.
#[cfg(any(windows, test))]
const DRIVE_REMOVABLE: u32 = 2;
#[cfg(any(windows, test))]
const DRIVE_FIXED: u32 = 3;
#[cfg(any(windows, test))]
const DRIVE_REMOTE: u32 = 4;
#[cfg(any(windows, test))]
const DRIVE_CDROM: u32 = 5;

/// The [`DriveKind`] for a `GetDriveTypeW` result. Unknown types (and a
/// RAM disk) count as fixed.
#[cfg(any(windows, test))]
fn kind_for_drive_type(drive_type: u32) -> DriveKind {
    match drive_type {
        DRIVE_REMOTE => DriveKind::Network,
        DRIVE_REMOVABLE | DRIVE_CDROM => DriveKind::Removable,
        _ => DriveKind::Fixed,
    }
}

/// Explorer's name for a mapped drive: `Movies (\\nas\Media) (Z:)` — the
/// last folder, then where it lives — or `Network Drive (Z:)` when the
/// target is unknown.
#[cfg(any(windows, test))]
fn network_drive_label(share: Option<&ShareAddress>, letter: char) -> String {
    let Some((leaf, parent)) = share.and_then(|s| s.segments.split_last()) else {
        return format!("Network Drive ({letter}:)");
    };
    let mut location = format!(r"\\{}", share.map_or("", |s| s.host.as_str()));
    for segment in parent {
        location.push('\\');
        location.push_str(segment);
    }
    format!("{leaf} ({location}) ({letter}:)")
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

    fn capacity(total_bytes: u64, free_bytes: u64) -> Capacity {
        Capacity {
            total_bytes,
            free_bytes,
        }
    }

    fn drive(path: &str, capacity: Option<Capacity>, share: Option<ShareAddress>) -> Drive {
        Drive {
            name: "x".into(),
            path: path.into(),
            kind: DriveKind::Network,
            capacity,
            share,
        }
    }

    #[test]
    fn used_fraction_is_clamped_and_safe() {
        assert_eq!(capacity(0, 0).used_fraction(), 0.0); // no divide-by-zero
        assert!((capacity(100, 25).used_fraction() - 0.75).abs() < 1e-6);
        // Free somehow exceeding total must not underflow.
        assert_eq!(capacity(100, 200).used_fraction(), 0.0);
    }

    #[test]
    fn an_unresponsive_drive_reads_as_empty() {
        assert_eq!(drive("/mnt/nas", None, None).used_fraction(), 0.0);
        let known = drive("/mnt/nas", Some(capacity(100, 50)), None);
        assert!((known.used_fraction() - 0.5).abs() < 1e-6);
    }

    fn entry(source: &str, mount: &str, fstype: &str) -> MountEntry {
        MountEntry {
            source: source.into(),
            mount: mount.into(),
            fstype: fstype.into(),
        }
    }

    #[test]
    fn proc_mounts_keeps_block_devices_and_network_mounts_deduped() {
        let contents = "\
sysfs /sys sysfs rw 0 0
/dev/sda1 / ext4 rw 0 0
proc /proc proc rw 0 0
/dev/sdb1 /mnt/data ext4 rw 0 0
tmpfs /run tmpfs rw 0 0
/dev/sda1 / ext4 rw,remount 0 0
/dev/sdc1 /media/My\\040Disk vfat rw 0 0
//nas/My\\040Share /mnt/nas cifs rw 0 0
fs1:/srv/data /mnt/fs1 nfs4 rw 0 0
me@host:/home/me /home/me/remote fuse.sshfs rw 0 0
gvfsd-fuse /run/user/1000/gvfs fuse.gvfsd-fuse rw 0 0
";
        assert_eq!(
            parse_proc_mounts(contents),
            vec![
                entry("/dev/sda1", "/", "ext4"),
                entry("/dev/sdb1", "/mnt/data", "ext4"),
                entry("/dev/sdc1", "/media/My Disk", "vfat"),
                entry("//nas/My Share", "/mnt/nas", "cifs"),
                entry("fs1:/srv/data", "/mnt/fs1", "nfs4"),
                entry("me@host:/home/me", "/home/me/remote", "fuse.sshfs"),
            ]
        );
    }

    #[test]
    fn the_gvfs_mount_is_this_sessions_own() {
        let contents = "/dev/sda1 / ext4 rw 0 0
gvfsd-fuse /run/user/1001/gvfs fuse.gvfsd-fuse rw 0 0
gvfsd-fuse /run/user/1000/gvfs fuse.gvfsd-fuse rw 0 0
";
        assert_eq!(
            gvfs_mount_point(contents, Path::new("/run/user/1000")),
            Some(PathBuf::from("/run/user/1000/gvfs"))
        );
        assert_eq!(
            gvfs_mount_point(contents, Path::new("/run/user/1002")),
            None
        );
        assert_eq!(
            gvfs_mount_point(
                "tmpfs /run/user/1000/gvfs tmpfs rw 0 0
",
                Path::new("/run/user/1000")
            ),
            None,
            "only the GVfs FUSE type counts"
        );
    }

    #[test]
    fn proc_mounts_lines_missing_a_field_are_skipped() {
        assert!(parse_proc_mounts("/dev/sda1 /\n\n//nas/s\n").is_empty());
    }

    #[test]
    fn mount_fields_unescape_octal_only() {
        assert_eq!(unescape_mount_field(r"a\040b\011c\134d"), "a b\tc\\d");
        // Not a three-digit octal escape: kept as written.
        assert_eq!(unescape_mount_field(r"a\09b\4"), r"a\09b\4");
        assert_eq!(unescape_mount_field(r"\777"), r"\777", "out of byte range");
        assert_eq!(unescape_mount_field("plain"), "plain");
    }

    #[test]
    fn linux_mounts_are_classified_by_type_then_place() {
        assert_eq!(
            linux_drive_kind(&entry("//nas/s", "/media/nas", "cifs")),
            DriveKind::Network
        );
        assert_eq!(
            linux_drive_kind(&entry("/dev/sdc1", "/run/media/me/USB", "vfat")),
            DriveKind::Removable
        );
        assert_eq!(
            linux_drive_kind(&entry("/dev/sdc1", "/media/USB", "exfat")),
            DriveKind::Removable
        );
        assert_eq!(
            linux_drive_kind(&entry("/dev/sda1", "/mediafiles", "ext4")),
            DriveKind::Fixed,
            "a path prefix, not a string prefix"
        );
        assert_eq!(
            linux_drive_kind(&entry("/dev/sda1", "/", "ext4")),
            DriveKind::Fixed
        );
    }

    #[test]
    fn network_fstypes_are_recognized() {
        for fstype in [
            "cifs",
            "smb3",
            "smbfs",
            "nfs",
            "nfs4",
            "fuse.sshfs",
            "webdav",
        ] {
            assert!(is_network_fstype(fstype), "{fstype}");
        }
        for fstype in ["ext4", "apfs", "vfat", "fuse.gvfsd-fuse", "tmpfs", "fuse"] {
            assert!(!is_network_fstype(fstype), "{fstype}");
        }
    }

    #[test]
    fn a_typed_share_opens_where_it_is_mounted() {
        let media = ShareAddress::from_mount_source("//nas/Media", "cifs");
        let drives = [
            drive("/", Some(capacity(1, 1)), None),
            drive("/mnt/media", None, media),
        ];
        let typed = ShareAddress::parse(r"\\NAS\media\Movies").unwrap();
        assert_eq!(
            locate_share(&typed, &drives),
            Some(PathBuf::from("/mnt/media/Movies"))
        );
        let typed = ShareAddress::parse("smb://nas/Photos").unwrap();
        assert_eq!(locate_share(&typed, &drives), None);
    }

    /// FIL-61: a share GVfs mounted is found by a typed address too.
    #[test]
    fn a_typed_share_opens_in_its_gvfs_folder() {
        let gvfs = "/run/user/1000/gvfs/smb-share:server=nas,share=photos,user=me";
        let mount =
            crate::network::GvfsMount::parse("smb-share:server=nas,share=photos,user=me").unwrap();
        let drives = [drive(gvfs, None, mount.share)];
        let typed = ShareAddress::parse("smb://NAS/Photos/2026").unwrap();
        assert_eq!(
            locate_share(&typed, &drives),
            Some(PathBuf::from(gvfs).join("2026"))
        );
    }

    fn timeout() -> Duration {
        Duration::from_millis(50)
    }

    #[test]
    fn a_quick_probe_answers() {
        let stuck = StuckProbes::default();
        assert_eq!(
            bounded_probe(
                Path::new("/mnt/a"),
                Duration::from_secs(5),
                &stuck,
                || Some(7)
            ),
            Some(7)
        );
        assert_eq!(
            bounded_probe(
                Path::new("/mnt/a"),
                Duration::from_secs(5),
                &stuck,
                || None::<u8>
            ),
            None
        );
        assert!(lock(&stuck).is_empty());
    }

    #[test]
    fn a_stuck_probe_is_not_restarted_until_it_returns() {
        let stuck = StuckProbes::default();
        let key = Path::new("/mnt/dead");
        let (release, wait) = mpsc::channel::<()>();
        let first = bounded_probe(key, timeout(), &stuck, move || {
            wait.recv().ok();
            Some(1)
        });
        assert_eq!(first, None, "timed out");
        assert!(lock(&stuck).contains(key));

        // While the first probe is parked, a second never starts.
        let started = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&started);
        let second = bounded_probe(key, timeout(), &stuck, move || {
            flag.store(true, Ordering::Relaxed);
            Some(2)
        });
        assert_eq!(second, None);
        assert!(!started.load(Ordering::Relaxed));

        // Other mounts are unaffected.
        assert_eq!(
            bounded_probe(
                Path::new("/mnt/alive"),
                Duration::from_secs(5),
                &stuck,
                || Some(3)
            ),
            Some(3)
        );

        // Once the server answers, the mount is probed again.
        release.send(()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while lock(&stuck).contains(key) {
            assert!(std::time::Instant::now() < deadline, "probe never finished");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            bounded_probe(key, Duration::from_secs(5), &stuck, || Some(4)),
            Some(4)
        );
    }

    #[test]
    fn windows_drive_types_map_to_kinds() {
        assert_eq!(kind_for_drive_type(DRIVE_FIXED), DriveKind::Fixed);
        assert_eq!(kind_for_drive_type(DRIVE_REMOTE), DriveKind::Network);
        assert_eq!(kind_for_drive_type(DRIVE_REMOVABLE), DriveKind::Removable);
        assert_eq!(kind_for_drive_type(DRIVE_CDROM), DriveKind::Removable);
        assert_eq!(kind_for_drive_type(0), DriveKind::Fixed, "unknown");
    }

    #[test]
    fn mapped_drives_are_named_like_explorer() {
        let media = ShareAddress::parse(r"\\nas\Media").unwrap();
        assert_eq!(
            network_drive_label(Some(&media), 'Z'),
            r"Media (\\nas) (Z:)"
        );
        let movies = ShareAddress::parse(r"\\nas\Media\Movies").unwrap();
        assert_eq!(
            network_drive_label(Some(&movies), 'Y'),
            r"Movies (\\nas\Media) (Y:)"
        );
        assert_eq!(network_drive_label(None, 'X'), "Network Drive (X:)");
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

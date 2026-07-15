//! Linux live watchers: fanotify (privileged fast path) with inotify
//! fallback.
//!
//! Strategy (docs/indexing-architecture.md §2):
//! - **fanotify** (`FAN_MARK_FILESYSTEM` + `FAN_REPORT_DFID_NAME`,
//!   kernel ≥ 5.9): one mark covers the whole filesystem and events carry
//!   (directory handle, name) — no per-directory bookkeeping, no watch
//!   budget. Requires `CAP_SYS_ADMIN`, so it's attempted first and
//!   permission failures fall back to inotify.
//! - **inotify** is per-directory, so every directory in the tree gets a
//!   watch, bounded by a budget derived from
//!   `fs.inotify.max_user_watches`. If the tree outgrows the budget the
//!   watcher degrades to partial coverage (a periodic reconcile walk is
//!   future work). Kernel queue overflow is surfaced as a root rescan.
//!
//! Assumptions about the OS:
//! - `struct inotify_event` layout (wd, mask, cookie, len, name[]) is
//!   stable kernel ABI; names are NUL-padded and encoded as raw bytes.
//! - Watch descriptors identify *directories* only (`IN_ONLYDIR`), so an
//!   event's full path is watch-dir + name.
//! - Rename pairs (`IN_MOVED_FROM`/`IN_MOVED_TO`) may be split across
//!   reads; they are treated as independent Remove + Upsert, which the
//!   idempotent delta application absorbs.
//!
//! The buffer parser, delta mapping, and watch registry below are pure and
//! compiled on every OS so fixture tests run in any CI; only the syscall
//! loop is Linux-gated.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::watcher::FsDelta;

// Event mask bits from linux/inotify.h (stable kernel ABI).
pub const IN_MOVED_FROM: u32 = 0x0000_0040;
pub const IN_MOVED_TO: u32 = 0x0000_0080;
pub const IN_CREATE: u32 = 0x0000_0100;
pub const IN_DELETE: u32 = 0x0000_0200;
pub const IN_DELETE_SELF: u32 = 0x0000_0400;
pub const IN_MOVE_SELF: u32 = 0x0000_0800;
pub const IN_Q_OVERFLOW: u32 = 0x0000_4000;
pub const IN_IGNORED: u32 = 0x0000_8000;
pub const IN_ONLYDIR: u32 = 0x0100_0000;
pub const IN_EXCL_UNLINK: u32 = 0x0400_0000;
pub const IN_ISDIR: u32 = 0x4000_0000;

/// The mask we register on every directory: name-changing events only.
pub const WATCH_MASK: u32 = IN_CREATE
    | IN_DELETE
    | IN_MOVED_FROM
    | IN_MOVED_TO
    | IN_DELETE_SELF
    | IN_MOVE_SELF
    | IN_ONLYDIR
    | IN_EXCL_UNLINK;

/// One decoded `struct inotify_event`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InotifyEvent {
    pub wd: i32,
    pub mask: u32,
    pub cookie: u32,
    /// Raw name bytes with NUL padding stripped; empty for events about
    /// the watched directory itself.
    pub name: Vec<u8>,
}

/// Fixed-size prefix of `struct inotify_event`.
const EVENT_HEADER_LEN: usize = 16;

/// Parse a read(2) buffer of packed `inotify_event` records. Truncated
/// trailing bytes are ignored (the kernel never splits an event, so any
/// remainder means a malformed fixture, not kernel output).
pub fn parse_events(buf: &[u8]) -> Vec<InotifyEvent> {
    let mut events = Vec::new();
    let mut offset = 0;
    while offset + EVENT_HEADER_LEN <= buf.len() {
        let header = &buf[offset..offset + EVENT_HEADER_LEN];
        let wd = i32::from_ne_bytes(header[0..4].try_into().expect("4 bytes"));
        let mask = u32::from_ne_bytes(header[4..8].try_into().expect("4 bytes"));
        let cookie = u32::from_ne_bytes(header[8..12].try_into().expect("4 bytes"));
        let len = u32::from_ne_bytes(header[12..16].try_into().expect("4 bytes")) as usize;

        let name_end = offset + EVENT_HEADER_LEN + len;
        if name_end > buf.len() {
            break; // malformed tail
        }
        let raw_name = &buf[offset + EVENT_HEADER_LEN..name_end];
        let name = raw_name
            .split(|&b| b == 0)
            .next()
            .unwrap_or_default()
            .to_vec();

        events.push(InotifyEvent { wd, mask, cookie, name });
        offset = name_end;
    }
    events
}

/// Bidirectional watch-descriptor ↔ directory-path bookkeeping with a
/// budget. `insert` refuses beyond the budget so callers degrade
/// explicitly instead of hitting `ENOSPC` from the kernel.
pub struct WatchRegistry {
    by_wd: HashMap<i32, PathBuf>,
    by_path: HashMap<PathBuf, i32>,
    budget: usize,
    /// True once the budget stopped at least one insert — the index may
    /// silently miss events under unwatched directories.
    degraded: bool,
}

impl WatchRegistry {
    pub fn new(budget: usize) -> Self {
        Self {
            by_wd: HashMap::new(),
            by_path: HashMap::new(),
            budget,
            degraded: false,
        }
    }

    /// Record a watch. Returns false (and flags degradation) over budget.
    pub fn insert(&mut self, wd: i32, path: PathBuf) -> bool {
        if self.by_wd.len() >= self.budget && !self.by_path.contains_key(&path) {
            self.degraded = true;
            return false;
        }
        self.by_wd.insert(wd, path.clone());
        self.by_path.insert(path, wd);
        true
    }

    pub fn path_of(&self, wd: i32) -> Option<&Path> {
        self.by_wd.get(&wd).map(PathBuf::as_path)
    }

    pub fn wd_of(&self, path: &Path) -> Option<i32> {
        self.by_path.get(path).copied()
    }

    pub fn remove_wd(&mut self, wd: i32) -> Option<PathBuf> {
        let path = self.by_wd.remove(&wd)?;
        self.by_path.remove(&path);
        Some(path)
    }

    pub fn len(&self) -> usize {
        self.by_wd.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_wd.is_empty()
    }

    pub fn at_capacity(&self) -> bool {
        self.by_wd.len() >= self.budget
    }

    pub fn is_degraded(&self) -> bool {
        self.degraded
    }

    /// Record that a watch was skipped (budget reached before the syscall).
    pub fn note_degraded(&mut self) {
        self.degraded = true;
    }
}

/// Translate one inotify event into a normalized delta. `root` is the
/// watch root (used for overflow rescans); `dir` is the directory the
/// event's watch descriptor refers to, or `None` if it isn't registered
/// (stale wd after IN_IGNORED). Pure — fixture-tested on every OS.
pub fn event_to_delta(root: &Path, dir: Option<&Path>, event: &InotifyEvent) -> Option<FsDelta> {
    if event.mask & IN_Q_OVERFLOW != 0 {
        // The kernel dropped events; only a reconcile walk restores truth.
        return Some(FsDelta::Rescan { path: root.to_path_buf() });
    }
    // Directory-self events carry no name; registry maintenance (dropping
    // the watch) is the caller's job, and the parent's IN_DELETE/IN_MOVED_*
    // event produces the index delta. IN_IGNORED likewise.
    if event.mask & (IN_DELETE_SELF | IN_MOVE_SELF | IN_IGNORED) != 0 {
        return None;
    }

    let dir = dir?;
    let name = std::str::from_utf8(&event.name).ok()?;
    if name.is_empty() {
        return None;
    }
    let path = dir.join(name);
    let is_dir = event.mask & IN_ISDIR != 0;

    if event.mask & (IN_CREATE | IN_MOVED_TO) != 0 {
        Some(FsDelta::Upsert { path, is_dir })
    } else if event.mask & (IN_DELETE | IN_MOVED_FROM) != 0 {
        Some(FsDelta::Remove { path })
    } else {
        None
    }
}

/// Default watch budget when `max_user_watches` can't be read. Matches the
/// historical kernel default.
pub const DEFAULT_WATCH_BUDGET: usize = 65_536;

/// Fraction of the system-wide watch limit this process will claim, leaving
/// room for other inotify users (IDEs, sync clients).
pub const WATCH_BUDGET_FRACTION: f64 = 0.5;

/// Compute the watch budget from the kernel limit (contents of
/// `/proc/sys/fs/inotify/max_user_watches`). Pure for testing.
pub fn watch_budget(max_user_watches: Option<&str>) -> usize {
    max_user_watches
        .and_then(|s| s.trim().parse::<usize>().ok())
        .map(|max| ((max as f64 * WATCH_BUDGET_FRACTION) as usize).max(1024))
        .unwrap_or(DEFAULT_WATCH_BUDGET)
}

// ---- fanotify: pure event parsing and mapping (fixture-tested on every
// ---- OS; the syscall loop is Linux-gated below) ----

// Event mask bits from linux/fanotify.h (stable kernel ABI). The dirent
// bits share values with inotify's, but keep them distinct for clarity.
pub const FAN_MOVED_FROM: u64 = 0x0000_0040;
pub const FAN_MOVED_TO: u64 = 0x0000_0080;
pub const FAN_CREATE: u64 = 0x0000_0100;
pub const FAN_DELETE: u64 = 0x0000_0200;
pub const FAN_Q_OVERFLOW: u64 = 0x0000_4000;
pub const FAN_ONDIR: u64 = 0x4000_0000;

/// The mask we mark the filesystem with: name-changing events only,
/// including those on directories.
pub const FANOTIFY_MASK: u64 =
    FAN_CREATE | FAN_DELETE | FAN_MOVED_FROM | FAN_MOVED_TO | FAN_ONDIR;

/// Info-record type carrying (directory file handle, entry name).
pub const FAN_EVENT_INFO_TYPE_DFID_NAME: u8 = 2;

/// One decoded fanotify event (from `FAN_REPORT_DFID_NAME` mode).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FanotifyEvent {
    pub mask: u64,
    /// Raw `struct file_handle` bytes (handle_bytes, handle_type,
    /// f_handle), exactly as the kernel reported the parent directory —
    /// resolvable via `open_by_handle_at`. Empty for overflow events.
    pub dir_handle: Vec<u8>,
    /// Entry name within that directory (NUL stripped). Empty for
    /// overflow events.
    pub name: Vec<u8>,
}

const FAN_METADATA_LEN: usize = 24;
const INFO_HEADER_LEN: usize = 4;
/// fsid (8) + handle_bytes (4) + handle_type (4) precede f_handle.
const DFID_NAME_FIXED_LEN: usize = 16;

/// Parse a fanotify read(2) buffer: a run of variable-length events, each
/// a `fanotify_event_metadata` followed by info records up to `event_len`.
/// Only `FAN_EVENT_INFO_TYPE_DFID_NAME` info is decoded; events without
/// it (other than overflow) are dropped. Malformed lengths terminate
/// parsing (kernel output is well-formed; anything else is a fixture bug).
pub fn parse_fanotify_events(buf: &[u8]) -> Vec<FanotifyEvent> {
    let mut events = Vec::new();
    let mut offset = 0usize;
    while offset + FAN_METADATA_LEN <= buf.len() {
        let meta = &buf[offset..];
        let event_len = u32::from_ne_bytes(meta[0..4].try_into().expect("4 bytes")) as usize;
        let metadata_len = u16::from_ne_bytes(meta[6..8].try_into().expect("2 bytes")) as usize;
        if event_len < FAN_METADATA_LEN
            || metadata_len < FAN_METADATA_LEN
            || metadata_len > event_len
            || offset + event_len > buf.len()
        {
            break;
        }
        let mask = u64::from_ne_bytes(meta[8..16].try_into().expect("8 bytes"));

        let mut dir_handle = Vec::new();
        let mut name = Vec::new();
        // Walk the info records within this event.
        let mut info_offset = metadata_len;
        while info_offset + INFO_HEADER_LEN <= event_len {
            let info = &meta[info_offset..];
            let info_type = info[0];
            let info_len =
                u16::from_ne_bytes(info[2..4].try_into().expect("2 bytes")) as usize;
            if info_len < INFO_HEADER_LEN || info_offset + info_len > event_len {
                break;
            }
            if info_type == FAN_EVENT_INFO_TYPE_DFID_NAME
                && info_len >= INFO_HEADER_LEN + DFID_NAME_FIXED_LEN
            {
                let handle_bytes = u32::from_ne_bytes(
                    info[12..16].try_into().expect("4 bytes"),
                ) as usize;
                let handle_end = INFO_HEADER_LEN + DFID_NAME_FIXED_LEN + handle_bytes;
                if handle_end <= info_len {
                    // file_handle = handle_bytes + handle_type + f_handle.
                    dir_handle = info[12..handle_end].to_vec();
                    name = info[handle_end..info_len]
                        .split(|&b| b == 0)
                        .next()
                        .unwrap_or_default()
                        .to_vec();
                }
            }
            info_offset += info_len;
        }

        if mask & FAN_Q_OVERFLOW != 0 || !name.is_empty() {
            events.push(FanotifyEvent { mask, dir_handle, name });
        }
        offset += event_len;
    }
    events
}

/// Translate one fanotify event into a delta. `dir` is the resolved
/// parent directory (None if the handle couldn't be resolved or lies
/// outside the indexed root). Removal bits win over creation bits when a
/// merged event carries both — mirroring the USN sticky-flag policy.
pub fn fanotify_event_to_delta(
    root: &Path,
    dir: Option<&Path>,
    event: &FanotifyEvent,
) -> Option<FsDelta> {
    if event.mask & FAN_Q_OVERFLOW != 0 {
        return Some(FsDelta::Rescan { path: root.to_path_buf() });
    }
    let dir = dir?;
    let name = std::str::from_utf8(&event.name).ok()?;
    if name.is_empty() {
        return None;
    }
    let path = dir.join(name);
    let is_dir = event.mask & FAN_ONDIR != 0;
    if event.mask & (FAN_DELETE | FAN_MOVED_FROM) != 0 {
        Some(FsDelta::Remove { path })
    } else if event.mask & (FAN_CREATE | FAN_MOVED_TO) != 0 {
        Some(FsDelta::Upsert { path, is_dir })
    } else {
        None
    }
}

#[cfg(target_os = "linux")]
pub use imp::{FanotifyWatcher, InotifyWatcher, LinuxWatcher};

#[cfg(target_os = "linux")]
mod imp {
    use super::*;
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::Sender;
    use std::thread::JoinHandle;

    use anyhow::{Context as _, Result, bail};

    /// The Linux live-update source: fanotify when this process has
    /// `CAP_SYS_ADMIN` (whole-filesystem mark, no watch budget), inotify
    /// otherwise.
    #[allow(dead_code)] // fields are RAII guards: dropping stops the watcher
    pub enum LinuxWatcher {
        Fanotify(FanotifyWatcher),
        Inotify(InotifyWatcher),
    }

    impl LinuxWatcher {
        pub fn spawn(root: &Path, deltas: Sender<Vec<FsDelta>>) -> Result<Self> {
            match FanotifyWatcher::spawn(root, deltas.clone()) {
                Ok(watcher) => return Ok(Self::Fanotify(watcher)),
                Err(err) => {
                    // EPERM without CAP_SYS_ADMIN is the expected case for
                    // ordinary users; anything else is still non-fatal.
                    eprintln!("filex: fanotify unavailable ({err:#}); using inotify");
                }
            }
            InotifyWatcher::spawn(root, deltas).map(Self::Inotify)
        }
    }

    // fanotify_init flags (linux/fanotify.h).
    const FAN_CLOEXEC: u32 = 0x0000_0001;
    const FAN_NONBLOCK: u32 = 0x0000_0002;
    const FAN_CLASS_NOTIF: u32 = 0x0000_0000;
    const FAN_REPORT_DIR_FID: u32 = 0x0000_0400;
    const FAN_REPORT_NAME: u32 = 0x0000_0800;
    // fanotify_mark flags.
    const FAN_MARK_ADD: u32 = 0x0000_0001;
    const FAN_MARK_FILESYSTEM: u32 = 0x0000_0100;

    /// Whole-filesystem watcher via fanotify (`FAN_MARK_FILESYSTEM` +
    /// `FAN_REPORT_DFID_NAME`). Requires `CAP_SYS_ADMIN` at spawn and
    /// `CAP_DAC_READ_SEARCH` to resolve directory handles (root has both).
    ///
    /// Assumptions about the OS: kernel ≥ 5.9 for `FAN_REPORT_NAME`; the
    /// filesystem mark covers exactly the filesystem hosting `root`, so
    /// events outside the indexed root arrive and are filtered after
    /// handle resolution; handles of already-deleted directories fail
    /// with ESTALE and are skipped (the directory's own removal event
    /// arrives via its parent's handle).
    pub struct FanotifyWatcher {
        shutdown: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }

    impl FanotifyWatcher {
        pub fn spawn(root: &Path, deltas: Sender<Vec<FsDelta>>) -> Result<Self> {
            // SAFETY: plain syscall, no pointers.
            let fan_fd = unsafe {
                libc::fanotify_init(
                    FAN_CLOEXEC | FAN_NONBLOCK | FAN_CLASS_NOTIF | FAN_REPORT_DIR_FID
                        | FAN_REPORT_NAME,
                    libc::O_RDONLY as u32,
                )
            };
            if fan_fd < 0 {
                bail!(
                    "fanotify_init failed (needs CAP_SYS_ADMIN): {}",
                    std::io::Error::last_os_error()
                );
            }
            let fan_guard = FdGuard(fan_fd);

            let c_root = CString::new(root.as_os_str().as_bytes())
                .context("root path contains NUL")?;
            // SAFETY: valid fd and NUL-terminated path.
            let marked = unsafe {
                libc::fanotify_mark(
                    fan_fd,
                    FAN_MARK_ADD | FAN_MARK_FILESYSTEM,
                    FANOTIFY_MASK,
                    libc::AT_FDCWD,
                    c_root.as_ptr(),
                )
            };
            if marked < 0 {
                bail!(
                    "fanotify_mark(FILESYSTEM) failed for {}: {}",
                    root.display(),
                    std::io::Error::last_os_error()
                );
            }

            // Directory handles resolve relative to any fd on the same
            // filesystem; the root itself is the natural anchor.
            // SAFETY: NUL-terminated path; O_PATH needs no read perms.
            let mount_fd =
                unsafe { libc::open(c_root.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
            if mount_fd < 0 {
                bail!(
                    "opening {} as handle-resolution anchor: {}",
                    root.display(),
                    std::io::Error::last_os_error()
                );
            }
            let mount_guard = FdGuard(mount_fd);

            let shutdown = Arc::new(AtomicBool::new(false));
            let thread = std::thread::Builder::new()
                .name("filex-fanotify".into())
                .spawn({
                    let shutdown = shutdown.clone();
                    let root = root.to_path_buf();
                    move || {
                        fanotify_loop(&fan_guard, &mount_guard, &root, &deltas, &shutdown);
                        // guards drop here, closing both fds
                    }
                })?;

            Ok(Self { shutdown, thread: Some(thread) })
        }
    }

    impl Drop for FanotifyWatcher {
        fn drop(&mut self) {
            self.shutdown.store(true, Ordering::Relaxed);
            if let Some(thread) = self.thread.take() {
                thread.join().ok(); // wakes within one poll timeout
            }
        }
    }

    struct FdGuard(i32);
    impl Drop for FdGuard {
        fn drop(&mut self) {
            // SAFETY: closing an fd we own.
            unsafe { libc::close(self.0) };
        }
    }

    fn fanotify_loop(
        fan: &FdGuard,
        mount: &FdGuard,
        root: &Path,
        deltas: &Sender<Vec<FsDelta>>,
        shutdown: &AtomicBool,
    ) {
        let mut buf = vec![0u8; 64 * 1024];
        while !shutdown.load(Ordering::Relaxed) {
            let mut pollfd = libc::pollfd { fd: fan.0, events: libc::POLLIN, revents: 0 };
            // SAFETY: pollfd points at one valid struct for the call.
            let ready = unsafe { libc::poll(&mut pollfd, 1, 500) };
            if ready <= 0 {
                continue; // timeout (shutdown check) or EINTR
            }
            // SAFETY: buf is valid for buf.len() writable bytes.
            let n = unsafe { libc::read(fan.0, buf.as_mut_ptr() as *mut _, buf.len()) };
            if n <= 0 {
                continue; // EAGAIN after spurious wakeup
            }

            let mut batch = Vec::new();
            for event in parse_fanotify_events(&buf[..n as usize]) {
                let dir = resolve_dir_handle(mount.0, &event.dir_handle)
                    // The filesystem mark sees the whole fs; keep only
                    // events under our root.
                    .filter(|dir| dir.starts_with(root));
                if let Some(delta) = fanotify_event_to_delta(root, dir.as_deref(), &event) {
                    batch.push(delta);
                }
            }
            if !batch.is_empty() && deltas.send(batch).is_err() {
                break; // receiver gone: shutting down
            }
        }
    }

    /// Resolve a raw `struct file_handle` (as captured from the event) to
    /// the directory's current path via open_by_handle_at + /proc/self/fd.
    /// Requires CAP_DAC_READ_SEARCH. None for stale handles (dir deleted).
    fn resolve_dir_handle(mount_fd: i32, raw_handle: &[u8]) -> Option<PathBuf> {
        if raw_handle.len() < 8 {
            return None;
        }
        // Copy into an 8-aligned buffer: file_handle starts with two u32s
        // and the kernel requires natural alignment.
        let mut aligned = vec![0u64; raw_handle.len().div_ceil(8)];
        // SAFETY: u64 buffer reinterpreted as bytes for the copy.
        let aligned_bytes = unsafe {
            std::slice::from_raw_parts_mut(aligned.as_mut_ptr() as *mut u8, raw_handle.len())
        };
        aligned_bytes.copy_from_slice(raw_handle);

        // SAFETY: open_by_handle_at reads handle_bytes from the struct we
        // built from kernel-provided bytes; O_PATH avoids permission-heavy
        // opens.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_open_by_handle_at,
                mount_fd,
                aligned.as_ptr(),
                libc::O_PATH | libc::O_CLOEXEC,
            )
        } as i32;
        if fd < 0 {
            return None; // ESTALE (deleted) or permissions
        }
        let guard = FdGuard(fd);
        let path = std::fs::read_link(format!("/proc/self/fd/{}", guard.0)).ok()?;
        Some(path)
    }

    /// Watches a directory tree via one inotify instance with a watch per
    /// directory. Dropping stops the reader thread and closes the instance.
    pub struct InotifyWatcher {
        shutdown: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }

    impl InotifyWatcher {
        /// Start watching `root` (must be canonicalized). Watches are
        /// registered for every directory up to the budget; new directories
        /// are watched as they appear.
        pub fn spawn(root: &Path, deltas: Sender<Vec<FsDelta>>) -> Result<Self> {
            // SAFETY: plain syscall, no pointers.
            let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
            if fd < 0 {
                bail!(
                    "inotify_init1 failed: {}",
                    std::io::Error::last_os_error()
                );
            }

            let budget = watch_budget(
                std::fs::read_to_string("/proc/sys/fs/inotify/max_user_watches")
                    .ok()
                    .as_deref(),
            );
            let mut registry = WatchRegistry::new(budget);
            let root = root.to_path_buf();
            add_watches_recursive(fd, &root, &mut registry)
                .with_context(|| format!("registering watches under {}", root.display()))?;
            if registry.is_degraded() {
                eprintln!(
                    "filex: inotify watch budget ({budget}) exhausted; live updates \
                     are partial — raise fs.inotify.max_user_watches for full coverage"
                );
            }

            let shutdown = Arc::new(AtomicBool::new(false));
            let thread = std::thread::Builder::new()
                .name("filex-inotify".into())
                .spawn({
                    let shutdown = shutdown.clone();
                    move || {
                        reader_loop(fd, &root, registry, &deltas, &shutdown);
                        // SAFETY: fd is owned by this thread from here on.
                        unsafe { libc::close(fd) };
                    }
                })?;

            Ok(Self { shutdown, thread: Some(thread) })
        }
    }

    impl Drop for InotifyWatcher {
        fn drop(&mut self) {
            self.shutdown.store(true, Ordering::Relaxed);
            if let Some(thread) = self.thread.take() {
                thread.join().ok(); // wakes within one poll timeout
            }
        }
    }

    fn add_watch(fd: i32, dir: &Path, registry: &mut WatchRegistry) -> Result<()> {
        if registry.at_capacity() {
            registry.note_degraded();
            return Ok(());
        }
        if registry.wd_of(dir).is_some() {
            return Ok(());
        }
        let c_path = CString::new(dir.as_os_str().as_bytes())
            .context("directory path contains NUL")?;
        // SAFETY: c_path is a valid NUL-terminated string.
        let wd = unsafe { libc::inotify_add_watch(fd, c_path.as_ptr(), WATCH_MASK) };
        if wd < 0 {
            // Racing deletes and permission errors are expected; skip.
            return Ok(());
        }
        registry.insert(wd, dir.to_path_buf());
        Ok(())
    }

    /// Watch `dir` and every subdirectory (used at startup and when a
    /// directory appears later — its contents may predate the watch, which
    /// the caller covers by emitting an Upsert for `dir`).
    fn add_watches_recursive(fd: i32, dir: &Path, registry: &mut WatchRegistry) -> Result<()> {
        add_watch(fd, dir, registry)?;
        let walk = jwalk::WalkDir::new(dir).skip_hidden(false).follow_links(false);
        for dirent in walk.into_iter().flatten() {
            if registry.at_capacity() {
                break;
            }
            if dirent.file_type().is_dir() && dirent.depth() > 0 {
                add_watch(fd, &dirent.path(), registry)?;
            }
        }
        Ok(())
    }

    fn reader_loop(
        fd: i32,
        root: &Path,
        mut registry: WatchRegistry,
        deltas: &Sender<Vec<FsDelta>>,
        shutdown: &AtomicBool,
    ) {
        let mut buf = vec![0u8; 64 * 1024];
        while !shutdown.load(Ordering::Relaxed) {
            let mut pollfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
            // SAFETY: pollfd points at one valid struct for the call.
            let ready = unsafe { libc::poll(&mut pollfd, 1, 500) };
            if ready <= 0 {
                continue; // timeout (shutdown check) or EINTR
            }
            // SAFETY: buf is valid for buf.len() writable bytes.
            let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut _, buf.len()) };
            if n <= 0 {
                continue; // EAGAIN after spurious poll wakeup
            }

            let mut batch = Vec::new();
            for event in parse_events(&buf[..n as usize]) {
                if event.mask & IN_IGNORED != 0 {
                    registry.remove_wd(event.wd);
                    continue;
                }
                let dir = registry.path_of(event.wd).map(Path::to_path_buf);
                let Some(delta) = event_to_delta(root, dir.as_deref(), &event) else {
                    continue;
                };
                // A new directory needs watches before its Upsert is applied
                // (apply walks it, so contents created before the watch
                // attach are indexed either way).
                if let FsDelta::Upsert { path, is_dir: true } = &delta {
                    add_watches_recursive(fd, path, &mut registry).ok();
                }
                batch.push(delta);
            }
            if !batch.is_empty() && deltas.send(batch).is_err() {
                break; // receiver gone: shutting down
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_event(wd: i32, mask: u32, cookie: u32, name: &[u8]) -> Vec<u8> {
        // Kernel pads names to alignment with NULs; emulate with +3 pad.
        let padded_len = if name.is_empty() { 0 } else { name.len() + 3 };
        let mut buf = Vec::new();
        buf.extend_from_slice(&wd.to_ne_bytes());
        buf.extend_from_slice(&mask.to_ne_bytes());
        buf.extend_from_slice(&cookie.to_ne_bytes());
        buf.extend_from_slice(&(padded_len as u32).to_ne_bytes());
        buf.extend_from_slice(name);
        buf.resize(buf.len() + padded_len - name.len(), 0);
        buf
    }

    #[test]
    fn parses_packed_events_with_padded_names() {
        let mut buf = encode_event(3, IN_CREATE, 0, b"new.txt");
        buf.extend(encode_event(7, IN_DELETE | IN_ISDIR, 0, b"olddir"));
        buf.extend(encode_event(1, IN_Q_OVERFLOW, 0, b""));

        let events = parse_events(&buf);
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].wd, 3);
        assert_eq!(events[0].name, b"new.txt");
        assert_eq!(events[1].mask, IN_DELETE | IN_ISDIR);
        assert_eq!(events[1].name, b"olddir");
        assert!(events[2].name.is_empty());
    }

    #[test]
    fn parser_ignores_malformed_tail() {
        let mut buf = encode_event(3, IN_CREATE, 0, b"ok.txt");
        buf.extend_from_slice(&[1, 2, 3]); // torn header
        assert_eq!(parse_events(&buf).len(), 1);
    }

    fn event(wd: i32, mask: u32, name: &[u8]) -> InotifyEvent {
        InotifyEvent { wd, mask, cookie: 0, name: name.to_vec() }
    }

    #[test]
    fn maps_create_and_moved_to_as_upserts() {
        let root = Path::new("/root");
        let dir = Path::new("/root/sub");
        assert_eq!(
            event_to_delta(root, Some(dir), &event(1, IN_CREATE, b"a.txt")),
            Some(FsDelta::Upsert { path: dir.join("a.txt"), is_dir: false })
        );
        assert_eq!(
            event_to_delta(root, Some(dir), &event(1, IN_MOVED_TO | IN_ISDIR, b"moved")),
            Some(FsDelta::Upsert { path: dir.join("moved"), is_dir: true })
        );
    }

    #[test]
    fn maps_delete_and_moved_from_as_removes() {
        let root = Path::new("/root");
        let dir = Path::new("/root/sub");
        assert_eq!(
            event_to_delta(root, Some(dir), &event(1, IN_DELETE, b"a.txt")),
            Some(FsDelta::Remove { path: dir.join("a.txt") })
        );
        assert_eq!(
            event_to_delta(root, Some(dir), &event(1, IN_MOVED_FROM | IN_ISDIR, b"gone")),
            Some(FsDelta::Remove { path: dir.join("gone") })
        );
    }

    #[test]
    fn overflow_maps_to_root_rescan_even_without_a_watch_dir() {
        let root = Path::new("/root");
        assert_eq!(
            event_to_delta(root, None, &event(-1, IN_Q_OVERFLOW, b"")),
            Some(FsDelta::Rescan { path: root.into() })
        );
    }

    #[test]
    fn self_events_and_unknown_watches_map_to_nothing() {
        let root = Path::new("/root");
        let dir = Path::new("/root/sub");
        assert_eq!(event_to_delta(root, Some(dir), &event(1, IN_DELETE_SELF, b"")), None);
        assert_eq!(event_to_delta(root, Some(dir), &event(1, IN_IGNORED, b"")), None);
        // Stale wd: no registered dir.
        assert_eq!(event_to_delta(root, None, &event(9, IN_CREATE, b"x")), None);
    }

    #[test]
    fn registry_enforces_budget_and_reports_degradation() {
        let mut registry = WatchRegistry::new(2);
        assert!(registry.insert(1, PathBuf::from("/a")));
        assert!(registry.insert(2, PathBuf::from("/b")));
        assert!(!registry.insert(3, PathBuf::from("/c")));
        assert!(registry.is_degraded());
        assert_eq!(registry.len(), 2);

        assert_eq!(registry.remove_wd(1), Some(PathBuf::from("/a")));
        assert_eq!(registry.wd_of(Path::new("/a")), None);
        assert_eq!(registry.path_of(2), Some(Path::new("/b")));
    }

    #[test]
    fn watch_budget_scales_kernel_limit_with_fallback() {
        assert_eq!(watch_budget(Some("100000\n")), 50_000);
        assert_eq!(watch_budget(Some("garbage")), DEFAULT_WATCH_BUDGET);
        assert_eq!(watch_budget(None), DEFAULT_WATCH_BUDGET);
        assert_eq!(watch_budget(Some("100")), 1024); // floor
    }

    /// Encode a fanotify event the way the kernel lays it out:
    /// metadata (24B) + one DFID_NAME info record.
    fn encode_fanotify_event(mask: u64, handle: &[u8], name: &[u8]) -> Vec<u8> {
        let info_len = INFO_HEADER_LEN + DFID_NAME_FIXED_LEN + handle.len() + name.len() + 1;
        let event_len = FAN_METADATA_LEN + info_len;
        let mut buf = Vec::with_capacity(event_len);
        buf.extend_from_slice(&(event_len as u32).to_ne_bytes());
        buf.push(3); // FANOTIFY_METADATA_VERSION
        buf.push(0);
        buf.extend_from_slice(&(FAN_METADATA_LEN as u16).to_ne_bytes());
        buf.extend_from_slice(&mask.to_ne_bytes());
        buf.extend_from_slice(&(-1i32).to_ne_bytes()); // fd = FAN_NOFD
        buf.extend_from_slice(&0i32.to_ne_bytes()); // pid
        // Info record: header + fsid + file_handle + NUL-terminated name.
        buf.push(FAN_EVENT_INFO_TYPE_DFID_NAME);
        buf.push(0);
        buf.extend_from_slice(&(info_len as u16).to_ne_bytes());
        buf.extend_from_slice(&[0u8; 8]); // fsid
        buf.extend_from_slice(&(handle.len() as u32).to_ne_bytes());
        buf.extend_from_slice(&1i32.to_ne_bytes()); // handle_type
        buf.extend_from_slice(handle);
        buf.extend_from_slice(name);
        buf.push(0);
        buf
    }

    #[test]
    fn parses_fanotify_dfid_name_events() {
        let mut buf = encode_fanotify_event(FAN_CREATE, &[0xAA, 0xBB], b"made.txt");
        buf.extend(encode_fanotify_event(
            FAN_DELETE | FAN_ONDIR,
            &[0xCC],
            b"gonedir",
        ));

        let events = parse_fanotify_events(&buf);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].mask, FAN_CREATE);
        assert_eq!(events[0].name, b"made.txt");
        // file_handle = handle_bytes(2) + handle_type(1) + payload.
        assert_eq!(events[0].dir_handle[0..4], 2u32.to_ne_bytes());
        assert_eq!(&events[0].dir_handle[8..], &[0xAA, 0xBB]);
        assert_eq!(events[1].name, b"gonedir");
        assert!(events[1].mask & FAN_ONDIR != 0);
    }

    #[test]
    fn fanotify_parser_handles_overflow_and_garbage() {
        // Overflow events carry no info record.
        let mut buf = Vec::new();
        buf.extend_from_slice(&(FAN_METADATA_LEN as u32).to_ne_bytes());
        buf.push(3);
        buf.push(0);
        buf.extend_from_slice(&(FAN_METADATA_LEN as u16).to_ne_bytes());
        buf.extend_from_slice(&FAN_Q_OVERFLOW.to_ne_bytes());
        buf.extend_from_slice(&(-1i32).to_ne_bytes());
        buf.extend_from_slice(&0i32.to_ne_bytes());

        let events = parse_fanotify_events(&buf);
        assert_eq!(events.len(), 1);
        assert!(events[0].mask & FAN_Q_OVERFLOW != 0);

        buf.extend_from_slice(&[9, 9, 9]); // torn tail
        assert_eq!(parse_fanotify_events(&buf).len(), 1);
    }

    #[test]
    fn fanotify_mapping_covers_upsert_remove_overflow_and_precedence() {
        let root = Path::new("/root");
        let dir = Path::new("/root/sub");
        let event = |mask: u64, name: &[u8]| FanotifyEvent {
            mask,
            dir_handle: vec![],
            name: name.to_vec(),
        };

        assert_eq!(
            fanotify_event_to_delta(root, Some(dir), &event(FAN_CREATE, b"a.txt")),
            Some(FsDelta::Upsert { path: dir.join("a.txt"), is_dir: false })
        );
        assert_eq!(
            fanotify_event_to_delta(root, Some(dir), &event(FAN_MOVED_TO | FAN_ONDIR, b"d")),
            Some(FsDelta::Upsert { path: dir.join("d"), is_dir: true })
        );
        assert_eq!(
            fanotify_event_to_delta(root, Some(dir), &event(FAN_MOVED_FROM, b"x")),
            Some(FsDelta::Remove { path: dir.join("x") })
        );
        // Merged create+delete: removal wins (final state).
        assert_eq!(
            fanotify_event_to_delta(root, Some(dir), &event(FAN_CREATE | FAN_DELETE, b"t")),
            Some(FsDelta::Remove { path: dir.join("t") })
        );
        // Overflow needs no resolved dir.
        assert_eq!(
            fanotify_event_to_delta(root, None, &event(FAN_Q_OVERFLOW, b"")),
            Some(FsDelta::Rescan { path: root.into() })
        );
        // Unresolvable handle: dropped.
        assert_eq!(fanotify_event_to_delta(root, None, &event(FAN_CREATE, b"y")), None);
    }

    /// Live smoke tests against the real inotify API — these are what CI's
    /// Linux runner executes; fixture tests above cover the logic on
    /// every OS.
    #[cfg(target_os = "linux")]
    mod live {
        use super::super::*;
        use std::fs;
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        fn wait_for(
            rx: &mpsc::Receiver<Vec<FsDelta>>,
            timeout: Duration,
            mut pred: impl FnMut(&FsDelta) -> bool,
        ) -> bool {
            let deadline = Instant::now() + timeout;
            while Instant::now() < deadline {
                if let Ok(batch) = rx.recv_timeout(Duration::from_millis(200))
                    && batch.iter().any(&mut pred)
                {
                    return true;
                }
            }
            false
        }

        #[test]
        fn reports_real_file_creation_and_removal() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().canonicalize().unwrap();
            let (tx, rx) = mpsc::channel();
            let _watcher = InotifyWatcher::spawn(&root, tx).unwrap();

            let target = root.join("inotify-smoke.txt");
            fs::write(&target, b"x").unwrap();
            assert!(
                wait_for(&rx, Duration::from_secs(10), |d| {
                    matches!(d, FsDelta::Upsert { path, is_dir: false } if path == &target)
                }),
                "no Upsert for created file"
            );

            fs::remove_file(&target).unwrap();
            assert!(
                wait_for(&rx, Duration::from_secs(10), |d| {
                    matches!(d, FsDelta::Remove { path } if path == &target)
                }),
                "no Remove for deleted file"
            );
        }

        /// Requires CAP_SYS_ADMIN — CI runs this under sudo; unprivileged
        /// local runs skip gracefully (the fallback path is what they'd
        /// exercise anyway).
        #[test]
        fn fanotify_live_reports_file_creation_and_removal() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().canonicalize().unwrap();
            let (tx, rx) = mpsc::channel();
            let _watcher = match FanotifyWatcher::spawn(&root, tx) {
                Ok(watcher) => watcher,
                Err(err) => {
                    eprintln!("skipping fanotify live test (needs CAP_SYS_ADMIN): {err:#}");
                    return;
                }
            };

            let target = root.join("fanotify-smoke.txt");
            fs::write(&target, b"x").unwrap();
            assert!(
                wait_for(&rx, Duration::from_secs(10), |d| {
                    matches!(d, FsDelta::Upsert { path, is_dir: false } if path == &target)
                }),
                "no Upsert for created file"
            );

            fs::remove_file(&target).unwrap();
            assert!(
                wait_for(&rx, Duration::from_secs(10), |d| {
                    matches!(d, FsDelta::Remove { path } if path == &target)
                }),
                "no Remove for deleted file"
            );
        }

        #[test]
        fn watches_directories_created_after_spawn() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().canonicalize().unwrap();
            let (tx, rx) = mpsc::channel();
            let _watcher = InotifyWatcher::spawn(&root, tx).unwrap();

            fs::create_dir(root.join("newdir")).unwrap();
            assert!(
                wait_for(&rx, Duration::from_secs(10), |d| {
                    matches!(d, FsDelta::Upsert { path, is_dir: true } if path.ends_with("newdir"))
                }),
                "no Upsert for created directory"
            );

            // Events inside the new directory require the recursively
            // added watch to be in place.
            let inner = root.join("newdir/inner.txt");
            fs::write(&inner, b"x").unwrap();
            assert!(
                wait_for(&rx, Duration::from_secs(10), |d| {
                    matches!(d, FsDelta::Upsert { path, .. } if path == &inner)
                }),
                "no Upsert from inside the new directory"
            );
        }
    }
}

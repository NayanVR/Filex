//! Linux live watcher built on inotify.
//!
//! Strategy (docs/indexing-architecture.md §2): inotify is per-directory,
//! so every directory in the tree gets a watch, bounded by a budget derived
//! from `fs.inotify.max_user_watches`. If the tree outgrows the budget the
//! watcher degrades to partial coverage (a periodic reconcile walk is
//! future work; fanotify with `FAN_MARK_FILESYSTEM` is the privileged
//! upgrade path). Kernel queue overflow is surfaced as a root rescan.
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

#[cfg(target_os = "linux")]
pub use imp::InotifyWatcher;

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
}

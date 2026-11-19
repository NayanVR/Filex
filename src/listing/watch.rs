//! Watches the one folder the browse view shows, so files that other apps
//! create, delete or rename appear without a manual Refresh. GUI-free: the
//! workspace feeds [`FolderWatch::retarget`] and debounces `on_change` with
//! [`Debounce`].
//!
//! Platform assumptions (all through notify, `RecursiveMode::NonRecursive`):
//! - **inotify (Linux):** a non-recursive watch reports the directory's
//!   direct entries with absolute `dir/child` paths. Queue overflow arrives
//!   as a rescan flag.
//! - **FSEvents (macOS):** streams are path-prefix based, so a
//!   "non-recursive" watch may still report deeper paths, and paths can
//!   come back canonicalized (`/private/var/...` for `/var/...`). Hence the
//!   parent check in [`affects_listing`] and the canonical second target.
//!   Coalesced or dropped history arrives as a rescan flag.
//! - **ReadDirectoryChangesW (Windows):** a non-recursive handle reports
//!   direct children only, with paths joined onto the path as given. A
//!   buffer overflow arrives as a rescan flag.
//!
//! A failed watch (unsupported filesystem, permission denied, network
//! share) is logged and leaves the folder unwatched: no auto-refresh,
//! which is how the app behaved before.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use notify::{EventKind, RecursiveMode, Watcher};

enum Msg {
    Target(Option<PathBuf>),
    Fs(notify::Result<notify::Event>),
    Stop,
}

/// A watch on (at most) one folder, run on its own thread so watch setup
/// never touches the caller's thread.
pub struct FolderWatch {
    tx: mpsc::Sender<Msg>,
}

impl FolderWatch {
    /// Start the watcher thread. `on_change` runs on that thread for every
    /// event that may change the target's listing; keep it cheap (send on
    /// a channel).
    pub fn new(on_change: impl Fn() + Send + Sync + 'static) -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        let fs_tx = tx.clone();
        let watcher = notify::recommended_watcher(move |res| {
            let _ = fs_tx.send(Msg::Fs(res));
        })
        .context("creating the folder watcher")?;
        std::thread::Builder::new()
            .name("filex-folder-watch".into())
            .spawn(move || run(watcher, rx, on_change))
            .context("spawning the folder watcher thread")?;
        Ok(Self { tx })
    }

    /// Watch `target` instead (`None` = nothing). Only queues the request;
    /// the watcher thread does the I/O.
    pub fn retarget(&self, target: Option<PathBuf>) {
        let _ = self.tx.send(Msg::Target(target));
    }
}

impl Drop for FolderWatch {
    /// Not joined: unwatching can take a moment (FSEvents stops a run
    /// loop), and drop may run on the UI thread. The thread exits on its
    /// own once it reads `Stop`.
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Stop);
    }
}

/// The watcher thread: one target at a time, events filtered against it.
fn run(mut watcher: notify::RecommendedWatcher, rx: mpsc::Receiver<Msg>, on_change: impl Fn()) {
    // (as given, canonical form when it differs)
    let mut target: Option<(PathBuf, Option<PathBuf>)> = None;
    while let Ok(msg) = rx.recv() {
        match msg {
            Msg::Stop => return,
            Msg::Target(mut next) => {
                // Drain to the latest target; queued events belong to the
                // folder being left.
                for msg in rx.try_iter() {
                    match msg {
                        Msg::Stop => return,
                        Msg::Target(later) => next = later,
                        Msg::Fs(_) => {}
                    }
                }
                if let Some((old, _)) = target.take() {
                    let _ = watcher.unwatch(&old);
                }
                if let Some(path) = next {
                    match watcher.watch(&path, RecursiveMode::NonRecursive) {
                        Ok(()) => {
                            let canonical =
                                std::fs::canonicalize(&path).ok().filter(|c| *c != path);
                            target = Some((path, canonical));
                        }
                        Err(err) => {
                            tracing::debug!("not watching {}: {err}", path.display());
                        }
                    }
                }
            }
            Msg::Fs(Ok(ev)) => {
                if let Some((path, canonical)) = &target
                    && (affects_listing(path, &ev)
                        || canonical
                            .as_deref()
                            .is_some_and(|c| affects_listing(c, &ev)))
                {
                    on_change();
                }
            }
            Msg::Fs(Err(err)) => tracing::debug!("folder watch error: {err}"),
        }
    }
}

/// Whether `ev` can change the listing of `target`: a rescan request, or
/// any path that is the folder itself or one of its direct entries. Reads
/// (`Access`) never change a listing; deeper paths (FSEvents reports the
/// subtree) don't either.
pub fn affects_listing(target: &Path, ev: &notify::Event) -> bool {
    if ev.need_rescan() {
        return true;
    }
    if matches!(ev.kind, EventKind::Access(_)) {
        return false;
    }
    ev.paths
        .iter()
        .any(|p| p == target || p.parent() == Some(target))
}

/// Coalesces watcher events into reloads. Pure: callers pass the times.
///
/// A reload is due once events stop for [`QUIET`], but never later than
/// [`MAX_WAIT`] after the first pending event (a file being written
/// continuously still refreshes), and never sooner than `min_interval`
/// after the previous reload. That interval is `max(1 s, 2 × last load
/// cost)`, so a slow 100k-entry folder can't spend its time reloading.
#[derive(Debug)]
pub struct Debounce {
    first: Option<Instant>,
    last: Option<Instant>,
    last_reload: Option<Instant>,
    min_interval: Duration,
}

pub const QUIET: Duration = Duration::from_millis(150);
pub const MAX_WAIT: Duration = Duration::from_secs(1);
const MIN_INTERVAL_FLOOR: Duration = Duration::from_secs(1);

impl Default for Debounce {
    fn default() -> Self {
        Self {
            first: None,
            last: None,
            last_reload: None,
            min_interval: MIN_INTERVAL_FLOOR,
        }
    }
}

impl Debounce {
    pub fn event(&mut self, now: Instant) {
        self.first.get_or_insert(now);
        self.last = Some(now);
    }

    /// The latest pending event, if any.
    pub fn last_event(&self) -> Option<Instant> {
        self.last
    }

    /// When the pending events should be reloaded; `None` when none are
    /// pending.
    pub fn due(&self) -> Option<Instant> {
        let (first, last) = (self.first?, self.last?);
        let settled = (last + QUIET).min(first + MAX_WAIT);
        Some(match self.last_reload {
            Some(at) => settled.max(at + self.min_interval),
            None => settled,
        })
    }

    /// A reload finished at `now` and took `cost`; pending events are
    /// covered.
    pub fn reloaded(&mut self, now: Instant, cost: Duration) {
        self.last_reload = Some(now);
        self.min_interval = MIN_INTERVAL_FLOOR.max(cost * 2);
        self.cancel();
    }

    /// Drop pending events without a reload of our own (another load
    /// already covers them).
    pub fn cancel(&mut self) {
        self.first = None;
        self.last = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{AccessKind, CreateKind, Flag};

    fn ev(kind: EventKind, path: &str) -> notify::Event {
        notify::Event::new(kind).add_path(PathBuf::from(path))
    }

    #[test]
    fn affects_direct_children_only() {
        let t = Path::new("/a/b");
        let create = EventKind::Create(CreateKind::File);
        assert!(affects_listing(t, &ev(create, "/a/b/x")));
        assert!(affects_listing(t, &ev(create, "/a/b")));
        assert!(!affects_listing(t, &ev(create, "/a/b/c/x"))); // grandchild
        assert!(!affects_listing(t, &ev(create, "/a/c"))); // sibling
        assert!(!affects_listing(t, &ev(create, "/a/bb/x"))); // prefix sibling
        assert!(!affects_listing(
            t,
            &ev(EventKind::Access(AccessKind::Read), "/a/b/x")
        ));
        let rescan = notify::Event::new(EventKind::Other).set_flag(Flag::Rescan);
        assert!(affects_listing(t, &rescan));
        // FSEvents reports `/private/var/...` for a watch on `/var/...`:
        // the canonical target matches, the given one doesn't.
        let canon = ev(create, "/private/var/t/x");
        assert!(!affects_listing(Path::new("/var/t"), &canon));
        assert!(affects_listing(Path::new("/private/var/t"), &canon));
    }

    #[test]
    fn debounce_coalesces_burst() {
        let t0 = Instant::now();
        let mut d = Debounce::default();
        assert_eq!(d.due(), None);
        for i in 0..100u64 {
            d.event(t0 + Duration::from_micros(i * 500)); // 100 in 50 ms
        }
        let last = t0 + Duration::from_micros(99 * 500);
        assert_eq!(d.due(), Some(last + QUIET));
        d.reloaded(last + QUIET, Duration::from_millis(5));
        assert_eq!(d.due(), None);
    }

    #[test]
    fn continuous_events_still_reload_by_max_wait() {
        let t0 = Instant::now();
        let mut d = Debounce::default();
        for i in 0..50u64 {
            d.event(t0 + Duration::from_millis(i * 100)); // never quiet
        }
        assert_eq!(d.due(), Some(t0 + MAX_WAIT));
    }

    #[test]
    fn min_interval_scales_with_load_cost() {
        let t0 = Instant::now();
        let mut d = Debounce::default();
        // Cheap load: the 1 s floor applies.
        d.reloaded(t0, Duration::from_millis(20));
        d.event(t0);
        assert_eq!(d.due(), Some(t0 + Duration::from_secs(1)));
        // A 900 ms load (100k entries): next reload no sooner than 1.8 s.
        d.reloaded(t0, Duration::from_millis(900));
        d.event(t0 + Duration::from_millis(10));
        assert_eq!(d.due(), Some(t0 + Duration::from_millis(1800)));
        // Back to cheap: the interval shrinks again.
        d.reloaded(t0, Duration::ZERO);
        d.event(t0);
        assert_eq!(d.due(), Some(t0 + Duration::from_secs(1)));
    }

    /// Waits up to `timeout` for a signal on `rx`.
    fn got(rx: &mpsc::Receiver<()>, timeout: Duration) -> bool {
        rx.recv_timeout(timeout).is_ok()
    }

    fn watch() -> (FolderWatch, mpsc::Receiver<()>) {
        let (tx, rx) = mpsc::channel();
        let tx = std::sync::Mutex::new(tx);
        let w = FolderWatch::new(move || {
            if let Ok(tx) = tx.lock() {
                let _ = tx.send(());
            }
        })
        .unwrap();
        (w, rx)
    }

    /// The watch is set up asynchronously; poke the folder until an event
    /// arrives, so the test doesn't race the watcher thread. The budget is
    /// generous: under load FSEventStreamStart can take seconds.
    fn wait_until_watching(dir: &Path, rx: &mpsc::Receiver<()>) {
        for i in 0..200 {
            std::fs::write(dir.join(format!("probe-{i}")), b"").unwrap();
            if got(rx, Duration::from_millis(100)) {
                // Drain until a full second of quiet: under load FSEvents
                // can deliver a probe's follow-up event (xattr, content)
                // late enough to land in a test's "must stay silent" window.
                while got(rx, Duration::from_secs(1)) {}
                return;
            }
        }
        panic!("watch never became active");
    }

    #[test]
    fn folder_watch_reports_external_create() {
        let dir = tempfile::tempdir().unwrap();
        let (w, rx) = watch();
        w.retarget(Some(dir.path().to_path_buf()));
        wait_until_watching(dir.path(), &rx);
        std::fs::write(dir.path().join("new.txt"), b"x").unwrap();
        assert!(got(&rx, Duration::from_secs(5)));
    }

    #[test]
    fn retarget_stops_old_folder_events() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let (w, rx) = watch();
        w.retarget(Some(a.path().to_path_buf()));
        wait_until_watching(a.path(), &rx);
        w.retarget(Some(b.path().to_path_buf()));
        wait_until_watching(b.path(), &rx);
        std::fs::write(a.path().join("old.txt"), b"x").unwrap();
        assert!(
            !got(&rx, Duration::from_secs(1)),
            "old folder still reports"
        );
        std::fs::write(b.path().join("new.txt"), b"x").unwrap();
        assert!(got(&rx, Duration::from_secs(5)));
        w.retarget(None);
        // Let `None` land before writing (it's queued behind nothing).
        std::thread::sleep(Duration::from_millis(200));
        while got(&rx, Duration::from_millis(100)) {}
        std::fs::write(b.path().join("after.txt"), b"x").unwrap();
        assert!(
            !got(&rx, Duration::from_secs(1)),
            "unwatched folder reports"
        );
    }

    #[test]
    fn retarget_returns_immediately() {
        let dir = tempfile::tempdir().unwrap();
        let (w, _rx) = watch();
        let start = Instant::now();
        for _ in 0..10 {
            w.retarget(Some(dir.path().to_path_buf()));
            w.retarget(None);
        }
        assert!(start.elapsed() < Duration::from_millis(5));
    }
}

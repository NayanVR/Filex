//! macOS and Windows: one native recursive watch per root. FSEvents and
//! ReadDirectoryChangesW cover a subtree without walking it, so excluded
//! areas cost nothing to watch; their events are dropped by the caller.
//!
//! Assumes notify's backends report a lost-details overflow as a `Rescan`
//! event (RDCW: `ERROR_NOTIFY_ENUM_DIR`, or success with zero bytes; FSEvents:
//! `MustScanSubDirs`), and that a fatal read error reaches the handler as an
//! `Err` carrying the root's path, after which notify has dropped that
//! watch. Such roots are re-armed here with backoff and, once watched again,
//! a `Rescan` asks the writer to reconcile what was missed. Re-watching a
//! path notify already watches replaces that watch (RDCW opens the new
//! handle first; FSEvents keeps an unchanged stream), so it is safe to
//! repeat.
use super::Handler;
use crate::ingest::IndexFilter;
use notify::{Event, EventKind, RecursiveMode, Watcher, event::Flag};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock, mpsc},
    time::{Duration, Instant},
};

const FIRST_RETRY: Duration = Duration::from_secs(1);
const MAX_RETRY: Duration = Duration::from_secs(60);
const TICK: Duration = Duration::from_secs(1);

pub struct RootWatches {
    watcher: Arc<Mutex<notify::RecommendedWatcher>>,
    roots: Arc<Mutex<Vec<PathBuf>>>,
    requests: mpsc::Sender<Request>,
}

enum Request {
    /// A watch error named these paths (none: unknown, so every root).
    Failed(Vec<PathBuf>),
    /// Re-watch every root now.
    Resync,
}

impl RootWatches {
    pub fn new(handler: Handler, _filter: Arc<RwLock<IndexFilter>>) -> notify::Result<Self> {
        let (requests, rx) = mpsc::channel();
        let watcher =
            notify::recommended_watcher(forward_errors(requests.clone(), handler.clone()))?;
        let watcher = Arc::new(Mutex::new(watcher));
        let roots = Arc::new(Mutex::new(Vec::new()));
        // Weak, so dropping `RootWatches` drops the watcher, whose callback
        // holds the last other sender: the registrar then sees the channel
        // disconnect and exits.
        let weak = Arc::downgrade(&watcher);
        spawn_registrar(rx, roots.clone(), handler, move |root: &Path| {
            match weak.upgrade() {
                Some(w) => w.lock().unwrap().watch(root, RecursiveMode::Recursive),
                None => Err(notify::Error::generic("watcher dropped")),
            }
        })
        .map_err(notify::Error::io)?;
        Ok(Self {
            watcher,
            roots,
            requests,
        })
    }

    pub fn add_root(&self, root: &Path) -> notify::Result<()> {
        self.watcher
            .lock()
            .unwrap()
            .watch(root, RecursiveMode::Recursive)?;
        self.roots.lock().unwrap().push(root.to_path_buf());
        Ok(())
    }

    pub fn remove_root(&self, root: &Path) {
        self.roots.lock().unwrap().retain(|r| r != root);
        let _ = self.watcher.lock().unwrap().unwatch(root);
    }

    /// Re-watch every root, off the calling thread: one notify dropped
    /// after an error comes back without waiting out its backoff.
    pub fn resync(&self) {
        let _ = self.requests.send(Request::Resync);
    }
}

/// The handler notify calls: errors also go to the registrar, since
/// `watch()` can't be called from here (on Windows it waits on the thread
/// running this callback).
fn forward_errors(
    requests: mpsc::Sender<Request>,
    handler: Handler,
) -> impl Fn(notify::Result<Event>) + Send + 'static {
    move |event| {
        if let Err(e) = &event {
            let _ = requests.send(Request::Failed(e.paths.clone()));
        }
        handler(event);
    }
}

use super::REARMED;

fn rescan() -> Event {
    Event::new(EventKind::Other)
        .set_flag(Flag::Rescan)
        .set_info(REARMED)
}

/// The roots `paths` fall under; every root when none match.
fn roots_for(roots: &[PathBuf], paths: &[PathBuf]) -> Vec<PathBuf> {
    let hit: Vec<_> = roots
        .iter()
        .filter(|r| paths.iter().any(|p| p.starts_with(r)))
        .cloned()
        .collect();
    if hit.is_empty() { roots.to_vec() } else { hit }
}

fn spawn_registrar(
    requests: mpsc::Receiver<Request>,
    roots: Arc<Mutex<Vec<PathBuf>>>,
    handler: Handler,
    watch: impl FnMut(&Path) -> notify::Result<()> + Send + 'static,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("filex-watch-rearm".into())
        .spawn(move || run_registrar(requests, &roots, &handler, watch))
}

/// Re-watches roots notify dropped, with backoff, until every sender is
/// gone.
fn run_registrar(
    requests: mpsc::Receiver<Request>,
    roots: &Mutex<Vec<PathBuf>>,
    handler: &Handler,
    mut watch: impl FnMut(&Path) -> notify::Result<()>,
) {
    let mut rearm = Rearm::default();
    loop {
        let request = match requests.recv_timeout(TICK) {
            Ok(request) => Some(request),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        };
        let live = roots.lock().unwrap().clone();
        let now = Instant::now();
        let mut attempt = BTreeSet::new();
        match request {
            Some(Request::Failed(paths)) => {
                for root in roots_for(&live, &paths) {
                    rearm.failed(root, now);
                }
            }
            Some(Request::Resync) => attempt.extend(live.iter().cloned()),
            None => {}
        }
        attempt.extend(rearm.take_due(now));
        for root in attempt {
            if !live.contains(&root) {
                rearm.succeeded(&root);
                continue;
            }
            match watch(&root) {
                Ok(()) => {
                    if rearm.succeeded(&root) {
                        handler(Ok(rescan().add_path(root)));
                    }
                }
                Err(e) => {
                    tracing::debug!("re-watching {}: {e}", root.display());
                    // A root that was healthy until now (a resync found it
                    // broken) is reported once, so coverage loss stays
                    // visible; backoff retries of a known-bad root aren't.
                    let newly_failing = !rearm.due.contains_key(&root);
                    rearm.failed(root.clone(), now);
                    if newly_failing {
                        handler(Err(e.add_path(root)));
                    }
                }
            }
        }
    }
}

/// When each dropped root is next retried: 1 s after the first failure,
/// doubling per failed retry, capped at 60 s.
#[derive(Default)]
struct Rearm {
    due: BTreeMap<PathBuf, (Instant, Duration)>,
}

impl Rearm {
    /// `root` failed (a reported error or a failed retry). A root already
    /// waiting out its delay keeps it, so a burst of errors backs off once.
    fn failed(&mut self, root: PathBuf, now: Instant) {
        let delay = match self.due.get(&root) {
            Some(&(at, _)) if at > now => return,
            Some(&(_, delay)) => (delay * 2).min(MAX_RETRY),
            None => FIRST_RETRY,
        };
        self.due.insert(root, (now + delay, delay));
    }

    /// Roots whose retry is due. They stay scheduled until reported
    /// [`succeeded`](Self::succeeded) or [`failed`](Self::failed).
    fn take_due(&mut self, now: Instant) -> Vec<PathBuf> {
        self.due
            .iter()
            .filter(|(_, (at, _))| *at <= now)
            .map(|(root, _)| root.clone())
            .collect()
    }

    /// Clears `root`'s schedule; whether it had one.
    fn succeeded(&mut self, root: &Path) -> bool {
        self.due.remove(root).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rearm_backs_off_and_caps() {
        let root = PathBuf::from("/r");
        let mut rearm = Rearm::default();
        let mut now = Instant::now();
        rearm.failed(root.clone(), now);
        rearm.failed(root.clone(), now + Duration::from_millis(500));
        let mut delays = Vec::new();
        for _ in 0..9 {
            let (at, delay) = rearm.due[&root];
            assert!(rearm.take_due(at - Duration::from_millis(1)).is_empty());
            delays.push(delay.as_secs());
            now = at;
            assert_eq!(rearm.take_due(now), std::slice::from_ref(&root));
            rearm.failed(root.clone(), now);
        }
        assert_eq!(delays, [1, 2, 4, 8, 16, 32, 60, 60, 60]);
    }

    #[test]
    fn rearm_success_clears_schedule() {
        let root = PathBuf::from("/r");
        let mut rearm = Rearm::default();
        let now = Instant::now();
        rearm.failed(root.clone(), now);
        rearm.failed(root.clone(), now + FIRST_RETRY);
        assert!(rearm.succeeded(&root));
        assert!(!rearm.succeeded(&root));
        assert!(rearm.take_due(now + MAX_RETRY).is_empty());
        rearm.failed(root.clone(), now + MAX_RETRY);
        assert_eq!(rearm.due[&root].1, FIRST_RETRY, "backoff starts over");
    }

    /// A fatal error for a path under a root re-watches that root (only)
    /// and then asks for exactly one rescan; the registrar exits once its
    /// senders are gone.
    #[test]
    fn fatal_error_rearms_root_and_requests_rescan() {
        let root = PathBuf::from("/data/root");
        let roots = Arc::new(Mutex::new(vec![root.clone(), PathBuf::from("/other")]));
        let (events_tx, events) = mpsc::channel();
        let events_tx = Mutex::new(events_tx);
        let handler: Handler = Arc::new(move |e: notify::Result<Event>| {
            let _ = events_tx.lock().unwrap().send(e);
        });
        let watched = Arc::new(Mutex::new(Vec::new()));
        let log = watched.clone();
        let (requests, rx) = mpsc::channel();
        let registrar = spawn_registrar(rx, roots, handler.clone(), move |p: &Path| {
            log.lock().unwrap().push(p.to_path_buf());
            Ok(())
        })
        .unwrap();
        let callback = forward_errors(requests, handler);

        callback(Err(
            notify::Error::generic("read failed").add_path(root.join("a/b"))
        ));
        assert!(
            events
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .is_err()
        );
        let event = events
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap();
        assert!(event.need_rescan());
        assert_eq!(event.info(), Some(REARMED));
        assert_eq!(
            event.paths,
            std::slice::from_ref(&root),
            "only that root reconciles"
        );
        assert!(events.recv_timeout(Duration::from_millis(2500)).is_err());
        assert_eq!(*watched.lock().unwrap(), [root]);

        drop(callback);
        registrar.join().unwrap();
    }

    /// Regression: a resync clears the daemon's coverage error, so a root
    /// it finds broken must be reported again (once, not per backoff step),
    /// or an unwatched root would look healthy.
    #[test]
    fn resync_failure_is_reported_once() {
        let root = PathBuf::from("/data/root");
        let roots = Arc::new(Mutex::new(vec![root.clone()]));
        let (events_tx, events) = mpsc::channel();
        let events_tx = Mutex::new(events_tx);
        let handler: Handler = Arc::new(move |e: notify::Result<Event>| {
            let _ = events_tx.lock().unwrap().send(e);
        });
        let (requests, rx) = mpsc::channel();
        let registrar = spawn_registrar(rx, roots, handler, |_: &Path| {
            Err(notify::Error::generic("volume gone"))
        })
        .unwrap();

        requests.send(Request::Resync).unwrap();
        let error = events
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap_err();
        assert_eq!(error.paths, [root]);
        // The 1 s backoff retry also fails, silently.
        assert!(events.recv_timeout(Duration::from_millis(2500)).is_err());

        drop(requests);
        registrar.join().unwrap();
    }

    /// RDCW overflow regression: notify 8 ignored a zero-byte completion,
    /// so events lost while the handler was busy were never reported.
    #[cfg(windows)]
    #[test]
    fn windows_overflow_reports_rescan() {
        use std::sync::{
            Barrier,
            atomic::{AtomicBool, Ordering},
        };
        let tree = tempfile::tempdir().unwrap();
        let root = tree.path().canonicalize().unwrap();
        let gate = Arc::new(Barrier::new(2));
        let blocked = AtomicBool::new(false);
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        let handler_gate = gate.clone();
        let watches = RootWatches::new(
            Arc::new(move |e: notify::Result<Event>| {
                let rescan = e.as_ref().is_ok_and(|e| e.need_rescan());
                let _ = tx.lock().unwrap().send(rescan);
                if !blocked.swap(true, Ordering::SeqCst) {
                    handler_gate.wait();
                }
            }),
            Arc::new(RwLock::new(IndexFilter::default())),
        )
        .unwrap();
        watches.add_root(&root).unwrap();

        std::fs::write(root.join("trigger"), b"x").unwrap();
        // The first event: the handler is now parked on the gate.
        rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let stem = "f".repeat(96);
        for i in 0..3000 {
            std::fs::write(root.join(format!("{stem}{i:04}")), b"").unwrap();
        }
        gate.wait();

        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Ok(true) = rx.recv_timeout(Duration::from_millis(100)) {
                return;
            }
        }
        panic!("no rescan after overflowing the RDCW buffer");
    }
}

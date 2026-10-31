//! One non-recursive watch per directory, registered by Filex rather than
//! notify, so the walk that sets watches up skips excluded areas. Used on
//! Linux; compiled into tests everywhere, since only notify's portable API
//! is involved.
//!
//! Assumes inotify semantics: a watch covers one directory's direct
//! entries, so a directory created or moved in needs its own watch (added
//! here when its event arrives); the kernel drops a deleted directory's
//! watch and notify forgets it. Watches can't be added from notify's own
//! event callback (it runs on the thread that serves `watch` calls), so a
//! registrar thread does that.
use super::{Handler, watch_dirs};
use crate::ingest::IndexFilter;
use notify::{EventKind, RecursiveMode, Watcher, event::ModifyKind};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock, Weak, mpsc},
};

struct State {
    watcher: notify::RecommendedWatcher,
    roots: Vec<PathBuf>,
    watched: BTreeSet<PathBuf>,
}

enum Request {
    /// Paths an event named that may be new directories.
    NewDirs(Vec<PathBuf>),
    /// Re-derive every root's watches: the filter changed, or the queue
    /// overflowed and directory creations may have been missed.
    Resync,
}

pub struct RootWatches {
    state: Arc<Mutex<State>>,
    filter: Arc<RwLock<IndexFilter>>,
    handler: Handler,
    requests: mpsc::Sender<Request>,
}

impl RootWatches {
    pub fn new(handler: Handler, filter: Arc<RwLock<IndexFilter>>) -> notify::Result<Self> {
        let (requests, rx) = mpsc::channel();
        let forward = requests.clone();
        let on_event = handler.clone();
        let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            if let Ok(event) = &event {
                if event.need_rescan() {
                    let _ = forward.send(Request::Resync);
                } else if may_add_directories(&event.kind) {
                    let _ = forward.send(Request::NewDirs(event.paths.clone()));
                }
            }
            on_event(event);
        })?;
        let state = Arc::new(Mutex::new(State {
            watcher,
            roots: Vec::new(),
            watched: BTreeSet::new(),
        }));
        let registrar = Registrar {
            state: Arc::downgrade(&state),
            filter: filter.clone(),
            handler: handler.clone(),
        };
        std::thread::Builder::new()
            .name("filex-watch".into())
            .spawn(move || registrar.run(rx))
            .map_err(notify::Error::io)?;
        Ok(Self {
            state,
            filter,
            handler,
            requests,
        })
    }

    /// Watch `root` and every directory under it the filter keeps. Fails
    /// only when the root itself can't be watched; a subdirectory that
    /// can't be is reported to the handler and skipped.
    pub fn add_root(&self, root: &Path) -> notify::Result<()> {
        let dirs = watch_dirs(root, root, &self.filter.read().unwrap());
        let mut state = self.state.lock().unwrap();
        state.watcher.watch(root, RecursiveMode::NonRecursive)?;
        state.watched.insert(root.to_path_buf());
        state.roots.push(root.to_path_buf());
        watch_all(&mut state, dirs, &self.handler);
        Ok(())
    }

    pub fn remove_root(&self, root: &Path) {
        let mut state = self.state.lock().unwrap();
        state.roots.retain(|r| r != root);
        let gone: Vec<_> = state
            .watched
            .iter()
            .filter(|p| p.starts_with(root))
            .cloned()
            .collect();
        for path in gone {
            let _ = state.watcher.unwatch(&path);
            state.watched.remove(&path);
        }
    }

    /// Bring the watches in line with the current filter, off the calling
    /// thread.
    pub fn resync(&self) {
        let _ = self.requests.send(Request::Resync);
    }

    #[cfg(test)]
    fn watched(&self) -> BTreeSet<PathBuf> {
        self.state.lock().unwrap().watched.clone()
    }
}

/// Event kinds that can bring a new directory into a watched one: a
/// creation, or a rename/move (whose paths include the new name).
fn may_add_directories(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_) | EventKind::Modify(ModifyKind::Name(_))
    )
}

/// Watch each of `dirs` not watched yet. Paths that vanished in the
/// meantime are skipped quietly; any other failure (the inotify watch
/// limit, say) goes to the handler, which marks coverage incomplete.
fn watch_all(state: &mut State, dirs: Vec<PathBuf>, handler: &Handler) {
    for dir in dirs {
        if state.watched.contains(&dir) {
            continue;
        }
        match state.watcher.watch(&dir, RecursiveMode::NonRecursive) {
            Ok(()) => {
                state.watched.insert(dir);
            }
            Err(e) if is_not_found(&e) => {}
            Err(e) => handler(Err(e)),
        }
    }
}

fn is_not_found(error: &notify::Error) -> bool {
    match &error.kind {
        notify::ErrorKind::PathNotFound => true,
        notify::ErrorKind::Io(io) => io.kind() == std::io::ErrorKind::NotFound,
        _ => false,
    }
}

/// Adds and drops watches as directories appear or the filter changes.
/// Holds the state weakly so dropping [`RootWatches`] ends it.
struct Registrar {
    state: Weak<Mutex<State>>,
    filter: Arc<RwLock<IndexFilter>>,
    handler: Handler,
}

impl Registrar {
    fn run(self, requests: mpsc::Receiver<Request>) {
        while let Ok(request) = requests.recv() {
            let Some(state) = self.state.upgrade() else {
                return;
            };
            let filter = self.filter.read().unwrap().clone();
            match request {
                Request::NewDirs(paths) => self.add_new(&state, &filter, paths),
                Request::Resync => self.resync(&state, &filter),
            }
        }
    }

    fn add_new(&self, state: &Mutex<State>, filter: &IndexFilter, paths: Vec<PathBuf>) {
        let roots = state.lock().unwrap().roots.clone();
        for path in paths {
            let Some(root) = roots.iter().find(|r| path.starts_with(r)) else {
                continue;
            };
            let is_dir = std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir());
            if !is_dir || filter.excludes_dir(root, &path) {
                continue;
            }
            let dirs = watch_dirs(root, &path, filter);
            watch_all(&mut state.lock().unwrap(), dirs, &self.handler);
        }
    }

    fn resync(&self, state: &Mutex<State>, filter: &IndexFilter) {
        let roots = state.lock().unwrap().roots.clone();
        let wanted: BTreeSet<PathBuf> = roots
            .iter()
            .flat_map(|root| watch_dirs(root, root, filter))
            .collect();
        let mut state = state.lock().unwrap();
        let stale: Vec<_> = state.watched.difference(&wanted).cloned().collect();
        for path in stale {
            let _ = state.watcher.unwatch(&path);
            state.watched.remove(&path);
        }
        watch_all(&mut state, wanted.into_iter().collect(), &self.handler);
    }
}

#[cfg(test)]
mod tests {
    use super::RootWatches;
    use crate::ingest::IndexFilter;
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::{Arc, RwLock, mpsc},
        time::{Duration, Instant},
    };

    fn dev_filter() -> IndexFilter {
        IndexFilter::from_settings(&crate::settings::Settings {
            exclude_dev_folders: true,
            excluded_folder_names: vec!["node_modules".into()],
            ..Default::default()
        })
    }

    fn start(
        filter: IndexFilter,
    ) -> (
        RootWatches,
        mpsc::Receiver<PathBuf>,
        Arc<RwLock<IndexFilter>>,
    ) {
        let (tx, rx) = mpsc::channel();
        let tx = std::sync::Mutex::new(tx);
        let filter = Arc::new(RwLock::new(filter));
        let watches = RootWatches::new(
            Arc::new(move |event: notify::Result<notify::Event>| {
                if let Ok(event) = event {
                    for path in event.paths {
                        let _ = tx.lock().unwrap().send(path);
                    }
                }
            }),
            filter.clone(),
        )
        .unwrap();
        (watches, rx, filter)
    }

    /// Paths reported until one equal to `marker` arrives.
    fn until(rx: &mpsc::Receiver<PathBuf>, marker: &Path) -> Vec<PathBuf> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut seen = Vec::new();
        while Instant::now() < deadline {
            if let Ok(path) = rx.recv_timeout(Duration::from_millis(100)) {
                let done = path == marker;
                seen.push(path);
                if done {
                    return seen;
                }
            }
        }
        panic!("no event for {marker:?}; saw {seen:?}");
    }

    fn wait_for(watches: &RootWatches, dir: &Path) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !watches.watched().contains(dir) {
            assert!(Instant::now() < deadline, "{dir:?} never watched");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// FIL-66: excluded folders get no watch, so changes inside them raise
    /// no events at all; a directory created later is watched too.
    #[test]
    fn excluded_folders_are_not_watched_and_new_folders_are() {
        let tree = tempfile::tempdir().unwrap();
        let root = tree.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("web/node_modules/pkg")).unwrap();
        let (watches, rx, _) = start(dev_filter());
        watches.add_root(&root).unwrap();
        assert!(watches.watched().contains(&root.join("web")));
        assert!(
            !watches
                .watched()
                .iter()
                .any(|p| p.starts_with(root.join("web/node_modules"))),
            "{:?}",
            watches.watched()
        );

        fs::write(root.join("web/node_modules/pkg/index.js"), b"x").unwrap();
        fs::create_dir(root.join("new")).unwrap();
        wait_for(&watches, &root.join("new"));
        let marker = root.join("new/marker.txt");
        fs::write(&marker, b"x").unwrap();
        let seen = until(&rx, &marker);
        assert!(
            !seen
                .iter()
                .any(|p| p.starts_with(root.join("web/node_modules/pkg"))),
            "{seen:?}"
        );
    }

    /// Turning a folder name's exclusion off watches that folder after a
    /// resync, and turning it back on drops the watch again.
    #[test]
    fn resync_follows_filter_changes() {
        let tree = tempfile::tempdir().unwrap();
        let root = tree.path().canonicalize().unwrap();
        let modules = root.join("node_modules");
        fs::create_dir_all(modules.join("pkg")).unwrap();
        let (watches, _rx, filter) = start(dev_filter());
        watches.add_root(&root).unwrap();
        assert!(!watches.watched().contains(&modules));

        *filter.write().unwrap() = IndexFilter::default();
        watches.resync();
        wait_for(&watches, &modules.join("pkg"));

        *filter.write().unwrap() = dev_filter();
        watches.resync();
        let deadline = Instant::now() + Duration::from_secs(10);
        while watches.watched().contains(&modules) {
            assert!(Instant::now() < deadline, "watch never dropped");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(watches.watched().contains(&root));
    }

    #[test]
    fn removing_a_root_drops_all_its_watches() {
        let tree = tempfile::tempdir().unwrap();
        let root = tree.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("a/b")).unwrap();
        let (watches, _rx, _) = start(IndexFilter::default());
        watches.add_root(&root).unwrap();
        assert_eq!(watches.watched().len(), 3);
        watches.remove_root(&root);
        assert!(watches.watched().is_empty());
    }
}

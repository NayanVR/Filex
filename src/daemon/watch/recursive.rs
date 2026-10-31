//! macOS and Windows: one native recursive watch per root. FSEvents and
//! ReadDirectoryChangesW cover a subtree without walking it, so excluded
//! areas cost nothing to watch; their events are dropped by the caller.
use super::Handler;
use crate::ingest::IndexFilter;
use notify::{RecursiveMode, Watcher};
use std::{
    path::Path,
    sync::{Arc, Mutex, RwLock},
};

pub struct RootWatches {
    watcher: Mutex<notify::RecommendedWatcher>,
}

impl RootWatches {
    pub fn new(handler: Handler, _filter: Arc<RwLock<IndexFilter>>) -> notify::Result<Self> {
        let watcher = notify::recommended_watcher(move |event| handler(event))?;
        Ok(Self {
            watcher: Mutex::new(watcher),
        })
    }
    pub fn add_root(&self, root: &Path) -> notify::Result<()> {
        self.watcher
            .lock()
            .unwrap()
            .watch(root, RecursiveMode::Recursive)
    }
    pub fn remove_root(&self, root: &Path) {
        let _ = self.watcher.lock().unwrap().unwatch(root);
    }
    /// Nothing to redo: a native recursive watch doesn't depend on the
    /// filter.
    pub fn resync(&self) {}
}

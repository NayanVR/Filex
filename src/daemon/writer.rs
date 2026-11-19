//! Single catalog writer. WAL durability precedes visible epochs. Frozen views
//! feed one build coordinator while notifications populate a fresh overlay.
use super::{
    builder::{self, BuildRequest, Built, Mode, Scope, Subtrees},
    changes::{apply, new_trees, normalize_batch},
    ipc::{RootStatus, Status},
    recovery,
    view::{Overlay, View},
};
use crate::{
    catalog::{
        manifest::{self, Manifest},
        segment::{Root, Segment},
        wal::{Delta, Transaction, Wal},
    },
    ingest,
};
use anyhow::Result;
use std::{
    collections::{BTreeMap, VecDeque},
    ops::ControlFlow,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, RwLock, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    time::{Duration, Instant},
};

/// The watcher queues one command per filesystem event, and a full queue
/// forces a reconcile of every root. 256 overflowed on any burst (a checkout,
/// an unzip), so no change ever reached a delta (FIL-30). Slots are ~40
/// bytes, allocated up front.
const COMMAND_CAPACITY: usize = 16_384;
const EVENT_BATCH_LIMIT: usize = 4096;
const OVERLAY_RECORD_LIMIT: usize = 25_000;
const OVERLAY_BYTE_LIMIT: usize = 16 * 1024 * 1024;
/// Entries listed under newly seen directories per batch before falling back
/// to a full reconcile.
const NEW_TREE_LIMIT: usize = 10_000;
/// The overlay is flushed into a small delta segment at this size (FIL-30).
/// Every search scans the overlay entry by entry, so this bounds that cost.
const FLUSH_RECORDS: usize = 4096;
const FLUSH_BYTES: usize = 4 * 1024 * 1024;
/// Below the flush size, flush once the overlay is this old and the
/// filesystem has been quiet for `QUIET_BEFORE_FLUSH`...
const MIN_FLUSH_AGE: Duration = Duration::from_secs(5 * 60);
const QUIET_BEFORE_FLUSH: Duration = Duration::from_secs(30);
/// ...or at this age on a machine that never goes quiet.
const MAX_FLUSH_AGE: Duration = Duration::from_secs(30 * 60);
/// A flush that would exceed this many deltas merges them all into one.
const MAX_DELTAS: usize = 4;
/// The base is rewritten once its deltas hold more than 1/`DELTA_RATIO` as
/// many entries (and at least `FLUSH_RECORDS`).
const DELTA_RATIO: usize = 8;
const BUILD_RETRY_DELAY: Duration = Duration::from_secs(30);
const BUILD_RETRY_LIMIT: Duration = Duration::from_secs(15 * 60);
const WRITER_POLL_INTERVAL: Duration = Duration::from_millis(100);
const BUILDER_SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
/// A watcher overflow reconciles no sooner than this, or twice the last
/// reconcile's build time if longer, after the last reconcile ended: each
/// one walks and rebuilds for 16–27 s at ~1.3 GiB, and overflows during it
/// would otherwise chain them back to back. Changes keep reaching the
/// overlay meanwhile.
const MIN_OVERFLOW_GAP: Duration = Duration::from_secs(60);

pub enum Command {
    Paths { paths: Vec<PathBuf>, remove: bool },
    AddRoot(PathBuf),
    RemoveRoot(PathBuf),
    Built(Box<Result<Built>>),
    Reconcile,
    Stop,
}
pub struct Shared {
    pub view: RwLock<Arc<View>>,
    pub status: RwLock<Status>,
    pub stop: Arc<AtomicBool>,
    /// Set with `stop` when the daemon must exit for its replacement
    /// binary to take over (FIL-21); `server::run` then exits as a failure.
    pub restart: AtomicBool,
    /// Roots a watcher overflow (or a full command queue) left stale, for
    /// the writer to reconcile once [`overflow_due`] allows.
    pub overflow: Mutex<Option<Scope>>,
    /// The index filter, reloaded only when settings change (a
    /// `Reconcile` request), not per event batch.
    pub filter: Arc<RwLock<ingest::IndexFilter>>,
    pub coverage_error: RwLock<Option<String>>,
    pub progress: Arc<super::progress::WalkProgress>,
}
impl Shared {
    /// Asks for a reconcile of what `paths` name (see [`affected`]); every
    /// root when `paths` is empty, i.e. the overflow named no path.
    ///
    /// Which overflows name a path, so reconcile only a subtree: macOS
    /// FSEvents `MustScanSubDirs` (the usual macOS overflow; its directory
    /// can be an ancestor of the root, even `/`, which takes the whole
    /// root), Linux per-directory watch errors, a full command queue (the
    /// refused events' paths) and the root-naming rescan after a re-arm.
    /// Windows RDCW overflow and Linux `IN_Q_OVERFLOW` name none and stay
    /// whole-root (every root); only a USN journal source can narrow those.
    /// No filesystem I/O here (it runs on watcher and IPC threads): paths
    /// stay raw until the reconcile starts, when
    /// [`builder::resolve_scope`] canonicalises and widens them.
    pub fn overflowed(&self, paths: &[PathBuf]) {
        let scope = if paths.is_empty() {
            Scope::All
        } else {
            affected(&self.view.read().unwrap().roots, paths)
        };
        widen(&mut self.overflow.lock().unwrap(), scope);
    }
}

/// The roots holding, or under, `path`.
fn touching<'a>(roots: &'a [Root], path: &'a Path) -> impl Iterator<Item = u32> + 'a {
    (roots.iter())
        .filter(move |r| path.starts_with(&r.path) || r.path.starts_with(path))
        .map(|r| r.id)
}

/// The reconcile that repairs `paths`: an overflow's, or a batch the
/// writer had to drop or couldn't inspect. A path inside a root walks the
/// subtree there (a file's: the builder widens it to its directory, whose
/// re-listing covers the file's creation, change or removal); a root, or a
/// path above one, walks the root whole. Batches keep only paths touching a
/// root, so one in none is under a root removed since, and needs no repair.
pub(super) fn affected<'a>(roots: &[Root], paths: impl IntoIterator<Item = &'a PathBuf>) -> Scope {
    let mut scope = Scope::Roots(Default::default());
    for path in paths {
        for root in roots {
            let part = if *path == root.path || root.path.starts_with(path) {
                Subtrees::Whole
            } else if path.starts_with(&root.path) {
                Subtrees::Dirs([path.clone()].into())
            } else {
                continue;
            };
            scope = scope.merge(Scope::Roots([(root.id, part)].into()));
        }
    }
    scope
}

/// Adds `scope` to a pending reconcile, if any. An empty scope asks for
/// nothing, so it never starts a reconcile on its own.
fn widen(pending: &mut Option<Scope>, scope: Scope) {
    if scope == Scope::Roots(Default::default()) {
        return;
    }
    *pending = Some(match pending.take() {
        Some(pending) => pending.merge(scope),
        None => scope,
    });
}

/// Whether a watcher overflow may reconcile at `now`, given when the last
/// reconcile ended and how long its build took (none yet: at once).
fn overflow_due(now: Instant, last: Option<(Instant, Duration)>) -> bool {
    last.is_none_or(|(ended, took)| {
        now.saturating_duration_since(ended) >= MIN_OVERFLOW_GAP.max(took.saturating_mul(2))
    })
}

pub struct Handle {
    pub shared: Arc<Shared>,
    pub commands: SyncSender<Command>,
    pub thread: Option<std::thread::JoinHandle<()>>,
}
pub(crate) fn start(directory: &Path, configured: Vec<PathBuf>) -> Result<Handle> {
    let (writer, commands, build_rx) = open(directory, configured)?;
    let shared = writer.owner.clone();
    let worker_commands = commands.clone();
    let worker_stop = shared.stop.clone();
    let builder = std::thread::Builder::new()
        .name("filex-segment-builder".into())
        .spawn(move || {
            while let Ok(request) = build_rx.recv() {
                let result = builder::build(request, &worker_stop);
                if worker_commands
                    .send(Command::Built(Box::new(result)))
                    .is_err()
                {
                    break;
                }
            }
        })?;
    let thread = std::thread::Builder::new()
        .name("filex-catalog-writer".into())
        .spawn(move || writer.run(builder))?;
    Ok(Handle {
        shared,
        commands,
        thread: Some(thread),
    })
}

/// Recovers the catalog in `directory` into a writer, without starting the
/// builder or writer threads.
fn open(
    directory: &Path,
    configured: Vec<PathBuf>,
) -> Result<(Writer, SyncSender<Command>, Receiver<BuildRequest>)> {
    std::fs::create_dir_all(directory)?;
    let canonical_directory = directory.canonicalize()?;
    let directory = canonical_directory.as_path();
    let recovery::Recovered {
        wal,
        roots,
        base,
        deltas,
        sequence: seq,
        next_id,
        manifest_epoch,
        active,
        files,
    } = recovery::recover(directory, configured)?;
    // Segment files mapped by some view; retention never deletes these.
    let live: Vec<(PathBuf, Weak<Segment>)> = files
        .iter()
        .cloned()
        .zip(std::iter::once(&base).chain(&deltas).map(Arc::downgrade))
        .collect();
    let initial = Arc::new(View {
        base,
        deltas,
        layers: vec![Arc::new(active.clone())],
        roots: roots.clone(),
        epoch: seq,
    });
    // Per-root entry counts for status, refreshed when a build publishes.
    let counts = initial.root_counts();
    let shared = Arc::new(Shared {
        view: RwLock::new(initial),
        status: RwLock::new(Status {
            epoch: seq,
            roots: Vec::new(),
            building: !roots.is_empty(),
            error: None,
            skipped: 0,
            progress: None,
        }),
        stop: Arc::new(AtomicBool::new(false)),
        restart: AtomicBool::new(false),
        overflow: Mutex::new(None),
        filter: Arc::new(RwLock::new(ingest::IndexFilter::load())),
        coverage_error: RwLock::new(None),
        progress: Default::default(),
    });
    let (commands, rx) = mpsc::sync_channel(COMMAND_CAPACITY);
    let (build_tx, build_rx) = mpsc::sync_channel::<BuildRequest>(1);
    let directory = directory.canonicalize()?;
    let next = Arc::new(AtomicU64::new(
        next_id.max(roots.iter().map(|r| u64::from(r.id) + 1).max().unwrap_or(1)),
    ));
    let now = Instant::now();
    let writer = Writer {
        owner: shared,
        directory,
        next,
        rx,
        build_tx,
        deferred: VecDeque::new(),
        wal,
        active,
        frozen: Vec::new(),
        roots,
        seq,
        manifest_epoch,
        files,
        live,
        counts,
        building: false,
        mode: Mode::Merge,
        pending_paths: BTreeMap::new(),
        reconcile: None,
        overflowed: None,
        last_reconcile: None,
        awaiting_watch: true,
        last_build: now,
        last_change: now,
        build_started: now,
        retry_after: now,
        retry_delay: BUILD_RETRY_DELAY,
        build_scope: Scope::All,
        unavailable: BTreeMap::new(),
    };
    Ok((writer, commands, build_rx))
}

/// The writer thread's state. Only this thread appends to the WAL, swaps
/// views and publishes manifests, so none of it needs a lock.
struct Writer {
    owner: Arc<Shared>,
    directory: PathBuf,
    next: Arc<AtomicU64>,
    rx: Receiver<Command>,
    build_tx: SyncSender<BuildRequest>,
    /// Commands read ahead while batching events, handled before `rx`.
    deferred: VecDeque<Command>,
    wal: Wal,
    active: Overlay,
    /// Overlays handed to the running build, searchable until it publishes.
    frozen: Vec<Arc<Overlay>>,
    roots: Vec<Root>,
    seq: u64,
    manifest_epoch: u64,
    /// The published generation's segment files, base first.
    files: Vec<PathBuf>,
    live: Vec<(PathBuf, Weak<Segment>)>,
    counts: BTreeMap<u32, u64>,
    building: bool,
    /// What the running (or last) build does.
    mode: Mode,
    /// Changes held back while a reconcile walks, applied once it publishes.
    pending_paths: BTreeMap<PathBuf, bool>,
    /// A pending reconcile; explicit commands and the writer's own
    /// recovery set it directly, watcher overflows via `overflowed`.
    reconcile: Option<Scope>,
    overflowed: Option<Scope>,
    /// When the last reconcile build ended, and how long it took.
    last_reconcile: Option<(Instant, Duration)>,
    awaiting_watch: bool,
    last_build: Instant,
    last_change: Instant,
    build_started: Instant,
    retry_after: Instant,
    retry_delay: Duration,
    /// What the running reconcile walks, and the roots the last walk of
    /// each couldn't reach, with why.
    build_scope: Scope,
    unavailable: BTreeMap<u32, String>,
}

impl Writer {
    fn run(mut self, builder: std::thread::JoinHandle<()>) {
        while !self.owner.stop.load(Ordering::Relaxed) {
            self.tick();
            let command = match self
                .deferred
                .pop_front()
                .map(Ok)
                .unwrap_or_else(|| self.rx.recv_timeout(WRITER_POLL_INTERVAL))
            {
                Ok(c) => c,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(_) => break,
            };
            if self.handle(command).is_break() {
                break;
            }
        }
        self.owner.stop.store(true, Ordering::Relaxed);
        drop(self.build_tx);
        drop(self.rx);
        join_within(builder, BUILDER_SHUTDOWN_GRACE);
    }

    /// Per-iteration work before the next command: admit overflows, start a
    /// due build, refresh status.
    fn tick(&mut self) {
        self.admit_overflow();
        self.maybe_start_build();
        update_status(
            &self.owner,
            &self.roots,
            &self.counts,
            &self.unavailable,
            self.seq,
            // A throttled overflow is known staleness, not idle.
            self.building
                || self.reconcile.is_some()
                || self.overflowed.is_some()
                || self.awaiting_watch,
        );
    }

    /// Moves watcher overflows into the pending reconcile once allowed.
    fn admit_overflow(&mut self) {
        let overflow = self.owner.overflow.lock().unwrap().take();
        if let Some(scope) = overflow {
            widen(&mut self.overflowed, scope);
        }
        // An overflow rides along with a reconcile already pending,
        // or waits out the gap since the last one.
        if self.overflowed.is_some()
            && (self.reconcile.is_some() || overflow_due(Instant::now(), self.last_reconcile))
            && let Some(scope) = self.overflowed.take()
        {
            widen(&mut self.reconcile, scope);
        }
    }

    /// Hands the builder a request when a build is due and none is running.
    fn maybe_start_build(&mut self) {
        let due = if self.reconcile.is_some() {
            Some(Mode::Reconcile)
        } else {
            let view = self.owner.view.read().unwrap().clone();
            next_build(
                view.base.len(),
                &view
                    .deltas
                    .iter()
                    .map(|d| delta_size(d))
                    .collect::<Vec<_>>(),
                self.active.records.len()
                    + self.frozen.iter().map(|l| l.records.len()).sum::<usize>(),
                self.active.bytes() + self.frozen.iter().map(|l| l.bytes()).sum::<usize>(),
                self.last_build.elapsed(),
                self.last_change.elapsed(),
            )
        };
        // Deltas need a published base under them.
        let due = due.map(|due| match due {
            Mode::Flush | Mode::MergeDeltas if self.files.is_empty() => Mode::Merge,
            due => due,
        });
        if !self.building
            && std::time::Instant::now() >= self.retry_after
            && let Some(due) = due
        {
            if !self.active.records.is_empty() {
                self.frozen.push(Arc::new(std::mem::take(&mut self.active)));
            }
            publish(
                &self.owner,
                &self.frozen,
                &self.active,
                &self.roots,
                self.seq,
            );
            let view = self.owner.view.read().unwrap().clone();
            self.mode = due;
            let filter = self.owner.filter.read().unwrap().clone();
            let mut scope = self.reconcile.take().unwrap_or(Scope::All);
            if due == Mode::Reconcile {
                // Resolved once against the view the build gets, so the
                // builder walks exactly this set.
                scope = builder::resolve_scope(&view, &filter, scope);
                if scope == Scope::Roots(Default::default()) {
                    // Every requested path resolved outside all roots:
                    // nothing to walk, so don't rewrite the base for it.
                    return;
                }
            }
            // A subtree reconcile may write a delta while one fits the
            // merge policy; the builder decides once it knows the size.
            let delta_budget = (due == Mode::Reconcile && !self.files.is_empty())
                .then(|| {
                    let deltas: Vec<_> = view.deltas.iter().map(|d| delta_size(d)).collect();
                    delta_budget(view.base.len(), &deltas)
                })
                .flatten();
            if self
                .build_tx
                .send(BuildRequest {
                    view,
                    mode: self.mode,
                    delta_budget,
                    scope: scope.clone(),
                    filter,
                    directory: self.directory.clone(),
                    next: self.next.clone(),
                    progress: self.owner.progress.clone(),
                })
                .is_ok()
            {
                self.building = true;
                self.build_started = std::time::Instant::now();
                self.build_scope = scope;
            } else if self.mode == Mode::Reconcile {
                self.reconcile = Some(scope);
            }
        }
    }

    /// Handles one command, then commits what it changed. Breaks when the
    /// writer must stop.
    fn handle(&mut self, command: Command) -> ControlFlow<()> {
        let (deltas, replacement) = match command {
            Command::Stop => return ControlFlow::Break(()),
            Command::Reconcile => {
                self.awaiting_watch = false;
                self.reconcile = Some(Scope::All);
                (Vec::new(), None)
            }
            Command::AddRoot(path) => (self.add_root(path), None),
            Command::RemoveRoot(path) => {
                self.roots.retain(|r| r.path != path);
                self.reconcile = Some(Scope::All);
                (vec![Delta::Roots(self.roots.clone())], None)
            }
            Command::Paths { paths, remove } => {
                let batch = self.collect_batch(paths, remove);
                (self.apply_batch(batch), None)
            }
            Command::Built(result) => self.finish_build(*result),
        };
        self.commit(deltas, replacement)
    }

    fn add_root(&mut self, path: PathBuf) -> Vec<Delta> {
        let paths: Vec<_> = self.roots.iter().map(|r| r.path.clone()).collect();
        match ingest::validate_new_root(&paths, &path) {
            Ok(path) => {
                let allocated = self.next.fetch_add(1, Ordering::Relaxed);
                if allocated > u32::MAX as u64 {
                    self.owner.status.write().unwrap().error =
                        Some("root identifier space exhausted".into());
                    return Vec::new();
                }
                let id = allocated as u32;
                let device = std::fs::symlink_metadata(&path)
                    .map(|m| ingest::identity(&path, &m).device)
                    .unwrap_or(0);
                self.roots.push(Root { id, path, device });
                self.reconcile = Some(Scope::All);
                vec![Delta::Roots(self.roots.clone())]
            }
            Err(e) => {
                self.owner.status.write().unwrap().error = Some(e.to_string());
                Vec::new()
            }
        }
    }

    /// Extends a `Paths` command with those already queued behind it, up to
    /// `EVENT_BATCH_LIMIT`; the first other command waits in `deferred`.
    fn collect_batch(&mut self, paths: Vec<PathBuf>, remove: bool) -> BTreeMap<PathBuf, bool> {
        let mut batch: BTreeMap<PathBuf, bool> = paths.into_iter().map(|p| (p, remove)).collect();
        while batch.len() < EVENT_BATCH_LIMIT {
            match self.rx.try_recv() {
                Ok(Command::Paths { paths, remove }) => {
                    for p in paths {
                        batch.insert(p, remove);
                    }
                }
                Ok(other) => {
                    self.deferred.push_back(other);
                    break;
                }
                Err(_) => break,
            }
        }
        batch
    }

    /// The deltas for a batch of changed paths (`true`: removed). Paths it
    /// cannot take now (a reconcile running, the overlay full, too many new
    /// entries, uninspectable) are held back or widen the pending reconcile.
    fn apply_batch(&mut self, mut batch: BTreeMap<PathBuf, bool>) -> Vec<Delta> {
        let mut deltas = Vec::new();
        {
            let filter = self.owner.filter.read().unwrap();
            // A path touching no root (a UI hint for a copy to
            // an unindexed drive) is not ours to index or repair.
            batch.retain(|p, _| {
                !p.starts_with(&self.directory)
                    && touching(&self.roots, p).next().is_some()
                    && !self.roots.iter().any(|r| filter.excludes(&r.path, p))
            });
        }
        if batch.is_empty() {
            return deltas;
        }
        if self.mode == Mode::Reconcile && self.building {
            // Every event waits for the build, even outside its walk: a
            // directory renamed across the walked boundary is one record
            // whose children hang off it by parent link, so applying one
            // side early can leave the moved directory empty.
            for (path, remove) in batch {
                if self.pending_paths.len() < OVERLAY_RECORD_LIMIT {
                    self.pending_paths.insert(path, remove);
                } else {
                    widen(&mut self.reconcile, affected(&self.roots, [&path]));
                }
            }
            return deltas;
        }
        let paths: Vec<_> = batch.keys().cloned().collect();
        if self.active.records.len() + self.frozen.iter().map(|l| l.records.len()).sum::<usize>()
            >= 2 * OVERLAY_RECORD_LIMIT
            || self.active.bytes() + self.frozen.iter().map(|l| l.bytes()).sum::<usize>()
                >= 2 * OVERLAY_BYTE_LIMIT
        {
            widen(&mut self.reconcile, affected(&self.roots, &paths));
            return deltas;
        }
        let view = self.owner.view.read().unwrap().clone();
        deltas.extend(self.normalize(&view, batch));
        deltas
    }

    /// The deltas for `batch` against `view`, with the contents of new
    /// directories in it listed first ([`new_trees`]: a directory moved or
    /// unpacked in sends one event, not one per entry). What it can't list
    /// or inspect widens the pending reconcile.
    fn normalize(&mut self, view: &View, mut batch: BTreeMap<PathBuf, bool>) -> Vec<Delta> {
        let paths: Vec<_> = batch.keys().cloned().collect();
        match new_trees(view, &paths, &self.directory, NEW_TREE_LIMIT) {
            Some(found) => batch.extend(found.into_iter().map(|p| (p, false))),
            None => widen(&mut self.reconcile, affected(&self.roots, &paths)),
        }
        let (changes, degraded, revived) = normalize_batch(view, batch, &self.next);
        // Re-listing a revived directory deletes stale children under it.
        widen(&mut self.reconcile, affected(&self.roots, &revived));
        if degraded {
            widen(&mut self.reconcile, affected(&self.roots, &paths));
            self.owner.status.write().unwrap().error =
                Some("Some changes could not be inspected; reconciling".into());
        }
        changes
    }

    /// Takes a build's result: publishes it, or backs off (or restarts the
    /// daemon) on failure. Returns the deltas to commit and the view to swap
    /// in with them.
    fn finish_build(&mut self, result: Result<Built>) -> (Vec<Delta>, Option<Arc<View>>) {
        self.building = false;
        self.last_build = std::time::Instant::now();
        if self.mode == Mode::Reconcile {
            self.last_reconcile = Some((self.last_build, self.build_started.elapsed()));
        }
        match result {
            Ok(built) => self.install(built),
            Err(e) if must_restart(&e) => {
                // Every retry would hand the same request to the
                // same newer worker. The WAL already holds every
                // change, so stopping loses nothing.
                tracing::warn!(error = %e, "binary replaced; restarting filex-indexd");
                self.owner.restart.store(true, Ordering::Relaxed);
                self.owner.stop.store(true, Ordering::Relaxed);
                (Vec::new(), None)
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    retry_in_secs = self.retry_delay.as_secs(),
                    "index build incomplete"
                );
                self.owner.status.write().unwrap().error =
                    Some(format!("index build incomplete: {e}"));
                self.back_off();
                (Vec::new(), None)
            }
        }
    }

    /// Renames the built segment into place and publishes a manifest naming
    /// it, then swaps it into the view. Assumes only this process writes
    /// `manifest-*`/`segment-*` names in the directory.
    fn install(&mut self, built: Built) -> (Vec<Delta>, Option<Arc<View>>) {
        let directory = &self.directory;
        self.manifest_epoch += 1;
        while directory
            .join(format!("manifest-{:020}.json", self.manifest_epoch))
            .exists()
            || directory
                .join(format!("segment-{:020}.fx2", self.manifest_epoch))
                .exists()
            || directory
                .join(format!("manifest-{:020}.tmp", self.manifest_epoch))
                .exists()
        {
            self.manifest_epoch += 1;
        }
        let final_path = directory.join(format!("segment-{:020}.fx2", self.manifest_epoch));
        // Base first; a flush appends a delta, a delta
        // merge replaces them, a base build drops them.
        let mut next_files = match built.installs_as(self.mode) {
            Mode::Flush => self.files.clone(),
            Mode::Reconcile | Mode::Merge => vec![],
            Mode::MergeDeltas => self.files[..1].to_vec(),
        };
        next_files.push(final_path.clone());
        let name = |path: &Path| {
            path.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        };
        let m = Manifest {
            version: manifest::VERSION,
            epoch: self.manifest_epoch,
            sequence: built.sequence,
            next_id: self.next.load(Ordering::Relaxed),
            segment: name(&next_files[0]),
            roots: built.segment.roots.clone(),
            deltas: next_files[1..].iter().map(|p| name(p)).collect(),
        };
        let publication = (|| -> Result<()> {
            std::fs::rename(&built.path, &final_path)?;
            manifest::sync_dir(directory)?;
            manifest::publish(directory, &m)?;
            Ok(())
        })();
        match publication {
            Ok(()) => self.swap_in(built, final_path, next_files),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    retry_in_secs = self.retry_delay.as_secs(),
                    "index publication failed"
                );
                self.owner.status.write().unwrap().error =
                    Some(format!("index publication failed: {e}"));
                let scratch = built.path.clone();
                drop(built);
                let _ = std::fs::remove_file(scratch);
                self.back_off();
                (Vec::new(), None)
            }
        }
    }

    /// After a published build: the next view, held-back changes as deltas,
    /// and retention of what the new generation supersedes.
    fn swap_in(
        &mut self,
        built: Built,
        final_path: PathBuf,
        next_files: Vec<PathBuf>,
    ) -> (Vec<Delta>, Option<Arc<View>>) {
        let mut deltas = Vec::new();
        self.retry_delay = BUILD_RETRY_DELAY;
        tracing::info!(
            mode = ?self.mode,
            delta = built.as_flush,
            secs = self.build_started.elapsed().as_secs_f32(),
            records = built.segment.len(),
            bytes = std::fs::metadata(&final_path)
                .map_or(0, |m| m.len()),
            "index generation published"
        );
        let old = self.owner.view.read().unwrap().clone();
        let shape = built.installs_as(self.mode);
        let built_segment = Arc::new(built.segment);
        let (base, levels) = match shape {
            Mode::Flush => {
                let mut deltas = old.deltas.clone();
                deltas.push(built_segment.clone());
                (old.base.clone(), deltas)
            }
            Mode::Reconcile | Mode::Merge => (built_segment.clone(), vec![]),
            Mode::MergeDeltas => (old.base.clone(), vec![built_segment.clone()]),
        };
        self.live.retain(|(_, weak)| weak.strong_count() > 0);
        self.live.push((final_path, Arc::downgrade(&built_segment)));
        self.files = next_files;
        self.frozen.clear();
        let layers = vec![Arc::new(self.active.clone())];
        let next_view = Arc::new(View {
            base,
            deltas: levels,
            layers,
            roots: self.roots.clone(),
            epoch: self.seq,
        });
        self.counts = next_view.root_counts();
        let replacement = Some(next_view.clone());
        {
            // A completed build clears any
            // earlier error; unreadable
            // entries are reported apart.
            let mut status = self.owner.status.write().unwrap();
            status.error = None;
            status.skipped = built.skipped as u64;
        }
        if self.mode == Mode::Reconcile {
            let build_scope = &self.build_scope;
            self.unavailable.retain(|&id, _| !build_scope.walks(id));
            self.unavailable.extend(built.unavailable);
        }
        deltas.push(Delta::Roots(self.roots.clone()));
        let pending = std::mem::take(&mut self.pending_paths);
        deltas.extend(self.normalize(&next_view, pending));
        if let Ok(manifests) = manifest::candidates(&self.directory) {
            if let Some((_, oldest)) = manifests.iter().take(2).next_back()
                && let Err(e) = self.wal.checkpoint(oldest.sequence)
            {
                self.owner.status.write().unwrap().error =
                    Some(format!("WAL checkpoint failed: {e}"));
            }
            retire(&self.directory, &manifests, &self.live);
        }
        // A full build supersedes everything older on
        // disk, readable or not.
        super::recovery::remove_legacy_files(&self.directory);
        (deltas, replacement)
    }

    /// Retries a failed build as a full reconcile after a growing delay.
    fn back_off(&mut self) {
        self.reconcile = Some(Scope::All);
        self.retry_after = std::time::Instant::now() + self.retry_delay;
        self.retry_delay = next_retry_delay(self.retry_delay);
    }

    /// Makes `deltas` durable in the WAL, applies them to the overlay and
    /// publishes the result (on `replacement`'s segments, if given). Breaks
    /// when the WAL fails, which stops the daemon.
    fn commit(&mut self, deltas: Vec<Delta>, replacement: Option<Arc<View>>) -> ControlFlow<()> {
        if deltas.is_empty() {
            return ControlFlow::Continue(());
        }
        if deltas.iter().any(|d| !matches!(d, Delta::Roots(_))) {
            self.last_change = std::time::Instant::now();
        }
        let tx = Transaction {
            sequence: self.seq + 1,
            deltas,
            next_id: self.next.load(Ordering::Relaxed),
        };
        if let Err(e) = self.wal.append(&tx) {
            self.owner.status.write().unwrap().error = Some(format!("index WAL failed: {e}"));
            self.owner.stop.store(true, Ordering::Relaxed);
            return ControlFlow::Break(());
        }
        self.seq = tx.sequence;
        for delta in tx.deltas {
            apply(&mut self.active, &mut self.roots, delta);
        }
        if let Some(view) = replacement {
            let mut layers = self.frozen.clone();
            layers.push(Arc::new(self.active.clone()));
            *self.owner.view.write().unwrap() = Arc::new(View {
                base: view.base.clone(),
                deltas: view.deltas.clone(),
                layers,
                roots: self.roots.clone(),
                epoch: self.seq,
            });
        } else {
            publish(
                &self.owner,
                &self.frozen,
                &self.active,
                &self.roots,
                self.seq,
            );
        }
        ControlFlow::Continue(())
    }
}
/// What to build next, if anything (FIL-27, FIL-30).
///
/// `deltas` and `records` count entries (records plus tombstones) in the
/// delta segments and the overlays. The overlay is already searchable, so a
/// flush only bounds per-query overlay work and WAL replay; a trickle of
/// changes waits for an old overlay *and* a quiet filesystem, with a ceiling
/// for machines that never go quiet. A flush writes only the changes; the
/// base, which costs a whole-catalog rewrite, is rebuilt only once the
/// deltas grow large relative to it.
fn next_build(
    base: usize,
    deltas: &[usize],
    records: usize,
    bytes: usize,
    since_build: Duration,
    since_change: Duration,
) -> Option<Mode> {
    let flush = records >= FLUSH_RECORDS
        || bytes >= FLUSH_BYTES
        || (records > 0
            && (since_build >= MAX_FLUSH_AGE
                || (since_build >= MIN_FLUSH_AGE && since_change >= QUIET_BEFORE_FLUSH)));
    let pending = deltas.iter().sum::<usize>() + if flush { records } else { 0 };
    if pending > (base / DELTA_RATIO).max(FLUSH_RECORDS) {
        Some(Mode::Merge)
    } else if flush && deltas.len() >= MAX_DELTAS {
        Some(Mode::MergeDeltas)
    } else {
        flush.then_some(Mode::Flush)
    }
}

/// How many entries (records plus tombstones) one more delta may hold
/// without making [`next_build`] merge the base right away; `None` when
/// none fits or the deltas are already at [`MAX_DELTAS`].
pub(super) fn delta_budget(base: usize, deltas: &[usize]) -> Option<usize> {
    let room = (base / DELTA_RATIO)
        .max(FLUSH_RECORDS)
        .saturating_sub(deltas.iter().sum());
    (deltas.len() < MAX_DELTAS && room > 0).then_some(room)
}

/// Entries a delta contributes to the merge budget.
fn delta_size(delta: &Segment) -> usize {
    delta.len() + delta.tombstones.as_ref().map_or(0, Vec::len)
}

/// Delete manifests beyond the newest two, and every segment file that
/// neither of those references and no live view still maps. Windows refuses
/// to delete a mapped file; that failure is ignored and retried next time.
fn retire(
    directory: &Path,
    manifests: &[(PathBuf, Manifest)],
    live: &[(PathBuf, std::sync::Weak<Segment>)],
) {
    let keep: std::collections::HashSet<PathBuf> = manifests
        .iter()
        .take(2)
        .flat_map(|(_, m)| m.files().map(|f| directory.join(f)))
        .collect();
    for (path, _) in manifests.iter().skip(2) {
        let _ = std::fs::remove_file(path);
    }
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("segment-")
            && name.ends_with(".fx2")
            && !keep.contains(&path)
            && !live
                .iter()
                .any(|(p, weak)| *p == path && weak.strong_count() > 0)
        {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Successive segment-build failures double the wait, capped at
/// `BUILD_RETRY_LIMIT`. A persistent failure (a worker whose segment this
/// build cannot open, a full disk) otherwise re-walks every root on a fixed
/// 30s cycle forever, which costs a saturated core and publishes nothing.
/// A build failed because the worker is a newer build *and* the binary on
/// disk really changed. Both, so a version check that misfires within one
/// build backs off like any failure instead of restarting in a loop.
fn must_restart(error: &anyhow::Error) -> bool {
    error.is::<super::builder::WorkerSkew>() && super::executable::replaced_since_startup()
}

fn next_retry_delay(previous: Duration) -> Duration {
    previous.saturating_mul(2).min(BUILD_RETRY_LIMIT)
}

/// A cancelled build returns within one filesystem call, but that call can
/// stall (an unresponsive mount). Shutdown abandons the builder after a grace
/// period instead of hanging until SIGKILL. That is safe: only the writer
/// publishes, the worker exits when its stdin closes with this process, and
/// startup removes leftover scratch files.
fn join_within(thread: std::thread::JoinHandle<()>, grace: Duration) {
    let deadline = std::time::Instant::now() + grace;
    while !thread.is_finished() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    if thread.is_finished() {
        let _ = thread.join();
    } else {
        tracing::warn!("abandoning a segment build that did not stop in time");
    }
}

fn publish(owner: &Shared, frozen: &[Arc<Overlay>], active: &Overlay, roots: &[Root], epoch: u64) {
    let (base, deltas) = {
        let view = owner.view.read().unwrap();
        (view.base.clone(), view.deltas.clone())
    };
    let mut layers = frozen.to_vec();
    layers.push(Arc::new(active.clone()));
    *owner.view.write().unwrap() = Arc::new(View {
        base,
        deltas,
        layers,
        roots: roots.to_vec(),
        epoch,
    });
}
fn update_status(
    owner: &Shared,
    roots: &[Root],
    counts: &std::collections::BTreeMap<u32, u64>,
    unavailable: &std::collections::BTreeMap<u32, String>,
    epoch: u64,
    building: bool,
) {
    let mut status = owner.status.write().unwrap();
    status.epoch = epoch;
    status.building = building;
    status.roots = roots
        .iter()
        .map(|r| RootStatus {
            path: r.path.clone(),
            files: counts.get(&r.id).copied().unwrap_or(0),
            state: if let Some(why) = unavailable.get(&r.id) {
                format!("Folder unavailable: {why}")
            } else if building {
                "Building index".into()
            } else if status.error.is_some() {
                "Index needs attention".into()
            } else {
                "Ready".into()
            },
        })
        .collect();
}
impl Drop for Handle {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        let _ = self.commands.try_send(Command::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Default, PartialEq)]
    struct Builds {
        flushes: usize,
        delta_merges: usize,
        base_merges: usize,
        most_deltas: usize,
    }

    /// Simulate `hours` of one changed record every `every` over a base of
    /// `base` entries, building whenever the policy says so.
    fn builds_for_trickle(base: usize, hours: u64, every: Duration) -> Builds {
        let step = Duration::from_secs(1);
        let (mut records, mut since_build, mut since_change) = (0, Duration::ZERO, Duration::MAX);
        let mut deltas = Vec::new();
        let mut builds = Builds::default();
        for second in 0..hours * 3600 {
            if second % every.as_secs() == 0 {
                records += 1;
                since_change = Duration::ZERO;
            }
            let due = next_build(
                base,
                &deltas,
                records,
                records * 200,
                since_build,
                since_change,
            );
            match due {
                Some(Mode::Flush) => {
                    builds.flushes += 1;
                    deltas.push(records);
                }
                Some(Mode::MergeDeltas) => {
                    builds.delta_merges += 1;
                    deltas = vec![deltas.iter().sum::<usize>() + records];
                }
                Some(Mode::Merge) => {
                    builds.base_merges += 1;
                    deltas.clear();
                }
                Some(Mode::Reconcile) => unreachable!("the policy never reconciles"),
                None => {}
            }
            if due.is_some() {
                records = 0;
                since_build = Duration::ZERO;
            }
            builds.most_deltas = builds.most_deltas.max(deltas.len());
            since_build += step;
            since_change = since_change.saturating_add(step);
        }
        builds
    }

    /// FIL-27 regression, revised for FIL-30: the old "any change + 30 s"
    /// rule ran ~120 full builds an hour under a trickle. A never-quiet
    /// machine now writes a small delta every `MAX_FLUSH_AGE` and never
    /// rewrites the base over a day.
    #[test]
    fn a_steady_trickle_flushes_deltas_without_rewriting_the_base() {
        let day = builds_for_trickle(1_000_000, 24, Duration::from_secs(5));
        assert_eq!(day.base_merges, 0);
        assert_eq!(day.flushes + day.delta_merges, 47);
        assert_eq!(day.delta_merges, 11);
        assert_eq!(day.most_deltas, MAX_DELTAS);
        // 17,280 changes a day exceed 1/8 of a 100k-entry base once.
        assert_eq!(
            builds_for_trickle(100_000, 24, Duration::from_secs(5)).base_merges,
            1
        );
    }

    #[test]
    fn a_quiet_overlay_flushes_once_it_is_old_enough() {
        // Changes every 10 minutes leave quiet gaps: one flush per change
        // once the overlay passes `MIN_FLUSH_AGE`, never a base rewrite.
        let builds = builds_for_trickle(1_000_000, 2, Duration::from_secs(600));
        assert_eq!(builds.base_merges, 0);
        assert_eq!(builds.flushes + builds.delta_merges, 12);
        let quiet = QUIET_BEFORE_FLUSH;
        let base = 1_000_000;
        assert_eq!(
            next_build(base, &[], 1, 200, MIN_FLUSH_AGE, quiet),
            Some(Mode::Flush)
        );
        assert_eq!(
            next_build(base, &[], 1, 200, MIN_FLUSH_AGE, quiet / 2),
            None
        );
        assert_eq!(
            next_build(base, &[], 1, 200, MIN_FLUSH_AGE / 2, quiet),
            None
        );
        assert_eq!(
            next_build(base, &[9], 0, 0, MAX_FLUSH_AGE * 100, quiet),
            None
        );
    }

    #[test]
    fn a_large_overlay_flushes_immediately() {
        let now = Duration::ZERO;
        let base = 1_000_000;
        assert_eq!(
            next_build(base, &[], FLUSH_RECORDS, 0, now, now),
            Some(Mode::Flush)
        );
        assert_eq!(
            next_build(base, &[], 1, FLUSH_BYTES, now, now),
            Some(Mode::Flush)
        );
        assert_eq!(next_build(base, &[], FLUSH_RECORDS - 1, 0, now, now), None);
    }

    #[test]
    fn deltas_merge_among_themselves_then_into_the_base() {
        let now = Duration::ZERO;
        let full = [10; MAX_DELTAS];
        assert_eq!(
            next_build(1_000_000, &full, FLUSH_RECORDS, 0, now, now),
            Some(Mode::MergeDeltas)
        );
        assert_eq!(next_build(1_000_000, &full, 0, 0, now, now), None);
        // Past 1/8 of the base, rewrite it, flush pending or not.
        assert_eq!(
            next_build(80_000, &[10_001], 0, 0, now, now),
            Some(Mode::Merge)
        );
        assert_eq!(next_build(80_000, &[9_000], 0, 0, now, now), None);
        assert_eq!(
            next_build(80_000, &[6_000], FLUSH_RECORDS, 0, now, now),
            Some(Mode::Merge)
        );
        // A small base still gets deltas worth one flush before a rewrite.
        assert_eq!(
            next_build(10, &[], FLUSH_RECORDS, 0, now, now),
            Some(Mode::Flush)
        );
        assert_eq!(
            next_build(10, &[1], FLUSH_RECORDS, 0, now, now),
            Some(Mode::Merge)
        );
    }

    /// FIL-30: a base is shared by every manifest that adds deltas to it, so
    /// retention keeps any file the newest two manifests or a live view use.
    #[test]
    fn retention_keeps_every_referenced_or_mapped_segment() {
        let directory = tempfile::tempdir().unwrap();
        let directory = directory.path();
        let manifest = |epoch, segment: &str, deltas: &[&str]| Manifest {
            version: manifest::VERSION,
            epoch,
            sequence: epoch,
            next_id: 1,
            segment: segment.into(),
            roots: vec![],
            deltas: deltas.iter().map(|d| d.to_string()).collect(),
        };
        let manifests = [
            manifest(4, "segment-1.fx2", &["segment-3.fx2", "segment-4.fx2"]),
            manifest(3, "segment-1.fx2", &["segment-3.fx2"]),
            manifest(2, "segment-1.fx2", &["segment-2.fx2"]),
            manifest(1, "segment-0.fx2", &[]),
        ];
        let mut published = Vec::new();
        for m in &manifests {
            manifest::publish(directory, m).unwrap();
            published.push((
                directory.join(format!("manifest-{:020}.json", m.epoch)),
                m.clone(),
            ));
        }
        for n in 0..=5 {
            std::fs::write(directory.join(format!("segment-{n}.fx2")), b"").unwrap();
        }
        // A query still holds the view that mapped segment-2.
        let mapped = Arc::new(Segment::build([], vec![], 0).unwrap());
        let live = vec![
            (directory.join("segment-2.fx2"), Arc::downgrade(&mapped)),
            (directory.join("segment-5.fx2"), std::sync::Weak::new()),
        ];
        retire(directory, &published, &live);
        let mut left: Vec<_> = std::fs::read_dir(directory)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                "manifest-00000000000000000003.json",
                "manifest-00000000000000000004.json",
                "segment-1.fx2",
                "segment-2.fx2",
                "segment-3.fx2",
                "segment-4.fx2",
            ]
        );
    }

    /// Reconcile storm: an overflow right after a reconcile (or during
    /// one) waits `MIN_OVERFLOW_GAP`, or twice a slow build's time.
    #[test]
    fn overflow_reconciles_only_after_the_gap() {
        let ended = Instant::now();
        assert!(overflow_due(ended, None), "the first overflow runs at once");
        let quick = Some((ended, Duration::from_secs(5)));
        assert!(!overflow_due(ended, quick));
        assert!(!overflow_due(ended + MIN_OVERFLOW_GAP / 2, quick));
        assert!(overflow_due(ended + MIN_OVERFLOW_GAP, quick));
        let slow = Some((ended, Duration::from_secs(45)));
        assert!(!overflow_due(ended + MIN_OVERFLOW_GAP, slow));
        assert!(!overflow_due(ended + Duration::from_secs(89), slow));
        assert!(overflow_due(ended + Duration::from_secs(90), slow));
    }

    #[test]
    fn overflow_scopes_merge_up_to_every_root() {
        let mut pending = None;
        widen(&mut pending, Scope::Roots(Default::default()));
        assert_eq!(pending, None, "an empty scope started a reconcile");
        widen(&mut pending, Scope::roots([1]));
        widen(&mut pending, Scope::roots([2]));
        assert_eq!(pending, Some(Scope::roots([1, 2])));
        widen(&mut pending, Scope::All);
        widen(&mut pending, Scope::roots([3]));
        assert_eq!(pending, Some(Scope::All));
    }

    /// A batch the writer drops or can't inspect, or an overflow, repairs
    /// only the subtrees it names; a root, or a path above one (FSEvents
    /// can name `/`), takes the whole root; a path under no root (its root
    /// since removed) repairs nothing.
    #[test]
    fn dropped_batches_reconcile_their_subtrees() {
        let root = |id, path: &str| Root {
            id,
            path: path.into(),
            device: 0,
        };
        let roots = [root(1, "/a"), root(2, "/b"), root(3, "/c")];
        let paths = |ps: &[&str]| ps.iter().map(PathBuf::from).collect::<Vec<_>>();
        let dirs = |ps: &[&str]| Subtrees::Dirs(ps.iter().map(PathBuf::from).collect());
        assert_eq!(
            affected(&roots, &paths(&["/a/x", "/b/y/z", "/a/w", "/a/x/deeper"])),
            Scope::Roots([(1, dirs(&["/a/w", "/a/x"])), (2, dirs(&["/b/y/z"]))].into())
        );
        assert_eq!(
            affected(&roots, &paths(&["/a/x", "/elsewhere"])),
            Scope::Roots([(1, dirs(&["/a/x"]))].into())
        );
        assert_eq!(
            affected(&roots, &paths(&["/a/x", "/a"])),
            Scope::roots([1]),
            "the root itself absorbs its subtrees"
        );
        assert_eq!(affected(&roots, &paths(&["/"])), Scope::roots([1, 2, 3]));
    }

    fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !done() {
            assert!(Instant::now() < deadline, "timed out: {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// End to end through real builds (needs the `filex-indexd` worker
    /// binary, which `cargo test`/`cargo build` produce): an overflow
    /// reconciles only its root; a batch too big to inspect does too, at
    /// once; and an overflow waiting out the gap reads as building.
    #[test]
    fn overflows_and_dropped_batches_reconcile_only_their_roots() {
        let data = tempfile::tempdir().unwrap();
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (a, b) = (
            a.path().canonicalize().unwrap(),
            b.path().canonicalize().unwrap(),
        );
        std::fs::write(a.join("a1"), b"x").unwrap();
        std::fs::write(b.join("b1"), b"x").unwrap();
        let handle = start(data.path(), vec![a.clone(), b.clone()]).unwrap();
        let shared = &handle.shared;
        let has = |p: &Path| shared.view.read().unwrap().resolve(p).is_some();
        let building = || shared.status.read().unwrap().building;

        // No reconcile yet, so the overflow is due at once.
        shared.overflowed(&[a.join("a1")]);
        wait_for("overflowed root walked", || has(&a.join("a1")));
        assert!(!has(&b.join("b1")), "the other root was walked");

        handle.commands.send(Command::Reconcile).unwrap();
        wait_for("full reconcile", || has(&b.join("b1")) && !building());

        // More new entries than one batch lists: reconcile A, not B.
        std::fs::create_dir(a.join("big")).unwrap();
        for i in 0..=NEW_TREE_LIMIT {
            std::fs::write(a.join("big").join(i.to_string()), b"").unwrap();
        }
        std::fs::write(b.join("b2"), b"x").unwrap();
        handle
            .commands
            .send(Command::Paths {
                paths: vec![a.join("big")],
                remove: false,
            })
            .unwrap();
        wait_for("new tree reconciled", || {
            has(&a.join("big").join("0")) && !building()
        });
        assert!(!has(&b.join("b2")), "a dropped batch re-walked every root");

        // Within the gap the overflow waits, and status says so.
        shared.overflowed(&[b.join("b2")]);
        wait_for("overflow pending in status", building);
        std::thread::sleep(Duration::from_millis(300));
        assert!(building() && !has(&b.join("b2")));
    }

    /// UI hints for paths outside every root (a big copy to an unindexed
    /// drive) are dropped: no new-tree walk, no reconcile. A path above a
    /// root still counts as touching it.
    #[test]
    fn hints_outside_every_root_are_dropped() {
        let data = tempfile::tempdir().unwrap();
        let (a, out) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (a, out) = (
            a.path().canonicalize().unwrap(),
            out.path().canonicalize().unwrap(),
        );
        let roots = [Root {
            id: 1,
            path: a.clone(),
            device: 0,
        }];
        assert!(touching(&roots, a.parent().unwrap()).next().is_some());
        assert!(touching(&roots, &out).next().is_none());

        std::fs::write(a.join("a1"), b"x").unwrap();
        let handle = start(data.path(), vec![a.clone()]).unwrap();
        let shared = &handle.shared;
        let has = |p: &Path| shared.view.read().unwrap().resolve(p).is_some();
        let building = || shared.status.read().unwrap().building;
        handle.commands.send(Command::Reconcile).unwrap();
        wait_for("first walk", || has(&a.join("a1")) && !building());

        // Only a reconcile of the root would find this unhinted file.
        std::fs::write(a.join("late"), b"x").unwrap();
        std::fs::create_dir(out.join("copy")).unwrap();
        for i in 0..=NEW_TREE_LIMIT {
            std::fs::write(out.join("copy").join(i.to_string()), b"").unwrap();
        }
        let send = |paths| {
            (handle.commands)
                .send(Command::Paths {
                    paths,
                    remove: false,
                })
                .unwrap()
        };
        send(vec![out.join("copy")]);
        std::fs::write(a.join("hinted"), b"x").unwrap();
        send(vec![a.join("hinted")]);
        wait_for("hint applied", || has(&a.join("hinted")) && !building());
        assert!(
            !has(&a.join("late")),
            "an outside batch reconciled the root"
        );
        assert!(!has(&out.join("copy")), "an outside batch was indexed");
    }

    /// Event batches use the filter the daemon holds in memory (reloaded
    /// on a settings change), not one read from disk per batch.
    #[test]
    fn event_batches_use_the_shared_filter() {
        let data = tempfile::tempdir().unwrap();
        let tree = tempfile::tempdir().unwrap();
        let root = tree.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("skipme")).unwrap();
        std::fs::write(root.join("skipme/hidden.txt"), b"x").unwrap();
        std::fs::write(root.join("kept.txt"), b"x").unwrap();
        let handle = start(data.path(), vec![root.clone()]).unwrap();
        *handle.shared.filter.write().unwrap() =
            ingest::IndexFilter::from_settings(&crate::settings::Settings {
                exclude_dev_folders: true,
                excluded_folder_names: vec!["skipme".into()],
                ..Default::default()
            });
        handle
            .commands
            .send(Command::Paths {
                paths: vec![root.join("skipme/hidden.txt"), root.join("kept.txt")],
                remove: false,
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let view = loop {
            let view = handle.shared.view.read().unwrap().clone();
            if view.resolve(&root.join("kept.txt")).is_some() {
                break view;
            }
            assert!(Instant::now() < deadline, "kept.txt never indexed");
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(view.resolve(&root.join("skipme/hidden.txt")), None);
    }

    #[test]
    fn skew_alone_does_not_restart_the_daemon() {
        // Without a replaced binary a skew report is a bug in this build:
        // back off and retry rather than exit, restart and walk in a loop.
        assert!(!must_restart(&anyhow::Error::new(
            super::super::builder::WorkerSkew
        )));
        assert!(!must_restart(&anyhow::anyhow!("disk full")));
    }

    #[test]
    fn retry_delay_doubles_then_holds_at_the_cap() {
        let mut delay = BUILD_RETRY_DELAY;
        let mut seen = vec![delay];
        for _ in 0..16 {
            delay = next_retry_delay(delay);
            seen.push(delay);
        }
        assert_eq!(
            seen[..5],
            [
                Duration::from_secs(30),
                Duration::from_secs(60),
                Duration::from_secs(120),
                Duration::from_secs(240),
                Duration::from_secs(480),
            ]
        );
        assert!(seen.iter().all(|delay| *delay <= BUILD_RETRY_LIMIT));
        assert_eq!(seen.last(), Some(&BUILD_RETRY_LIMIT));
    }

    /// A writer over a fresh catalog in `data`, with no builder or writer
    /// thread: requests it sends wait in the returned receiver.
    fn writer(data: &Path, roots: &[PathBuf]) -> (Writer, Receiver<BuildRequest>) {
        let (writer, _commands, build_rx) = open(data, roots.to_vec()).unwrap();
        (writer, build_rx)
    }

    fn id_of(writer: &Writer, path: &Path) -> u32 {
        writer.roots.iter().find(|r| r.path == path).unwrap().id
    }

    /// During a reconcile, a batch is held back (minus paths outside every
    /// root or in the index directory) until the hold is full; past that it
    /// widens the reconcile to its roots only.
    #[test]
    fn a_reconciling_writer_holds_batches_then_widens_to_their_roots() {
        let data = tempfile::tempdir().unwrap();
        let (a, b, out) = (
            tempfile::tempdir().unwrap(),
            tempfile::tempdir().unwrap(),
            tempfile::tempdir().unwrap(),
        );
        let [a, b, out] = [&a, &b, &out].map(|d| d.path().canonicalize().unwrap());
        let (mut writer, _build_rx) = writer(data.path(), &[a.clone(), b.clone()]);
        writer.mode = Mode::Reconcile;
        writer.building = true;
        let inside_data = writer.directory.join("x");

        let batch = [
            (a.join("x"), false),
            (out.join("y"), false),
            (inside_data, true),
        ];
        assert!(writer.apply_batch(batch.into()).is_empty());
        assert_eq!(
            writer.pending_paths.keys().collect::<Vec<_>>(),
            [&a.join("x")]
        );
        assert_eq!(writer.reconcile, None);

        for i in writer.pending_paths.len()..OVERLAY_RECORD_LIMIT {
            writer.pending_paths.insert(a.join(i.to_string()), false);
        }
        assert!(writer.apply_batch([(b.join("y"), false)].into()).is_empty());
        assert_eq!(
            writer.reconcile,
            Some(Scope::Roots(
                [(id_of(&writer, &b), Subtrees::Dirs([b.join("y")].into()))].into()
            ))
        );
        assert!(!writer.pending_paths.contains_key(&b.join("y")));
    }

    /// Starts a reconcile of `scope` and returns its request, unbuilt.
    fn start_reconcile(
        writer: &mut Writer,
        build_rx: &Receiver<BuildRequest>,
        scope: Scope,
    ) -> BuildRequest {
        writer.reconcile = Some(scope);
        writer.retry_after = Instant::now();
        writer.maybe_start_build();
        build_rx.try_recv().unwrap()
    }

    /// Builds `request` (needs the `filex-indexd` worker binary) and hands
    /// the writer its result, as the builder thread would.
    fn finish(writer: &mut Writer, request: BuildRequest) {
        let built = builder::build(request, &AtomicBool::new(false));
        assert!(built.is_ok(), "{:?}", built.err());
        assert!(writer.handle(Command::Built(Box::new(built))).is_continue());
        assert!(!writer.building);
    }

    fn event(writer: &mut Writer, path: PathBuf) {
        let command = Command::Paths {
            paths: vec![path],
            remove: false,
        };
        assert!(writer.handle(command).is_continue());
    }

    fn named(writer: &Writer, name: &str) -> usize {
        let view = writer.owner.view.read().unwrap().clone();
        view.records().filter(|r| r.name == name.as_bytes()).count()
    }

    /// A writer over `root` (with `sub/` in it), walked once.
    fn walked_writer(data: &Path, root: &Path) -> (Writer, Receiver<BuildRequest>) {
        std::fs::create_dir(root.join("sub")).unwrap();
        let (mut writer, build_rx) = writer(data, &[root.to_path_buf()]);
        let request = start_reconcile(&mut writer, &build_rx, Scope::All);
        finish(&mut writer, request);
        (writer, build_rx)
    }

    /// A directory that comes back by native identity after its deletion
    /// reached the overlay takes back its hidden ID, so the children older
    /// levels hold under that ID resurface, gone or not: the event widens
    /// the reconcile to that directory, which drops them.
    #[test]
    fn a_revived_directory_reconciles_its_stale_children() {
        let data = tempfile::tempdir().unwrap();
        let tree = tempfile::tempdir().unwrap();
        let (root, away) = (
            tree.path().canonicalize().unwrap(),
            tree.path().join("away"),
        );
        let root = root.join("root");
        std::fs::create_dir_all(root.join("d")).unwrap();
        std::fs::create_dir(&away).unwrap();
        std::fs::write(root.join("d/stale.txt"), b"x").unwrap();
        if ingest::inspect(&root.join("d"), 0, 0, 0)
            .unwrap()
            .identity
            .birth
            == 0
        {
            return; // No birth time here: identities are never reused.
        }
        let (mut writer, build_rx) = writer(data.path(), std::slice::from_ref(&root));
        let request = start_reconcile(&mut writer, &build_rx, Scope::All);
        finish(&mut writer, request);
        let before = writer.owner.view.read().unwrap().resolve(&root.join("d"));

        std::fs::rename(root.join("d"), away.join("d")).unwrap();
        event(&mut writer, root.join("d"));
        std::fs::remove_file(away.join("d/stale.txt")).unwrap();
        std::fs::write(away.join("d/fresh.txt"), b"x").unwrap();
        std::fs::rename(away.join("d"), root.join("d")).unwrap();
        event(&mut writer, root.join("d"));
        let view = writer.owner.view.read().unwrap().clone();
        assert_eq!(view.resolve(&root.join("d")), before, "ID taken back");
        let id = writer.roots[0].id;
        let scope = Scope::Roots([(id, Subtrees::Dirs([root.join("d")].into()))].into());
        assert_eq!(writer.reconcile, Some(scope.clone()), "no reconcile");

        let request = start_reconcile(&mut writer, &build_rx, scope);
        finish(&mut writer, request);
        let view = writer.owner.view.read().unwrap().clone();
        assert!(view.resolve(&root.join("d/fresh.txt")).is_some());
        assert_eq!(view.resolve(&root.join("d/stale.txt")), None);
    }

    /// Regression: a plain directory rename reported in one batch (`a`
    /// sorts first and is deleted, then `b` takes its ID back) was taken
    /// for a revived hidden directory and queued a subtree reconcile,
    /// holding every live event back for the walk.
    #[test]
    fn a_renamed_directory_is_not_a_revival() {
        let data = tempfile::tempdir().unwrap();
        let tree = tempfile::tempdir().unwrap();
        let root = tree.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("sub/a")).unwrap();
        std::fs::write(root.join("sub/a/kept.txt"), b"x").unwrap();
        if ingest::inspect(&root.join("sub/a"), 0, 0, 0)
            .unwrap()
            .identity
            .birth
            == 0
        {
            return; // No birth time here: identities are never reused.
        }
        let (mut writer, build_rx) = writer(data.path(), std::slice::from_ref(&root));
        let request = start_reconcile(&mut writer, &build_rx, Scope::All);
        finish(&mut writer, request);
        let id = writer
            .owner
            .view
            .read()
            .unwrap()
            .resolve(&root.join("sub/a"));

        std::fs::rename(root.join("sub/a"), root.join("sub/b")).unwrap();
        let command = Command::Paths {
            paths: vec![root.join("sub/a"), root.join("sub/b")],
            remove: false,
        };
        assert!(writer.handle(command).is_continue());

        assert_eq!(writer.reconcile, None, "a rename queued a reconcile");
        let view = writer.owner.view.read().unwrap().clone();
        assert_eq!(
            view.resolve(&root.join("sub/b")),
            id,
            "the ID moved with it"
        );
        assert!(view.resolve(&root.join("sub/b/kept.txt")).is_some());
        assert_eq!(view.resolve(&root.join("sub/a")), None);
    }

    /// B1 regression: a new directory whose event waited out a reconcile
    /// has its contents listed when the event is replayed, as it would
    /// live, rather than becoming a bare directory record.
    #[test]
    fn held_back_new_directories_are_listed_after_the_build() {
        let data = tempfile::tempdir().unwrap();
        let tree = tempfile::tempdir().unwrap();
        let root = tree.path().canonicalize().unwrap();
        let (mut writer, build_rx) = walked_writer(data.path(), &root);
        let id = writer.roots[0].id;
        let scope = Scope::Roots([(id, Subtrees::Dirs([root.join("sub")].into()))].into());
        let request = start_reconcile(&mut writer, &build_rx, scope);
        let built = builder::build(request, &AtomicBool::new(false));
        // Unpacked after the walk listed `sub`: only the event knows.
        std::fs::create_dir_all(root.join("sub/unzipped/inner")).unwrap();
        std::fs::write(root.join("sub/unzipped/inner/f.txt"), b"x").unwrap();
        event(&mut writer, root.join("sub/unzipped"));
        assert!(
            writer
                .pending_paths
                .contains_key(&root.join("sub/unzipped"))
        );
        assert!(writer.handle(Command::Built(Box::new(built))).is_continue());
        let view = writer.owner.view.read().unwrap().clone();
        assert!(view.resolve(&root.join("sub/unzipped")).is_some());
        assert!(
            view.resolve(&root.join("sub/unzipped/inner/f.txt"))
                .is_some(),
            "a replayed new directory stayed empty"
        );
    }

    /// Regression: a directory moved out of the walked subtree while the
    /// build runs kept its record but lost its children when the outside
    /// half of the rename was applied before the build (the build drops
    /// everything under the subtree and no longer finds it there).
    #[test]
    fn a_directory_moved_out_of_the_walked_subtree_keeps_its_children() {
        let data = tempfile::tempdir().unwrap();
        let tree = tempfile::tempdir().unwrap();
        let root = tree.path().canonicalize().unwrap();
        let (mut writer, build_rx) = walked_writer(data.path(), &root);
        std::fs::create_dir_all(root.join("sub/x")).unwrap();
        std::fs::write(root.join("sub/x/child.txt"), b"x").unwrap();
        std::fs::create_dir(root.join("out")).unwrap();
        event(&mut writer, root.join("sub/x"));
        event(&mut writer, root.join("out"));
        let resolves = |writer: &Writer, path: &Path| {
            writer.owner.view.read().unwrap().resolve(path).is_some()
        };
        assert!(resolves(&writer, &root.join("sub/x/child.txt")));

        let id = writer.roots[0].id;
        let scope = Scope::Roots([(id, Subtrees::Dirs([root.join("sub")].into()))].into());
        let request = start_reconcile(&mut writer, &build_rx, scope);
        std::fs::rename(root.join("sub/x"), root.join("out/x")).unwrap();
        event(&mut writer, root.join("sub/x"));
        event(&mut writer, root.join("out/x"));
        finish(&mut writer, request);

        assert!(resolves(&writer, &root.join("out/x")));
        assert!(
            resolves(&writer, &root.join("out/x/child.txt")),
            "the moved directory lost its children"
        );
        assert!(!resolves(&writer, &root.join("sub/x")));
        assert_eq!(named(&writer, "child.txt"), 1);
    }

    /// Every visible record of `writer`'s view by path, for comparing views.
    fn by_path(writer: &Writer) -> BTreeMap<PathBuf, crate::catalog::segment::Record> {
        let view = writer.owner.view.read().unwrap().clone();
        view.records()
            .map(|r| (view.path(r.id).unwrap(), r))
            .collect()
    }

    /// Search hits for `text`, by path.
    fn hits(writer: &Writer, text: &str) -> Vec<PathBuf> {
        let view = writer.owner.view.read().unwrap().clone();
        let query = super::super::ipc::Query {
            text: text.into(),
            fuzzy: false,
            ..Default::default()
        };
        let page = super::super::query::search(
            &view,
            &query,
            &AtomicBool::new(false),
            &Default::default(),
        )
        .unwrap();
        page.hits.into_iter().map(|h| h.path).collect()
    }

    /// R9: a reconcile of one subtree writes a delta over the published
    /// base, not a new base: inside it, adds appear, deletes (and a deleted
    /// directory's descendants) go, unchanged entries keep their IDs;
    /// outside it nothing is read, and an overlay change frozen at build
    /// start survives as a flush would keep it. A restart serves the same.
    #[test]
    fn a_subtree_reconcile_appends_a_delta_and_keeps_the_base() {
        let data = tempfile::tempdir().unwrap();
        let tree = tempfile::tempdir().unwrap();
        let root = tree.path().canonicalize().unwrap();
        for dir in ["sub/deep/b", "other"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        for file in [
            "sub/keep.txt",
            "sub/gone.txt",
            "sub/deep/a.txt",
            "sub/deep/b/c.txt",
            "other/outside.txt",
        ] {
            std::fs::write(root.join(file), b"x").unwrap();
        }
        let (mut writer, build_rx) = writer(data.path(), std::slice::from_ref(&root));
        let request = start_reconcile(&mut writer, &build_rx, Scope::All);
        finish(&mut writer, request);
        let base = writer.files.clone();
        assert_eq!(base.len(), 1);
        let base_bytes = std::fs::read(&base[0]).unwrap();
        let before = by_path(&writer);

        // Applied live, so it sits in the overlay the build freezes.
        std::fs::write(root.join("other/evented.txt"), b"x").unwrap();
        event(&mut writer, root.join("other/evented.txt"));
        std::fs::write(root.join("sub/new.txt"), b"x").unwrap();
        std::fs::remove_file(root.join("sub/gone.txt")).unwrap();
        std::fs::remove_dir_all(root.join("sub/deep")).unwrap();
        // Outside the subtree and never reported: stays unseen.
        std::fs::write(root.join("other/unseen.txt"), b"x").unwrap();
        let id = writer.roots[0].id;
        let scope = Scope::Roots([(id, Subtrees::Dirs([root.join("sub")].into()))].into());
        let request = start_reconcile(&mut writer, &build_rx, scope);
        assert!(request.delta_budget.is_some());
        finish(&mut writer, request);

        assert_eq!(writer.files.len(), 2, "no delta appended");
        assert_eq!(writer.files[0], base[0], "the base was rewritten");
        assert_eq!(std::fs::read(&base[0]).unwrap(), base_bytes);
        let (_, newest) = manifest::candidates(&writer.directory).unwrap().remove(0);
        assert_eq!(
            newest.segment,
            base[0].file_name().unwrap().to_string_lossy()
        );
        assert_eq!(newest.deltas.len(), 1);
        let view = writer.owner.view.read().unwrap().clone();
        assert_eq!(view.deltas.len(), 1);
        let tombstones = view.deltas[0].tombstones.clone().unwrap();
        // The deleted directory and its descendants: hidden only behind it,
        // they would still cost every search until a merge.
        let gone = |p: &str| before[&root.join(p)].id;
        assert_eq!(tombstones, {
            let mut t = [
                "sub/gone.txt",
                "sub/deep",
                "sub/deep/a.txt",
                "sub/deep/b",
                "sub/deep/b/c.txt",
            ]
            .map(gone)
            .to_vec();
            t.sort_unstable();
            t
        });

        let after = by_path(&writer);
        for kept in ["", "sub", "sub/keep.txt", "other", "other/outside.txt"] {
            let path = root.join(kept);
            assert_eq!(after[&path].id, before[&path].id, "{path:?} changed ID");
        }
        let mut paths: Vec<_> = after
            .keys()
            .map(|p| p.strip_prefix(&root).unwrap())
            .collect();
        paths.sort();
        assert_eq!(
            paths,
            [
                "",
                "other",
                "other/evented.txt",
                "other/outside.txt",
                "sub",
                "sub/keep.txt",
                "sub/new.txt",
            ]
            .map(Path::new)
        );
        assert_eq!(hits(&writer, "new.txt"), [root.join("sub/new.txt")]);
        assert_eq!(hits(&writer, "evented"), [root.join("other/evented.txt")]);
        assert_eq!(hits(&writer, "outside"), [root.join("other/outside.txt")]);
        for text in ["gone", "a.txt", "c.txt", "deep", "unseen"] {
            assert!(hits(&writer, text).is_empty(), "{text}");
        }
        assert_eq!(writer.counts[&id], 6);

        let directory = writer.directory.clone();
        drop(writer);
        let (reopened, _build_rx) = self::writer(&directory, std::slice::from_ref(&root));
        assert_eq!(reopened.files.len(), 2);
        assert_eq!(by_path(&reopened), after);
    }

    /// R9 size guard: a subtree reconcile too big for a delta under the
    /// merge policy builds a base, within the same request.
    #[test]
    fn an_oversized_subtree_reconcile_builds_a_base() {
        let data = tempfile::tempdir().unwrap();
        let tree = tempfile::tempdir().unwrap();
        let root = tree.path().canonicalize().unwrap();
        let (mut writer, build_rx) = walked_writer(data.path(), &root);
        for i in 0..10 {
            std::fs::write(root.join(format!("sub/{i}.txt")), b"x").unwrap();
        }
        let id = writer.roots[0].id;
        let scope = Scope::Roots([(id, Subtrees::Dirs([root.join("sub")].into()))].into());
        let mut request = start_reconcile(&mut writer, &build_rx, scope);
        request.delta_budget = Some(5);
        finish(&mut writer, request);
        assert_eq!(writer.files.len(), 1, "an oversized subtree wrote a delta");
        assert_eq!(named(&writer, "9.txt"), 1);
    }

    #[test]
    fn a_delta_fits_only_under_the_merge_policy() {
        assert_eq!(delta_budget(1_000_000, &[]), Some(125_000));
        assert_eq!(delta_budget(1_000_000, &[100_000]), Some(25_000));
        assert_eq!(delta_budget(1_000_000, &[125_000]), None);
        assert_eq!(delta_budget(10, &[]), Some(FLUSH_RECORDS));
        assert_eq!(delta_budget(10, &[1; MAX_DELTAS]), None);
    }

    /// A reconcile whose paths all resolve outside every root (say, a
    /// symlink into an unindexed folder) walks nothing, so it starts no
    /// build rather than rewriting the base unchanged.
    #[test]
    fn a_reconcile_that_resolves_to_nothing_starts_no_build() {
        let data = tempfile::tempdir().unwrap();
        let tree = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let root = tree.path().canonicalize().unwrap();
        let (mut writer, build_rx) = walked_writer(data.path(), &root);
        let id = writer.roots[0].id;
        let outside = elsewhere.path().canonicalize().unwrap();
        writer.reconcile = Some(Scope::Roots(
            [(id, Subtrees::Dirs([outside].into()))].into(),
        ));
        writer.retry_after = Instant::now();
        writer.maybe_start_build();
        assert!(
            build_rx.try_recv().is_err(),
            "an empty scope started a build"
        );
        assert!(!writer.building);
        assert!(writer.reconcile.is_none());
    }

    /// A full reconcile still holds every event back.
    #[test]
    fn a_full_reconcile_holds_back_everything() {
        let data = tempfile::tempdir().unwrap();
        let tree = tempfile::tempdir().unwrap();
        let root = tree.path().canonicalize().unwrap();
        let (mut writer, build_rx) = walked_writer(data.path(), &root);
        let request = start_reconcile(&mut writer, &build_rx, Scope::All);
        std::fs::write(root.join("new.txt"), b"x").unwrap();
        event(&mut writer, root.join("new.txt"));
        assert!(writer.pending_paths.contains_key(&root.join("new.txt")));
        finish(&mut writer, request);
        assert_eq!(named(&writer, "new.txt"), 1);
    }

    /// Outside a reconcile a batch commits straight to the WAL and the view.
    #[test]
    fn a_batch_commits_to_the_wal_and_the_view() {
        let data = tempfile::tempdir().unwrap();
        let tree = tempfile::tempdir().unwrap();
        let root = tree.path().canonicalize().unwrap();
        std::fs::write(root.join("new.txt"), b"x").unwrap();
        let (mut writer, _build_rx) = writer(data.path(), std::slice::from_ref(&root));
        let seq = writer.seq;
        let deltas = writer.apply_batch([(root.join("new.txt"), false)].into());
        assert!(!deltas.is_empty());
        assert!(writer.commit(deltas, None).is_continue());
        assert_eq!(writer.seq, seq + 1);
        let view = writer.owner.view.read().unwrap().clone();
        assert_eq!(view.epoch, seq + 1);
        assert!(view.resolve(&root.join("new.txt")).is_some());
    }

    /// A due reconcile waits out a retry delay, then hands the builder its
    /// scope; nothing more starts while it runs.
    #[test]
    fn a_due_reconcile_sends_its_scope_once_the_retry_delay_passes() {
        let data = tempfile::tempdir().unwrap();
        let (mut writer, build_rx) = writer(data.path(), &[]);
        writer.reconcile = Some(Scope::roots([7]));
        writer.retry_after = Instant::now() + Duration::from_secs(3600);
        writer.maybe_start_build();
        assert!(!writer.building && build_rx.try_recv().is_err());

        writer.retry_after = Instant::now();
        writer.maybe_start_build();
        let request = build_rx.try_recv().unwrap();
        assert!(request.mode == Mode::Reconcile);
        assert_eq!(request.scope, Scope::roots([7]));
        assert!(writer.building && writer.reconcile.is_none());
        assert_eq!(writer.build_scope, Scope::roots([7]));

        writer.reconcile = Some(Scope::All);
        writer.maybe_start_build();
        assert!(build_rx.try_recv().is_err(), "a second build started");
    }

    /// A failed build reconciles everything later, with a doubled delay.
    #[test]
    fn a_failed_build_backs_off_into_a_full_reconcile() {
        let data = tempfile::tempdir().unwrap();
        let (mut writer, _build_rx) = writer(data.path(), &[]);
        writer.mode = Mode::Reconcile;
        writer.building = true;
        let before = Instant::now();
        let (deltas, replacement) = writer.finish_build(Err(anyhow::anyhow!("disk full")));
        assert!(deltas.is_empty() && replacement.is_none());
        assert!(!writer.building && writer.last_reconcile.is_some());
        assert_eq!(writer.reconcile, Some(Scope::All));
        assert!(writer.retry_after >= before + BUILD_RETRY_DELAY);
        assert_eq!(writer.retry_delay, next_retry_delay(BUILD_RETRY_DELAY));
        let error = writer.owner.status.read().unwrap().error.clone();
        assert!(error.unwrap().contains("index build incomplete"));
    }
}

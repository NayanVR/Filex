//! Single catalog writer. WAL durability precedes visible epochs. Frozen views
//! feed one build coordinator while notifications populate a fresh overlay.
use super::{
    builder::{self, BuildRequest, Built, Mode},
    changes::{apply, new_trees, normalize_batch},
    ipc::{RootStatus, Status},
    recovery,
    view::{Overlay, View},
};
use crate::{
    catalog::{
        manifest::{self, Manifest},
        segment::{Root, Segment},
        wal::{Delta, Transaction},
    },
    ingest,
};
use anyhow::Result;
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, SyncSender},
    },
    time::Duration,
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
    pub overflow: AtomicBool,
    pub coverage_error: RwLock<Option<String>>,
}
pub struct Handle {
    pub shared: Arc<Shared>,
    pub commands: SyncSender<Command>,
    pub thread: Option<std::thread::JoinHandle<()>>,
}
pub(crate) fn start(directory: &Path, configured: Vec<PathBuf>) -> Result<Handle> {
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
        mut manifest_epoch,
        active,
        files,
    } = recovery::recover(directory, configured)?;
    // Segment files mapped by some view; retention never deletes these.
    let mut live: Vec<(PathBuf, std::sync::Weak<Segment>)> = files
        .iter()
        .cloned()
        .zip(std::iter::once(&base).chain(&deltas).map(Arc::downgrade))
        .collect();
    let mut files = files;
    let initial = Arc::new(View {
        base,
        deltas,
        layers: vec![Arc::new(active.clone())],
        roots: roots.clone(),
        epoch: seq,
    });
    // Per-root entry counts for status, refreshed when a build publishes.
    let mut counts = initial.root_counts();
    let shared = Arc::new(Shared {
        view: RwLock::new(initial),
        status: RwLock::new(Status {
            epoch: seq,
            roots: Vec::new(),
            building: !roots.is_empty(),
            error: None,
        }),
        stop: Arc::new(AtomicBool::new(false)),
        overflow: AtomicBool::new(false),
        coverage_error: RwLock::new(None),
    });
    let (commands, rx) = mpsc::sync_channel(COMMAND_CAPACITY);
    let (build_tx, build_rx) = mpsc::sync_channel::<BuildRequest>(1);
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
    let directory = directory.canonicalize()?;
    let owner = shared.clone();
    let next = Arc::new(AtomicU64::new(
        next_id.max(roots.iter().map(|r| u64::from(r.id) + 1).max().unwrap_or(1)),
    ));
    let thread = std::thread::Builder::new()
        .name("filex-catalog-writer".into())
        .spawn(move || {
            let mut wal = wal;
            let mut active = active;
            let mut frozen = Vec::<Arc<Overlay>>::new();
            let mut roots = roots;
            let mut seq = seq;
            let mut building = false;
            let mut mode = Mode::Merge;
            let mut pending_paths = std::collections::BTreeMap::<PathBuf, bool>::new();
            let mut reconcile = false;
            let mut awaiting_watch = true;
            let mut deferred = VecDeque::new();
            let mut last_build = std::time::Instant::now();
            let mut last_change = last_build;
            let mut build_started = last_build;
            let mut retry_after = last_build;
            let mut retry_delay = BUILD_RETRY_DELAY;
            loop {
                if owner.stop.load(Ordering::Relaxed) {
                    break;
                }
                if owner.overflow.swap(false, Ordering::Relaxed) {
                    reconcile = true;
                }
                let due = if reconcile {
                    Some(Mode::Reconcile)
                } else {
                    let view = owner.view.read().unwrap().clone();
                    next_build(
                        view.base.len(),
                        &view
                            .deltas
                            .iter()
                            .map(|d| delta_size(d))
                            .collect::<Vec<_>>(),
                        active.records.len()
                            + frozen.iter().map(|l| l.records.len()).sum::<usize>(),
                        active.bytes() + frozen.iter().map(|l| l.bytes()).sum::<usize>(),
                        last_build.elapsed(),
                        last_change.elapsed(),
                    )
                };
                // Deltas need a published base under them.
                let due = due.map(|due| match due {
                    Mode::Flush | Mode::MergeDeltas if files.is_empty() => Mode::Merge,
                    due => due,
                });
                if !building
                    && std::time::Instant::now() >= retry_after
                    && let Some(due) = due
                {
                    if !active.records.is_empty() {
                        frozen.push(Arc::new(std::mem::take(&mut active)));
                    }
                    publish(&owner, &frozen, &active, &roots, seq);
                    let view = owner.view.read().unwrap().clone();
                    mode = due;
                    if build_tx
                        .send(BuildRequest {
                            view,
                            mode,
                            directory: directory.clone(),
                            next: next.clone(),
                        })
                        .is_ok()
                    {
                        building = true;
                        build_started = std::time::Instant::now();
                        reconcile = false;
                    }
                }
                update_status(
                    &owner,
                    &roots,
                    &counts,
                    seq,
                    building || reconcile || awaiting_watch,
                );
                let command = match deferred
                    .pop_front()
                    .map(Ok)
                    .unwrap_or_else(|| rx.recv_timeout(WRITER_POLL_INTERVAL))
                {
                    Ok(c) => c,
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(_) => break,
                };
                let mut deltas = Vec::new();
                let mut replacement = None;
                match command {
                    Command::Stop => break,
                    Command::Reconcile => {
                        awaiting_watch = false;
                        reconcile = true;
                    }
                    Command::AddRoot(path) => {
                        let paths: Vec<_> = roots.iter().map(|r| r.path.clone()).collect();
                        match ingest::validate_new_root(&paths, &path) {
                            Ok(path) => {
                                let allocated = next.fetch_add(1, Ordering::Relaxed);
                                if allocated > u32::MAX as u64 {
                                    owner.status.write().unwrap().error =
                                        Some("root identifier space exhausted".into());
                                    continue;
                                }
                                let id = allocated as u32;
                                let device = std::fs::symlink_metadata(&path)
                                    .map(|m| ingest::identity(&path, &m).device)
                                    .unwrap_or(0);
                                roots.push(Root { id, path, device });
                                deltas.push(Delta::Roots(roots.clone()));
                                reconcile = true;
                            }
                            Err(e) => owner.status.write().unwrap().error = Some(e.to_string()),
                        }
                    }
                    Command::RemoveRoot(path) => {
                        roots.retain(|r| r.path != path);
                        deltas.push(Delta::Roots(roots.clone()));
                        reconcile = true;
                    }
                    Command::Paths { paths, remove } => {
                        let include_system = ingest::include_system_files();
                        let mut batch: std::collections::BTreeMap<PathBuf, bool> =
                            paths.into_iter().map(|p| (p, remove)).collect();
                        while batch.len() < EVENT_BATCH_LIMIT {
                            match rx.try_recv() {
                                Ok(Command::Paths { paths, remove }) => {
                                    for p in paths {
                                        batch.insert(p, remove);
                                    }
                                }
                                Ok(other) => {
                                    deferred.push_back(other);
                                    break;
                                }
                                Err(_) => break,
                            }
                        }
                        batch.retain(|p, _| {
                            !p.starts_with(&directory)
                                && (include_system
                                    || !roots.iter().any(|r| ingest::excluded_system(&r.path, p)))
                        });
                        let paths: Vec<_> = batch.keys().cloned().collect();
                        if mode == Mode::Reconcile && building {
                            for (path, remove) in batch {
                                if pending_paths.len() < OVERLAY_RECORD_LIMIT {
                                    pending_paths.insert(path, remove);
                                } else {
                                    reconcile = true;
                                }
                            }
                            continue;
                        }
                        if active.records.len()
                            + frozen.iter().map(|l| l.records.len()).sum::<usize>()
                            >= 2 * OVERLAY_RECORD_LIMIT
                            || active.bytes() + frozen.iter().map(|l| l.bytes()).sum::<usize>()
                                >= 2 * OVERLAY_BYTE_LIMIT
                        {
                            reconcile = true;
                            continue;
                        }
                        let view = owner.view.read().unwrap().clone();
                        match new_trees(&view, &paths, &directory, NEW_TREE_LIMIT) {
                            Some(found) => batch.extend(found.into_iter().map(|p| (p, false))),
                            None => reconcile = true,
                        }
                        let (changes, degraded) = normalize_batch(&view, batch, &next);
                        deltas.extend(changes);
                        if degraded {
                            reconcile = true;
                            owner.status.write().unwrap().error =
                                Some("Some changes could not be inspected; reconciling".into());
                        }
                    }
                    Command::Built(result) => {
                        building = false;
                        last_build = std::time::Instant::now();
                        match *result {
                            Ok(built) => {
                                manifest_epoch += 1;
                                while directory
                                    .join(format!("manifest-{manifest_epoch:020}.json"))
                                    .exists()
                                    || directory
                                        .join(format!("segment-{manifest_epoch:020}.fx2"))
                                        .exists()
                                    || directory
                                        .join(format!("manifest-{manifest_epoch:020}.tmp"))
                                        .exists()
                                {
                                    manifest_epoch += 1;
                                }
                                let final_path =
                                    directory.join(format!("segment-{manifest_epoch:020}.fx2"));
                                // Base first; a flush appends a delta, a delta
                                // merge replaces them, a base build drops them.
                                let mut next_files = match mode {
                                    Mode::Reconcile | Mode::Merge => vec![],
                                    Mode::Flush => files.clone(),
                                    Mode::MergeDeltas => files[..1].to_vec(),
                                };
                                next_files.push(final_path.clone());
                                let name = |path: &Path| {
                                    path.file_name()
                                        .map(|n| n.to_string_lossy().into_owned())
                                        .unwrap_or_default()
                                };
                                let m = Manifest {
                                    version: manifest::VERSION,
                                    epoch: manifest_epoch,
                                    sequence: built.sequence,
                                    next_id: next.load(Ordering::Relaxed),
                                    segment: name(&next_files[0]),
                                    roots: built.segment.roots.clone(),
                                    deltas: next_files[1..].iter().map(|p| name(p)).collect(),
                                };
                                let publication = (|| -> Result<()> {
                                    std::fs::rename(&built.path, &final_path)?;
                                    manifest::sync_dir(&directory)?;
                                    manifest::publish(&directory, &m)?;
                                    Ok(())
                                })();
                                match publication {
                                    Ok(()) => {
                                        retry_delay = BUILD_RETRY_DELAY;
                                        tracing::info!(
                                            mode = ?mode,
                                            secs = build_started.elapsed().as_secs_f32(),
                                            records = built.segment.len(),
                                            bytes = std::fs::metadata(&final_path)
                                                .map_or(0, |m| m.len()),
                                            "index generation published"
                                        );
                                        let old = owner.view.read().unwrap().clone();
                                        let built_segment = Arc::new(built.segment);
                                        let (base, levels) = match mode {
                                            Mode::Reconcile | Mode::Merge => {
                                                (built_segment.clone(), vec![])
                                            }
                                            Mode::Flush => {
                                                let mut deltas = old.deltas.clone();
                                                deltas.push(built_segment.clone());
                                                (old.base.clone(), deltas)
                                            }
                                            Mode::MergeDeltas => {
                                                (old.base.clone(), vec![built_segment.clone()])
                                            }
                                        };
                                        live.retain(|(_, weak)| weak.strong_count() > 0);
                                        live.push((final_path, Arc::downgrade(&built_segment)));
                                        files = next_files;
                                        frozen.clear();
                                        let layers = vec![Arc::new(active.clone())];
                                        let next_view = Arc::new(View {
                                            base,
                                            deltas: levels,
                                            layers,
                                            roots: roots.clone(),
                                            epoch: seq,
                                        });
                                        counts = next_view.root_counts();
                                        replacement = Some(next_view.clone());
                                        owner.status.write().unwrap().error = if built.skipped > 0 {
                                            Some(format!(
                                                "{} inaccessible entries/subtrees were omitted",
                                                built.skipped
                                            ))
                                        } else {
                                            None
                                        };
                                        deltas.push(Delta::Roots(roots.clone()));
                                        let current = next_view;
                                        let (changes, degraded) = normalize_batch(
                                            &current,
                                            std::mem::take(&mut pending_paths),
                                            &next,
                                        );
                                        deltas.extend(changes);
                                        reconcile |= degraded;
                                        if let Ok(manifests) = manifest::candidates(&directory) {
                                            if let Some((_, oldest)) =
                                                manifests.iter().take(2).next_back()
                                                && let Err(e) = wal.checkpoint(oldest.sequence)
                                            {
                                                owner.status.write().unwrap().error =
                                                    Some(format!("WAL checkpoint failed: {e}"));
                                            }
                                            retire(&directory, &manifests, &live);
                                        }
                                        // First successful v2 activation is the only migration cleanup point.
                                        if built.skipped == 0
                                            && let Some(parent) = directory.parent()
                                        {
                                            let old_dir = parent.join("index");
                                            if let Ok(entries) = std::fs::read_dir(old_dir) {
                                                for entry in entries.flatten() {
                                                    if entry
                                                        .path()
                                                        .extension()
                                                        .is_some_and(|e| e == "fxidx")
                                                    {
                                                        let _ = std::fs::remove_file(entry.path());
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            error = %e,
                                            retry_in_secs = retry_delay.as_secs(),
                                            "index publication failed"
                                        );
                                        owner.status.write().unwrap().error =
                                            Some(format!("index publication failed: {e}"));
                                        let scratch = built.path.clone();
                                        drop(built);
                                        let _ = std::fs::remove_file(scratch);
                                        reconcile = true;
                                        retry_after = std::time::Instant::now() + retry_delay;
                                        retry_delay = next_retry_delay(retry_delay);
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::warn!(
                                    error = %e,
                                    retry_in_secs = retry_delay.as_secs(),
                                    "index build incomplete"
                                );
                                owner.status.write().unwrap().error =
                                    Some(format!("index build incomplete: {e}"));
                                reconcile = true;
                                retry_after = std::time::Instant::now() + retry_delay;
                                retry_delay = next_retry_delay(retry_delay);
                            }
                        }
                    }
                }
                if !deltas.is_empty() {
                    if deltas.iter().any(|d| !matches!(d, Delta::Roots(_))) {
                        last_change = std::time::Instant::now();
                    }
                    let tx = Transaction {
                        sequence: seq + 1,
                        deltas,
                        next_id: next.load(Ordering::Relaxed),
                    };
                    if let Err(e) = wal.append(&tx) {
                        owner.status.write().unwrap().error =
                            Some(format!("index WAL failed: {e}"));
                        owner.stop.store(true, Ordering::Relaxed);
                        break;
                    }
                    seq = tx.sequence;
                    for delta in tx.deltas {
                        apply(&mut active, &mut roots, delta);
                    }
                    if let Some(view) = replacement {
                        let mut layers = frozen.clone();
                        layers.push(Arc::new(active.clone()));
                        *owner.view.write().unwrap() = Arc::new(View {
                            base: view.base.clone(),
                            deltas: view.deltas.clone(),
                            layers,
                            roots: roots.clone(),
                            epoch: seq,
                        });
                    } else {
                        publish(&owner, &frozen, &active, &roots, seq);
                    }
                }
            }
            owner.stop.store(true, Ordering::Relaxed);
            drop(build_tx);
            drop(rx);
            join_within(builder, BUILDER_SHUTDOWN_GRACE);
        })?;
    Ok(Handle {
        shared,
        commands,
        thread: Some(thread),
    })
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
            state: if building {
                "Building index"
            } else if status.error.is_some() {
                "Index needs attention"
            } else {
                "Ready"
            }
            .into(),
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
}

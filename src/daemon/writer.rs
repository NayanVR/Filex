//! Single catalog writer. WAL durability precedes visible epochs. Frozen views
//! feed one build coordinator while notifications populate a fresh overlay.
use super::{
    builder::{self, BuildRequest, Built},
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

const COMMAND_CAPACITY: usize = 256;
const EVENT_BATCH_LIMIT: usize = 4096;
const OVERLAY_RECORD_LIMIT: usize = 25_000;
const OVERLAY_BYTE_LIMIT: usize = 16 * 1024 * 1024;
/// Entries listed under newly seen directories per batch before falling back
/// to a full reconcile.
const NEW_TREE_LIMIT: usize = 10_000;
/// Below the overlay limits, compact only once the overlay is this old and the
/// filesystem has been quiet for `QUIET_BEFORE_COMPACTION` (FIL-27).
const IDLE_COMPACTION_AGE: Duration = Duration::from_secs(30 * 60);
const QUIET_BEFORE_COMPACTION: Duration = Duration::from_secs(2 * 60);
/// Ceiling for a never-quiet machine; bounds WAL growth and replay time.
const MAX_COMPACTION_AGE: Duration = Duration::from_secs(6 * 60 * 60);
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
        sequence: seq,
        next_id,
        mut manifest_epoch,
        active,
        current_segment: selected_path,
    } = recovery::recover(directory, configured)?;
    let initial = Arc::new(View {
        base,
        layers: vec![Arc::new(active.clone())],
        roots: roots.clone(),
        epoch: seq,
    });
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
            let mut reconciling = false;
            let mut pending_paths = std::collections::BTreeMap::<PathBuf, bool>::new();
            let mut reconcile = false;
            let mut awaiting_watch = true;
            let mut deferred = VecDeque::new();
            let mut last_build = std::time::Instant::now();
            let mut last_change = last_build;
            let mut build_started = last_build;
            let mut retry_after = last_build;
            let mut retry_delay = BUILD_RETRY_DELAY;
            let mut retired = VecDeque::<(PathBuf, std::sync::Weak<Segment>, u64)>::new();
            let mut current_segment = selected_path;
            loop {
                if owner.stop.load(Ordering::Relaxed) {
                    break;
                }
                if owner.overflow.swap(false, Ordering::Relaxed) {
                    reconcile = true;
                }
                if !building
                    && std::time::Instant::now() >= retry_after
                    && (reconcile
                        || compaction_due(
                            active.records.len(),
                            active.bytes(),
                            last_build.elapsed(),
                            last_change.elapsed(),
                        ))
                {
                    if !active.records.is_empty() {
                        frozen.push(Arc::new(std::mem::take(&mut active)));
                    }
                    publish(&owner, &frozen, &active, &roots, seq);
                    let view = owner.view.read().unwrap().clone();
                    reconciling = reconcile;
                    if build_tx
                        .send(BuildRequest {
                            view,
                            reconcile,
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
                update_status(&owner, &roots, seq, building || reconcile || awaiting_watch);
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
                        if reconciling && building {
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
                                let name = format!("segment-{manifest_epoch:020}.fx2");
                                let final_path = directory.join(&name);
                                let m = Manifest {
                                    version: 2,
                                    epoch: manifest_epoch,
                                    sequence: built.sequence,
                                    next_id: next.load(Ordering::Relaxed),
                                    segment: name,
                                    roots: built.segment.roots.clone(),
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
                                            reconcile = reconciling,
                                            secs = build_started.elapsed().as_secs_f32(),
                                            records = built.segment.len(),
                                            bytes = std::fs::metadata(&final_path)
                                                .map_or(0, |m| m.len()),
                                            "index generation published"
                                        );
                                        let old = owner.view.read().unwrap().base.clone();
                                        if let Some((path, epoch)) =
                                            current_segment.replace((final_path, manifest_epoch))
                                        {
                                            retired.push_back((path, Arc::downgrade(&old), epoch));
                                        }
                                        let base = Arc::new(built.segment);
                                        frozen.clear();
                                        let layers = vec![Arc::new(active.clone())];
                                        let next_view = Arc::new(View {
                                            base,
                                            layers,
                                            roots: roots.clone(),
                                            epoch: seq,
                                        });
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
                                            // Retain two recoverable generations. Startup leftovers are
                                            // safe to reap when no view in this process references them.
                                            for (path, old) in manifests.iter().skip(2) {
                                                if !retired.iter().any(|(p, _, _)| {
                                                    *p == directory.join(&old.segment)
                                                }) {
                                                    let _ = std::fs::remove_file(
                                                        directory.join(&old.segment),
                                                    );
                                                    let _ = std::fs::remove_file(path);
                                                }
                                            }
                                            if let Some((_, oldest)) =
                                                manifests.iter().take(2).next_back()
                                                && let Err(e) = wal.checkpoint(oldest.sequence)
                                            {
                                                owner.status.write().unwrap().error =
                                                    Some(format!("WAL checkpoint failed: {e}"));
                                            }
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
                                        let retained = manifest::candidates(&directory)
                                            .unwrap_or_default()
                                            .into_iter()
                                            .take(2)
                                            .map(|(_, m)| directory.join(m.segment))
                                            .collect::<Vec<_>>();
                                        while retired.front().is_some_and(|(path, weak, _)| {
                                            weak.upgrade().is_none() && !retained.contains(path)
                                        }) {
                                            let (path, _, epoch) = retired.pop_front().unwrap();
                                            let _ = std::fs::remove_file(path);
                                            let _ = std::fs::remove_file(
                                                directory
                                                    .join(format!("manifest-{epoch:020}.json")),
                                            );
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
/// Whether the live overlay should be frozen into a new base (FIL-27).
///
/// The overlay is already searchable, so compaction only bounds overlay size,
/// per-query overlay work and WAL replay. A full build costs a worker process
/// and a whole new segment file, so a trickle of changes waits for an old
/// overlay *and* a quiet filesystem, with a hard ceiling for machines that
/// never go quiet. The previous rule ("any change, 30 s since the last build")
/// rebuilt back to back on every real machine.
fn compaction_due(
    records: usize,
    bytes: usize,
    since_build: Duration,
    since_change: Duration,
) -> bool {
    records >= OVERLAY_RECORD_LIMIT
        || bytes >= OVERLAY_BYTE_LIMIT
        || (records > 0
            && (since_build >= MAX_COMPACTION_AGE
                || (since_build >= IDLE_COMPACTION_AGE && since_change >= QUIET_BEFORE_COMPACTION)))
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
    let base = owner.view.read().unwrap().base.clone();
    let mut layers = frozen.to_vec();
    layers.push(Arc::new(active.clone()));
    *owner.view.write().unwrap() = Arc::new(View {
        base,
        layers,
        roots: roots.to_vec(),
        epoch,
    });
}
fn update_status(owner: &Shared, roots: &[Root], epoch: u64, building: bool) {
    let view = owner.view.read().unwrap();
    let mut status = owner.status.write().unwrap();
    status.epoch = epoch;
    status.building = building;
    status.roots = roots
        .iter()
        .map(|r| RootStatus {
            path: r.path.clone(),
            files: view.base.root_count(r.id),
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

    /// Simulate `hours` of one changed record every `every`, compacting
    /// whenever the policy says so; return how many builds ran.
    fn builds_for_trickle(hours: u64, every: Duration) -> usize {
        let step = Duration::from_secs(1);
        let (mut records, mut since_build, mut since_change) = (0, Duration::ZERO, Duration::MAX);
        let mut builds = 0;
        for second in 0..hours * 3600 {
            if second % every.as_secs() == 0 {
                records += 1;
                since_change = Duration::ZERO;
            }
            if compaction_due(records, records * 200, since_build, since_change) {
                builds += 1;
                records = 0;
                since_build = Duration::ZERO;
            }
            since_build += step;
            since_change = since_change.saturating_add(step);
        }
        builds
    }

    /// FIL-27 regression: the old "any change + 30 s" rule ran ~120 full
    /// builds an hour under a trickle. A never-quiet machine now builds once
    /// per `MAX_COMPACTION_AGE` (6 h, 12 h and 18 h in a day).
    #[test]
    fn a_steady_trickle_does_not_rebuild_back_to_back() {
        assert_eq!(builds_for_trickle(1, Duration::from_secs(5)), 0);
        assert_eq!(builds_for_trickle(1, Duration::from_secs(60)), 0);
        assert_eq!(builds_for_trickle(24, Duration::from_secs(5)), 3);
    }

    #[test]
    fn a_quiet_old_overlay_compacts_once() {
        // Changes every 10 minutes leave quiet gaps: one build roughly every
        // half hour (32 minutes, waiting for quiet), not one per change.
        assert_eq!(builds_for_trickle(2, Duration::from_secs(600)), 3);
        let quiet = QUIET_BEFORE_COMPACTION;
        assert!(compaction_due(1, 200, IDLE_COMPACTION_AGE, quiet));
        assert!(!compaction_due(1, 200, IDLE_COMPACTION_AGE, quiet / 2));
        assert!(!compaction_due(0, 0, MAX_COMPACTION_AGE, quiet));
    }

    #[test]
    fn a_large_overlay_compacts_immediately() {
        let now = Duration::ZERO;
        assert!(compaction_due(OVERLAY_RECORD_LIMIT, 0, now, now));
        assert!(compaction_due(1, OVERLAY_BYTE_LIMIT, now, now));
        assert!(!compaction_due(OVERLAY_RECORD_LIMIT - 1, 0, now, now));
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

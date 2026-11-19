//! Prepare immutable generations in a disposable process.
//!
//! The owner assigns file IDs and spools records. The worker sorts and builds
//! the segment, then exits, returning all construction allocations to the OS.
//! Only the catalog writer can publish the completed file in a manifest.
use super::view::View;
use crate::{
    catalog::{
        segment::{Identity, Record, Root, Segment, raw_name},
        storage::SEGMENT_FORMAT,
    },
    ingest,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs::File,
    io::{BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// Version 3 adds delta tombstones; an older worker must reject it rather
/// than silently build a delta without them.
const REQUEST_VERSION: u32 = 3;
const RECORD_HEADER_BYTES: usize = 66;
const MAX_RECORD_BYTES: u64 = 1024 * 1024;
/// The header carries a delta's tombstones: ~21 bytes each in JSON.
const MAX_HEADER_BYTES: u64 = 64 * 1024 * 1024;
const MAX_ERROR_BYTES: u64 = 16 * 1024;
const WORKER_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Exit status of a segment worker handed a request from a different build
/// of `filex-indexd` — the binary on disk was replaced under a running
/// daemon. Distinct from failure (1) so the daemon restarts instead of
/// retrying a build that can never succeed (FIL-21).
pub const WORKER_SKEW_EXIT: i32 = 3;

/// The daemon and its segment worker are different builds.
#[derive(Debug)]
pub struct WorkerSkew;
impl std::fmt::Display for WorkerSkew {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("segment worker is a different build of filex-indexd")
    }
}
impl std::error::Error for WorkerSkew {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Mode {
    /// Walk every root into a new base.
    Reconcile,
    /// Merge every level of the view into a new base.
    Merge,
    /// Write the overlays as one new delta on top of the existing ones.
    Flush,
    /// Merge every delta and overlay into one delta.
    MergeDeltas,
}

/// What a reconcile walks again: every root, or some roots, each whole or
/// only under some of its directories. The rest keep their records exactly
/// as the view holds them, so one watcher overflow doesn't re-walk every
/// root, or a whole root when it named a directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Scope {
    All,
    /// By root ID; an ID no longer in the view walks nothing.
    Roots(BTreeMap<u32, Subtrees>),
}

/// The part of one root a reconcile walks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Subtrees {
    Whole,
    /// Only under these directories, none inside another. The builder
    /// widens one the view or the filesystem no longer holds as a directory
    /// to its nearest ancestor that is.
    Dirs(BTreeSet<PathBuf>),
}

/// More subtrees than this in one root reconcile it whole: each costs a
/// view lookup and a listing, and a burst that names this many is better
/// served by one walk.
const MAX_SUBTREES: usize = 64;

impl Scope {
    /// Whole roots.
    #[cfg(test)]
    pub fn roots(ids: impl IntoIterator<Item = u32>) -> Scope {
        Scope::Roots(ids.into_iter().map(|id| (id, Subtrees::Whole)).collect())
    }

    /// The scope covering both.
    pub fn merge(self, other: Scope) -> Scope {
        match (self, other) {
            (Scope::Roots(mut a), Scope::Roots(b)) => {
                for (id, part) in b {
                    let merged = match a.remove(&id) {
                        Some(held) => held.merge(part),
                        None => part,
                    };
                    a.insert(id, merged);
                }
                Scope::Roots(a)
            }
            _ => Scope::All,
        }
    }

    /// Whether this scope walks any of `root`.
    pub(super) fn walks(&self, root: u32) -> bool {
        self.part(root).is_some()
    }

    /// The part of `root` this scope walks.
    fn part(&self, root: u32) -> Option<&Subtrees> {
        match self {
            Scope::All => Some(&Subtrees::Whole),
            Scope::Roots(parts) => parts.get(&root),
        }
    }
}

impl Subtrees {
    /// The part covering both: directories union, an ancestor absorbing
    /// its descendants, up to [`MAX_SUBTREES`].
    fn merge(self, other: Subtrees) -> Subtrees {
        match (self, other) {
            (Subtrees::Dirs(mut a), Subtrees::Dirs(b)) => {
                for dir in b {
                    if !a.iter().any(|held| dir.starts_with(held)) {
                        a.retain(|held| !held.starts_with(&dir));
                        a.insert(dir);
                    }
                    if a.len() > MAX_SUBTREES {
                        return Subtrees::Whole;
                    }
                }
                Subtrees::Dirs(a)
            }
            _ => Subtrees::Whole,
        }
    }
}

pub(super) struct BuildRequest {
    pub view: Arc<View>,
    pub mode: Mode,
    /// Which roots a [`Mode::Reconcile`] walks; ignored by other modes.
    pub scope: Scope,
    /// The writer's filter as of this request, so walked roots use the same
    /// settings as event batches.
    pub filter: ingest::IndexFilter,
    pub directory: PathBuf,
    pub next: Arc<AtomicU64>,
    pub progress: Arc<super::progress::WalkProgress>,
    /// A [`Mode::Reconcile`] of subtrees only may come back as a delta of
    /// at most this many entries (records plus tombstones) instead of a
    /// new base; `None` always builds a base.
    pub delta_budget: Option<usize>,
}

pub(super) struct Built {
    pub(super) segment: Segment,
    pub(super) path: PathBuf,
    pub(super) sequence: u64,
    pub(super) skipped: usize,
    /// Roots a reconcile meant to walk but couldn't reach, with why; their
    /// records were kept as the view held them.
    pub(super) unavailable: Vec<(u32, String)>,
    /// A reconcile that came back as a delta: installed like a flush.
    pub(super) as_flush: bool,
}

impl Built {
    /// How the writer installs this output of a `mode` build.
    pub(super) fn installs_as(&self, mode: Mode) -> Mode {
        if self.as_flush { Mode::Flush } else { mode }
    }
}

#[derive(Serialize, Deserialize)]
struct Header {
    version: u32,
    sequence: u64,
    roots: Vec<Root>,
    tombstones: Option<Vec<u64>>,
    /// [`SEGMENT_FORMAT`] the daemon can open; empty from builds before it
    /// was sent, which fails the check as it should.
    #[serde(default)]
    segment_format: String,
}

/// Scratch files belong to a build until the writer accepts its output.
struct ScratchFiles(Vec<PathBuf>);
impl Drop for ScratchFiles {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Always reap a worker, including cancellation and parent-side I/O failures.
struct Worker(Child);
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub(super) fn build(request: BuildRequest, stop: &AtomicBool) -> Result<Built> {
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let output = request.directory.join(format!("build-{stamp}.tmp"));
    let input = request.directory.join(format!("build-{stamp}-input.tmp"));
    let errors = request.directory.join(format!("build-{stamp}-errors.tmp"));
    let mut scratch = ScratchFiles(vec![input.clone(), output.clone(), errors.clone()]);
    let file = File::options().write(true).create_new(true).open(&input)?;
    let mut spool = BufWriter::new(file);
    // A walk reports progress until this build ends, however it ends.
    let walking = (request.mode == Mode::Reconcile).then(|| {
        request.progress.start();
        super::progress::Finish(&request.progress)
    });
    let (mut skipped, mut unavailable) = (0, vec![]);
    let delta = match request.mode {
        Mode::Flush | Mode::MergeDeltas => Some(delta_changes(
            &request.view,
            request.mode == Mode::MergeDeltas,
        )),
        Mode::Reconcile => subtree_delta(&request, stop)?.map(|rescan| {
            (skipped, unavailable) = (rescan.skipped, rescan.unavailable);
            rescan.changes
        }),
        Mode::Merge => None,
    };
    let as_flush = request.mode == Mode::Reconcile && delta.is_some();
    write_line(
        &mut spool,
        &Header {
            version: REQUEST_VERSION,
            sequence: request.view.epoch,
            roots: request.view.roots.clone(),
            tombstones: delta.as_ref().map(|(_, deleted)| deleted.clone()),
            segment_format: segment_format(),
        },
    )?;

    match (request.mode, delta) {
        (_, Some((records, _))) => {
            for record in &records {
                ensure!(!stop.load(Ordering::Relaxed), "segment build cancelled");
                write_record(&mut spool, record)?;
            }
        }
        (Mode::Reconcile, None) => (skipped, unavailable) = enumerate(&request, &mut spool, stop)?,
        _ => {
            for record in request.view.records() {
                ensure!(!stop.load(Ordering::Relaxed), "segment build cancelled");
                write_record(&mut spool, &record)?;
            }
        }
    }
    spool.flush()?;
    drop(spool);
    ensure!(!stop.load(Ordering::Relaxed), "segment build cancelled");

    if walking.is_some() {
        request.progress.indexing();
    }
    run_process(&input, &output, &errors, stop)?;
    // SAFETY: the worker has exited. This output is immutable, and the writer
    // keeps the generation alive until all query views release their mappings.
    let segment = unsafe { Segment::open(&output) }.context("loading the completed segment")?;
    ensure!(
        segment.sequence == request.view.epoch && segment.roots == request.view.roots,
        "segment worker returned a different generation"
    );
    scratch.0.retain(|path| path != &output);
    Ok(Built {
        segment,
        path: output,
        sequence: request.view.epoch,
        skipped,
        unavailable,
        as_flush,
    })
}

/// The records and sorted tombstones of a new delta, newest version winning.
///
/// A flush takes only the overlays; `merge_deltas` also folds in every
/// existing delta, which the result then replaces. A tombstone is kept only
/// while an older surviving level still holds the ID.
// ponytail: materializes the merged changes in memory; bounded by the delta
// budget (1/8 of the base), stream a k-way merge if that budget grows.
pub(super) fn delta_changes(view: &View, merge_deltas: bool) -> (Vec<Record>, Vec<u64>) {
    let older: &[Arc<Segment>] = if merge_deltas { &[] } else { &view.deltas };
    split_changes(view, older, pending_changes(view, merge_deltas))
}

/// Every change [`delta_changes`] takes, by ID, newest winning; `None`
/// deletes.
fn pending_changes(view: &View, merge_deltas: bool) -> BTreeMap<u64, Option<Record>> {
    let mut changes = BTreeMap::<u64, Option<Record>>::new();
    if merge_deltas {
        for delta in &view.deltas {
            for slot in 0..delta.len() {
                changes.insert(delta.id(slot), Some(delta.record(slot)));
            }
            for &id in delta.tombstones.iter().flatten() {
                changes.insert(id, None);
            }
        }
    }
    for layer in &view.layers {
        changes.extend(layer.records.iter().map(|(&id, r)| (id, r.clone())));
    }
    changes
}

/// A delta's records and sorted tombstones from `changes`: a deletion
/// becomes a tombstone only while the base or an `older` delta (one the
/// new delta sits on) still holds the ID.
fn split_changes(
    view: &View,
    older: &[Arc<Segment>],
    changes: BTreeMap<u64, Option<Record>>,
) -> (Vec<Record>, Vec<u64>) {
    let mut records = Vec::new();
    let mut tombstones = Vec::new();
    for (id, record) in changes {
        match record {
            Some(record) => records.push(record),
            None if view.base.slot(id).is_some() || older.iter().any(|d| d.slot(id).is_some()) => {
                tombstones.push(id)
            }
            None => {}
        }
    }
    (records, tombstones)
}

/// Walks what `request.scope` names into `spool` and copies every other
/// record from the view unchanged. Copied first, so a walk never hands out
/// an ID a kept record holds.
///
/// A root whose own `lstat` fails (unmounted volume, deleted or renamed
/// folder, permission lost), or that can't be listed, is kept too and
/// returned as unavailable, so one missing root can't fail every reconcile. Assumes a root that `lstat`s
/// here still does when the walk reaches it; one that vanishes in between
/// fails the build as before. Errors inside a reachable root are the walk's.
///
/// A root scoped to subtrees (already [`resolve_scope`]d) keeps every
/// record except those strictly under a subtree directory, then walks each
/// subtree's contents under the directory's kept ID.
// ponytail: `skipped` counts only the walked roots, so status under-reports
// unreadable entries after a scoped reconcile; keep per-root counts if that
// ever matters.
fn enumerate(
    request: &BuildRequest,
    spool: &mut impl Write,
    stop: &AtomicBool,
) -> Result<(usize, Vec<(u32, String)>)> {
    let started = std::time::Instant::now();
    let view = &request.view;
    let mut allocated = HashSet::new();
    let (plans, unavailable) = plan(request);

    if plans.values().any(Option::is_some) || plans.len() < view.roots.len() {
        let mut under: HashMap<u32, Under> = (plans.iter())
            .filter_map(|(&id, plan)| Some((id, Under::new(plan.as_ref()?.iter().map(|s| s.1)))))
            .collect();
        let parent_of = |id| view.record_projected(id, false, false).map(|r| r.parent);
        // A subtree directory keeps its ID but is re-read: its listing changed.
        let starts: HashMap<u64, &PathBuf> = (plans.values().flatten().flatten())
            .map(|(dir, id)| (*id, dir))
            .collect();
        for mut record in view.records() {
            ensure!(!stop.load(Ordering::Relaxed), "segment build cancelled");
            let keep = match (plans.get(&record.root), under.get_mut(&record.root)) {
                (None, _) => true,
                (Some(_), Some(under)) => !under.holds(record.parent, parent_of),
                (Some(_), None) => false,
            };
            if let Some(dir) = starts.get(&record.id)
                && let Ok(fresh) = ingest::inspect(dir, record.id, record.parent, record.root)
            {
                record = fresh;
            }
            if keep && allocated.insert(record.id) {
                write_record(spool, &record)?;
            }
        }
    }

    let mut records = 0;
    let skipped = walk_plans(
        request,
        &plans,
        |id| allocated.insert(id),
        |record| {
            records += 1;
            write_record(spool, &record)
        },
        stop,
    )?;
    // The walk alone, so a slow rebuild shows whether the filesystem or the
    // segment worker is to blame.
    tracing::info!(
        secs = started.elapsed().as_secs_f32(),
        records,
        skipped,
        "roots enumerated"
    );
    Ok((skipped, unavailable))
}

/// Each walked root's start directories (with IDs), `None` for the whole root.
type Plans = BTreeMap<u32, Option<Vec<(PathBuf, u64)>>>;

/// What each root `request` walks: the whole root, or subtree directories
/// with their IDs. Roots not in it are kept as the view holds them; those
/// the scope named but that can't be reached come back as unavailable,
/// with why.
fn plan(request: &BuildRequest) -> (Plans, Vec<(u32, String)>) {
    let view = &request.view;
    let mut unavailable = Vec::new();
    let mut plans = Plans::new();
    for root in &view.roots {
        let Some(part) = request.scope.part(root.id) else {
            continue;
        };
        // A root that stats but can't be listed (macOS Full Disk Access
        // revoked) would walk as empty and drop every record: keep them too.
        if let Err(e) =
            std::fs::symlink_metadata(&root.path).and_then(|_| std::fs::read_dir(&root.path))
        {
            tracing::warn!(root = %root.path.display(), error = %e, "root unavailable; keeping its records");
            unavailable.push((root.id, e.to_string()));
            continue;
        }
        let plan = match part {
            Subtrees::Whole => None,
            // Resolved by the writer ([`resolve_scope`]) against this same
            // view, so each resolves; walk exactly these, never wider.
            Subtrees::Dirs(dirs) => dirs
                .iter()
                .map(|dir| Some((dir.clone(), view.resolve(dir)?)))
                .collect(),
        };
        plans.insert(root.id, plan);
    }
    tracing::info!(
        roots = plans.len(),
        whole = plans.values().filter(|p| p.is_none()).count(),
        subtrees = plans.values().flatten().map(Vec::len).sum::<usize>(),
        "reconcile scope"
    );
    (plans, unavailable)
}

/// Walks every root and subtree in `plans`, emitting what it finds. An
/// entry keeps the ID of the view's record for it (same parent and name,
/// or a moved one by native identity) when `claim` grants that ID, else
/// takes a new one; `claim` must refuse an ID already emitted or kept.
/// Returns the entries it couldn't read.
fn walk_plans(
    request: &BuildRequest,
    plans: &Plans,
    mut claim: impl FnMut(u64) -> bool,
    mut emit: impl FnMut(Record) -> Result<()>,
    stop: &AtomicBool,
) -> Result<usize> {
    let view = &request.view;
    let mut allocate = |root: u32, path: &Path, parent, identity: &Identity| {
        let existing = unchanged_id(view, path, parent, identity).or_else(|| {
            view.find_native(root, *identity)
                .filter(|&id| identity.birth != 0 && view.path(id).is_none_or(|old| !old.exists()))
        });
        existing
            .filter(|&id| claim(id))
            .unwrap_or_else(|| request.next.fetch_add(1, Ordering::Relaxed))
    };
    let mut emit = |record: Record| {
        request.progress.note(&record.name);
        emit(record)
    };
    let mut skipped = 0;
    for root in view.roots.iter() {
        match plans.get(&root.id) {
            None => {}
            Some(None) => {
                skipped += ingest::walk(
                    root,
                    &request.directory,
                    &request.filter,
                    |path, parent, identity| allocate(root.id, path, parent, identity),
                    &mut emit,
                    stop,
                )?;
            }
            Some(Some(starts)) => {
                for (dir, id) in starts {
                    skipped += ingest::walk_under(
                        root,
                        dir,
                        *id,
                        &request.directory,
                        &request.filter,
                        |path, parent, identity| allocate(root.id, path, parent, identity),
                        &mut emit,
                        stop,
                    )?;
                }
            }
        }
    }
    Ok(skipped)
}

/// What a subtree-only reconcile found, as a delta over its view.
struct Rescan {
    changes: (Vec<Record>, Vec<u64>),
    skipped: usize,
    unavailable: Vec<(u32, String)>,
}

/// A reconcile of subtrees only (no whole root), as a delta over the view
/// rather than a new base: the overlays' changes as a flush takes them,
/// overridden by the walk's records that differ from the view's, plus a
/// deletion for every record whose parent the walk listed but that it
/// didn't find, and for everything under such a deleted directory. Records
/// outside the subtrees are neither read nor copied; walked records the
/// view already holds as found stay where they are.
///
/// The result answers like the base [`enumerate`] would build: the walk
/// takes over only IDs that `enumerate` wouldn't have copied (records
/// strictly inside a subtree, or not visible), and every visible record
/// strictly inside a subtree that the walk didn't emit has an ancestor
/// (or is one) whose parent the walk listed. That ancestor and all its
/// descendants are deleted explicitly: hidden only by the broken parent
/// chain, they would still be ranked by every search until a merge, and
/// count toward no merge budget.
///
/// `None` builds a base instead: no budget, a whole root in scope, or a
/// delta over budget. An over-budget build stops as soon as its entries
/// exceed the budget, so it holds at most that many and wastes at most
/// one walk of the subtrees before [`enumerate`] starts over.
fn subtree_delta(request: &BuildRequest, stop: &AtomicBool) -> Result<Option<Rescan>> {
    let Some(budget) = request.delta_budget else {
        return Ok(None);
    };
    if matches!(request.scope, Scope::All) {
        return Ok(None);
    }
    let (plans, unavailable) = plan(request);
    if plans.values().any(Option::is_none) {
        return Ok(None);
    }
    let view = &request.view;
    let starts: Vec<(&PathBuf, u64)> = (plans.values().flatten().flatten())
        .map(|(dir, id)| (dir, *id))
        .collect();
    let mut under = Under::new(starts.iter().map(|s| s.1));
    let parent_of = |id| view.record_projected(id, false, false).map(|r| r.parent);
    // Upper bound on the delta's entries: an overlay-only deletion in it
    // writes nothing (see [`split_changes`]).
    let mut changes = pending_changes(view, false);
    let mut emitted = HashSet::new();
    // Every walked ID counts for the deletions below; only what differs
    // from the view's newest version is written.
    let mut take = |record: Record, changes: &mut BTreeMap<u64, Option<Record>>| {
        emitted.insert(record.id);
        if view.record(record.id).as_ref() != Some(&record) {
            changes.insert(record.id, Some(record));
        }
    };
    // A subtree directory keeps its ID but is re-read: its listing changed.
    for &(dir, id) in &starts {
        if let Some(kept) = view.record(id)
            && let Ok(fresh) = ingest::inspect(dir, id, kept.parent, kept.root)
        {
            take(fresh, &mut changes);
        }
    }
    let mut claimed = HashSet::new();
    let mut over = false;
    let walk = walk_plans(
        request,
        &plans,
        // `enumerate` copies every visible record outside the subtrees
        // first, so its walk can't take one over: neither may this one.
        |id| {
            (parent_of(id).is_some_and(|p| under.holds(p, parent_of)) || view.path(id).is_none())
                && claimed.insert(id)
        },
        |record| {
            take(record, &mut changes);
            over = changes.len() > budget;
            ensure!(!over, "subtree delta over budget");
            Ok(())
        },
        stop,
    );
    if over {
        return Ok(None);
    }
    let skipped = walk?;

    let mut overlay_children = HashMap::<u64, Vec<u64>>::new();
    for layer in &view.layers {
        for (&id, record) in &layer.records {
            if let Some(record) = record {
                overlay_children.entry(record.parent).or_default().push(id);
            }
        }
    }
    // The IDs whose newest version hangs off `parent` that the walk didn't
    // emit, wherever their stale copies sit.
    let emitted = &emitted;
    let unwalked = |parent: u64| {
        (view.segments())
            .flat_map(|s| s.child_ids(parent))
            .chain(overlay_children.get(&parent).into_iter().flatten().copied())
            .filter(move |id| !emitted.contains(id) && parent_of(*id) == Some(parent))
            .collect::<Vec<_>>()
    };
    // Gone: what a listed directory (or any emitted ID, in case one stopped
    // being a directory) no longer holds...
    let listed = emitted.iter().copied().chain(starts.iter().map(|s| s.1));
    let mut gone: Vec<u64> = listed.flat_map(&unwalked).collect();
    // ...and, depth first, everything still under it. Each ID is expanded
    // at most once, so a parent cycle can't loop, and the budget caps the
    // whole walk.
    let mut seen = HashSet::new();
    while let Some(id) = gone.pop() {
        ensure!(!stop.load(Ordering::Relaxed), "segment build cancelled");
        if !seen.insert(id) {
            continue;
        }
        changes.insert(id, None);
        if changes.len() > budget {
            return Ok(None);
        }
        gone.extend(unwalked(id));
    }
    let walked = emitted.len();
    let changes = split_changes(view, &view.deltas, changes);
    let entries = changes.0.len() + changes.1.len();
    tracing::info!(walked, entries, budget, "subtree reconcile as a delta");
    if entries > budget {
        return Ok(None);
    }
    Ok(Some(Rescan {
        changes,
        skipped,
        unavailable,
    }))
}

/// The scope a reconcile of `scope` walks, exactly: each subtree
/// directory canonicalised and widened as [`walk_starts`] does, so the
/// builder walks no more than this and the writer can hold back events
/// for just this area while it builds. Whole roots, and roots no longer in
/// the view, pass through.
///
/// Requested directories can be raw (overflow paths, refused hints): one
/// reached through a symlinked directory canonicalises to where it lives,
/// which may be elsewhere in its root, in another root (that root's subtree
/// then) or in none (dropped: nothing indexed lives there). Does filesystem
/// I/O (`realpath`, `lstat`, `read_dir`, a few per directory, at most
/// [`MAX_SUBTREES`] directories per root).
pub(super) fn resolve_scope(view: &View, filter: &ingest::IndexFilter, scope: Scope) -> Scope {
    let Scope::Roots(parts) = scope else {
        return Scope::All;
    };
    let mut whole = BTreeMap::new();
    let mut dirs = Vec::new();
    for (id, part) in parts {
        match part {
            Subtrees::Whole => {
                whole.insert(id, Subtrees::Whole);
            }
            Subtrees::Dirs(d) if view.roots.iter().any(|r| r.id == id) => {
                dirs.extend(d.iter().map(|d| ingest::canonical_event_path(d)));
            }
            Subtrees::Dirs(_) => {}
        }
    }
    let Scope::Roots(found) =
        Scope::Roots(whole).merge(super::writer::affected(&view.roots, &dirs))
    else {
        return Scope::All;
    };
    let found = found.into_iter().map(|(id, part)| {
        let part = match (part, view.roots.iter().find(|r| r.id == id)) {
            (Subtrees::Dirs(dirs), Some(root)) => walk_starts(view, root, filter, &dirs)
                .map_or(Subtrees::Whole, |starts| {
                    Subtrees::Dirs(starts.into_iter().map(|(dir, _)| dir).collect())
                }),
            (part, _) => part,
        };
        (id, part)
    });
    Scope::Roots(found.collect())
}

/// Where a reconcile of `dirs` (canonical, inside `root`) starts walking:
/// each directory with its ID in the view, or `None` to walk the root whole.
///
/// A directory the view doesn't hold (new since), or that is gone, not a
/// directory any more (a symlink is not one: walks don't follow them),
/// can't be listed or is excluded, widens to its nearest ancestor that
/// passes, so the re-listed parent drops a vanished subtree's records
/// rather than keeping them. Widening up to the root walks the root whole.
/// An excluded directory widens rather than being dropped because an
/// overflow can name one that still covers indexed entries: on macOS with
/// root `/`, FSEvents `MustScanSubDirs` may report the firmlink form
/// (`/System/Volumes/Data/...`) of a directory indexed under `/Users/...`
/// (unverified on hardware), and dropping it would repair nothing.
/// Assumes `lstat` + `read_dir` here still describe the directory when the
/// walk lists it; one that vanishes in between lists as unreadable (counted
/// skipped) and loses its records, which the next event or overflow
/// repairs.
fn walk_starts(
    view: &View,
    root: &Root,
    filter: &ingest::IndexFilter,
    dirs: &BTreeSet<PathBuf>,
) -> Option<Vec<(PathBuf, u64)>> {
    let mut starts = Vec::new();
    for dir in dirs {
        let mut at = dir.as_path();
        let start = loop {
            if at == root.path || !at.starts_with(&root.path) {
                break None;
            }
            if !filter.excludes_dir(&root.path, at)
                && let Some(id) = view.resolve(at)
                && std::fs::symlink_metadata(at).is_ok_and(|m| m.is_dir())
                && std::fs::read_dir(at).is_ok()
            {
                break Some((at.to_path_buf(), id));
            }
            at = at.parent()?;
        };
        starts.push(start?);
    }
    // Component order puts a directory right before its descendants;
    // widening may have made one start another's ancestor.
    starts.sort();
    starts.dedup_by(|later, earlier| later.0.starts_with(&earlier.0));
    Some(starts)
}

/// Which directories lie under a reconcile's subtree directories, decided
/// by parent links rather than by rebuilding each record's path: one
/// lookup per directory, however many records share it.
struct Under {
    subtrees: HashSet<u64>,
    known: HashMap<u64, bool>,
}

impl Under {
    fn new(subtrees: impl IntoIterator<Item = u64>) -> Under {
        Under {
            subtrees: subtrees.into_iter().collect(),
            known: HashMap::new(),
        }
    }

    /// Whether `id` is a subtree directory or lies under one, given each
    /// record's parent (0 above a root's own record). A broken chain or one
    /// longer than any real path (as [`View::path`] bounds it) is outside.
    fn holds(&mut self, id: u64, parent_of: impl Fn(u64) -> Option<u64>) -> bool {
        let mut chain = Vec::new();
        let mut at = id;
        let found = loop {
            if self.subtrees.contains(&at) {
                break true;
            }
            if at == 0 || chain.len() >= 4096 {
                break false;
            }
            if let Some(&known) = self.known.get(&at) {
                break known;
            }
            chain.push(at);
            match parent_of(at) {
                Some(parent) => at = parent,
                None => break false,
            }
        };
        for id in chain {
            self.known.insert(id, found);
        }
        found
    }
}

/// The ID of the record at `path` whose native identity still matches.
///
/// The walk already holds the parent's ID, so look one level down instead of
/// resolving the path from the root for every entry: that per-entry
/// root-to-leaf walk, a name-page decode per binary-search probe, was ~94% of
/// a rebuild's CPU (FIL-27). A miss still tries the full path, which covers a
/// parent that was replaced under its children; it costs only for changed
/// entries.
fn unchanged_id(view: &View, path: &Path, parent: u64, identity: &Identity) -> Option<u64> {
    let unchanged = |id| {
        view.record(id)
            .filter(|record| record.identity == *identity)
            .map(|record| record.id)
    };
    path.file_name()
        .filter(|_| parent != 0)
        .and_then(|name| view.child(parent, &raw_name(name)))
        .and_then(unchanged)
        .or_else(|| view.resolve(path).and_then(unchanged))
}

fn write_line(writer: &mut impl Write, value: &impl Serialize) -> Result<()> {
    serde_json::to_writer(&mut *writer, value)?;
    writer.write_all(b"\n")?;
    Ok(())
}

// Private worker spool: a JSON header followed by fixed-width metadata and raw
// name bytes. This is scratch, not the durable WAL or segment format.
// Public only for the worker-process integration tests.
#[doc(hidden)]
pub fn write_record(writer: &mut impl Write, record: &Record) -> Result<()> {
    ensure!(
        record.name.len() <= MAX_RECORD_BYTES as usize - RECORD_HEADER_BYTES,
        "segment build record exceeds size limit"
    );
    let mut header = [0u8; RECORD_HEADER_BYTES];
    header[0..8].copy_from_slice(&record.id.to_le_bytes());
    header[8..16].copy_from_slice(&record.parent.to_le_bytes());
    header[16..20].copy_from_slice(&record.root.to_le_bytes());
    header[20] = record.flags;
    header[21] = u8::from(record.size.is_some()) | (u8::from(record.mtime.is_some()) << 1);
    header[22..30].copy_from_slice(&record.identity.device.to_le_bytes());
    header[30..38].copy_from_slice(&record.identity.key.to_le_bytes());
    header[38..46].copy_from_slice(&record.identity.birth.to_le_bytes());
    header[46..54].copy_from_slice(&record.size.unwrap_or(0).to_le_bytes());
    header[54..62].copy_from_slice(&record.mtime.unwrap_or(0).to_le_bytes());
    header[62..66].copy_from_slice(&(record.name.len() as u32).to_le_bytes());
    writer.write_all(&header)?;
    writer.write_all(&record.name)?;
    Ok(())
}

fn read_record(reader: &mut impl BufRead) -> Result<Option<Record>> {
    let mut header = [0u8; RECORD_HEADER_BYTES];
    if reader.read(&mut header[..1])? == 0 {
        return Ok(None);
    }
    reader
        .read_exact(&mut header[1..])
        .context("truncated segment build record")?;
    ensure!(header[21] & !3 == 0, "invalid record presence bits");
    let size = u32::from_le_bytes(header[62..66].try_into()?) as usize;
    ensure!(
        size <= MAX_RECORD_BYTES as usize - RECORD_HEADER_BYTES,
        "segment build record exceeds size limit"
    );
    let mut name = vec![0; size];
    reader
        .read_exact(&mut name)
        .context("truncated segment build name")?;
    Ok(Some(Record {
        id: u64::from_le_bytes(header[0..8].try_into()?),
        parent: u64::from_le_bytes(header[8..16].try_into()?),
        root: u32::from_le_bytes(header[16..20].try_into()?),
        flags: header[20],
        identity: crate::catalog::segment::Identity {
            device: u64::from_le_bytes(header[22..30].try_into()?),
            key: u64::from_le_bytes(header[30..38].try_into()?),
            birth: u64::from_le_bytes(header[38..46].try_into()?),
        },
        size: (header[21] & 1 != 0).then(|| u64::from_le_bytes(header[46..54].try_into().unwrap())),
        mtime: (header[21] & 2 != 0)
            .then(|| i64::from_le_bytes(header[54..62].try_into().unwrap())),
        name,
    }))
}

fn run_process(input: &Path, output: &Path, errors: &Path, stop: &AtomicBool) -> Result<()> {
    let error_file = File::options().write(true).create_new(true).open(errors)?;
    let mut command = Command::new(super::executable::daemon()?);
    command
        .arg("--build-segment")
        .arg(input)
        .arg(output)
        // The open stdin pipe is a lifetime signal. If the owner crashes, EOF
        // terminates the worker before a replacement owner cleans its scratch.
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(error_file);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let mut worker = Worker(command.spawn().context("starting the segment worker")?);
    loop {
        ensure!(!stop.load(Ordering::Relaxed), "segment build cancelled");
        if let Some(status) = worker.0.try_wait()? {
            if !status.success() {
                let mut message = String::new();
                File::open(errors)?
                    .take(MAX_ERROR_BYTES)
                    .read_to_string(&mut message)?;
                if status.code() == Some(WORKER_SKEW_EXIT) {
                    return Err(anyhow::Error::new(WorkerSkew).context(message.trim().to_string()));
                }
                anyhow::bail!("segment worker failed ({status}): {}", message.trim());
            }
            return Ok(());
        }
        std::thread::sleep(WORKER_POLL_INTERVAL);
    }
}

/// Internal companion-daemon entry point; does not open the live database.
pub fn run_worker(input: &Path, output: &Path) -> Result<()> {
    std::thread::Builder::new()
        .name("filex-builder-owner-watch".into())
        .spawn(|| {
            let mut byte = [0];
            loop {
                match std::io::stdin().read(&mut byte) {
                    Ok(0) | Err(_) => std::process::exit(1),
                    Ok(_) => {}
                }
            }
        })?;
    compile(input, output)
}

fn segment_format() -> String {
    String::from_utf8_lossy(SEGMENT_FORMAT).into_owned()
}

fn read_line(reader: &mut impl BufRead, line: &mut String, limit: u64) -> Result<usize> {
    line.clear();
    let bytes = reader.take(limit + 1).read_line(line)?;
    ensure!(
        bytes as u64 <= limit,
        "segment build record exceeds size limit"
    );
    Ok(bytes)
}

fn compile(input: &Path, output: &Path) -> Result<()> {
    let mut reader = BufReader::new(File::open(input)?);
    let mut line = String::new();
    let header_bytes = read_line(&mut reader, &mut line, MAX_HEADER_BYTES)?;
    ensure!(header_bytes != 0, "missing segment build header");
    let header: Header = serde_json::from_str(&line).context("invalid segment build header")?;
    if header.version != REQUEST_VERSION || header.segment_format != segment_format() {
        return Err(anyhow::Error::new(WorkerSkew).context(format!(
            "unsupported segment build request (version {}, format {:?})",
            header.version, header.segment_format
        )));
    }

    // A move can preserve an ID smaller than its newly enumerated parent.
    // Sort compact (ID, offset) pairs, never a second arena of complete records.
    let mut order = Vec::new();
    let mut position = header_bytes as u64;
    loop {
        let Some(record) = read_record(&mut reader)? else {
            break;
        };
        order.push((record.id, position));
        position += (RECORD_HEADER_BYTES + record.name.len()) as u64;
    }
    order.sort_unstable();
    ensure!(
        order.windows(2).all(|pair| pair[0].0 != pair[1].0),
        "duplicate file ID in build input"
    );
    let mut failure = None;
    let records = order.into_iter().map_while(|(_, offset)| {
        let record = (|| -> Result<Record> {
            // Compaction spools are already ID-ordered. Preserve the reader's
            // buffer across adjacent records; enumeration may still need seeks.
            if position != offset {
                reader.seek(SeekFrom::Start(offset))?;
            }
            let record = read_record(&mut reader)?.context("truncated segment build input")?;
            position = offset + (RECORD_HEADER_BYTES + record.name.len()) as u64;
            Ok(record)
        })();
        match record {
            Ok(record) => Some(record),
            Err(error) => {
                failure = Some(error);
                None
            }
        }
    });
    let tombstones = header.tombstones.clone();
    let segment = Segment::build(records, header.roots, header.sequence);
    if let Some(error) = failure {
        return Err(error);
    }
    let mut segment = segment?;
    segment.tombstones = tombstones;
    segment.save(output)
}

/// Laboratory helper that uses the same isolated compaction path as the writer.
#[cfg(feature = "index-v2-lab")]
pub fn compact_to(input: &Path, output: &Path) -> Result<()> {
    // SAFETY: laboratory input is an immutable generation owned by the caller.
    let base = Arc::new(unsafe { Segment::open(input) }?);
    let view = Arc::new(View {
        roots: base.roots.clone(),
        epoch: base.sequence,
        base,
        deltas: vec![],
        layers: vec![],
    });
    compact_view_to(view, output)?;
    Ok(())
}

/// Keep the owner process and its old view alive for lifecycle memory probes.
#[cfg(feature = "index-v2-lab")]
pub fn compact_view_to(view: Arc<View>, output: &Path) -> Result<Segment> {
    ensure!(!output.exists(), "compaction output already exists");
    let built = build(
        BuildRequest {
            view,
            mode: Mode::Merge,
            scope: Scope::All,
            filter: Default::default(),
            directory: output
                .parent()
                .context("output has no parent directory")?
                .to_owned(),
            next: Arc::new(AtomicU64::new(0)),
            progress: Default::default(),
            delta_budget: None,
        },
        &AtomicBool::new(false),
    )?;
    if let Err(error) = std::fs::rename(&built.path, output) {
        drop(built.segment);
        let _ = std::fs::remove_file(built.path);
        return Err(error.into());
    }
    Ok(built.segment)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_spool_preserves_native_bytes_and_rejects_every_truncation() {
        let record = Record {
            id: u64::MAX,
            parent: 19,
            root: u32::MAX,
            name: vec![255, 254, 13, 10, 0, 128],
            flags: 2,
            identity: crate::catalog::segment::Identity {
                device: u64::MAX,
                key: 123,
                birth: 987,
            },
            size: Some(u64::MAX),
            mtime: Some(i64::MIN),
        };
        for record in [
            record.clone(),
            Record {
                size: None,
                mtime: None,
                ..record
            },
        ] {
            let mut encoded = Vec::new();
            write_record(&mut encoded, &record).unwrap();
            let mut reader = encoded.as_slice();
            assert_eq!(read_record(&mut reader).unwrap(), Some(record));
            assert!(read_record(&mut reader).unwrap().is_none());
            for end in 1..encoded.len() {
                assert!(read_record(&mut &encoded[..end]).is_err());
            }
            encoded[62..66].copy_from_slice(&u32::MAX.to_le_bytes());
            assert!(read_record(&mut encoded.as_slice()).is_err());
        }
    }

    #[test]
    fn binary_spool_reorders_records_and_reuses_adjacent_input() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("input");
        let output = directory.path().join("output");
        let mut file = File::create(&input).unwrap();
        write_line(
            &mut file,
            &Header {
                version: REQUEST_VERSION,
                sequence: 9,
                tombstones: None,
                segment_format: segment_format(),
                roots: vec![Root {
                    id: 1,
                    path: "/fixture".into(),
                    device: 1,
                }],
            },
        )
        .unwrap();
        for id in [4, 5, 1, 3, 2, 6] {
            write_record(
                &mut file,
                &Record {
                    id,
                    parent: if id == 1 { 0 } else { 1 },
                    root: 1,
                    name: format!("東京-{}", "x".repeat(id as usize * 2000)).into_bytes(),
                    flags: u8::from(id == 1),
                    identity: Default::default(),
                    size: Some(id),
                    mtime: Some(-(id as i64)),
                },
            )
            .unwrap();
        }
        drop(file);
        compile(&input, &output).unwrap();
        let segment = unsafe { Segment::open(&output) }.unwrap();
        assert_eq!(segment.sequence, 9);
        assert_eq!(segment.len(), 6);
        for slot in 0..segment.len() {
            let record = segment.record(slot);
            assert_eq!(record.id, slot as u64 + 1);
            assert_eq!(record.size, Some(record.id));
            assert_eq!(record.mtime, Some(-(record.id as i64)));
            assert_eq!(record.name.len(), 7 + record.id as usize * 2000);
        }
    }

    #[test]
    fn worker_rejects_duplicate_ids_and_does_not_publish_partial_output() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("input");
        let output = directory.path().join("output");
        let mut file = File::create(&input).unwrap();
        write_line(
            &mut file,
            &Header {
                version: REQUEST_VERSION,
                sequence: 1,
                roots: vec![],
                tombstones: None,
                segment_format: segment_format(),
            },
        )
        .unwrap();
        let record = Record {
            id: 1,
            parent: 0,
            root: 1,
            name: b"root".to_vec(),
            flags: 1,
            identity: Default::default(),
            size: None,
            mtime: None,
        };
        write_record(&mut file, &record).unwrap();
        write_record(&mut file, &record).unwrap();
        assert!(
            compile(&input, &output)
                .unwrap_err()
                .to_string()
                .contains("duplicate file ID")
        );
        assert!(!output.exists());
    }

    /// FIL-27 regression: a stopping daemon cancels a reconcile build without
    /// leaving scratch files or anything a writer could publish.
    #[test]
    fn stopped_build_fails_and_leaves_nothing_behind() {
        let directory = tempfile::tempdir().unwrap();
        let files = directory.path().join("files");
        let data = directory.path().join("index");
        std::fs::create_dir_all(files.join("nested")).unwrap();
        std::fs::create_dir(&data).unwrap();
        for i in 0..200 {
            std::fs::write(files.join(format!("nested/{i}.txt")), b"x").unwrap();
        }
        let roots = vec![Root {
            id: 1,
            path: files,
            device: 0,
        }];
        let request = BuildRequest {
            view: Arc::new(View {
                base: Arc::new(Segment::build([], roots.clone(), 0).unwrap()),
                deltas: vec![],
                layers: vec![],
                roots,
                epoch: 0,
            }),
            mode: Mode::Reconcile,
            scope: Scope::All,
            filter: Default::default(),
            directory: data.clone(),
            next: Arc::new(AtomicU64::new(1)),
            progress: Default::default(),
            delta_budget: None,
        };
        let progress = request.progress.clone();
        let error = build(request, &AtomicBool::new(true)).err().unwrap();
        assert!(error.to_string().contains("cancelled"), "{error}");
        assert_eq!(
            progress.snapshot(),
            None,
            "a cancelled walk stops reporting progress"
        );
        assert_eq!(std::fs::read_dir(&data).unwrap().count(), 0);
    }

    /// Resolves and walks `scope` of `view` the way a reconcile does and returns the
    /// next view, built from exactly the records the worker would get.
    fn reconcile(view: View, scope: Scope, data: &Path, next: &Arc<AtomicU64>) -> View {
        reconcile_reporting(view, scope, data, next).0
    }

    /// [`reconcile`], also returning the roots it found unavailable.
    fn reconcile_reporting(
        view: View,
        scope: Scope,
        data: &Path,
        next: &Arc<AtomicU64>,
    ) -> (View, Vec<(u32, String)>) {
        let roots = view.roots.clone();
        // As the writer does before it sends a reconcile.
        let scope = resolve_scope(&view, &Default::default(), scope);
        let request = BuildRequest {
            view: Arc::new(view),
            mode: Mode::Reconcile,
            scope,
            filter: Default::default(),
            directory: data.to_path_buf(),
            next: next.clone(),
            progress: Default::default(),
            delta_budget: None,
        };
        let mut spool = Vec::new();
        let (_, unavailable) = enumerate(&request, &mut spool, &AtomicBool::new(false)).unwrap();
        let mut reader = std::io::Cursor::new(spool);
        let mut records = Vec::new();
        while let Some(record) = read_record(&mut reader).unwrap() {
            records.push(record);
        }
        // The worker sorts by ID too.
        records.sort_by_key(|r| r.id);
        let view = View {
            base: Arc::new(Segment::build(records, roots.clone(), 0).unwrap()),
            deltas: vec![],
            layers: vec![],
            roots,
            epoch: 0,
        };
        (view, unavailable)
    }

    /// Two roots, empty and unwalked; returns the view and their paths.
    fn two_roots() -> (View, [tempfile::TempDir; 2]) {
        let dirs = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
        let roots: Vec<_> = (dirs.iter().zip(1..))
            .map(|(dir, id)| Root {
                id,
                path: dir.path().canonicalize().unwrap(),
                device: 0,
            })
            .collect();
        let view = View {
            base: Arc::new(Segment::build([], roots.clone(), 0).unwrap()),
            deltas: vec![],
            layers: vec![],
            roots,
            epoch: 0,
        };
        (view, dirs)
    }

    /// A reconcile walks with the filter it was handed (the writer's, as
    /// event batches use), not one reloaded from disk.
    #[test]
    fn reconcile_walks_with_the_requested_filter() {
        let data = tempfile::tempdir().unwrap();
        let (view, _dirs) = two_roots();
        let a = view.roots[0].path.clone();
        std::fs::create_dir(a.join("skipme")).unwrap();
        std::fs::write(a.join("skipme/hidden.txt"), b"x").unwrap();
        std::fs::write(a.join("kept.txt"), b"x").unwrap();
        let request = BuildRequest {
            view: Arc::new(view),
            mode: Mode::Reconcile,
            scope: Scope::All,
            filter: ingest::IndexFilter::from_settings(&crate::settings::Settings {
                exclude_dev_folders: true,
                excluded_folder_names: vec!["skipme".into()],
                ..Default::default()
            }),
            directory: data.path().to_path_buf(),
            next: Arc::new(AtomicU64::new(10)),
            progress: Default::default(),
            delta_budget: None,
        };
        let mut spool = Vec::new();
        enumerate(&request, &mut spool, &AtomicBool::new(false)).unwrap();
        let mut reader = std::io::Cursor::new(spool);
        let mut names = Vec::new();
        while let Some(record) = read_record(&mut reader).unwrap() {
            names.push(String::from_utf8_lossy(&record.name).into_owned());
        }
        assert!(names.contains(&"kept.txt".into()), "{names:?}");
        assert!(
            !names
                .iter()
                .any(|n| n.contains("skipme") || n == "hidden.txt")
        );
    }

    /// One root gone (unmounted, deleted, renamed away) must not fail a full
    /// reconcile: the reachable root is walked, the missing one keeps its
    /// records, and it is reported.
    #[test]
    fn full_reconcile_keeps_an_unavailable_root() {
        let data = tempfile::tempdir().unwrap();
        let (view, [_a_dir, b]) = two_roots();
        let (a, b_dir) = (view.roots[0].path.clone(), view.roots[1].path.clone());
        std::fs::write(a.join("alpha-old.txt"), b"x").unwrap();
        std::fs::write(b_dir.join("bravo-old.txt"), b"x").unwrap();
        let next = Arc::new(AtomicU64::new(10));
        let full = reconcile(view, Scope::All, data.path(), &next);
        let kept = full.resolve(&b_dir.join("bravo-old.txt")).unwrap();

        drop(b);
        assert!(!b_dir.exists());
        std::fs::write(a.join("alpha-new.txt"), b"x").unwrap();
        let (after, unavailable) = reconcile_reporting(full, Scope::All, data.path(), &next);
        assert!(after.resolve(&a.join("alpha-new.txt")).is_some());
        assert_eq!(after.resolve(&b_dir.join("bravo-old.txt")), Some(kept));
        assert_eq!(
            unavailable.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            [2]
        );
    }

    /// Regression: a root that stats but can't be listed (Full Disk Access
    /// revoked) walked as empty and lost every record. It keeps them and is
    /// reported like a missing root.
    #[cfg(unix)]
    #[test]
    fn full_reconcile_keeps_an_unlistable_root() {
        use std::os::unix::fs::PermissionsExt;
        let data = tempfile::tempdir().unwrap();
        let (view, _dirs) = two_roots();
        let (a, b) = (view.roots[0].path.clone(), view.roots[1].path.clone());
        std::fs::write(a.join("alpha.txt"), b"x").unwrap();
        std::fs::write(b.join("bravo.txt"), b"x").unwrap();
        let next = Arc::new(AtomicU64::new(10));
        let full = reconcile(view, Scope::All, data.path(), &next);
        let kept = full.resolve(&b.join("bravo.txt")).unwrap();

        std::fs::set_permissions(&b, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read_dir(&b).is_ok() {
            // Root ignores permissions; nothing to test.
            std::fs::set_permissions(&b, std::fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }
        let (after, unavailable) = reconcile_reporting(full, Scope::All, data.path(), &next);
        std::fs::set_permissions(&b, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(after.resolve(&a.join("alpha.txt")).is_some());
        assert_eq!(after.resolve(&b.join("bravo.txt")), Some(kept));
        assert_eq!(
            unavailable.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            [2]
        );
    }

    /// A root-scoped reconcile walks only that root; the other root's
    /// records are kept as they were, IDs included, and stay searchable.
    #[test]
    fn scoped_reconcile_walks_only_its_roots() {
        use super::super::{ipc::Query, query};
        let data = tempfile::tempdir().unwrap();
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (a, b) = (
            a.path().canonicalize().unwrap(),
            b.path().canonicalize().unwrap(),
        );
        std::fs::write(a.join("alpha-old.txt"), b"x").unwrap();
        std::fs::write(b.join("bravo-old.txt"), b"x").unwrap();
        let roots = vec![
            Root {
                id: 1,
                path: a.clone(),
                device: 0,
            },
            Root {
                id: 2,
                path: b.clone(),
                device: 0,
            },
        ];
        let empty = View {
            base: Arc::new(Segment::build([], roots.clone(), 0).unwrap()),
            deltas: vec![],
            layers: vec![],
            roots,
            epoch: 0,
        };
        let next = Arc::new(AtomicU64::new(10));
        let full = reconcile(empty, Scope::All, data.path(), &next);
        let kept = full.resolve(&b.join("bravo-old.txt")).unwrap();

        std::fs::write(a.join("alpha-new.txt"), b"x").unwrap();
        std::fs::write(b.join("bravo-new.txt"), b"x").unwrap();
        let scoped = reconcile(full, Scope::roots([1]), data.path(), &next);
        assert!(scoped.resolve(&a.join("alpha-new.txt")).is_some());
        assert!(scoped.resolve(&a.join("alpha-old.txt")).is_some());
        assert_eq!(scoped.resolve(&b.join("bravo-old.txt")), Some(kept));
        assert_eq!(
            scoped.resolve(&b.join("bravo-new.txt")),
            None,
            "the other root was not walked"
        );
        let query = Query {
            text: "bravo".into(),
            limit: 10,
            ..Default::default()
        };
        let page = query::search(
            &scoped,
            &query,
            &AtomicBool::new(false),
            &Default::default(),
        )
        .unwrap();
        assert_eq!(page.hits.iter().map(|h| h.id).collect::<Vec<_>>(), [kept]);
    }

    fn dirs(paths: &[&str]) -> Subtrees {
        Subtrees::Dirs(paths.iter().map(PathBuf::from).collect())
    }

    #[test]
    fn subtree_scopes_merge() {
        let one = |id, part| Scope::Roots([(id, part)].into());
        // Union, an ancestor absorbing descendants whichever comes first.
        assert_eq!(
            one(1, dirs(&["/r/a/b", "/r/c"])).merge(one(1, dirs(&["/r/a", "/r/c/d", "/r/ab"]))),
            one(1, dirs(&["/r/a", "/r/ab", "/r/c"]))
        );
        // Other roots stay apart; a whole root absorbs its subtrees.
        assert_eq!(
            one(1, dirs(&["/r/a"]))
                .merge(one(2, dirs(&["/s/x"])))
                .merge(one(1, Subtrees::Whole)),
            Scope::Roots([(1, Subtrees::Whole), (2, dirs(&["/s/x"]))].into())
        );
        assert_eq!(
            one(1, Subtrees::Whole).merge(one(1, dirs(&["/r/a"]))),
            one(1, Subtrees::Whole)
        );
        assert_eq!(one(1, dirs(&["/r/a"])).merge(Scope::All), Scope::All);
        assert_eq!(Scope::All.merge(one(1, dirs(&["/r/a"]))), Scope::All);
        // Past the cap, the whole root.
        let mut scope = Scope::Roots(Default::default());
        for i in 0..MAX_SUBTREES {
            scope = scope.merge(one(1, dirs(&[&format!("/r/{i}")])));
        }
        assert!(matches!(&scope, Scope::Roots(p) if p[&1] != Subtrees::Whole));
        assert_eq!(
            scope.merge(one(1, dirs(&["/r/more"]))),
            one(1, Subtrees::Whole)
        );
    }

    /// Membership by parent links on a deep chain with side branches,
    /// looking each directory's parent up once.
    #[test]
    fn under_follows_parent_links_once_per_directory() {
        // 1 is the root's record (parent 0); i's parent is i - 1 up to
        // 3000; 10_000 + i hangs off i as a leaf.
        let parent = |id: u64| match id {
            1 => Some(0),
            2..=3000 => Some(id - 1),
            10_001..=13_000 => Some(id - 10_000),
            _ => None,
        };
        let lookups = std::cell::Cell::new(0);
        let parent_of = |id| {
            lookups.set(lookups.get() + 1);
            parent(id)
        };
        let mut under = Under::new([1500, 2500]);
        for id in (1..=3000).chain(10_001..=13_000) {
            let expected = (id % 10_000) >= 1500;
            assert_eq!(under.holds(id, parent_of), expected, "{id}");
        }
        assert!(!under.holds(0, parent_of));
        assert!(!under.holds(99_999, parent_of), "a broken chain is outside");
        // Each ID above was looked up at most once.
        assert!(lookups.get() <= 6001, "{} lookups", lookups.get());
    }

    /// One root; an overflow names a directory: only it is walked again.
    /// Changes inside appear (adds, deletes, a directory moved within it),
    /// changes elsewhere under the same root don't, kept records keep their
    /// IDs and stay searchable.
    #[test]
    fn subtree_reconcile_walks_only_its_subtree() {
        use super::super::{ipc::Query, query};
        let data = tempfile::tempdir().unwrap();
        let (view, _dirs) = two_roots();
        let r = view.roots[0].path.clone();
        for dir in ["sub/inner", "sub/mv", "other"] {
            std::fs::create_dir_all(r.join(dir)).unwrap();
        }
        for file in [
            "sub/keep.txt",
            "sub/gone.txt",
            "sub/mv/moved-file.txt",
            "other/outside-old.txt",
        ] {
            std::fs::write(r.join(file), b"x").unwrap();
        }
        let next = Arc::new(AtomicU64::new(10));
        let full = reconcile(view, Scope::All, data.path(), &next);
        let ids = |view: &View, paths: &[&str]| -> Vec<_> {
            paths.iter().map(|p| view.resolve(&r.join(p))).collect()
        };
        let kept = [
            "sub",
            "sub/keep.txt",
            "sub/inner",
            "other",
            "other/outside-old.txt",
        ];
        let before = ids(&full, &kept);
        assert!(before.iter().all(Option::is_some));

        std::fs::write(r.join("sub/new.txt"), b"x").unwrap();
        std::fs::remove_file(r.join("sub/gone.txt")).unwrap();
        std::fs::rename(r.join("sub/mv"), r.join("sub/inner/moved")).unwrap();
        std::fs::write(r.join("other/outside-new.txt"), b"x").unwrap();
        std::fs::write(r.join("top-new.txt"), b"x").unwrap();
        let scope = Scope::Roots([(1, Subtrees::Dirs([r.join("sub")].into()))].into());
        let after = reconcile(full, scope, data.path(), &next);

        assert_eq!(ids(&after, &kept), before, "kept records keep their IDs");
        assert!(after.resolve(&r.join("sub/new.txt")).is_some());
        assert_eq!(after.resolve(&r.join("sub/gone.txt")), None);
        assert_eq!(after.resolve(&r.join("sub/mv")), None);
        let moved = after
            .resolve(&r.join("sub/inner/moved/moved-file.txt"))
            .unwrap();
        assert_eq!(
            after.path(moved),
            Some(r.join("sub/inner/moved/moved-file.txt"))
        );
        assert_eq!(
            after.resolve(&r.join("other/outside-new.txt")),
            None,
            "walked outside the subtree"
        );
        assert_eq!(
            after.resolve(&r.join("top-new.txt")),
            None,
            "walked outside the subtree"
        );
        assert_eq!(after.records().filter(|r| r.root == 1).count(), 9);

        let query = Query {
            text: "outside".into(),
            limit: 10,
            ..Default::default()
        };
        let page =
            query::search(&after, &query, &AtomicBool::new(false), &Default::default()).unwrap();
        assert_eq!(
            page.hits.iter().map(|h| Some(h.id)).collect::<Vec<_>>(),
            [before[4]]
        );
    }

    /// Builds `mode` of `view` through the worker, as the writer does.
    fn build_with(
        view: &Arc<View>,
        mode: Mode,
        scope: Scope,
        data: &Path,
        budget: Option<usize>,
        next: u64,
    ) -> Built {
        build(
            BuildRequest {
                view: view.clone(),
                mode,
                scope,
                filter: Default::default(),
                directory: data.to_path_buf(),
                next: Arc::new(AtomicU64::new(next)),
                progress: Default::default(),
                delta_budget: budget,
            },
            &AtomicBool::new(false),
        )
        .unwrap()
    }

    /// Every visible record by path.
    fn by_path(view: &View) -> BTreeMap<PathBuf, Record> {
        view.records()
            .map(|r| (view.path(r.id).unwrap(), r))
            .collect()
    }

    /// R9: a subtree reconcile written as a delta answers exactly like the
    /// base a full reconcile of the same scope builds from the same view,
    /// and still does once its deltas merge and then merge into the base.
    /// Covers overlay changes inside and outside the subtree (overridden,
    /// dropped, tombstoned), moves within it, a directory moved in from
    /// outside, and one the overlay deleted whose ID the walk takes back:
    /// its old children the walk doesn't find must not resurface.
    #[test]
    fn a_subtree_delta_merges_to_what_a_full_reconcile_builds() {
        use super::super::changes::{apply, normalize_batch};
        let data = tempfile::tempdir().unwrap();
        let (view, _dirs) = two_roots();
        let r = view.roots[0].path.clone();
        for dir in ["sub/deep/b", "sub/mv", "sub/inner", "sub/hid", "other/into"] {
            std::fs::create_dir_all(r.join(dir)).unwrap();
        }
        for file in [
            "sub/keep.txt",
            "sub/gone.txt",
            "sub/mv/m.txt",
            "sub/deep/a.txt",
            "sub/deep/b/c.txt",
            "sub/hid/k.txt",
            "sub/hid/r.txt",
            "other/outside.txt",
            "other/into/i.txt",
        ] {
            std::fs::write(r.join(file), b"x").unwrap();
        }
        let next = Arc::new(AtomicU64::new(10));
        let walked = reconcile(view, Scope::All, data.path(), &next);
        let old = by_path(&walked);

        // Changes applied live before the build, so they sit in an overlay.
        std::fs::write(r.join("other/evented.txt"), b"x").unwrap();
        std::fs::write(r.join("sub/keep.txt"), b"longer").unwrap();
        std::fs::write(r.join("sub/ov.txt"), b"x").unwrap();
        std::fs::remove_file(r.join("other/outside.txt")).unwrap();
        std::fs::rename(r.join("sub/hid"), r.join("sub/inner/hid2")).unwrap();
        let batch = [
            "other/evented.txt",
            "sub/keep.txt",
            "sub/ov.txt",
            "other/outside.txt",
            "sub/hid",
        ]
        .map(|p| (r.join(p), false));
        let (deltas, degraded, _) = normalize_batch(&walked, batch, &next);
        assert!(!degraded);
        let mut overlay = super::super::view::Overlay::default();
        let mut roots = walked.roots.clone();
        for delta in deltas {
            apply(&mut overlay, &mut roots, delta);
        }
        assert!(overlay.records.get(&old[&r.join("sub/hid")].id) == Some(&None));
        // Then the walk sees more than the overlay did.
        std::fs::write(r.join("sub/new.txt"), b"x").unwrap();
        std::fs::remove_file(r.join("sub/gone.txt")).unwrap();
        std::fs::remove_file(r.join("sub/ov.txt")).unwrap();
        std::fs::remove_file(r.join("sub/inner/hid2/r.txt")).unwrap();
        std::fs::remove_dir_all(r.join("sub/deep")).unwrap();
        std::fs::rename(r.join("sub/mv"), r.join("sub/inner/moved")).unwrap();
        std::fs::rename(r.join("other/into"), r.join("sub/into")).unwrap();

        let view = Arc::new(View {
            layers: vec![Arc::new(overlay)],
            epoch: 1,
            ..walked.clone()
        });
        let scope = resolve_scope(
            &view,
            &Default::default(),
            Scope::Roots([(1, dirs(&[r.join("sub").to_str().unwrap()]))].into()),
        );
        let start = next.load(Ordering::Relaxed);
        let delta = build_with(
            &view,
            Mode::Reconcile,
            scope.clone(),
            data.path(),
            Some(10_000),
            start,
        );
        assert!(delta.as_flush && delta.segment.tombstones.is_some());
        // Nothing outside the subtree beyond the overlay's own changes.
        assert!(delta.segment.len() < 15, "{}", delta.segment.len());
        let full = build_with(&view, Mode::Reconcile, scope, data.path(), None, start);
        assert!(!full.as_flush && full.segment.tombstones.is_none());

        let level = |base: Arc<Segment>, deltas| View {
            base,
            deltas,
            layers: vec![],
            roots: walked.roots.clone(),
            epoch: 1,
        };
        let as_delta = level(walked.base.clone(), vec![Arc::new(delta.segment)]);
        let as_base = level(Arc::new(full.segment), vec![]);
        let (got, want) = (by_path(&as_delta), by_path(&as_base));
        assert_eq!(
            got.keys().collect::<Vec<_>>(),
            want.keys().collect::<Vec<_>>()
        );
        for (path, record) in &want {
            // New entries may number differently: the walk lists in parallel.
            let same = |r: &Record| (r.name.clone(), r.flags, r.identity, r.size, r.mtime);
            assert_eq!(same(&got[path]), same(record), "{path:?}");
            if old.contains_key(path) {
                assert_eq!(got[path].id, record.id, "{path:?}");
            }
        }
        assert!(!got.contains_key(&r.join("sub/inner/hid2/r.txt")));
        assert!(got.contains_key(&r.join("sub/inner/hid2/k.txt")));
        assert!(
            got.contains_key(&r.join("other/into/i.txt")),
            "stale in both until walked"
        );
        assert_eq!(as_delta.root_counts(), as_base.root_counts());

        // Deltas merge, then merge into the base: same records, same IDs.
        let merged = build_with(
            &Arc::new(as_delta.clone()),
            Mode::MergeDeltas,
            Scope::All,
            data.path(),
            None,
            0,
        );
        let merged = level(walked.base.clone(), vec![Arc::new(merged.segment)]);
        assert_eq!(by_path(&merged), got);
        let compacted = build_with(
            &Arc::new(merged),
            Mode::Merge,
            Scope::All,
            data.path(),
            None,
            0,
        );
        let compacted = level(Arc::new(compacted.segment), vec![]);
        assert_eq!(by_path(&compacted), got);
    }

    /// The subtree reconcile of `dirs` (in root 1) built from `view` as a
    /// delta must answer like the full base built from it, and still once
    /// all the deltas merge and then merge into the base. Records `old`
    /// holds keep their IDs. Returns the delta.
    fn delta_matches_full(
        view: &Arc<View>,
        dirs: Scope,
        old: &BTreeMap<PathBuf, Record>,
        data: &Path,
        next: &Arc<AtomicU64>,
    ) -> Arc<Segment> {
        let scope = resolve_scope(view, &Default::default(), dirs);
        let start = next.load(Ordering::Relaxed);
        let delta = build_with(
            view,
            Mode::Reconcile,
            scope.clone(),
            data,
            Some(10_000),
            start,
        );
        assert!(delta.as_flush);
        let full = build_with(view, Mode::Reconcile, scope, data, None, start);
        assert!(!full.as_flush);
        let delta = Arc::new(delta.segment);
        let level = |base, deltas| View {
            base,
            deltas,
            layers: vec![],
            roots: view.roots.clone(),
            epoch: view.epoch,
        };
        let as_delta = level(view.base.clone(), {
            let mut deltas = view.deltas.clone();
            deltas.push(delta.clone());
            deltas
        });
        let as_base = level(Arc::new(full.segment), vec![]);
        let (got, want) = (by_path(&as_delta), by_path(&as_base));
        assert_eq!(
            got.keys().collect::<Vec<_>>(),
            want.keys().collect::<Vec<_>>()
        );
        for (path, record) in &want {
            // New entries may number differently: the walk lists in parallel.
            let same = |r: &Record| (r.name.clone(), r.flags, r.identity, r.size, r.mtime);
            assert_eq!(same(&got[path]), same(record), "{path:?}");
            if old.contains_key(path) {
                assert_eq!(got[path].id, record.id, "{path:?}");
            }
        }
        assert_eq!(as_delta.root_counts(), as_base.root_counts());
        let merged = build_with(
            &Arc::new(as_delta.clone()),
            Mode::MergeDeltas,
            Scope::All,
            data,
            None,
            0,
        );
        let merged = level(view.base.clone(), vec![Arc::new(merged.segment)]);
        assert_eq!(by_path(&merged), got);
        assert_eq!(merged.root_counts(), as_base.root_counts());
        let compacted = build_with(&Arc::new(merged), Mode::Merge, Scope::All, data, None, 0);
        assert_eq!(by_path(&level(Arc::new(compacted.segment), vec![])), got);
        delta
    }

    /// Regression: a subtree delta tombstoned only the top of a deleted
    /// tree. Everything under it stayed in the base, hidden by the broken
    /// parent chain but still ranked by search (an `rm -rf` of 25k files
    /// starved every query of its work budget) and never counted toward a
    /// merge. Now each descendant is tombstoned and counts toward the
    /// budget: within it the delta holds them all, past it (the
    /// `Some(100)` pass) the reconcile builds a base instead.
    #[test]
    fn a_deleted_tree_leaves_nothing_for_search_to_skip() {
        use super::super::{ipc::Query, query, writer::delta_budget};
        let data = tempfile::tempdir().unwrap();
        let (view, _dirs) = two_roots();
        let r = view.roots[0].path.clone();
        std::fs::create_dir_all(r.join("sub/big/inner")).unwrap();
        for i in 0..300 {
            let dir = if i % 2 == 0 {
                "sub/big"
            } else {
                "sub/big/inner"
            };
            std::fs::write(r.join(format!("{dir}/aaa{i}.txt")), b"x").unwrap();
        }
        let live = r.join("zz_live_aaa_long_name.txt");
        std::fs::write(&live, b"x").unwrap();
        let next = Arc::new(AtomicU64::new(10));
        let view = Arc::new(reconcile(view, Scope::All, data.path(), &next));
        std::fs::remove_dir_all(r.join("sub/big")).unwrap();

        let budget = delta_budget(view.base.len(), &[]);
        for (budget, as_delta) in [(budget, true), (Some(100), false)] {
            let scope = Scope::Roots([(1, dirs(&[r.join("sub").to_str().unwrap()]))].into());
            let scope = resolve_scope(&view, &Default::default(), scope);
            let start = next.load(Ordering::Relaxed);
            let built = build_with(&view, Mode::Reconcile, scope, data.path(), budget, start);
            assert_eq!(built.as_flush, as_delta, "budget {budget:?}");
            let after = if as_delta {
                // `big`, `inner` and the 300 files.
                assert_eq!(built.segment.tombstones.as_ref().map(Vec::len), Some(302));
                View {
                    deltas: vec![Arc::new(built.segment)],
                    ..(*view).clone()
                }
            } else {
                View {
                    base: Arc::new(built.segment),
                    ..(*view).clone()
                }
            };
            let query = Query {
                text: "aaa".into(),
                limit: 10,
                ..Default::default()
            };
            let page = query::search(&after, &query, &AtomicBool::new(false), &Default::default())
                .unwrap();
            let hits: Vec<_> = page.hits.iter().map(|h| h.path.clone()).collect();
            assert_eq!(hits, std::slice::from_ref(&live), "budget {budget:?}");
            assert!(!page.partial);
            // A tombstoned name is still skipped one by one until a merge.
            let skipped = if as_delta { 300 } else { 0 };
            assert!(page.examined <= skipped + 5, "{}", page.examined);
        }
    }

    /// A subtree delta over a view that already has deltas (one holding a
    /// tombstone for a base record) answers like a full reconcile, and so
    /// does every merge after it.
    #[test]
    fn a_subtree_delta_over_older_deltas_merges_to_a_full_reconcile() {
        use super::super::changes::{apply, normalize_batch};
        let data = tempfile::tempdir().unwrap();
        let (view, _dirs) = two_roots();
        let r = view.roots[0].path.clone();
        for dir in ["sub/x/deep", "sub/y", "other"] {
            std::fs::create_dir_all(r.join(dir)).unwrap();
        }
        for file in [
            "sub/x/a.txt",
            "sub/x/deep/b.txt",
            "sub/y/c.txt",
            "other/o.txt",
        ] {
            std::fs::write(r.join(file), b"x").unwrap();
        }
        let next = Arc::new(AtomicU64::new(10));
        let walked = reconcile(view, Scope::All, data.path(), &next);
        let old = by_path(&walked);

        // Flushed into a delta before the reconcile: a deletion outside the
        // subtree, additions inside and outside it.
        std::fs::remove_file(r.join("other/o.txt")).unwrap();
        std::fs::write(r.join("other/p.txt"), b"x").unwrap();
        std::fs::write(r.join("sub/x/deep/n.txt"), b"x").unwrap();
        let batch = ["other/o.txt", "other/p.txt", "sub/x/deep/n.txt"].map(|p| (r.join(p), false));
        let (changes, degraded, _) = normalize_batch(&walked, batch, &next);
        assert!(!degraded);
        let mut overlay = super::super::view::Overlay::default();
        let mut roots = walked.roots.clone();
        for change in changes {
            apply(&mut overlay, &mut roots, change);
        }
        let pending = View {
            layers: vec![Arc::new(overlay)],
            ..walked.clone()
        };
        let older = flush(&pending, false, data.path(), "older.fx2");
        assert_eq!(older.tombstones.as_ref().map(Vec::len), Some(1));

        // Then the subtree changes: the tree holding the flushed addition
        // goes, a file changes.
        std::fs::remove_dir_all(r.join("sub/x")).unwrap();
        std::fs::write(r.join("sub/y/c.txt"), b"longer").unwrap();
        let view = Arc::new(View {
            deltas: vec![older],
            epoch: 1,
            ..walked
        });
        let scope = Scope::Roots([(1, dirs(&[r.join("sub").to_str().unwrap()]))].into());
        let delta = delta_matches_full(&view, scope, &old, data.path(), &next);
        // `x`, `deep`, `a`, `b` and the flushed `n`; `c` and `sub` rewritten.
        assert_eq!(delta.tombstones.as_ref().map(Vec::len), Some(5));
        assert_eq!(delta.len(), 2);
    }

    /// One reconcile of several subtrees, in both roots, as one delta.
    #[test]
    fn several_subtrees_reconcile_as_one_delta() {
        let data = tempfile::tempdir().unwrap();
        let (view, _dirs) = two_roots();
        let (r, s) = (view.roots[0].path.clone(), view.roots[1].path.clone());
        for dir in ["a/gone", "b", "c"] {
            std::fs::create_dir_all(r.join(dir)).unwrap();
            std::fs::create_dir_all(s.join(dir)).unwrap();
        }
        for file in ["a/gone/f.txt", "a/k.txt", "b/k.txt", "c/k.txt"] {
            std::fs::write(r.join(file), b"x").unwrap();
            std::fs::write(s.join(file), b"x").unwrap();
        }
        let next = Arc::new(AtomicU64::new(10));
        let walked = reconcile(view, Scope::All, data.path(), &next);
        let old = by_path(&walked);
        for root in [&r, &s] {
            std::fs::remove_dir_all(root.join("a/gone")).unwrap();
            std::fs::write(root.join("b/new.txt"), b"x").unwrap();
            // Not in scope: stays unseen.
            std::fs::write(root.join("c/unseen.txt"), b"x").unwrap();
        }
        let scope = Scope::Roots(
            [
                (
                    1,
                    dirs(&[r.join("a").to_str().unwrap(), r.join("b").to_str().unwrap()]),
                ),
                (2, dirs(&[s.join("a").to_str().unwrap()])),
            ]
            .into(),
        );
        let delta = delta_matches_full(&Arc::new(walked), scope, &old, data.path(), &next);
        // Per root `gone` and its file; `r/b` and its new file, `r/a` and `s/a`.
        assert_eq!(delta.tombstones.as_ref().map(Vec::len), Some(4));
        assert_eq!(delta.len(), 4);
    }

    /// A subtree delta writes only what changed: nothing for an unchanged
    /// subtree, the new file and its directory for one addition.
    #[test]
    fn a_subtree_delta_writes_only_what_changed() {
        let data = tempfile::tempdir().unwrap();
        let (view, _dirs) = two_roots();
        let r = view.roots[0].path.clone();
        std::fs::create_dir_all(r.join("sub/inner")).unwrap();
        for i in 0..50 {
            std::fs::write(r.join(format!("sub/inner/f{i}.txt")), b"x").unwrap();
        }
        let next = Arc::new(AtomicU64::new(10));
        let view = Arc::new(reconcile(view, Scope::All, data.path(), &next));
        let reconcile_sub = || {
            let scope = Scope::Roots([(1, dirs(&[r.join("sub").to_str().unwrap()]))].into());
            let scope = resolve_scope(&view, &Default::default(), scope);
            let start = next.load(Ordering::Relaxed);
            let built = build_with(
                &view,
                Mode::Reconcile,
                scope,
                data.path(),
                Some(1000),
                start,
            );
            assert!(built.as_flush);
            built.segment
        };
        let unchanged = reconcile_sub();
        assert_eq!((unchanged.len(), unchanged.tombstones), (0, Some(vec![])));

        std::fs::write(r.join("sub/inner/new.txt"), b"x").unwrap();
        let added = reconcile_sub();
        let names: BTreeSet<_> = (0..added.len()).map(|s| added.record(s).name).collect();
        assert_eq!(names, [b"inner".to_vec(), b"new.txt".to_vec()].into());
    }

    /// A subtree that vanished widens to its nearest ancestor the view and
    /// the disk still hold, so its records go; one new since the last walk
    /// widens the same way, up to the whole root.
    #[test]
    fn vanished_or_new_subtrees_widen_to_their_parent() {
        let data = tempfile::tempdir().unwrap();
        let (view, _dirs) = two_roots();
        let r = view.roots[0].path.clone();
        std::fs::create_dir_all(r.join("p/q/deep")).unwrap();
        std::fs::write(r.join("p/q/deep/f.txt"), b"x").unwrap();
        std::fs::write(r.join("p/sibling.txt"), b"x").unwrap();
        let next = Arc::new(AtomicU64::new(10));
        let full = reconcile(view, Scope::All, data.path(), &next);
        let p = full.resolve(&r.join("p")).unwrap();

        std::fs::remove_dir_all(r.join("p/q")).unwrap();
        std::fs::write(r.join("top-new.txt"), b"x").unwrap();
        let scope = Scope::Roots([(1, Subtrees::Dirs([r.join("p/q/deep")].into()))].into());
        let after = reconcile(full, scope, data.path(), &next);
        assert_eq!(after.resolve(&r.join("p/q")), None);
        assert_eq!(after.resolve(&r.join("p/q/deep/f.txt")), None);
        assert_eq!(after.resolve(&r.join("p")), Some(p));
        assert!(after.resolve(&r.join("p/sibling.txt")).is_some());
        assert_eq!(after.resolve(&r.join("top-new.txt")), None);

        // A directory new since the last walk, right under the root: the
        // root is walked whole.
        std::fs::create_dir(r.join("brand-new")).unwrap();
        let scope = Scope::Roots([(1, Subtrees::Dirs([r.join("brand-new")].into()))].into());
        let after = reconcile(after, scope, data.path(), &next);
        assert!(after.resolve(&r.join("brand-new")).is_some());
        assert!(after.resolve(&r.join("top-new.txt")).is_some());
    }

    /// B2 regression: an overflow or refused-hint path reaches the builder
    /// raw. One through a symlinked directory repairs where the entries
    /// live (`other`), not the symlink's parent (`p`); one landing in
    /// another root repairs there, one in no root nothing.
    #[cfg(unix)]
    #[test]
    fn subtrees_named_through_a_symlink_repair_where_they_live() {
        let data = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let (view, _dirs) = two_roots();
        let (r, s) = (view.roots[0].path.clone(), view.roots[1].path.clone());
        std::fs::create_dir_all(r.join("p")).unwrap();
        std::fs::create_dir_all(r.join("other")).unwrap();
        std::fs::create_dir_all(s.join("there")).unwrap();
        std::os::unix::fs::symlink(r.join("other"), r.join("p/link")).unwrap();
        std::os::unix::fs::symlink(s.join("there"), r.join("p/cross")).unwrap();
        std::os::unix::fs::symlink(outside.path(), r.join("p/away")).unwrap();
        let next = Arc::new(AtomicU64::new(10));
        let full = reconcile(view, Scope::All, data.path(), &next);

        let named = |paths: &[PathBuf]| {
            Scope::Roots([(1, Subtrees::Dirs(paths.iter().cloned().collect()))].into())
        };
        assert_eq!(
            resolve_scope(
                &full,
                &Default::default(),
                named(&[r.join("p/link/f"), r.join("p/cross/g"), r.join("p/away/h"),])
            ),
            Scope::Roots(
                [
                    (1, Subtrees::Dirs([r.join("other")].into())),
                    (2, Subtrees::Dirs([s.join("there")].into())),
                ]
                .into()
            )
        );

        std::fs::write(r.join("other/new.txt"), b"x").unwrap();
        let after = reconcile(full, named(&[r.join("p/link/new.txt")]), data.path(), &next);
        assert!(after.resolve(&r.join("other/new.txt")).is_some());
    }

    /// B3: an excluded subtree (FSEvents can name a firmlink path under an
    /// excluded area) widens to its nearest included ancestor, up to the
    /// whole root, instead of reconciling nothing.
    #[test]
    fn excluded_subtrees_widen_to_an_included_ancestor() {
        let data = tempfile::tempdir().unwrap();
        let (view, _dirs) = two_roots();
        let r = view.roots[0].path.clone();
        std::fs::create_dir_all(r.join("a/skipme/x")).unwrap();
        std::fs::create_dir_all(r.join("skipme")).unwrap();
        let next = Arc::new(AtomicU64::new(10));
        let full = reconcile(view, Scope::All, data.path(), &next);
        let filter = ingest::IndexFilter::from_settings(&crate::settings::Settings {
            exclude_dev_folders: true,
            excluded_folder_names: vec!["skipme".into()],
            ..Default::default()
        });
        let named = |dir: &str| Scope::Roots([(1, dirs(&[r.join(dir).to_str().unwrap()]))].into());
        assert_eq!(
            resolve_scope(&full, &filter, named("a/skipme/x")),
            named("a")
        );
        assert_eq!(
            resolve_scope(&full, &filter, named("skipme")),
            Scope::roots([1])
        );
    }

    /// Timing, not a check: `cargo test --release -- --ignored --nocapture
    /// subtree_vs_full`. Over a ~200k-entry root (200 directories of 1000
    /// files): enumerate the root whole, then one 1k-entry directory; then
    /// build that subtree reconcile through the worker as a delta (R9) and
    /// as a full base, reporting wall time, output bytes and (Unix) the
    /// largest worker's peak RSS so far.
    #[test]
    #[ignore]
    fn subtree_vs_full_reconcile_timing() {
        let data = tempfile::tempdir().unwrap();
        let (view, _dirs) = two_roots();
        let r = view.roots[0].path.clone();
        for d in 0..200 {
            std::fs::create_dir(r.join(format!("d{d}"))).unwrap();
            for f in 0..1000 {
                std::fs::write(r.join(format!("d{d}/f{f}.txt")), b"").unwrap();
            }
        }
        let next = Arc::new(AtomicU64::new(10));
        let full = Arc::new(reconcile(view, Scope::All, data.path(), &next));
        let subtree = || Scope::Roots([(1, dirs(&[r.join("d7").to_str().unwrap()]))].into());
        let time = |scope: Scope| {
            let request = BuildRequest {
                view: full.clone(),
                mode: Mode::Reconcile,
                scope,
                filter: Default::default(),
                directory: data.path().to_path_buf(),
                next: next.clone(),
                progress: Default::default(),
                delta_budget: None,
            };
            let started = std::time::Instant::now();
            let mut spool = Vec::new();
            enumerate(&request, &mut spool, &AtomicBool::new(false)).unwrap();
            started.elapsed()
        };
        let whole = time(Scope::roots([1]));
        let one = time(subtree());
        eprintln!("enumerate 200k entries: whole root {whole:?}, one 1k-entry subtree {one:?}");

        // Peak RSS of the largest reaped child so far; bytes on Apple, KiB elsewhere.
        #[cfg(unix)]
        let worker_peak = || {
            let mut usage = unsafe { std::mem::zeroed::<libc::rusage>() };
            unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, &mut usage) };
            let scale = if cfg!(target_vendor = "apple") {
                1
            } else {
                1024
            };
            usage.ru_maxrss as u64 * scale / (1024 * 1024)
        };
        #[cfg(not(unix))]
        let worker_peak = || 0;
        // The delta first: the full build's worker then raises the peak.
        for budget in [Some(25_000), None, Some(25_000), None, Some(25_000), None] {
            let started = std::time::Instant::now();
            let scope = resolve_scope(&full, &Default::default(), subtree());
            let next = next.load(Ordering::Relaxed);
            let built = build_with(&full, Mode::Reconcile, scope, data.path(), budget, next);
            let elapsed = started.elapsed();
            assert_eq!(built.as_flush, budget.is_some());
            eprintln!(
                "subtree reconcile as {}: {elapsed:?}, {} records, {} bytes written, worker peak RSS {} MiB",
                if built.as_flush { "delta" } else { "base" },
                built.segment.len(),
                std::fs::metadata(&built.path).unwrap().len(),
                worker_peak(),
            );
        }
    }

    #[test]
    fn malformed_record_is_not_silently_omitted() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("input");
        let output = directory.path().join("output");
        std::fs::write(
            &input,
            b"{\"version\":3,\"sequence\":0,\"roots\":[],\"tombstones\":null}\nnot-a-record\n",
        )
        .unwrap();
        assert!(compile(&input, &output).is_err());
        assert!(!output.exists());
    }

    fn entry(id: u64, parent: u64, name: &str, directory: bool) -> Record {
        Record {
            id,
            parent,
            root: 1,
            name: name.as_bytes().to_vec(),
            flags: if directory { Record::DIRECTORY } else { 0 },
            identity: Identity {
                device: 1,
                key: id,
                birth: 1,
            },
            size: Some(id * 10),
            mtime: Some(id as i64),
        }
    }

    fn overlay(changes: &[(u64, Option<Record>)]) -> Arc<super::super::view::Overlay> {
        let mut overlay = super::super::view::Overlay::default();
        for (id, record) in changes {
            overlay.put(*id, record.clone());
        }
        Arc::new(overlay)
    }

    /// Build `view`'s pending changes into a delta the way the worker does,
    /// through a save and a mapped reopen.
    fn flush(view: &View, merge_deltas: bool, directory: &Path, name: &str) -> Arc<Segment> {
        let (records, tombstones) = delta_changes(view, merge_deltas);
        let mut segment = Segment::build(records, view.roots.clone(), view.epoch).unwrap();
        segment.tombstones = Some(tombstones);
        let path = directory.join(name);
        segment.save(&path).unwrap();
        Arc::new(unsafe { Segment::open(&path) }.unwrap())
    }

    /// Everything a client can observe from a view, for comparing levels.
    fn observe(view: &View) -> Vec<String> {
        use super::super::{ipc::Query, query};
        let cancel = AtomicBool::new(false);
        let mut seen = Vec::new();
        for record in view.records() {
            let path = view.path(record.id).unwrap();
            assert_eq!(view.resolve(&path), Some(record.id), "{path:?}");
            seen.push(format!("record {record:?} at {path:?}"));
        }
        let mut scoped = view.scoped_ids(Path::new("/fixture"), 10_000, &cancel);
        scoped.sort_unstable();
        seen.push(format!("scope {scoped:?}"));
        for text in ["", "report", "txt", "summary", "deep", "fresh", "zz"] {
            for (offset, filters) in [
                (0, vec![]),
                (40, vec![]),
                (0, vec![crate::search::filter::Filter::Ext("md".into())]),
            ] {
                let query = Query {
                    text: text.into(),
                    filters,
                    limit: 40,
                    offset,
                    fuzzy: false,
                    ..Default::default()
                };
                let page = query::search(view, &query, &cancel, &Default::default()).unwrap();
                let hits: Vec<_> = page.hits.iter().map(|h| (h.id, h.tier, &h.path)).collect();
                seen.push(format!(
                    "search {text:?} {offset}: {hits:?} more={}",
                    page.more
                ));
                let mut streamed = Vec::new();
                query::stream(view, &query, &cancel, |batch| {
                    streamed.extend(batch.hits.into_iter().map(|h| (h.id, h.tier, h.path)));
                    Ok(())
                })
                .unwrap();
                seen.push(format!("stream {text:?}: {streamed:?}"));
            }
        }
        seen
    }

    /// FIL-30: changes answer every query identically whether they sit in
    /// overlays, in flushed deltas, or in merged deltas.
    #[test]
    fn deltas_answer_exactly_like_the_overlays_they_replace() {
        let directory = tempfile::tempdir().unwrap();
        let roots = vec![Root {
            id: 1,
            path: "/fixture".into(),
            device: 1,
        }];
        let mut base = vec![
            entry(1, 0, "fixture", true),
            entry(2, 1, "a", true),
            entry(3, 1, "b", true),
            entry(4, 2, "report.txt", false),
            entry(5, 2, "notes.md", false),
            entry(6, 3, "report-final.txt", false),
            entry(7, 3, "c", true),
            entry(8, 7, "deep.txt", false),
        ];
        base.extend((100..200).map(|id| entry(id, 2, &format!("report-{id}.txt"), false)));
        let base = Arc::new(Segment::build(base, roots.clone(), 1).unwrap());
        let first = overlay(&[
            (4, Some(entry(4, 2, "summary.txt", false))),
            (5, None),
            // Deleting a directory hides its base descendants.
            (3, None),
            (9, Some(entry(9, 2, "report-new.txt", false))),
            (10, Some(entry(10, 2, "fresh", true))),
            (11, Some(entry(11, 10, "report-deep.md", false))),
            (150, None),
            (151, Some(entry(151, 10, "report-moved.txt", false))),
        ]);
        let second = overlay(&[
            // Rename a delta record, delete another, recreate a deleted name.
            (9, Some(entry(9, 2, "renamed.md", false))),
            (11, None),
            (12, Some(entry(12, 2, "notes.md", false))),
            (160, None),
            (13, Some(entry(13, 10, "zz-only-new.txt", false))),
        ]);
        let view = |deltas: Vec<Arc<Segment>>, layers, epoch| View {
            base: base.clone(),
            deltas,
            layers,
            roots: roots.clone(),
            epoch,
        };

        let overlays_only = view(vec![], vec![first.clone()], 2);
        let expected = observe(&overlays_only);
        let one = flush(&overlays_only, false, directory.path(), "one");
        assert_eq!(one.tombstones.as_deref(), Some(&[3, 5, 150][..]));
        assert_eq!(observe(&view(vec![one.clone()], vec![], 2)), expected);

        let overlays_only = view(vec![], vec![first, second.clone()], 3);
        let expected = observe(&overlays_only);
        let pending = view(vec![one.clone()], vec![second], 3);
        assert_eq!(observe(&pending), expected);
        // 11 existed only in the first delta: the flush keeps its tombstone
        // to hide that copy, but a merge of both deltas drops it.
        let two = flush(&pending, false, directory.path(), "two");
        assert_eq!(two.tombstones.as_deref(), Some(&[11, 160][..]));
        assert_eq!(
            observe(&view(vec![one.clone(), two.clone()], vec![], 3)),
            expected
        );
        let merged = flush(&pending, true, directory.path(), "merged");
        assert_eq!(merged.tombstones.as_deref(), Some(&[3, 5, 150, 160][..]));
        let merged = view(vec![merged], vec![], 3);
        assert_eq!(observe(&merged), expected);

        // Status counts follow the deltas without a base rewrite.
        let counted = |view: &View| {
            let mut counts = std::collections::BTreeMap::new();
            for record in view.records().filter(|r| r.parent != 0) {
                *counts.entry(record.root).or_insert(0) += 1;
            }
            counts
        };
        let only = view(vec![one.clone()], vec![], 2);
        let split = view(vec![one, two], vec![], 3);
        for view in [&only, &split, &merged] {
            assert_eq!(view.root_counts(), counted(view));
        }
        assert_ne!(only.root_counts(), counted(&view(vec![], vec![], 1)));
    }

    #[test]
    fn a_request_from_another_build_is_skew_not_failure() {
        // FIL-21: a request version, segment format or (from builds that
        // predate it) a missing format all mean a different build.
        let current = format!(
            "{{\"version\":{REQUEST_VERSION},\"sequence\":0,\"roots\":[],\"tombstones\":null"
        );
        for header in [
            format!(
                "{{\"version\":{},\"sequence\":0,\"roots\":[],\"tombstones\":null,\"segment_format\":\"{}\"}}",
                REQUEST_VERSION - 1,
                segment_format()
            ),
            format!("{current},\"segment_format\":\"FXSEG999\"}}"),
            format!("{current}}}"),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let input = directory.path().join("input");
            std::fs::write(&input, format!("{header}\n")).unwrap();
            let error = compile(&input, &directory.path().join("output")).unwrap_err();
            assert!(error.is::<WorkerSkew>(), "{header}: {error:#}");
        }
    }
}

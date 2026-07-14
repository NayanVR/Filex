//! macOS live watcher built directly on FSEvents (CoreServices).
//!
//! Raw FFI rather than the `notify` crate because the indexing design
//! depends on FSEvents' *persistent event IDs*: a future startup will pass
//! the last-seen ID as `sinceWhen` and replay the gap instead of rescanning
//! (docs/indexing-architecture.md §2). `notify` does not expose them.
//!
//! Assumptions about the OS:
//! - FSEvents delivers *canonical* absolute paths (symlinks in the watched
//!   root resolved), so the watched root must be canonicalized to match.
//! - With `kFSEventStreamCreateFlagFileEvents`, events arrive per-file, but
//!   the OS may still coalesce under load and set `MustScanSubDirs`, which
//!   we surface as [`FsDelta::Rescan`].
//! - Event flags alone don't reliably distinguish create/remove/rename
//!   (bits are sticky per-path within the latency window), so the delta is
//!   decided by `symlink_metadata` on the reported path: present ⇒ Upsert,
//!   absent ⇒ Remove. Rename pairs thus become Remove(old) + Upsert(new).

use std::ffi::{CStr, c_char, c_void};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;

use anyhow::{Context as _, Result, anyhow, bail};
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2_core_foundation::{CFArray, CFString};
use objc2_core_services as fse;

use super::watcher::FsDelta;

struct CallbackState {
    deltas: Sender<Vec<FsDelta>>,
    latest_event_id: Arc<AtomicU64>,
}

/// Watches one root via an FSEvents stream on a private dispatch queue.
/// Normalized [`FsDelta`]s are sent to the channel given at spawn time.
/// Dropping stops the stream.
pub struct FsEventsWatcher {
    stream: fse::FSEventStreamRef,
    state: *mut CallbackState,
    _queue: DispatchRetained<DispatchQueue>,
    latest_event_id: Arc<AtomicU64>,
}

// SAFETY: the stream handle is only used for stop/invalidate/release, which
// FSEvents permits from any thread once the stream is scheduled on a
// dispatch queue; the state pointer is freed only after invalidation.
unsafe impl Send for FsEventsWatcher {}

impl FsEventsWatcher {
    /// Start watching `root` (must be canonicalized). `since` is a
    /// previously persisted FSEvents event ID to replay from; `None` means
    /// "events from now on".
    pub fn spawn(
        root: &Path,
        since: Option<u64>,
        deltas: Sender<Vec<FsDelta>>,
    ) -> Result<Self> {
        let root_str = root
            .to_str()
            .ok_or_else(|| anyhow!("non-UTF-8 watch root {}", root.display()))?;

        // Seed the checkpoint so a save-before-any-event is still correct:
        // resuming streams continue from the persisted id; fresh streams
        // from the journal's current head.
        // SAFETY: trivial query with no arguments.
        let initial_id = since.unwrap_or_else(|| unsafe { fse::FSEventsGetCurrentEventId() });
        let latest_event_id = Arc::new(AtomicU64::new(initial_id));
        let state = Box::into_raw(Box::new(CallbackState {
            deltas,
            latest_event_id: latest_event_id.clone(),
        }));

        let mut context = fse::FSEventStreamContext {
            version: 0,
            info: state as *mut c_void,
            retain: None,
            release: None,
            copyDescription: None,
        };
        let path_strings = [CFString::from_str(root_str)];
        let paths = CFArray::from_retained_objects(&path_strings);

        // SAFETY: `context` and `paths` outlive the Create call (the stream
        // copies the context struct and retains the array).
        let stream = unsafe {
            fse::FSEventStreamCreate(
                None, // default allocator
                Some(fsevents_callback),
                &mut context,
                paths.as_opaque(),
                since.unwrap_or(fse::kFSEventStreamEventIdSinceNow),
                0.15, // seconds of OS-side coalescing latency
                fse::kFSEventStreamCreateFlagFileEvents
                    | fse::kFSEventStreamCreateFlagNoDefer,
            )
        };
        if stream.is_null() {
            // SAFETY: the callback never ran; reclaim the state box.
            drop(unsafe { Box::from_raw(state) });
            bail!("FSEventStreamCreate failed for {}", root.display());
        }

        let queue = DispatchQueue::new("dev.filex.fsevents", None);
        // SAFETY: stream is valid and not yet started.
        let started = unsafe {
            fse::FSEventStreamSetDispatchQueue(stream, Some(&queue));
            fse::FSEventStreamStart(stream)
        };
        if !started {
            // SAFETY: stream was scheduled but never started; tear it down
            // in the documented order before reclaiming state.
            unsafe {
                fse::FSEventStreamInvalidate(stream);
                fse::FSEventStreamRelease(stream);
                drop(Box::from_raw(state));
            }
            bail!("FSEventStreamStart failed for {}", root.display());
        }

        Ok(Self {
            stream,
            state,
            _queue: queue,
            latest_event_id,
        })
    }

    /// Highest FSEvents event ID seen so far — persist this and pass it as
    /// `since` on the next launch to replay missed events instead of
    /// rescanning. Zero until the first event arrives.
    pub fn latest_event_id(&self) -> u64 {
        self.latest_event_id.load(Ordering::Relaxed)
    }

    /// Shared handle to the latest event id that outlives the watcher —
    /// read *after* dropping the watcher and joining the writer, it is the
    /// exact checkpoint for which every event has been applied.
    pub fn latest_event_id_handle(&self) -> Arc<AtomicU64> {
        self.latest_event_id.clone()
    }
}

impl Drop for FsEventsWatcher {
    fn drop(&mut self) {
        // SAFETY: documented teardown order. After Invalidate returns the
        // callback will not run again, so the state box can be reclaimed.
        unsafe {
            fse::FSEventStreamStop(self.stream);
            fse::FSEventStreamInvalidate(self.stream);
            fse::FSEventStreamRelease(self.stream);
            drop(Box::from_raw(self.state));
        }
    }
}

/// The C callback: one invocation carries parallel arrays of paths, flags,
/// and event IDs. Kept thin — all interpretation lives in [`map_event`].
unsafe extern "C-unwind" fn fsevents_callback(
    _stream: fse::ConstFSEventStreamRef,
    info: *mut c_void,
    num_events: usize,
    event_paths: NonNull<c_void>,
    event_flags: NonNull<fse::FSEventStreamEventFlags>,
    event_ids: NonNull<fse::FSEventStreamEventId>,
) {
    // SAFETY: `info` is the CallbackState passed at stream creation; it is
    // freed only after FSEventStreamInvalidate, which precludes this call.
    let state = unsafe { &*(info as *const CallbackState) };
    let paths = event_paths.as_ptr() as *const *const c_char;
    let (event_flags, event_ids) = (event_flags.as_ptr(), event_ids.as_ptr());

    let mut deltas = Vec::with_capacity(num_events);
    for i in 0..num_events {
        // SAFETY: FSEvents guarantees `num_events` valid entries per array.
        let (path_ptr, flags, event_id) =
            unsafe { (*paths.add(i), *event_flags.add(i), *event_ids.add(i)) };
        if path_ptr.is_null() {
            continue;
        }
        // SAFETY: paths are NUL-terminated C strings valid for this call.
        let bytes = unsafe { CStr::from_ptr(path_ptr) }.to_bytes();
        let path = PathBuf::from(std::ffi::OsStr::from_bytes(bytes));

        state.latest_event_id.fetch_max(event_id, Ordering::Relaxed);

        let presence = std::fs::symlink_metadata(&path).ok().map(|m| m.is_dir());
        if let Some(delta) = map_event(path, flags, presence) {
            deltas.push(delta);
        }
    }
    if !deltas.is_empty() {
        // Receiver gone means we're shutting down; nothing to do.
        state.deltas.send(deltas).ok();
    }
}

/// Translate one FSEvents (path, flags) pair into a normalized delta.
/// `presence` is the current filesystem truth for the path: `Some(is_dir)`
/// if it exists, `None` if it doesn't. Pure — unit tested without FSEvents.
fn map_event(
    path: PathBuf,
    flags: fse::FSEventStreamEventFlags,
    presence: Option<bool>,
) -> Option<FsDelta> {
    if flags & fse::kFSEventStreamEventFlagMustScanSubDirs != 0 {
        return Some(FsDelta::Rescan { path });
    }
    const NAME_CHANGING: fse::FSEventStreamEventFlags = fse::kFSEventStreamEventFlagItemCreated
        | fse::kFSEventStreamEventFlagItemRemoved
        | fse::kFSEventStreamEventFlagItemRenamed;
    if flags & NAME_CHANGING == 0 {
        return None; // content/metadata chatter; names are unaffected
    }
    match presence {
        Some(is_dir) => Some(FsDelta::Upsert { path, is_dir }),
        None => Some(FsDelta::Remove { path }),
    }
}

/// Convenience for callers that don't have a canonical root yet.
#[allow(dead_code)]
pub fn canonical_watch_root(root: &Path) -> Result<PathBuf> {
    root.canonicalize()
        .with_context(|| format!("canonicalizing watch root {}", root.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    #[test]
    fn map_event_prefers_rescan_over_everything() {
        let delta = map_event(
            PathBuf::from("/x"),
            fse::kFSEventStreamEventFlagMustScanSubDirs
                | fse::kFSEventStreamEventFlagItemCreated,
            Some(true),
        );
        assert_eq!(delta, Some(FsDelta::Rescan { path: PathBuf::from("/x") }));
    }

    #[test]
    fn map_event_uses_filesystem_presence_not_sticky_flags() {
        // Created+Removed bits together (sticky within the latency window):
        // presence decides.
        let flags = fse::kFSEventStreamEventFlagItemCreated
            | fse::kFSEventStreamEventFlagItemRemoved;
        assert_eq!(
            map_event(PathBuf::from("/x"), flags, Some(false)),
            Some(FsDelta::Upsert { path: PathBuf::from("/x"), is_dir: false })
        );
        assert_eq!(
            map_event(PathBuf::from("/x"), flags, None),
            Some(FsDelta::Remove { path: PathBuf::from("/x") })
        );
    }

    #[test]
    fn map_event_ignores_content_only_changes() {
        assert_eq!(
            map_event(
                PathBuf::from("/x"),
                fse::kFSEventStreamEventFlagItemModified,
                Some(false),
            ),
            None
        );
    }

    /// Live smoke test against the real FSEvents daemon: create a file,
    /// expect a delta touching it (or a rescan of an ancestor).
    #[test]
    fn reports_real_file_creation() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (tx, rx) = mpsc::channel();
        let _watcher = FsEventsWatcher::spawn(&root, None, tx).unwrap();

        // Give the stream a moment to attach before generating the event.
        std::thread::sleep(Duration::from_millis(500));
        let target = root.join("fsevents-smoke.txt");
        fs::write(&target, b"x").unwrap();

        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            let Ok(batch) = rx.recv_timeout(Duration::from_millis(250)) else {
                continue;
            };
            let hit = batch.iter().any(|delta| match delta {
                FsDelta::Upsert { path, .. } | FsDelta::Remove { path } => path == &target,
                FsDelta::Rescan { path } => target.starts_with(path),
            });
            if hit {
                return;
            }
        }
        panic!("no FSEvents delta observed for {}", target.display());
    }
}

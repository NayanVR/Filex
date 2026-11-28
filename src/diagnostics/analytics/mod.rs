//! PostHog product analytics and error tracking (UI process only).
//! Design and the full event catalog: `docs/design-telemetry.md`.
//!
//! **No PII, by construction:**
//! - Product events are [`Event`]s, whose properties can only be numbers,
//!   bools or `&'static str` labels (see [`event`]). No path, file name,
//!   query, tag or typed text can become a property.
//! - Error text (panics, `error!` logs) and feedback pass
//!   [`exception::scrub_text`] before they leave.
//! - Events are anonymous (`process_person_profile: false`) and keyed by a
//!   random install ID that is deleted on opt-out ([`identity`]). GeoIP is
//!   disabled per event.
//!
//! **Gating:** nothing is sent unless the build has the `analytics`
//! feature *and* a PostHog key (`FILEX_POSTHOG_KEY`, baked by CI) *and* the
//! user's `share_diagnostics` setting is on. Every function here is a
//! cheap no-op otherwise, so call sites need no `cfg`.
//!
//! **Threading:** [`track`] is a channel send. Batching and HTTP happen on
//! a dedicated worker thread ([`pipeline`]), never the UI thread.

pub mod event;
pub mod exception;
pub mod identity;
#[cfg(feature = "analytics")]
pub mod layer;
pub mod pipeline;
pub mod snapshot;
pub mod wire;

#[cfg(feature = "analytics")]
mod client;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

pub use event::{Event, Prop, names};

use pipeline::{Limits, Msg, PostOutcome, Shared, Transport};
use wire::{Identity, Pending, SuperProps};

/// Fast check for hot paths (the tracing layer, keystroke observer).
static ENABLED: AtomicBool = AtomicBool::new(false);
static HANDLE: RwLock<Option<Handle>> = RwLock::new(None);

struct Handle {
    tx: Sender<Msg>,
    shared: Arc<Shared>,
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
}

/// What [`init`] learned, for the `app_launched` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Started {
    /// The install ID was minted this run (first launch with sharing on).
    pub new_install: bool,
}

/// Start analytics for the UI process if the build is configured and the
/// user consents. Returns `None` (and does nothing) otherwise. Safe to call
/// again after [`disable`], e.g. when the user re-enables sharing.
pub fn init(app: &'static str, version: &'static str, consent: bool) -> Option<Started> {
    if !consent || is_enabled() {
        return None;
    }
    let transport = transport()?;
    let id_path = identity::default_path()?;
    Some(start(transport, app, version, &id_path, Limits::default()))
}

#[cfg(feature = "analytics")]
fn transport() -> Option<Arc<dyn Transport>> {
    let (key, host) = client::config()?;
    Some(Arc::new(client::Ureq::new(&key, &host)))
}

#[cfg(not(feature = "analytics"))]
fn transport() -> Option<Arc<dyn Transport>> {
    None
}

/// Start the worker with an explicit transport (tests use a recorder).
fn start(
    transport: Arc<dyn Transport>,
    app: &'static str,
    version: &'static str,
    id_path: &std::path::Path,
    limits: Limits,
) -> Started {
    let install = identity::load_or_create(id_path);
    let shared = Arc::new(Shared {
        transport,
        identity: Identity {
            distinct_id: install.id,
            session_id: wire::uuid_v4(wire::random_bytes()),
        },
        sup: SuperProps { app, version },
    });
    let (tx, rx) = channel();
    let worker_shared = shared.clone();
    let worker = std::thread::Builder::new()
        .name("filex-analytics".into())
        .spawn(move || pipeline::run(rx, worker_shared, limits))
        .ok();
    if let Ok(mut slot) = HANDLE.write() {
        *slot = Some(Handle {
            tx,
            shared,
            worker: Mutex::new(worker),
        });
        ENABLED.store(true, Ordering::Release);
    }
    Started {
        new_install: install.created,
    }
}

/// Whether events are currently being collected.
pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::Acquire)
}

fn send(msg: Msg) {
    if !is_enabled() {
        return;
    }
    if let Ok(slot) = HANDLE.read()
        && let Some(handle) = slot.as_ref()
    {
        let _ = handle.tx.send(msg);
    }
}

/// Record a product event. Never blocks.
pub fn track(event: Event) {
    if is_enabled() {
        send(Msg::Event(Box::new(Pending::new(
            event,
            wire::random_bytes(),
        ))));
    }
}

/// Bump an aggregate counter (reported in `usage_summary` /
/// `app_session_ended`), for things too frequent to send one by one.
pub fn count(key: &'static str) {
    send(Msg::Count(key));
}

/// Queue an event whose properties were built by [`exception`] (already
/// scrubbed). Crate-private: product code goes through [`track`].
#[cfg_attr(not(feature = "analytics"), allow(dead_code))]
pub(crate) fn track_raw(name: &'static str, props: serde_json::Map<String, serde_json::Value>) {
    if !is_enabled() {
        return;
    }
    let mut pending = Pending::new(Event::new(name), wire::random_bytes());
    pending.raw_props = Some(props);
    send(Msg::Event(Box::new(pending)));
}

/// Flush and stop, emitting `app_session_ended`. Waits up to `timeout`
/// for the final POST, so call it off the UI thread or with a short
/// timeout at exit.
pub fn shutdown(timeout: Duration) -> bool {
    ENABLED.store(false, Ordering::Release);
    let Some(handle) = HANDLE.write().ok().and_then(|mut slot| slot.take()) else {
        return false;
    };
    let (reply, done) = channel();
    let _ = handle.tx.send(Msg::Shutdown(reply));
    let flushed = done.recv_timeout(timeout).unwrap_or(false);
    // Join only once it has finished; never hang exit on a slow network.
    if flushed && let Some(worker) = handle.worker.lock().ok().and_then(|mut w| w.take()) {
        let _ = worker.join();
    }
    flushed
}

/// The user turned sharing off: stop collecting, drop what's buffered,
/// and forget the install ID so a later opt-in is a new identity.
pub fn disable() {
    ENABLED.store(false, Ordering::Release);
    if let Ok(mut slot) = HANDLE.write() {
        // Dropping the sender ends the worker. Its final flush happens with
        // nothing buffered that wasn't consented to.
        slot.take();
    }
    if let Some(path) = identity::default_path() {
        identity::forget(&path);
    }
}

/// Deliver queued crash reports synchronously, deleting each one PostHog
/// accepted. Needs analytics running (consent). Call off the UI thread.
pub fn drain_crashes(dir: &std::path::Path) -> usize {
    let Some(shared) = HANDLE
        .read()
        .ok()
        .and_then(|slot| slot.as_ref().map(|h| h.shared.clone()))
    else {
        return 0;
    };
    crate::diagnostics::telemetry::drain(dir, |report| {
        let mut pending = Pending::new(Event::new(names::EXCEPTION), wire::random_bytes());
        pending.raw_props = Some(exception::crash_props(report));
        // Keep the crash's own time, not the upload's.
        if let Some(at) = chrono::DateTime::from_timestamp_millis(report.unix_millis as i64) {
            pending.at = at;
        }
        // A rejected report would be rejected forever: let it go.
        shared.send_now(std::slice::from_ref(&pending)) != PostOutcome::Retry
    })
}

/// Longest feedback message sent, in chars.
const MAX_FEEDBACK_CHARS: usize = 5_000;

/// Send feedback the user typed. Pressing Send is explicit consent, so this
/// works with sharing off too, under a throwaway ID that links to nothing.
/// The text is scrubbed like error text (paths, file names, emails, URLs).
/// Blocks on the POST: call it off the UI thread.
pub fn send_feedback(message: &str, version: &'static str) -> bool {
    let props = feedback_props(message);
    let mut pending = Pending::new(Event::new(names::FEEDBACK_SUBMITTED), wire::random_bytes());
    pending.raw_props = Some(props);
    let shared = HANDLE
        .read()
        .ok()
        .and_then(|slot| slot.as_ref().map(|h| h.shared.clone()));
    let shared = match shared {
        Some(shared) => shared,
        None => {
            let Some(transport) = transport() else {
                return false;
            };
            Arc::new(Shared {
                transport,
                identity: Identity {
                    distinct_id: wire::uuid_v4(wire::random_bytes()),
                    session_id: wire::uuid_v4(wire::random_bytes()),
                },
                sup: SuperProps {
                    app: "filex",
                    version,
                },
            })
        }
    };
    shared.send_now(&[pending]) == PostOutcome::Delivered
}

fn feedback_props(message: &str) -> serde_json::Map<String, serde_json::Value> {
    let scrubbed = exception::scrub_text(message);
    let clipped: String = scrubbed.chars().take(MAX_FEEDBACK_CHARS).collect();
    let mut props = serde_json::Map::new();
    props.insert("message".into(), clipped.into());
    props.insert("length".into(), message.chars().count().into());
    props
}

/// Resident memory of this process, or `None` if the OS won't say.
#[cfg(feature = "analytics")]
pub fn process_rss_bytes() -> Option<u64> {
    memory_stats::memory_stats().map(|stats| stats.physical_mem as u64)
}

#[cfg(not(feature = "analytics"))]
pub fn process_rss_bytes() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use pipeline::tests::Recorder;

    /// The global handle is process-wide; tests that start it serialize.
    static GLOBAL: Mutex<()> = Mutex::new(());

    fn quick() -> Limits {
        Limits {
            batch: 50,
            interval: Duration::from_secs(60),
            ..Limits::default()
        }
    }

    #[test]
    fn nothing_is_recorded_until_started() {
        let _guard = GLOBAL.lock().unwrap_or_else(|e| e.into_inner());
        assert!(!is_enabled());
        track(Event::new("ignored"));
        count("ignored");
        assert_eq!(drain_crashes(std::path::Path::new("/nonexistent")), 0);
    }

    #[test]
    fn start_track_shutdown_round_trip() {
        let _guard = GLOBAL.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let rec = Arc::new(Recorder::default());
        let started = start(
            rec.clone(),
            "filex",
            "1.2.3",
            &dir.path().join("id"),
            quick(),
        );
        assert!(started.new_install);
        track(Event::new(names::FOLDER_OPENED).with("via", "up"));
        assert!(shutdown(Duration::from_secs(5)));
        assert!(!is_enabled());

        let events = rec.events();
        assert_eq!(events[0]["event"], "folder_opened");
        assert_eq!(events[0]["properties"]["$app_version"], "1.2.3");
        assert_eq!(events.last().unwrap()["event"], "app_session_ended");
        // Same install, same session for both.
        assert_eq!(events[0]["distinct_id"], events[1]["distinct_id"]);
        assert_eq!(events[0]["session_id"], events[1]["session_id"]);
    }

    #[test]
    fn drained_crashes_keep_their_time_and_are_deleted() {
        let _guard = GLOBAL.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let queue = dir.path().join("crash-reports");
        let report = crate::diagnostics::telemetry::CrashReport {
            schema: crate::diagnostics::telemetry::SCHEMA,
            app: "filex-indexd".into(),
            version: "0.1.0".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            unix_millis: 1_700_000_000_123,
            thread: "main".into(),
            message: "boom".into(),
            location: None,
            backtrace: String::new(),
        };
        crate::diagnostics::telemetry::enqueue(&queue, &report, 10).unwrap();
        let rec = Arc::new(Recorder::default());
        start(
            rec.clone(),
            "filex",
            "0.5.0",
            &dir.path().join("id"),
            quick(),
        );
        assert_eq!(drain_crashes(&queue), 1);
        shutdown(Duration::from_secs(5));
        assert!(crate::diagnostics::telemetry::queued(&queue).is_empty());

        let crash = &rec.events()[0];
        assert_eq!(crash["event"], "$exception");
        assert_eq!(crash["timestamp"], "2023-11-14T22:13:20.123Z");
        assert_eq!(crash["properties"]["$app_version"], "0.1.0");
        assert_eq!(crash["properties"]["process"], "filex-indexd");
    }

    #[test]
    fn feedback_is_scrubbed_and_never_carries_an_email() {
        let props = feedback_props("search is slow in /Users/ann/Work/x, mail ann@x.io");
        let message = props["message"].as_str().unwrap();
        assert!(!message.contains("ann"), "{message}");
        assert!(message.contains("search is slow"));
        assert!(props.get("email").is_none());
    }
}

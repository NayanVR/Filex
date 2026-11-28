//! The delivery worker: one background thread owns the buffer, batches
//! events, retries with backoff, caps per-event volume, summarizes the
//! session, and spools unsent events to disk at quit.
//!
//! Nothing here runs on the UI thread. `track` is a channel send. HTTP
//! only happens on this worker.
//!
//! **Spool.** Quit never waits on the network. At shutdown the worker
//! writes whatever is unsent (including `app_session_ended`) to
//! `<data_local_dir>/filex/analytics-spool.jsonl`, and the next run sends
//! it first. That also keeps the events of a session that ended offline.
//! Opting out deletes the spool ([`super::disable`]).

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::catalog::{AppSessionEnded, UsageSummary};
use super::event::Event;
use super::schema::Tally;
use super::wire::{self, Identity, Pending, SuperProps};

/// What one POST achieved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostOutcome {
    /// 2xx: the batch is PostHog's now.
    Delivered,
    /// Offline, timeout, 429 or 5xx: keep the events and try later.
    Retry,
    /// Any other 4xx: the batch will never be accepted, so drop it.
    Reject,
}

/// Sends one batch body. Implemented over `ureq` in `client.rs`, by
/// [`LogTransport`] for local development, and by a recorder in tests.
pub trait Transport: Send + Sync {
    fn post(&self, body: &Value) -> PostOutcome;
}

/// Writes each batch to the log instead of the network
/// (`FILEX_ANALYTICS=log`), so events can be inspected with no PostHog
/// project: `RUST_LOG=filex=info` shows every batch.
pub struct LogTransport;

impl Transport for LogTransport {
    fn post(&self, body: &Value) -> PostOutcome {
        for event in body["batch"].as_array().into_iter().flatten() {
            tracing::info!(
                target: "filex::analytics_log",
                "{} {}",
                event["event"].as_str().unwrap_or("?"),
                event["properties"]
            );
        }
        PostOutcome::Delivered
    }
}

/// Tunables, overridable in tests.
#[derive(Debug, Clone)]
pub struct Limits {
    /// Flush once this many events are buffered.
    pub batch: usize,
    /// Flush at least this often while events are waiting.
    pub interval: Duration,
    /// Oldest events are dropped past this (memory and spool bound).
    pub max_buffer: usize,
    /// Emit a `usage_summary` this often.
    pub summary_every: Duration,
    /// Upper bound for retry backoff.
    pub max_backoff: Duration,
    /// Most events of one name per session; the rest are counted in the
    /// summary's `dropped`. A guard against a bug spending the quota.
    pub max_per_event: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            batch: 20,
            interval: Duration::from_secs(30),
            max_buffer: 1_000,
            summary_every: Duration::from_secs(30 * 60),
            max_backoff: Duration::from_secs(5 * 60),
            max_per_event: 2_000,
        }
    }
}

pub enum Msg {
    Event(Box<Pending>),
    /// Bump a named counter (one keyboard shortcut) reported in aggregate.
    Count(&'static str),
    /// Flush now; replies whether the buffer emptied.
    Flush(Sender<bool>),
    /// Emit the session summary, spool what's unsent, and stop. Replies
    /// once the spool is written (local disk, never the network).
    Shutdown(Sender<bool>),
}

/// Shared, immutable facts the worker and synchronous senders (crash
/// drain, feedback) need.
pub struct Shared {
    pub transport: Arc<dyn Transport>,
    pub identity: Identity,
    pub sup: SuperProps,
    /// Where unsent events wait between runs; `None` disables spooling.
    pub spool: Option<PathBuf>,
}

impl Shared {
    /// POST these events right now, on the calling thread.
    pub fn send_now(&self, pending: &[Pending]) -> PostOutcome {
        let events = pending
            .iter()
            .map(|p| wire::wire_event(p, &self.identity, &self.sup))
            .collect();
        self.transport
            .post(&wire::batch_body(events, chrono::Utc::now()))
    }
}

/// `<data_local_dir>/filex/analytics-spool.jsonl`.
pub fn default_spool_path() -> Option<PathBuf> {
    Some(
        dirs::data_local_dir()?
            .join("filex")
            .join("analytics-spool.jsonl"),
    )
}

/// Read and delete the spool: one wire event per line. A torn or corrupt
/// line is skipped. Assumes the file is only ever written by
/// [`write_spool`] of this or an earlier build.
pub fn take_spool(path: &Path) -> Vec<Value> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let _ = std::fs::remove_file(path);
    text.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event| event["event"].is_string())
        .collect()
}

/// Write unsent wire events as JSON lines, replacing any previous spool
/// (the worker took it at start, so nothing is lost). Writes to a temp
/// file and renames, so a crash mid-write leaves the old file or none.
fn write_spool(path: &Path, events: &VecDeque<Value>) {
    if events.is_empty() {
        let _ = std::fs::remove_file(path);
        return;
    }
    let mut text = String::new();
    for event in events {
        text.push_str(&event.to_string());
        text.push('\n');
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = path.with_extension("jsonl.tmp");
    if std::fs::write(&tmp, text).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

struct Worker {
    shared: Arc<Shared>,
    limits: Limits,
    buf: VecDeque<Value>,
    /// Events sent per name this session.
    events: BTreeMap<&'static str, u64>,
    /// Events dropped by `max_per_event` since the last summary.
    dropped: BTreeMap<&'static str, u64>,
    /// Counters since the last summary.
    counters: BTreeMap<&'static str, u64>,
    started: Instant,
    last_summary: Instant,
    retry_at: Option<Instant>,
    backoff: Duration,
}

/// Run the worker until `Shutdown` or the channel closes. A closed
/// channel means sharing was turned off: the buffer is discarded, not
/// spooled.
pub fn run(rx: Receiver<Msg>, shared: Arc<Shared>, limits: Limits) {
    let now = Instant::now();
    let spooled = shared.spool.as_deref().map(take_spool).unwrap_or_default();
    let mut worker = Worker {
        shared,
        backoff: Duration::from_secs(5).min(limits.max_backoff),
        limits,
        buf: spooled.into(),
        events: BTreeMap::new(),
        dropped: BTreeMap::new(),
        counters: BTreeMap::new(),
        started: now,
        last_summary: now,
        retry_at: None,
    };
    loop {
        match rx.recv_timeout(worker.limits.interval) {
            Ok(Msg::Event(pending)) => {
                worker.push(*pending);
                if worker.buf.len() >= worker.limits.batch {
                    worker.flush(false);
                }
            }
            Ok(Msg::Count(key)) => *worker.counters.entry(key).or_default() += 1,
            Ok(Msg::Flush(reply)) => {
                let _ = reply.send(worker.flush(true));
            }
            Ok(Msg::Shutdown(reply)) => {
                worker.session_ended();
                if let Some(path) = worker.shared.spool.clone() {
                    write_spool(&path, &worker.buf);
                }
                let _ = reply.send(true);
                return;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        if worker.last_summary.elapsed() >= worker.limits.summary_every {
            worker.usage_summary();
        }
        if !worker.buf.is_empty() {
            worker.flush(false);
        }
    }
}

impl Worker {
    fn push(&mut self, pending: Pending) {
        let name = pending.event.name;
        let sent = self.events.entry(name).or_default();
        if *sent >= self.limits.max_per_event {
            *self.dropped.entry(name).or_default() += 1;
            return;
        }
        *sent += 1;
        let wire = wire::wire_event(&pending, &self.shared.identity, &self.shared.sup);
        tracing::debug!(target: "filex::analytics_log", "queued {name} {}", wire["properties"]);
        self.buf.push_back(wire);
        while self.buf.len() > self.limits.max_buffer {
            self.buf.pop_front();
        }
    }

    /// Send buffered events in batches. Respects the backoff window unless
    /// `force`. Returns whether the buffer is now empty.
    fn flush(&mut self, force: bool) -> bool {
        if !force && self.retry_at.is_some_and(|at| Instant::now() < at) {
            return self.buf.is_empty();
        }
        while !self.buf.is_empty() {
            let n = self.buf.len().min(self.limits.batch.max(1));
            let events: Vec<Value> = self.buf.iter().take(n).cloned().collect();
            let body = wire::batch_body(events, chrono::Utc::now());
            match self.shared.transport.post(&body) {
                PostOutcome::Delivered | PostOutcome::Reject => {
                    self.buf.drain(..n);
                    self.retry_at = None;
                    self.backoff = Duration::from_secs(5).min(self.limits.max_backoff);
                }
                PostOutcome::Retry => {
                    self.retry_at = Some(Instant::now() + self.backoff);
                    self.backoff = (self.backoff * 2).min(self.limits.max_backoff);
                    return false;
                }
            }
        }
        true
    }

    fn take_tallies(&mut self) -> (Tally<&'static str>, Tally<&'static str>) {
        let counters = Tally(std::mem::take(&mut self.counters).into_iter().collect());
        let dropped = Tally(std::mem::take(&mut self.dropped).into_iter().collect());
        (counters, dropped)
    }

    fn usage_summary(&mut self) {
        let (counters, dropped) = self.take_tallies();
        let event = UsageSummary {
            duration_s: self.started.elapsed().as_secs(),
            since_last_summary_s: self.last_summary.elapsed().as_secs(),
            counters,
            dropped,
        };
        self.last_summary = Instant::now();
        self.push(Pending::new(Event::typed(&event), wire::random_bytes()));
    }

    fn session_ended(&mut self) {
        let (counters, dropped) = self.take_tallies();
        let event = AppSessionEnded {
            duration_s: self.started.elapsed().as_secs(),
            events: Tally(self.events.iter().map(|(k, v)| (*k, *v)).collect()),
            counters,
            dropped,
        };
        self.push(Pending::new(Event::typed(&event), wire::random_bytes()));
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::mpsc::channel;

    /// Records bodies; answers from a script, then `Delivered`.
    #[derive(Default)]
    pub struct Recorder {
        pub bodies: Mutex<Vec<Value>>,
        pub script: Mutex<VecDeque<PostOutcome>>,
    }

    impl Transport for Recorder {
        fn post(&self, body: &Value) -> PostOutcome {
            self.bodies.lock().unwrap().push(body.clone());
            self.script
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(PostOutcome::Delivered)
        }
    }

    impl Recorder {
        pub fn events(&self) -> Vec<Value> {
            self.bodies
                .lock()
                .unwrap()
                .iter()
                .flat_map(|b| b["batch"].as_array().cloned().unwrap_or_default())
                .collect()
        }
    }

    pub fn shared(transport: Arc<Recorder>, spool: Option<PathBuf>) -> Arc<Shared> {
        Arc::new(Shared {
            transport,
            identity: Identity {
                distinct_id: "install".into(),
                session_id: "session".into(),
            },
            sup: SuperProps {
                app: "filex",
                version: "0.0.0",
            },
            spool,
        })
    }

    fn quick() -> Limits {
        Limits {
            batch: 2,
            interval: Duration::from_millis(10),
            max_buffer: 3,
            summary_every: Duration::from_secs(3600),
            max_backoff: Duration::from_millis(1),
            max_per_event: 100,
        }
    }

    fn pending(name: &'static str) -> Box<Pending> {
        Box::new(Pending::new(Event::raw(name), [0; 10]))
    }

    fn worker(rec: &Arc<Recorder>, limits: Limits) -> Worker {
        Worker {
            shared: shared(rec.clone(), None),
            limits,
            buf: VecDeque::new(),
            events: BTreeMap::new(),
            dropped: BTreeMap::new(),
            counters: BTreeMap::new(),
            started: Instant::now(),
            last_summary: Instant::now(),
            retry_at: None,
            backoff: Duration::from_millis(1),
        }
    }

    fn names(events: &[Value]) -> Vec<&str> {
        events.iter().filter_map(|e| e["event"].as_str()).collect()
    }

    #[test]
    fn shutdown_spools_instead_of_posting_and_next_run_sends_it() {
        let dir = tempfile::tempdir().unwrap();
        let spool = dir.path().join("spool.jsonl");
        // An offline network: nothing may be posted at shutdown anyway.
        let rec = Arc::new(Recorder::default());
        rec.script.lock().unwrap().extend([PostOutcome::Retry; 8]);
        let limits = Limits {
            interval: Duration::from_secs(60),
            batch: 50,
            ..quick()
        };
        let (tx, rx) = channel();
        let first = shared(rec.clone(), Some(spool.clone()));
        let handle = std::thread::spawn({
            let limits = limits.clone();
            move || run(rx, first, limits)
        });
        tx.send(Msg::Event(pending("folder_opened"))).unwrap();
        tx.send(Msg::Count("filex::GoUp")).unwrap();
        tx.send(Msg::Count("filex::GoUp")).unwrap();
        let (reply, done) = channel();
        tx.send(Msg::Shutdown(reply)).unwrap();
        assert!(done.recv().unwrap());
        handle.join().unwrap();
        // Offline the whole run: everything unsent is on disk now.
        let spooled = std::fs::read_to_string(&spool).unwrap();
        assert_eq!(spooled.lines().count(), 2, "{spooled}");

        // Next run: the spool goes out first, then the file is gone.
        let rec2 = Arc::new(Recorder::default());
        let (tx, rx) = channel();
        let second = shared(rec2.clone(), Some(spool.clone()));
        let handle = std::thread::spawn(move || run(rx, second, limits));
        let (reply, flushed) = channel();
        tx.send(Msg::Flush(reply)).unwrap();
        assert!(flushed.recv().unwrap());
        drop(tx);
        handle.join().unwrap();
        let events = rec2.events();
        assert_eq!(names(&events), ["folder_opened", "app_session_ended"]);
        let summary = &events[1]["properties"];
        assert_eq!(summary["counters"]["filex::GoUp"], 2);
        assert_eq!(summary["events"]["folder_opened"], 1);
        assert!(!spool.exists());
    }

    #[test]
    fn revoked_consent_discards_instead_of_spooling() {
        let dir = tempfile::tempdir().unwrap();
        let spool = dir.path().join("spool.jsonl");
        let rec = Arc::new(Recorder::default());
        rec.script.lock().unwrap().extend([PostOutcome::Retry; 8]);
        let (tx, rx) = channel();
        let s = shared(rec, Some(spool.clone()));
        let limits = Limits {
            batch: 50,
            ..quick()
        };
        let handle = std::thread::spawn(move || run(rx, s, limits));
        tx.send(Msg::Event(pending("a"))).unwrap();
        drop(tx);
        handle.join().unwrap();
        assert!(!spool.exists());
    }

    #[test]
    fn corrupt_spool_lines_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let spool = dir.path().join("spool.jsonl");
        std::fs::write(&spool, "{\"event\":\"a\"}\n{torn\n{\"no_event\":1}\n").unwrap();
        assert_eq!(names(&take_spool(&spool)), ["a"]);
        assert!(!spool.exists());
    }

    #[test]
    fn retry_keeps_events_and_reject_drops_them() {
        let rec = Arc::new(Recorder::default());
        rec.script
            .lock()
            .unwrap()
            .extend([PostOutcome::Retry, PostOutcome::Reject]);
        let (tx, rx) = channel();
        let s = shared(rec.clone(), None);
        let handle = std::thread::spawn(move || run(rx, s, quick()));
        tx.send(Msg::Event(pending("a"))).unwrap();
        let (reply, flushed) = channel();
        std::thread::sleep(Duration::from_millis(30));
        tx.send(Msg::Flush(reply)).unwrap();
        assert!(flushed.recv().unwrap());
        drop(tx);
        handle.join().unwrap();
        let bodies = rec.bodies.lock().unwrap();
        assert!(bodies.len() >= 2, "retried after the first failure");
        assert_eq!(bodies[0]["batch"][0]["event"], "a");
        assert_eq!(bodies[1]["batch"][0]["event"], "a");
    }

    #[test]
    fn buffer_is_bounded_dropping_oldest() {
        let rec = Arc::new(Recorder::default());
        let mut worker = worker(&rec, quick());
        for name in ["e1", "e2", "e3", "e4", "e5"] {
            worker.push(*pending(name));
        }
        assert_eq!(names(worker.buf.make_contiguous()), ["e3", "e4", "e5"]);
        assert!(worker.flush(true));
        assert_eq!(rec.bodies.lock().unwrap().len(), 2, "batches of two");
    }

    #[test]
    fn per_event_cap_counts_what_it_drops() {
        let rec = Arc::new(Recorder::default());
        let mut worker = worker(
            &rec,
            Limits {
                max_buffer: 100,
                max_per_event: 2,
                ..quick()
            },
        );
        for _ in 0..5 {
            worker.push(*pending("noisy"));
        }
        worker.push(*pending("quiet"));
        worker.session_ended();
        let kept = names(worker.buf.make_contiguous()).join(",");
        assert_eq!(kept, "noisy,noisy,quiet,app_session_ended");
        let summary = worker.buf.back().unwrap();
        assert_eq!(summary["properties"]["dropped"]["noisy"], 3);
        assert_eq!(summary["properties"]["events"]["noisy"], 2);
    }
}

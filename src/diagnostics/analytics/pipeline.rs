//! The delivery worker: one background thread owns the buffer, batches
//! events, retries with backoff, and summarizes the session. Transport is
//! a trait so the whole pipeline is tested without a network.
//!
//! Nothing here runs on the UI thread. `track` is a channel send, and the
//! HTTP POST happens on the worker.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::event::{Event, Prop, names};
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

/// Sends one batch body. Implemented over `ureq` in `client.rs`; tests
/// substitute a recorder.
pub trait Transport: Send + Sync {
    fn post(&self, body: &Value) -> PostOutcome;
}

/// Tunables, overridable in tests.
#[derive(Debug, Clone)]
pub struct Limits {
    /// Flush once this many events are buffered.
    pub batch: usize,
    /// Flush at least this often while events are waiting.
    pub interval: Duration,
    /// Oldest events are dropped past this, so being offline for a week
    /// can't grow memory without bound.
    pub max_buffer: usize,
    /// Emit a `usage_summary` (counters) this often.
    pub summary_every: Duration,
    /// Upper bound for retry backoff.
    pub max_backoff: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            batch: 20,
            interval: Duration::from_secs(30),
            max_buffer: 1_000,
            summary_every: Duration::from_secs(30 * 60),
            max_backoff: Duration::from_secs(5 * 60),
        }
    }
}

pub enum Msg {
    Event(Box<Pending>),
    /// Bump a named counter (e.g. one keyboard shortcut) that is reported
    /// in aggregate, not per occurrence.
    Count(&'static str),
    /// Flush now; replies whether the buffer emptied.
    Flush(Sender<bool>),
    /// Emit the session summary, flush, and stop; replies when done.
    Shutdown(Sender<bool>),
}

/// Shared, immutable facts both the worker and synchronous senders
/// (crash drain, feedback) need.
pub struct Shared {
    pub transport: Arc<dyn Transport>,
    pub identity: Identity,
    pub sup: SuperProps,
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

struct Worker {
    shared: Arc<Shared>,
    limits: Limits,
    buf: VecDeque<Value>,
    /// Per-event-name totals for this session.
    events: BTreeMap<&'static str, u64>,
    /// Counters since the last summary.
    counters: BTreeMap<&'static str, u64>,
    started: Instant,
    last_summary: Instant,
    retry_at: Option<Instant>,
    backoff: Duration,
}

/// Run the worker until `Shutdown` or the channel closes.
pub fn run(rx: Receiver<Msg>, shared: Arc<Shared>, limits: Limits) {
    let now = Instant::now();
    let mut worker = Worker {
        shared,
        backoff: Duration::from_secs(5).min(limits.max_backoff),
        limits,
        buf: VecDeque::new(),
        events: BTreeMap::new(),
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
                worker.summarize(names::APP_SESSION_ENDED);
                let _ = reply.send(worker.flush(true));
                return;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                worker.flush(true);
                return;
            }
        }
        if worker.last_summary.elapsed() >= worker.limits.summary_every {
            worker.summarize(names::USAGE_SUMMARY);
        }
        if !worker.buf.is_empty() {
            worker.flush(false);
        }
    }
}

impl Worker {
    fn push(&mut self, pending: Pending) {
        *self.events.entry(pending.event.name).or_default() += 1;
        let wire = wire::wire_event(&pending, &self.shared.identity, &self.shared.sup);
        self.buf.push_back(wire);
        while self.buf.len() > self.limits.max_buffer {
            self.buf.pop_front();
        }
    }

    /// Send buffered events in batches. Respects the backoff window unless
    /// `force` (an explicit flush or shutdown gets one real attempt).
    /// Returns whether the buffer is now empty.
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

    /// Queue a summary event: session length, how often each event fired,
    /// and the aggregated counters (shortcuts). Counters reset after.
    fn summarize(&mut self, name: &'static str) {
        let counters = std::mem::take(&mut self.counters);
        let mut event = Event::new(name)
            .with("duration_s", self.started.elapsed().as_secs())
            .with(
                "since_last_summary_s",
                self.last_summary.elapsed().as_secs(),
            );
        if !counters.is_empty() {
            event = event.with("counters", Prop::Tally(counters.into_iter().collect()));
        }
        if name == names::APP_SESSION_ENDED {
            let events = self.events.iter().map(|(k, v)| (*k, *v)).collect();
            event = event.with("events", Prop::Tally(events));
        }
        self.last_summary = Instant::now();
        self.push(Pending::new(event, wire::random_bytes()));
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

    pub fn shared(transport: Arc<Recorder>) -> Arc<Shared> {
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
        })
    }

    fn quick() -> Limits {
        Limits {
            batch: 2,
            interval: Duration::from_millis(10),
            max_buffer: 3,
            summary_every: Duration::from_secs(3600),
            max_backoff: Duration::from_millis(1),
        }
    }

    fn pending(name: &'static str) -> Box<Pending> {
        Box::new(Pending::new(Event::new(name), [0; 10]))
    }

    #[test]
    fn shutdown_flushes_events_and_reports_the_session() {
        let rec = Arc::new(Recorder::default());
        let (tx, rx) = channel();
        let shared = shared(rec.clone());
        let handle = std::thread::spawn(move || run(rx, shared, quick()));
        tx.send(Msg::Event(pending("folder_opened"))).unwrap();
        tx.send(Msg::Count("filex::GoUp")).unwrap();
        tx.send(Msg::Count("filex::GoUp")).unwrap();
        let (reply, done) = channel();
        tx.send(Msg::Shutdown(reply)).unwrap();
        assert!(done.recv().unwrap());
        handle.join().unwrap();

        let events = rec.events();
        let names: Vec<&str> = events.iter().filter_map(|e| e["event"].as_str()).collect();
        assert_eq!(names, ["folder_opened", "app_session_ended"]);
        let summary = &events[1]["properties"];
        assert_eq!(summary["counters"]["filex::GoUp"], 2);
        assert_eq!(summary["events"]["folder_opened"], 1);
    }

    #[test]
    fn retry_keeps_events_and_reject_drops_them() {
        let rec = Arc::new(Recorder::default());
        rec.script
            .lock()
            .unwrap()
            .extend([PostOutcome::Retry, PostOutcome::Reject]);
        let (tx, rx) = channel();
        let shared = shared(rec.clone());
        let handle = std::thread::spawn(move || run(rx, shared, quick()));
        tx.send(Msg::Event(pending("a"))).unwrap();
        // First attempt: Retry (kept). Explicit flush: Reject (dropped).
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
        let mut worker = Worker {
            shared: shared(rec.clone()),
            limits: quick(),
            buf: VecDeque::new(),
            events: BTreeMap::new(),
            counters: BTreeMap::new(),
            started: Instant::now(),
            last_summary: Instant::now(),
            retry_at: None,
            backoff: Duration::from_millis(1),
        };
        for name in ["e1", "e2", "e3", "e4", "e5"] {
            worker.push(*pending(name));
        }
        let kept: Vec<&str> = worker
            .buf
            .iter()
            .filter_map(|e| e["event"].as_str())
            .collect();
        assert_eq!(kept, ["e3", "e4", "e5"]);
        assert!(worker.flush(true));
        assert_eq!(rec.bodies.lock().unwrap().len(), 2, "batches of two");
    }
}

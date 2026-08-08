//! Sentry integration (UI process only) — the sole network transport for
//! crash/error reporting, performance measurements, and release-health
//! sessions. On-by-default and opt-out: the `crash_reports` setting is
//! consent (default true) and the only thing that turns this off short of
//! building without the `observability` feature.
//!
//! Everything here upholds the same privacy invariant as [`crate::telemetry`]:
//! **no path-shaped data ever leaves the machine.** It is enforced twice —
//! the crash reports drained here were already
//! [`scrub`](crate::telemetry::scrub)bed at capture time, and the
//! `before_send` / `before_breadcrumb` hooks scrub every live event
//! (including anything the `sentry-tracing` layer feeds in from `error!` /
//! `warn!` logs) as a backstop. Sentry is initialised only with the user's
//! consent and only when a DSN is present in the environment; no DSN ships
//! in a bare build, so a build without a CI-provided DSN phones home to
//! nothing.
//!
//! Panics are *not* delivered live here — Sentry's `panic` integration is
//! deliberately disabled (see Cargo.toml). They are captured by
//! [`crate::telemetry`]'s hook into a durable on-disk queue and drained via
//! [`drain_crashes_to_sentry`] on the next launch, which survives aborts and
//! SIGKILL where an in-process flush would be lost.

use std::borrow::Cow;
use std::sync::Arc;

use sentry::protocol::{Event, Level, Value};
use sentry::{ClientInitGuard, ClientOptions};

use crate::telemetry::{self, CrashReport};

/// Environment variable holding the Sentry DSN. Unset or empty disables the
/// integration entirely, so a build with no CI-provided DSN sends nothing
/// even with consent granted.
const DSN_ENV: &str = "FILEX_SENTRY_DSN";

/// The Sentry DSN, or `None` when unset/empty (which disables the whole
/// integration).
///
/// Prefers the *runtime* env var (a dev override), falling back to the DSN
/// baked in at **compile time** by CI via the same variable. The
/// compile-time path is what makes shipped builds work: an end user has no
/// `FILEX_SENTRY_DSN` in their environment, so the DSN must travel in the
/// binary. A Sentry DSN is a public client key (like the embedded update
/// public key), not a secret, so embedding it is expected. When CI hasn't
/// set the variable, `option_env!` yields nothing and Sentry stays off.
fn dsn() -> Option<String> {
    std::env::var(DSN_ENV)
        .ok()
        .or_else(|| option_env!("FILEX_SENTRY_DSN").map(str::to_string))
        .filter(|dsn| !dsn.trim().is_empty())
}

/// Initialise Sentry for the UI process. Returns the guard that must be
/// held for the whole process lifetime (dropping it flushes pending events
/// and disables the client); `None` when disabled by lack of consent or a
/// missing DSN.
///
/// `consent` mirrors the `crash_reports` setting — this must never run
/// without it. PII is off, no server name is sent, and both the event and
/// breadcrumb hooks run the path scrubber.
pub fn init(app: &'static str, version: &'static str, consent: bool) -> Option<ClientInitGuard> {
    if !consent {
        return None;
    }
    let dsn = dsn()?;
    // `ClientOptions` is `#[non_exhaustive]`, so build from its default and
    // set fields.
    let mut options = ClientOptions::default();
    options.release = Some(Cow::Borrowed(version));
    // Separate real-user data from developer runs (a debug build reporting
    // to the same DSN would otherwise pollute the release-health numbers).
    options.environment = Some(Cow::Borrowed(if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    }));
    // Release health: start a session on init and close it on guard drop, so
    // Sentry reports crash-free session/user rates and per-release adoption —
    // the headline insight for a desktop app. Application mode = one session
    // per process run (not per request).
    options.auto_session_tracking = true;
    options.session_mode = sentry::SessionMode::Application;
    // Attach a stack trace to error events raised via the tracing layer (not
    // just panics), so a logged `error!` is actionable. Frame paths are
    // redacted by `scrub_event` before send.
    options.attach_stacktrace = true;
    // Never attach IP / username / host — the privacy invariant.
    options.send_default_pii = false;
    options.server_name = None;
    options.before_send = Some(Arc::new(|mut event| {
        scrub_event(&mut event);
        Some(event)
    }));
    options.before_breadcrumb = Some(Arc::new(|mut crumb| {
        crumb.message = crumb.message.map(|m| telemetry::scrub(&m));
        scrub_value_map(&mut crumb.data);
        Some(crumb)
    }));
    let guard = sentry::init((dsn, options));
    // Coarse, non-identifying facets for filtering in the Sentry UI. `os` /
    // `arch` are compile targets, not machine identity.
    sentry::configure_scope(|scope| {
        scope.set_tag("app", app);
        scope.set_tag("os", std::env::consts::OS);
        scope.set_tag("arch", std::env::consts::ARCH);
    });
    Some(guard)
}

/// Scrub every free-text field of an outgoing event. Belt-and-suspenders:
/// crash events drained from the queue are already scrubbed, but live
/// events the tracing layer will produce (stage 2) are not — and a stack
/// frame's `filename`/`abs_path` or an error `value` routinely carries a
/// path. Losing a little context to over-redaction is the accepted trade,
/// exactly as in [`crate::telemetry`].
fn scrub_event(event: &mut Event) {
    if let Some(message) = event.message.take() {
        event.message = Some(telemetry::scrub(&message));
    }
    if let Some(transaction) = event.transaction.take() {
        event.transaction = Some(telemetry::scrub(&transaction));
    }
    if let Some(logentry) = event.logentry.as_mut() {
        logentry.message = telemetry::scrub(&logentry.message);
    }
    for exception in &mut event.exception.values {
        if let Some(value) = exception.value.take() {
            exception.value = Some(telemetry::scrub(&value));
        }
        if let Some(stacktrace) = exception.stacktrace.as_mut() {
            for frame in &mut stacktrace.frames {
                frame.filename = frame.filename.take().map(|f| telemetry::scrub(&f));
                frame.abs_path = frame.abs_path.take().map(|p| telemetry::scrub(&p));
            }
        }
    }
    for crumb in &mut event.breadcrumbs.values {
        crumb.message = crumb.message.take().map(|m| telemetry::scrub(&m));
        scrub_value_map(&mut crumb.data);
    }
    scrub_value_map(&mut event.extra);
}

/// Scrub the string values of a Sentry key→value map in place. Non-string
/// values (numbers, bools) can't carry a path, so they're left untouched.
fn scrub_value_map(map: &mut std::collections::BTreeMap<String, Value>) {
    for value in map.values_mut() {
        if let Value::String(text) = value {
            *text = telemetry::scrub(text);
        }
    }
}

/// Turn a queued [`CrashReport`] into a Sentry event. The report's strings
/// were scrubbed at capture time, so this is a pure field mapping; the
/// `before_send` hook re-scrubs regardless.
fn crash_event(report: &CrashReport) -> Event<'static> {
    let mut event = Event {
        level: Level::Fatal,
        message: Some(report.message.clone()),
        release: Some(Cow::Owned(report.version.clone())),
        ..Default::default()
    };
    event.platform = "native".into();
    event.tags.insert("app".into(), report.app.clone());
    event.tags.insert("os".into(), report.os.clone());
    event.tags.insert("arch".into(), report.arch.clone());
    if let Some(location) = &report.location {
        event.tags.insert("panic.location".into(), location.clone());
    }
    event
        .extra
        .insert("thread".into(), report.thread.clone().into());
    event
        .extra
        .insert("backtrace".into(), report.backtrace.clone().into());
    event
}

/// Drain the local crash queue into Sentry, replacing the raw HTTP POST in
/// the old uploader. A live client and the user's consent are the caller's
/// precondition. Returns how many reports were handed to the transport.
///
/// Fire-and-forget: Sentry's background transport acknowledges delivery
/// asynchronously, so a report is removed from the queue once captured, not
/// once confirmed on the wire. The init guard flushes on process exit; a
/// crash report is best-effort, so a rare loss on immediate shutdown is an
/// acceptable trade for reusing the durable queue as the offline buffer.
pub fn drain_crashes_to_sentry(dir: &std::path::Path) -> usize {
    telemetry::drain(dir, |report| {
        sentry::capture_event(crash_event(report));
        true
    })
}

// --- Measurements (stage 3) ------------------------------------------------
//
// Numeric samples are sent as scrubbed *info events* carrying the values in
// `extra`, not as performance transactions: events are robust and need no
// tracing-sampling setup, and they still answer the question ("what are the
// numbers on real machines?"). The trade is Sentry's native Performance UI
// — if that's wanted later, these become transaction measurements.

/// Build a measurement event: an info-level event named `name` whose
/// `extra` carries the numeric `fields`. Pure, so the shape is testable.
fn measurement_event(name: &str, fields: &[(&str, f64)]) -> Event<'static> {
    let mut event = Event {
        level: Level::Info,
        message: Some(name.to_string()),
        logger: Some("measurement".into()),
        ..Default::default()
    };
    for (key, value) in fields {
        event.extra.insert((*key).into(), (*value).into());
    }
    event
}

/// Capture a measurement, but only when a client is actually bound — no
/// point building an event that goes nowhere.
fn record_measurement(name: &str, fields: &[(&str, f64)]) {
    if sentry::Hub::current().client().is_some() {
        sentry::capture_event(measurement_event(name, fields));
    }
}

/// Resident set size of this process, or `None` if the OS query fails.
fn process_rss_bytes() -> Option<u64> {
    memory_stats::memory_stats().map(|stats| stats.physical_mem as u64)
}

/// Sample resource use: the index arena bytes (the filex-controlled figure
/// behind #2) plus total process RSS. Called on a slow timer.
pub fn record_resource_sample(arena_bytes: u64, roots: usize) {
    let mut fields = vec![
        ("index.arena_bytes", arena_bytes as f64),
        ("index.roots", roots as f64),
    ];
    if let Some(rss) = process_rss_bytes() {
        fields.push(("process.rss_bytes", rss as f64));
    }
    record_measurement("measurement.resource", &fields);
}

/// Record a search-as-you-type latency sample. Callers should only invoke
/// this for slow scans (above the slow-op threshold) so the volume stays
/// low and the hot path is never burdened per keystroke.
pub fn record_search_latency(elapsed_ms: u64, results: usize) {
    record_measurement(
        "measurement.search",
        &[
            ("search.latency_ms", elapsed_ms as f64),
            ("search.results", results as f64),
        ],
    );
}

/// Record how long one root's initial index bootstrap (the whole-drive
/// walk) took. Low volume — once per root at startup.
pub fn record_index_bootstrap(elapsed_ms: u64, files: usize) {
    record_measurement(
        "measurement.bootstrap",
        &[
            ("index.bootstrap_ms", elapsed_ms as f64),
            ("index.files", files as f64),
        ],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report_with(message: &str, backtrace: &str) -> CrashReport {
        CrashReport {
            schema: telemetry::SCHEMA,
            app: "filex".into(),
            version: "0.1.0".into(),
            os: "windows".into(),
            arch: "x86_64".into(),
            unix_millis: 0,
            thread: "main".into(),
            message: message.into(),
            location: Some("src/x.rs:9".into()),
            backtrace: backtrace.into(),
        }
    }

    #[test]
    fn before_send_scrubs_message_and_frames() {
        // A live event whose message and stack frames carry paths must
        // leave with those redacted, never verbatim.
        let mut event = Event {
            message: Some(r"failed to open C:\Users\bob\secret.txt".into()),
            ..Default::default()
        };
        event.exception.values.push(sentry::protocol::Exception {
            ty: "io".into(),
            value: Some("/home/bob/report.pdf missing".into()),
            stacktrace: Some(sentry::protocol::Stacktrace {
                frames: vec![sentry::protocol::Frame {
                    abs_path: Some("/home/bob/src/main.rs".into()),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        });

        scrub_event(&mut event);

        assert_eq!(event.message.as_deref(), Some("failed to open <path>"));
        let exc = &event.exception.values[0];
        assert_eq!(exc.value.as_deref(), Some("<path> missing"));
        let frame_path = exc.stacktrace.as_ref().unwrap().frames[0].abs_path.as_deref();
        assert_eq!(frame_path, Some("<path>"));
    }

    #[test]
    fn measurement_event_carries_named_numeric_fields() {
        let event = measurement_event(
            "measurement.resource",
            &[("index.arena_bytes", 1234.0), ("index.roots", 2.0)],
        );
        assert_eq!(event.level, Level::Info);
        assert_eq!(event.message.as_deref(), Some("measurement.resource"));
        assert_eq!(
            event.extra.get("index.arena_bytes").and_then(Value::as_f64),
            Some(1234.0)
        );
        assert_eq!(
            event.extra.get("index.roots").and_then(Value::as_f64),
            Some(2.0)
        );
    }

    #[test]
    fn rss_query_returns_a_positive_value() {
        // The running test process must have some resident memory.
        let rss = process_rss_bytes().expect("RSS should be queryable on the test host");
        assert!(rss > 0);
    }

    #[test]
    fn crash_event_carries_scrubbed_report_and_env_tags() {
        // The queued report is pre-scrubbed, so its message maps through
        // unchanged; environment facts land as tags for filtering.
        let report = report_with("index panicked at <path>", "0: filex::index");
        let event = crash_event(&report);
        assert_eq!(event.level, Level::Fatal);
        assert_eq!(event.message.as_deref(), Some("index panicked at <path>"));
        assert_eq!(event.tags.get("os").map(String::as_str), Some("windows"));
        assert_eq!(event.tags.get("arch").map(String::as_str), Some("x86_64"));
        assert_eq!(
            event.tags.get("panic.location").map(String::as_str),
            Some("src/x.rs:9")
        );
    }
}

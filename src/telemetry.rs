//! Crash reporting (Phase 2c — see `docs/design-telemetry.md`).
//!
//! A panic is captured into a [`CrashReport`], **scrubbed** of any
//! path-shaped data (the privacy backstop — filenames are private), and
//! written to a local queue directory. A later launch, *only with the
//! user's consent*, uploads the queued reports. This module is the pure
//! core — the report model, the [`scrub`] redactor, and the queue
//! read/write/prune — with no panic hook, no UI, and no network, so it is
//! unit-tested in isolation.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

/// On-the-wire schema version, so a receiver can tolerate later additions.
pub const SCHEMA: u32 = 1;

/// How many crash files the queue retains; a crash-looping install drops
/// its oldest reports rather than filling the disk.
pub const QUEUE_CAP: usize = 50;

/// Default queue location: `<data_local_dir>/filex/crash-reports/`.
pub fn default_queue_dir() -> Option<PathBuf> {
    Some(dirs::data_local_dir()?.join("filex").join("crash-reports"))
}

/// One captured crash. Carries only coarse environment facts plus the
/// (scrubbed) panic details — never a query, path, tag, or index content
/// (see `docs/design-telemetry.md`, privacy invariants).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrashReport {
    pub schema: u32,
    /// Which binary crashed: `"filex"` or `"filex-indexd"`.
    pub app: String,
    pub version: String,
    pub os: String,
    pub arch: String,
    pub unix_millis: u128,
    pub thread: String,
    /// Panic message, already [`scrub`]bed.
    pub message: String,
    /// `file:line` of the panic site, already scrubbed.
    pub location: Option<String>,
    /// Backtrace, already scrubbed.
    pub backtrace: String,
}

/// Install a panic hook that captures every panic into a scrubbed
/// [`CrashReport`] in the default queue directory, then chains the
/// previously-installed hook (so the normal message/abort still happens).
/// `app` names the binary (`"filex"` / `"filex-indexd"`). Call once at
/// startup, after logging is initialised.
pub fn install_panic_hook(app: &'static str) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        capture_panic(app, info);
        previous(info);
    }));
}

/// Build and enqueue a scrubbed report for a panic. Best-effort: any
/// failure (no data dir, write error) is swallowed — a panic handler must
/// never itself panic or block.
fn capture_panic(app: &'static str, info: &std::panic::PanicHookInfo) {
    let report = CrashReport {
        schema: SCHEMA,
        app: app.to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        unix_millis: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
        thread: std::thread::current().name().unwrap_or("unnamed").to_string(),
        message: scrub(&payload_message(info.payload())),
        location: info.location().map(|l| scrub(&format!("{}:{}", l.file(), l.line()))),
        backtrace: scrub(&std::backtrace::Backtrace::force_capture().to_string()),
    };
    tracing::error!("panic in {app}: {} (at {:?})", report.message, report.location);
    if let Some(dir) = default_queue_dir() {
        let _ = enqueue(&dir, &report, QUEUE_CAP);
    }
}

/// The panic message from its payload — `&str` for `panic!("literal")`,
/// `String` for a formatted panic, else a placeholder.
fn payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

/// Redact path-shaped data from `text` — the privacy backstop applied to
/// every report before it can be queued. The user's home directory
/// becomes `~`; whitespace-delimited tokens that look like absolute paths
/// (Unix `/a/b…`, Windows `X:\a\b…`) become `<path>`. Symbol names
/// (`filex::index::…`) and single-segment roots (`/etc`) are left alone.
/// Over-redaction is deliberate: losing a `file:line` beats leaking a path.
pub fn scrub(text: &str) -> String {
    scrub_with_home(text, dirs::home_dir().as_deref())
}

fn scrub_with_home(text: &str, home: Option<&Path>) -> String {
    let homed = match home.and_then(Path::to_str) {
        Some(home) if !home.is_empty() => text.replace(home, "~"),
        _ => text.to_string(),
    };
    // Preserve line structure (backtraces are multi-line); redact
    // space-delimited path tokens within each line.
    homed
        .lines()
        .map(|line| {
            line.split(' ')
                .map(|tok| if looks_like_path(tok) { "<path>" } else { tok })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Does `token` look like an absolute filesystem path worth redacting?
fn looks_like_path(token: &str) -> bool {
    // Trim punctuation a path might be wrapped in (`"…"`, `(…)`, trailing
    // comma/colon).
    let t = token.trim_matches(|c: char| "\"'`(),".contains(c));
    let bytes = t.as_bytes();
    // Windows drive path: `X:\…` or `X:/…`.
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
    {
        return true;
    }
    // Unix absolute path with at least two segments (`/a/b`).
    t.starts_with('/') && t[1..].contains('/')
}

/// Write `report` to `dir` as `crash-<nanos>.json` (creating `dir`), then
/// prune the queue to the newest `cap` files. Synchronous and best-effort
/// — called from the panic hook, where nothing fancier is safe.
pub fn enqueue(dir: &Path, report: &CrashReport, cap: usize) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // Zero-pad so the filename's lexical order is always chronological,
    // independent of the timestamp's digit count.
    let path = dir.join(format!("crash-{nanos:020}.json"));
    let json = serde_json::to_vec(report).context("serializing crash report")?;
    std::fs::write(&path, json).with_context(|| format!("writing {}", path.display()))?;
    prune(dir, cap);
    Ok(())
}

/// The queued report files, oldest first (the filename's nanos timestamp
/// sorts chronologically).
pub fn queued(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| is_report_file(p))
            .collect(),
        Err(_) => Vec::new(),
    };
    files.sort();
    files
}

fn is_report_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("crash-") && n.ends_with(".json"))
}

/// Load a report from a queue file.
pub fn load(path: &Path) -> Result<CrashReport> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))
}

/// Keep the newest `cap` report files, deleting the rest.
fn prune(dir: &Path, cap: usize) {
    let files = queued(dir); // oldest first
    if files.len() <= cap {
        return;
    }
    for stale in &files[..files.len() - cap] {
        let _ = std::fs::remove_file(stale);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrub_redacts_home_and_paths_but_keeps_symbols() {
        let home = Some(Path::new("/Users/alice"));
        // Home dir → ~.
        assert_eq!(scrub_with_home("failed at /Users/alice/Secret", home), "failed at ~/Secret");
        // Absolute paths (Unix + Windows) → <path>; symbols survive.
        assert_eq!(
            scrub_with_home("panic in filex::index at /home/bob/report.pdf", None),
            "panic in filex::index at <path>"
        );
        assert_eq!(
            scrub_with_home(r"read C:\Users\bob\report.pdf failed", None),
            "read <path> failed"
        );
        // Single-segment roots and non-paths are untouched.
        assert_eq!(scrub_with_home("using /etc and value 42", None), "using /etc and value 42");
    }

    #[test]
    fn scrub_preserves_line_structure() {
        let bt = "0: filex::foo\n   at /Users/alice/src/index/mod.rs:12";
        let scrubbed = scrub_with_home(bt, Path::new("/Users/alice").into());
        assert_eq!(scrubbed, "0: filex::foo\n   at ~/src/index/mod.rs:12");
    }

    fn sample() -> CrashReport {
        CrashReport {
            schema: SCHEMA,
            app: "filex".into(),
            version: "0.1.0".into(),
            os: "macos".into(),
            arch: "aarch64".into(),
            unix_millis: 1_700_000_000_000,
            thread: "main".into(),
            message: "boom".into(),
            location: Some("src/x.rs:9".into()),
            backtrace: "0: foo".into(),
        }
    }

    #[test]
    fn enqueue_load_and_list_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let q = dir.path().join("crash-reports");
        assert!(queued(&q).is_empty()); // missing dir → empty, no panic

        enqueue(&q, &sample(), QUEUE_CAP).unwrap();
        let files = queued(&q);
        assert_eq!(files.len(), 1);
        assert_eq!(load(&files[0]).unwrap(), sample());
    }

    #[test]
    fn enqueue_prunes_to_cap_keeping_newest() {
        let dir = tempfile::tempdir().unwrap();
        let q = dir.path().join("crash-reports");
        std::fs::create_dir_all(&q).unwrap();
        // Five older reports, same 20-digit padding enqueue uses (small
        // values sort before the real nanos file added below).
        for i in 1..=5u128 {
            std::fs::write(q.join(format!("crash-{i:020}.json")), serde_json::to_vec(&sample()).unwrap())
                .unwrap();
        }
        // enqueue adds the newest (nanos) file, then prunes to cap 3.
        enqueue(&q, &sample(), 3).unwrap();

        let names: Vec<String> = queued(&q)
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap().to_string())
            .collect();
        assert_eq!(names.len(), 3);
        // The three oldest were dropped; 4 and 5 (the newest fakes) survive.
        for gone in [1u128, 2, 3] {
            assert!(!names.contains(&format!("crash-{gone:020}.json")));
        }
        assert!(names.contains(&format!("crash-{:020}.json", 4)));
        assert!(names.contains(&format!("crash-{:020}.json", 5)));
    }

    #[test]
    fn payload_message_reads_str_and_string_panics() {
        // `panic!("literal")` payloads are `&str`; formatted ones `String`.
        let s: &str = "boom";
        assert_eq!(payload_message(&s), "boom");
        assert_eq!(payload_message(&String::from("formatted 42")), "formatted 42");
        assert_eq!(payload_message(&5u32), "non-string panic payload");
    }

    #[test]
    fn corrupt_queue_file_errors_not_panics() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("crash-1.json");
        std::fs::write(&path, "{ not json").unwrap();
        assert!(load(&path).is_err());
    }
}

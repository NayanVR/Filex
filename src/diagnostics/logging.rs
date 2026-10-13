//! Structured local logging.
//!
//! `tracing` events go to a daily-rotating file under
//! `<data_local_dir>/filex/logs/` (stderr if that directory can't be
//! determined), named `<component>.<YYYY-MM-DD>.log` so it opens as a log
//! file everywhere. Only the newest [`KEEP_LOG_FILES`] per component are
//! kept. Local-only by design — the telemetry stance in
//! docs/roadmap.md: filenames are private, nothing leaves the machine.
//!
//! Writes are non-blocking so the UI thread never waits on log I/O.
//! The returned guard flushes buffered events on drop; hold it for the
//! whole process lifetime. Level defaults to `info`, overridable via
//! `RUST_LOG`.

use std::{
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use tracing_appender::{
    non_blocking::WorkerGuard,
    rolling::{RollingFileAppender, Rotation},
};
use tracing_subscriber::EnvFilter;

/// Daily files kept per component, today's included.
const KEEP_LOG_FILES: usize = 7;

/// Install the global subscriber. `component` names the log file
/// (`filex.<date>.log`, `filex-indexd.<date>.log`). Logs land under the user's local
/// data dir. Safe to call twice (later calls are no-ops), so tests can't
/// panic the process by initializing.
pub fn init(component: &str) -> Option<WorkerGuard> {
    init_in(component, None)
}

/// Like [`init`], but writes under `base/filex/logs` when `base` is
/// given. Falls back to the user's local data dir when `base` is `None`,
/// and to stderr when neither the override nor the fallback directory (or
/// the log file in it) can be created.
pub fn init_in(component: &str, base: Option<PathBuf>) -> Option<WorkerGuard> {
    use tracing_subscriber::fmt::writer::BoxMakeWriter;
    use tracing_subscriber::prelude::*;

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    // A single fmt layer over whichever writer we resolved, so the Sentry
    // breadcrumb layer (below) can compose onto the same subscriber. The
    // guard is `Some` only for the file writer (the stderr fallback needs
    // none).
    let (writer, guard): (BoxMakeWriter, Option<WorkerGuard>) =
        match log_dir(base).and_then(|dir| PruningAppender::new(dir, component)) {
            Some(appender) => {
                let (non_blocking, guard) = tracing_appender::non_blocking(appender);
                (BoxMakeWriter::new(non_blocking), Some(guard))
            }
            None => (BoxMakeWriter::new(std::io::stderr), None),
        };
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_writer(writer)
        .with_ansi(false);
    let subscriber = tracing_subscriber::registry().with(filter).with(fmt_layer);
    // Under `observability`, tracing events also become Sentry signals:
    // its default mapping sends `error` as events and `warn`/`info` as
    // breadcrumbs. Every one is path-scrubbed by the before_send /
    // before_breadcrumb hooks in `crate::diagnostics::observability` before it leaves
    // the machine — the tracing strings here are full of `path.display()`.
    #[cfg(feature = "observability")]
    let subscriber = subscriber.with(sentry_tracing::layer());
    subscriber.try_init().ok();
    guard
}

/// The `filex/logs` directory under `base` (or the user's local data dir
/// when `base` is `None`), but only if it can be created. `None` signals
/// "fall back to stderr" — so an unwritable override (e.g. `ProgramData`
/// on a dev run without admin) can never silently swallow every event.
fn log_dir(base: Option<PathBuf>) -> Option<PathBuf> {
    base.or_else(dirs::data_local_dir)
        .map(|base| base.join("filex").join("logs"))
        .filter(|dir| std::fs::create_dir_all(dir).is_ok())
}

/// The daily appender, plus retention. `tracing-appender`'s own
/// `max_log_files` matches files by prefix, so the app's `filex` would
/// also count, and delete, `filex-indexd` logs. This prunes by exact name
/// at startup and again on the first write of each UTC day, which is when
/// the appender rolls over. Writes run on the non-blocking worker thread.
struct PruningAppender {
    inner: RollingFileAppender,
    dir: PathBuf,
    component: String,
    pruned_day: u64,
}

impl PruningAppender {
    fn new(dir: PathBuf, component: &str) -> Option<Self> {
        let inner = RollingFileAppender::builder()
            .rotation(Rotation::DAILY)
            .filename_prefix(component)
            .filename_suffix("log")
            .build(&dir)
            .ok()?;
        prune(&dir, component);
        Some(Self {
            inner,
            dir,
            component: component.to_string(),
            pruned_day: utc_day(),
        })
    }
}

impl Write for PruningAppender {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let today = utc_day();
        if today != self.pruned_day {
            // Write first so today's file exists before older ones go.
            let written = self.inner.write(buf)?;
            self.pruned_day = today;
            prune(&self.dir, &self.component);
            return Ok(written);
        }
        self.inner.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

fn utc_day() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() / 86_400)
}

/// Delete `component`'s logs beyond the newest [`KEEP_LOG_FILES`].
/// Best-effort: a file that can't be removed is retried next time.
fn prune(dir: &Path, component: &str) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let names: Vec<String> = entries
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .collect();
    for name in stale_logs(&names, component, KEEP_LOG_FILES) {
        let _ = std::fs::remove_file(dir.join(name));
    }
}

/// Names of `component`'s log files to delete so only the newest `keep`
/// remain. Matches `<component>.<YYYY-MM-DD>.log` and the pre-0.3.2
/// `<component>.log.<YYYY-MM-DD>` exactly, so `filex` never matches
/// `filex-indexd`'s files. ISO dates sort chronologically as text.
fn stale_logs<'a>(names: &'a [String], component: &str, keep: usize) -> Vec<&'a str> {
    let mut logs: Vec<(&str, &str)> = names
        .iter()
        .filter_map(|name| Some((log_date(name, component)?, name.as_str())))
        .collect();
    logs.sort_unstable_by(|a, b| b.cmp(a));
    logs.into_iter().skip(keep).map(|(_, name)| name).collect()
}

/// The date in one of `component`'s log file names, if `name` is one.
fn log_date<'a>(name: &'a str, component: &str) -> Option<&'a str> {
    let rest = name.strip_prefix(component)?;
    let date = rest
        .strip_prefix('.')
        .and_then(|r| r.strip_suffix(".log"))
        .or_else(|| rest.strip_prefix(".log."))?;
    let iso = date.len() == 10
        && date.bytes().enumerate().all(|(i, b)| match i {
            4 | 7 => b == b'-',
            _ => b.is_ascii_digit(),
        });
    iso.then_some(date)
}

#[cfg(test)]
mod tests {
    use super::{KEEP_LOG_FILES, PruningAppender, log_dir, stale_logs};

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn keeps_the_newest_files_of_one_component() {
        let all = names(&[
            "filex.2026-10-10.log",
            "filex.2026-10-13.log",
            "filex.2026-10-11.log",
            "filex.2026-10-12.log",
        ]);
        let mut stale = stale_logs(&all, "filex", 2);
        stale.sort();
        assert_eq!(stale, ["filex.2026-10-10.log", "filex.2026-10-11.log"]);
    }

    /// `filex` is a prefix of `filex-indexd`; the app must never prune the
    /// daemon's logs (tracing-appender's prefix match would).
    #[test]
    fn a_component_never_prunes_another_sharing_its_prefix() {
        let all = names(&[
            "filex-indexd.2026-10-01.log",
            "filex-indexd.2026-10-02.log",
            "filex.2026-10-13.log",
        ]);
        assert!(
            stale_logs(&all, "filex", 0)
                .iter()
                .all(|n| n.starts_with("filex."))
        );
        assert_eq!(
            stale_logs(&all, "filex-indexd", 1),
            ["filex-indexd.2026-10-01.log"]
        );
    }

    /// Files from before the `.log` suffix change are pruned too, oldest
    /// first, alongside the new names.
    #[test]
    fn old_style_names_are_pruned_by_date() {
        let all = names(&[
            "filex-indexd.log.2026-10-11",
            "filex-indexd.log.2026-10-12",
            "filex-indexd.2026-10-13.log",
        ]);
        assert_eq!(
            stale_logs(&all, "filex-indexd", 2),
            ["filex-indexd.log.2026-10-11"]
        );
    }

    #[test]
    fn unrelated_files_are_left_alone() {
        let all = names(&[
            "filex.log",
            "filex.notes.log",
            "filex.2026-1-13.log",
            "filex.2026-10-13.log.bak",
            "crash.2026-10-01.log",
        ]);
        assert!(stale_logs(&all, "filex", 0).is_empty());
    }

    #[test]
    fn writable_override_yields_the_logs_subdir() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = log_dir(Some(tmp.path().to_path_buf())).expect("writable base resolves");
        assert_eq!(dir.file_name().unwrap(), "logs");
        assert_eq!(dir.parent().unwrap().file_name().unwrap(), "filex");
        assert!(dir.exists(), "the directory should have been created");
    }

    #[test]
    fn unwritable_override_falls_back_to_none() {
        // A regular file can't have subdirectories created under it, so
        // create_dir_all fails and log_dir must return None (→ stderr)
        // rather than handing back a path nothing can be written to.
        let file = tempfile::NamedTempFile::new().unwrap();
        let under_a_file = file.path().join("nested");
        assert!(log_dir(Some(under_a_file)).is_none());
    }

    /// The appender writes today's file as `<component>.<date>.log` and
    /// prunes the component's old files, both naming styles, at startup.
    #[test]
    fn appender_names_files_with_a_log_suffix_and_prunes_on_start() {
        let dir = tempfile::tempdir().unwrap();
        let mut old: Vec<String> = (1..=KEEP_LOG_FILES + 2)
            .map(|day| format!("filex-indexd.2020-01-{day:02}.log"))
            .collect();
        old.push("filex-indexd.log.2019-12-31".into());
        old.push("filex.2020-01-01.log".into());
        for name in &old {
            std::fs::write(dir.path().join(name), b"").unwrap();
        }
        let _appender = PruningAppender::new(dir.path().to_path_buf(), "filex-indexd").unwrap();
        let mut left: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        let today = left
            .iter()
            .filter(|n| n.starts_with("filex-indexd.") && !n.starts_with("filex-indexd.2020"))
            .collect::<Vec<_>>();
        assert_eq!(today.len(), 1, "{left:?}");
        assert!(today[0].ends_with(".log"), "{left:?}");
        assert!(
            left.contains(&"filex.2020-01-01.log".to_string()),
            "{left:?}"
        );
        assert!(!left.iter().any(|n| n.contains(".log.")), "{left:?}");
        let kept_old = left
            .iter()
            .filter(|n| n.starts_with("filex-indexd.2020"))
            .count();
        assert_eq!(kept_old, KEEP_LOG_FILES - 1, "{left:?}");
    }
}

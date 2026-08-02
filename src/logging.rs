//! Structured local logging.
//!
//! `tracing` events go to a daily-rotating file under
//! `<data_local_dir>/filex/logs/` (stderr if that directory can't be
//! determined). Local-only by design — the telemetry stance in
//! docs/roadmap.md: filenames are private, nothing leaves the machine.
//!
//! Writes are non-blocking so the UI thread never waits on log I/O.
//! The returned guard flushes buffered events on drop; hold it for the
//! whole process lifetime. Level defaults to `info`, overridable via
//! `RUST_LOG`.

use std::path::PathBuf;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;

/// Install the global subscriber. `component` names the log file
/// (`filex.log`, `filex-indexd.log`). Logs land under the user's local
/// data dir. Safe to call twice (later calls are no-ops), so tests can't
/// panic the process by initializing.
pub fn init(component: &str) -> Option<WorkerGuard> {
    init_in(component, None)
}

/// Like [`init`], but writes under `base/filex/logs` when `base` is
/// given. The Windows index service passes an explicit machine-wide base
/// (`C:\ProgramData`) because it runs as LocalSystem, whose local data
/// dir is buried under `System32\config\systemprofile` — not somewhere an
/// admin would ever think to look for logs. Falls back to the user's
/// local data dir when `base` is `None`, and to stderr when neither the
/// override nor the fallback directory can be created.
pub fn init_in(component: &str, base: Option<PathBuf>) -> Option<WorkerGuard> {
    use tracing_subscriber::fmt::writer::BoxMakeWriter;
    use tracing_subscriber::prelude::*;

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    // A single fmt layer over whichever writer we resolved, so the Sentry
    // breadcrumb layer (below) can compose onto the same subscriber. The
    // guard is `Some` only for the file writer (the stderr fallback needs
    // none).
    let (writer, guard): (BoxMakeWriter, Option<WorkerGuard>) = match log_dir(base) {
        Some(dir) => {
            let appender = tracing_appender::rolling::daily(dir, format!("{component}.log"));
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
    // before_breadcrumb hooks in `crate::observability` before it leaves
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

#[cfg(test)]
mod tests {
    use super::log_dir;

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
}

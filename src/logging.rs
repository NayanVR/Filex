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

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;

/// Install the global subscriber. `component` names the log file
/// (`filex.log`, `filex-indexd.log`). Safe to call twice (later calls
/// are no-ops), so tests can't panic the process by initializing.
pub fn init(component: &str) -> Option<WorkerGuard> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    match dirs::data_local_dir() {
        Some(base) => {
            let appender = tracing_appender::rolling::daily(
                base.join("filex").join("logs"),
                format!("{component}.log"),
            );
            let (writer, guard) = tracing_appender::non_blocking(appender);
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(writer)
                .with_ansi(false)
                .try_init()
                .ok();
            Some(guard)
        }
        None => {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(std::io::stderr)
                .try_init()
                .ok();
            None
        }
    }
}

//! Persistent v2 index owner.
pub mod builder;
pub mod ipc;
pub mod query;
pub mod server;
pub mod view;

mod changes;
mod executable;
mod progress;
mod recovery;
mod watch;
mod writer;

/// Make every panic, on any thread, end the process with exit code 101
/// (what a main-thread panic already exits with). Call once in the daemon
/// (owner) process, after `telemetry::install_panic_hook`, so the crash
/// report and the stderr message are written first.
///
/// The writer, builder-coordinator and query threads are unsupervised: a
/// panicking writer leaves search serving a frozen epoch that still reports
/// "Ready", and a panicking query worker drops its job channel so every
/// search answers "busy" forever. Dying is the better failure. Nothing
/// acknowledged is lost: the writer appends to the WAL before it publishes,
/// and a restart re-runs recovery plus reconcile. The UI's service probe
/// restarts the daemon (`Client::start` on error), as does launchd/systemd
/// where configured.
///
/// `exit`, not `abort`: the telemetry hook has already captured the panic,
/// and on Windows `abort` is `__fastfail`, which goes to Windows Error
/// Reporting and can show a "stopped working" dialog for this background
/// process.
pub fn exit_on_panic() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        previous(info);
        std::process::exit(101);
    }));
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    const CHILD: &str = "FILEX_ABORT_CHILD";

    #[test]
    fn panic_on_any_daemon_thread_ends_process() {
        if std::env::var_os(CHILD).is_some() {
            super::exit_on_panic();
            let _ = std::thread::spawn(|| panic!("worker thread panic"));
            std::thread::sleep(Duration::from_secs(5));
            return;
        }
        let started = Instant::now();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "daemon::tests::panic_on_any_daemon_thread_ends_process",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        let elapsed = started.elapsed();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "child survived: {stderr}");
        assert!(stderr.contains("worker thread panic"), "{stderr}");
        assert_eq!(output.status.code(), Some(101), "{stderr}");
        assert!(elapsed < Duration::from_secs(4), "took {elapsed:?}");
    }
}

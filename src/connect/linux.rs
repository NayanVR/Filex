//! Linux: connect through GVfs with `gio mount`, which needs no root and
//! puts the share under `$XDG_RUNTIME_DIR/gvfs` (listed as a drive by
//! FIL-61). `mount.cifs` needs root, so it stays out.
//!
//! Credentials: GVfs uses the password saved in the desktop keyring when
//! there is one. `gio` itself would otherwise ask on its terminal; it runs
//! here with stdin closed, so a share that needs a password fails with a
//! message pointing at the desktop file manager (which can save it) rather
//! than Filex collecting the password itself (docs/design-network.md).
//!
//! The process plumbing and message parsing build everywhere so they're
//! tested on every CI runner.
use super::ConnectError;
use crate::network::ShareAddress;
use std::{
    process::{Command, Output, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

/// How a finished `gio mount` turned out.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum GioOutcome {
    Mounted,
    Failed(ConnectError),
}

/// Read `gio mount`'s exit status and output (run with `LC_ALL=C`, so the
/// messages are GVfs' English ones). "Already mounted" is a success: the
/// share is there to open.
pub(super) fn gio_outcome(output: &Output, address: &ShareAddress) -> GioOutcome {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    if output.status.success() || stderr.contains("already mounted") {
        return GioOutcome::Mounted;
    }
    let name = address.display_name();
    let asked_for_password = stdout.contains("Authentication Required")
        || stdout.contains("Password:")
        || stderr.contains("Password dialog cancelled")
        || stderr.contains("password");
    GioOutcome::Failed(ConnectError::Failed(if asked_for_password {
        format!(
            "{name} needs a password. Connect to it once from your desktop’s file \
             manager (it can save the password), or run “gio mount {}” in a terminal.",
            address.share_root().to_url()
        )
    } else {
        let reason = last_reason(&stderr);
        if reason.is_empty() {
            format!("Couldn’t connect to {name}.")
        } else {
            format!("Couldn’t connect to {name}: {reason}")
        }
    }))
}

/// The last line of `gio`'s error output without its `gio: <url>: `
/// prefix — the reason GVfs gave.
fn last_reason(stderr: &str) -> &str {
    let line = stderr
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("");
    line.strip_prefix("gio: ")
        .and_then(|rest| rest.split_once(": ").map(|(_, reason)| reason))
        .unwrap_or(line)
        .trim()
}

/// Run `command` to completion with its output captured, killing it if
/// `cancel` is raised first. `None` when cancelled.
#[cfg_attr(not(any(target_os = "linux", unix)), allow(dead_code))]
pub(super) fn run_cancellable(
    mut command: Command,
    cancel: &AtomicBool,
) -> std::io::Result<Option<Output>> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        if child.try_wait()?.is_some() {
            // Exited: the pipes are at EOF once drained, so this returns
            // straight away.
            return child.wait_with_output().map(Some);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(target_os = "linux")]
pub(super) struct GvfsConnector;

#[cfg(target_os = "linux")]
impl super::ShareConnector for GvfsConnector {
    fn connect(
        &self,
        request: &super::ConnectRequest,
        cancel: &AtomicBool,
    ) -> Result<std::path::PathBuf, ConnectError> {
        let address = &request.address;
        let mut gio = Command::new("gio");
        gio.args(["mount", &address.share_root().to_url()])
            .env("LC_ALL", "C");
        let output = match run_cancellable(gio, cancel) {
            Ok(Some(output)) => output,
            Ok(None) => return Err(ConnectError::Cancelled),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(ConnectError::Failed(
                    "Connecting needs GVfs (the “gio” command), which isn’t installed.".into(),
                ));
            }
            Err(error) => {
                return Err(ConnectError::Failed(format!("Couldn’t run gio: {error}")));
            }
        };
        match gio_outcome(&output, address) {
            GioOutcome::Mounted => super::locate_mounted(address),
            GioOutcome::Failed(error) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn output(code: i32, stdout: &str, stderr: &str) -> Output {
        use std::os::unix::process::ExitStatusExt;
        Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }
    #[cfg(windows)]
    fn output(code: i32, stdout: &str, stderr: &str) -> Output {
        use std::os::windows::process::ExitStatusExt;
        Output {
            status: std::process::ExitStatus::from_raw(code as u32),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    fn media() -> ShareAddress {
        ShareAddress::parse("smb://nas/Media/Films").unwrap()
    }

    #[test]
    fn success_and_already_mounted_both_mean_mounted() {
        assert_eq!(
            gio_outcome(&output(0, "", ""), &media()),
            GioOutcome::Mounted
        );
        let already = "gio: smb://nas/Media: Location is already mounted\n";
        assert_eq!(
            gio_outcome(&output(2, "", already), &media()),
            GioOutcome::Mounted
        );
    }

    #[test]
    fn a_password_prompt_points_at_the_desktop_file_manager() {
        let asked = "Authentication Required\nEnter user and password for share “Media” on “nas”:\nUser [me]: ";
        let GioOutcome::Failed(ConnectError::Failed(message)) =
            gio_outcome(&output(2, asked, ""), &media())
        else {
            panic!("expected a failure");
        };
        assert!(
            message.starts_with("Media (nas) needs a password."),
            "{message}"
        );
        assert!(message.contains("gio mount smb://nas/Media"), "{message}");
    }

    #[test]
    fn other_failures_give_gvfs_reason() {
        let refused = "gio: smb://nas/Media: Failed to mount Windows share: Connection refused\n";
        assert_eq!(
            gio_outcome(&output(2, "", refused), &media()),
            GioOutcome::Failed(ConnectError::Failed(
                "Couldn’t connect to Media (nas): Failed to mount Windows share: Connection refused"
                    .into()
            ))
        );
        assert_eq!(
            gio_outcome(&output(1, "", ""), &media()),
            GioOutcome::Failed(ConnectError::Failed(
                "Couldn’t connect to Media (nas).".into()
            ))
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_cancelled_run_is_killed_promptly() {
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let raise = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            raise.store(true, Ordering::Relaxed);
        });
        let started = std::time::Instant::now();
        let mut sleep = Command::new("sleep");
        sleep.arg("30");
        assert!(run_cancellable(sleep, &cancel).unwrap().is_none());
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[cfg(unix)]
    #[test]
    fn a_finished_run_reports_its_output() {
        let mut echo = Command::new("sh");
        echo.args(["-c", "echo out; echo err >&2; exit 3"]);
        let output = run_cancellable(echo, &AtomicBool::new(false))
            .unwrap()
            .unwrap();
        assert_eq!(output.status.code(), Some(3));
        assert_eq!(output.stdout, b"out\n");
        assert_eq!(output.stderr, b"err\n");
    }
}

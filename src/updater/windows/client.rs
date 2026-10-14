//! The app's side: start the updater service on demand and talk to it.
//!
//! Each call re-resolves the service through the SCM, because the service
//! exits when idle; starting it again is cheap and transparent.

use std::time::Duration;

use anyhow::{Result, bail};

use super::{pipe, scm};
use crate::updater::PIPE_NAME;
use crate::updater::components::UPDATER_SERVICE;
use crate::updater::protocol::{Command, PROTOCOL, Phase, Response};

pub use scm::Unavailable;

const START_TIMEOUT: Duration = Duration::from_secs(10);
const CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// Send `command` to the updater service, starting it if needed. The
/// service's identity is checked: the pipe's server must be the PID the SCM
/// reports for `filex-updater`.
pub fn call(command: Command) -> Result<Response, Unavailable> {
    let pid = scm::ensure_running(UPDATER_SERVICE, START_TIMEOUT)?;
    pipe::call(PIPE_NAME, command, Some(pid), CALL_TIMEOUT).map_err(Unavailable::Failed)
}

/// `call`, expecting a [`Phase`] back.
pub fn phase(command: Command) -> Result<Phase, Unavailable> {
    match call(command)? {
        Response::Status(phase) => Ok(phase),
        other => Err(Unavailable::Failed(anyhow::anyhow!(
            "unexpected updater response {other:?}"
        ))),
    }
}

/// Confirm the service speaks our protocol; returns its version.
pub fn hello() -> Result<String, Unavailable> {
    match call(Command::Hello)? {
        Response::Hello { protocol, version } if protocol == PROTOCOL => Ok(version),
        other => Err(Unavailable::Failed(anyhow::anyhow!(
            "incompatible updater: {other:?}"
        ))),
    }
}

/// Poll `Status` until the phase is no longer busy, calling `on_phase` with
/// each phase seen. Returns the settled phase.
pub fn wait_settled(
    mut on_phase: impl FnMut(&Phase),
    interval: Duration,
    timeout: Duration,
) -> Result<Phase> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let phase = phase(Command::Status)?;
        on_phase(&phase);
        if !phase.is_busy() {
            return Ok(phase);
        }
        if std::time::Instant::now() >= deadline {
            bail!("the updater is still busy after {timeout:?}");
        }
        std::thread::sleep(interval);
    }
}

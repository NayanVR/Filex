//! Service Control Manager helpers: start the updater on demand (client),
//! stop sibling services before an install, and set the updater's own DACL.
//!
//! Assumes the SCM's demand-start model: a stopped service is started by
//! `StartService` (callers need `SERVICE_START`, which the updater grants
//! interactive users via [`super::security::SERVICE_SDDL`]).

use std::ffi::OsStr;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use windows::Win32::Security::DACL_SECURITY_INFORMATION;
use windows::Win32::System::Services::{SC_HANDLE, SetServiceObjectSecurity};
use windows_service::service::{ServiceAccess, ServiceState};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

use super::security::{SERVICE_SDDL, SecurityDescriptor};

/// `ERROR_SERVICE_DOES_NOT_EXIST`.
const SERVICE_DOES_NOT_EXIST: i32 = 1060;
/// `ERROR_SERVICE_ALREADY_RUNNING`.
const SERVICE_ALREADY_RUNNING: i32 = 1056;

/// Why the updater service can't be reached.
#[derive(Debug)]
pub enum Unavailable {
    /// Not installed (dev build, or an install older than the updater).
    NotInstalled,
    /// Installed but couldn't be started or queried.
    Failed(anyhow::Error),
}

impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unavailable::NotInstalled => write!(f, "the update service is not installed"),
            Unavailable::Failed(error) => write!(f, "the update service is unavailable: {error:#}"),
        }
    }
}

impl std::error::Error for Unavailable {}

fn os_code(error: &windows_service::Error) -> Option<i32> {
    match error {
        windows_service::Error::Winapi(io) => io.raw_os_error(),
        _ => None,
    }
}

/// Start `name` if it is stopped and return its process id once running.
pub fn ensure_running(name: &str, timeout: Duration) -> Result<u32, Unavailable> {
    let manager = ServiceManager::local_computer(None::<&OsStr>, ServiceManagerAccess::CONNECT)
        .map_err(|e| Unavailable::Failed(e.into()))?;
    let service = manager
        .open_service(name, ServiceAccess::QUERY_STATUS | ServiceAccess::START)
        .map_err(|e| {
            if os_code(&e) == Some(SERVICE_DOES_NOT_EXIST) {
                Unavailable::NotInstalled
            } else {
                Unavailable::Failed(anyhow::Error::from(e).context(format!("opening {name}")))
            }
        })?;
    let deadline = Instant::now() + timeout;
    let mut started = false;
    loop {
        let status = service
            .query_status()
            .map_err(|e| Unavailable::Failed(e.into()))?;
        match status.current_state {
            ServiceState::Running => {
                return status.process_id.ok_or_else(|| {
                    Unavailable::Failed(anyhow::anyhow!("{name} is running without a process id"))
                });
            }
            ServiceState::Stopped if !started => {
                if let Err(error) = service.start::<&OsStr>(&[])
                    && os_code(&error) != Some(SERVICE_ALREADY_RUNNING)
                {
                    return Err(Unavailable::Failed(
                        anyhow::Error::from(error).context(format!("starting {name}")),
                    ));
                }
                started = true;
            }
            _ => {}
        }
        if Instant::now() >= deadline {
            return Err(Unavailable::Failed(anyhow::anyhow!(
                "{name} did not reach Running (last state {:?})",
                status.current_state
            )));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Stop `name` and wait until it reports Stopped. A missing service is not
/// an error (nothing to stop).
pub fn stop_and_wait(name: &str, timeout: Duration) -> Result<()> {
    let manager = ServiceManager::local_computer(None::<&OsStr>, ServiceManagerAccess::CONNECT)?;
    let service =
        match manager.open_service(name, ServiceAccess::QUERY_STATUS | ServiceAccess::STOP) {
            Ok(service) => service,
            Err(error) if os_code(&error) == Some(SERVICE_DOES_NOT_EXIST) => return Ok(()),
            Err(error) => return Err(error).with_context(|| format!("opening {name}")),
        };
    if service.query_status()?.current_state == ServiceState::Stopped {
        return Ok(());
    }
    // A stop that races with the service stopping on its own is fine.
    let _ = service.stop();
    let deadline = Instant::now() + timeout;
    while service.query_status()?.current_state != ServiceState::Stopped {
        if Instant::now() >= deadline {
            bail!("{name} did not stop within {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(())
}

/// Replace `name`'s DACL with [`SERVICE_SDDL`]. Run by the service itself
/// (as SYSTEM) on every start, so the grant survives repairs and doesn't
/// depend on a WiX extension.
pub fn apply_service_dacl(name: &str) -> Result<()> {
    let manager = ServiceManager::local_computer(None::<&OsStr>, ServiceManagerAccess::CONNECT)?;
    let service = manager
        .open_service(name, ServiceAccess::READ_CONTROL | ServiceAccess::WRITE_DAC)
        .with_context(|| format!("opening {name} to set its DACL"))?;
    let descriptor = SecurityDescriptor::from_sddl(SERVICE_SDDL)?;
    // SAFETY: a valid service handle and descriptor, both outliving the call.
    unsafe {
        SetServiceObjectSecurity(
            SC_HANDLE(service.raw_handle() as _),
            DACL_SECURITY_INFORMATION,
            descriptor.as_ptr(),
        )
    }
    .with_context(|| format!("setting {name}'s DACL"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_service_reports_not_installed() {
        let result = ensure_running("filex-updater-test-missing", Duration::from_millis(10));
        assert!(
            matches!(result, Err(Unavailable::NotInstalled)),
            "{result:?}"
        );
    }

    #[test]
    fn stopping_a_missing_service_is_a_no_op() {
        stop_and_wait("filex-updater-test-missing", Duration::from_millis(10)).unwrap();
    }
}

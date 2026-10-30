//! Connecting to network shares (FIL-59). One [`ShareConnector`] per OS,
//! so the UI doesn't care how a share gets mounted:
//!
//! - Windows: `WNetAddConnection2W`, with the OS credential prompt when the
//!   current sign-in isn't enough; optionally to a drive letter.
//! - macOS: NetFS (`NetFSMountURLSync`), the system's own sign-in sheet and
//!   Keychain; the share lands under `/Volumes`.
//! - Linux: GVfs (`gio mount`), user-space and root-free; the share lands
//!   under `$XDG_RUNTIME_DIR/gvfs`. `mount.cifs` needs root and stays out.
//!
//! Every call blocks — on the network, and on the user typing into an OS
//! password prompt — so callers run it on a thread of its own
//! ([`connect_on_thread`]), never the UI thread or the shared background
//! executor. See docs/design-network.md.

#[cfg(any(target_os = "linux", test))]
mod linux;
#[cfg(any(target_os = "macos", test))]
mod macos;
#[cfg(any(windows, test))]
mod windows;

use crate::network::ShareAddress;
use std::{
    path::PathBuf,
    sync::{Arc, atomic::AtomicBool},
};

/// What to connect to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectRequest {
    pub address: ShareAddress,
    /// Windows only: map the share to the next free drive letter rather
    /// than opening it by UNC path. Ignored elsewhere.
    pub map_drive_letter: bool,
}

/// Why a connect didn't produce a folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectError {
    /// The user dismissed the OS sign-in prompt, or the caller cancelled.
    Cancelled,
    /// A message to show as is.
    Failed(String),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => f.write_str("Cancelled."),
            Self::Failed(message) => f.write_str(message),
        }
    }
}

/// Mounts a share and says where it can be browsed.
pub trait ShareConnector: Send + Sync {
    /// Connect, blocking until the share is mounted, the user cancels the
    /// OS prompt, or it fails. `cancel` is checked where the OS allows
    /// stopping midway (Linux); elsewhere a cancelled call is abandoned and
    /// its result ignored. Returns the folder for `request.address`,
    /// including any folders below the share itself.
    fn connect(
        &self,
        request: &ConnectRequest,
        cancel: &AtomicBool,
    ) -> Result<PathBuf, ConnectError>;
}

/// This platform's connector.
pub fn connector() -> &'static dyn ShareConnector {
    #[cfg(windows)]
    {
        &windows::WNetConnector
    }
    #[cfg(target_os = "macos")]
    {
        &macos::NetFsConnector
    }
    #[cfg(target_os = "linux")]
    {
        &linux::GvfsConnector
    }
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        &Unsupported
    }
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
struct Unsupported;
#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
impl ShareConnector for Unsupported {
    fn connect(&self, _: &ConnectRequest, _: &AtomicBool) -> Result<PathBuf, ConnectError> {
        Err(ConnectError::Failed(
            "Connecting to servers isn't supported on this system.".into(),
        ))
    }
}

/// Run `connector.connect` on a new thread and hand its result to
/// `done` there. The thread may sit in an OS password prompt for minutes,
/// which is why it isn't a pooled executor thread.
pub fn connect_on_thread(
    connector: &'static dyn ShareConnector,
    request: ConnectRequest,
    cancel: Arc<AtomicBool>,
    done: impl FnOnce(Result<PathBuf, ConnectError>) + Send + 'static,
) {
    let spawned = std::thread::Builder::new()
        .name("filex-connect".into())
        .spawn(move || done(connector.connect(&request, &cancel)));
    if let Err(error) = spawned {
        tracing::warn!("couldn't start a connect thread: {error}");
    }
}

/// Where a just-mounted share's folder is, from a fresh drive listing.
/// Shared by the connectors that mount somewhere the OS picks (macOS,
/// Linux).
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn locate_mounted(address: &ShareAddress) -> Result<PathBuf, ConnectError> {
    crate::drives::locate_share(address, &crate::drives::list_drives()).ok_or_else(|| {
        ConnectError::Failed(format!(
            "Connected to {}, but couldn’t find where it was mounted.",
            address.display_name()
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    struct Fixed(Result<PathBuf, ConnectError>);
    impl ShareConnector for Fixed {
        fn connect(&self, _: &ConnectRequest, _: &AtomicBool) -> Result<PathBuf, ConnectError> {
            self.0.clone()
        }
    }

    #[test]
    fn a_connect_runs_off_the_calling_thread_and_reports_back() {
        static OK: std::sync::OnceLock<Fixed> = std::sync::OnceLock::new();
        let connector = OK.get_or_init(|| Fixed(Ok(PathBuf::from("/mnt/media"))));
        let (tx, rx) = mpsc::channel();
        let caller = std::thread::current().id();
        connect_on_thread(
            connector,
            ConnectRequest {
                address: ShareAddress::parse("smb://nas/media").unwrap(),
                map_drive_letter: false,
            },
            Arc::default(),
            move |result| {
                let _ = tx.send((result, std::thread::current().id()));
            },
        );
        let (result, thread) = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert_eq!(result, Ok(PathBuf::from("/mnt/media")));
        assert_ne!(thread, caller);
    }

    #[test]
    fn cancelled_reads_as_such() {
        assert_eq!(ConnectError::Cancelled.to_string(), "Cancelled.");
        assert_eq!(ConnectError::Failed("Nope.".into()).to_string(), "Nope.");
    }
}

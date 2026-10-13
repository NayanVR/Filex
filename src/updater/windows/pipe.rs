//! The updater's named-pipe transport: one request and one response per
//! connection.
//!
//! Assumes Windows named-pipe semantics: a pipe name exists only while at
//! least one server instance handle is open, so the server always keeps a
//! listening instance open before handing a connected one to its handler —
//! otherwise another process could create the name in the gap ("pipe
//! squatting"). The first instance uses `FILE_FLAG_FIRST_PIPE_INSTANCE`, so
//! a squatter that got there first makes the server fail loudly.

use std::fs::File;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_NO_DATA, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, HANDLE,
};
use windows::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientSessionId, GetNamedPipeServerProcessId,
    PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES,
    PIPE_WAIT, WaitNamedPipeW,
};
use windows::core::HSTRING;

use super::security::{PIPE_CLIENT_ACCESS, SecurityDescriptor};
use crate::updater::protocol::{Command, PROTOCOL, Request, Response, read_frame, write_frame};

/// Connections served concurrently; more are closed unanswered. Real
/// traffic is one app polling once a second.
const MAX_CONNECTIONS: usize = 8;

/// `SECURITY_IDENTIFICATION` for `security_qos_flags`: the server may learn
/// who we are but can't impersonate us.
const SECURITY_IDENTIFICATION: u32 = 0x0001_0000;

/// A request handler: the command and the client's Windows session id.
pub type Handler = Arc<dyn Fn(Command, u32) -> Response + Send + Sync>;

/// A pipe instance handle, closed on drop unless converted into a `File`.
struct Instance(HANDLE);

impl Instance {
    fn create(name: &str, security: &SecurityDescriptor, first: bool) -> Result<Self> {
        let attributes = security.attributes();
        let mut open_mode = PIPE_ACCESS_DUPLEX;
        if first {
            open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
        }
        // SAFETY: `name` and `attributes` outlive the call.
        let handle = unsafe {
            CreateNamedPipeW(
                &HSTRING::from(name),
                open_mode,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                4096,
                4096,
                0,
                Some(&attributes),
            )
        };
        if handle.is_invalid() {
            return Err(windows::core::Error::from_thread())
                .with_context(|| format!("creating pipe {name} (already in use?)"));
        }
        Ok(Self(handle))
    }

    /// Block until a client connects. `Ok(false)` when the client came and
    /// went before we got to it — a dead connection to discard, not a server
    /// failure (any local user can cause one).
    fn accept(&self) -> Result<bool> {
        // SAFETY: an unconnected instance we own.
        match unsafe { ConnectNamedPipe(self.0, None) } {
            Ok(()) => Ok(true),
            Err(error) if error.code() == ERROR_PIPE_CONNECTED.to_hresult() => Ok(true),
            Err(error) if error.code() == ERROR_NO_DATA.to_hresult() => Ok(false),
            Err(error) => Err(error).context("waiting for an updater client"),
        }
    }

    fn into_file(self) -> File {
        let handle = self.0;
        std::mem::forget(self);
        // SAFETY: ownership of the connected instance moves into the File.
        unsafe { File::from_raw_handle(handle.0) }
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        // SAFETY: closing the instance we own.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// Serve `handler` on `name` until `stop` is set (then call [`wake`]).
pub fn serve(name: &str, sddl: &str, handler: Handler, stop: Arc<AtomicBool>) -> Result<()> {
    let security = SecurityDescriptor::from_sddl(sddl)?;
    let active = Arc::new(AtomicUsize::new(0));
    let mut listening = Instance::create(name, &security, true)?;
    loop {
        let live = listening.accept()?;
        if stop.load(Ordering::SeqCst) {
            return Ok(());
        }
        // Open the next instance *before* releasing this one, so the name
        // never disappears.
        let next = Instance::create(name, &security, false)?;
        let accepted = std::mem::replace(&mut listening, next);
        if !live {
            continue; // dropped: the client already left
        }
        let connected = accepted.into_file();
        if active.load(Ordering::SeqCst) >= MAX_CONNECTIONS {
            continue; // dropped: closes the connection
        }
        active.fetch_add(1, Ordering::SeqCst);
        let handler = handler.clone();
        let active = active.clone();
        std::thread::Builder::new()
            .name("filex-updater-pipe".into())
            .spawn(move || {
                if let Err(error) = serve_connection(connected, &handler) {
                    tracing::debug!("updater connection ended: {error:#}");
                }
                active.fetch_sub(1, Ordering::SeqCst);
            })?;
    }
}

fn serve_connection(mut stream: File, handler: &Handler) -> Result<()> {
    let mut session = 0u32;
    // SAFETY: a connected server instance.
    unsafe { GetNamedPipeClientSessionId(HANDLE(stream.as_raw_handle()), &mut session) }
        .context("reading the client's session")?;
    let request: Request = read_frame(&mut stream)?;
    let response = if request.protocol == PROTOCOL {
        handler(request.command, session)
    } else {
        Response::Error(format!(
            "updater protocol {} is not supported (expected {PROTOCOL})",
            request.protocol
        ))
    };
    write_frame(&mut stream, &response)
}

/// Unblock a [`serve`] loop waiting in `ConnectNamedPipe` after setting its
/// stop flag. Best effort.
pub fn wake(name: &str) {
    let _ = open(name, Duration::from_millis(500));
}

/// Open the pipe as a client, retrying while it is busy or not yet created.
fn open(name: &str, timeout: Duration) -> Result<File> {
    let deadline = Instant::now() + timeout;
    loop {
        let attempt = std::fs::OpenOptions::new()
            .access_mode(PIPE_CLIENT_ACCESS)
            .security_qos_flags(SECURITY_IDENTIFICATION)
            .open(name);
        match attempt {
            Ok(file) => return Ok(file),
            Err(error) if Instant::now() >= deadline => {
                return Err(error).with_context(|| format!("connecting to {name}"));
            }
            Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY.0 as i32) => {
                // SAFETY: plain wait on a pipe name.
                let _ = unsafe { WaitNamedPipeW(&HSTRING::from(name), 250) };
            }
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

/// Send one command and read the response. When `expected_server` is set,
/// the pipe's server process must be that PID (the service's, from the
/// SCM) — so a process squatting the name can't answer in its place.
pub fn call(
    name: &str,
    command: Command,
    expected_server: Option<u32>,
    timeout: Duration,
) -> Result<Response> {
    let mut stream = open(name, timeout)?;
    if let Some(expected) = expected_server {
        let mut server = 0u32;
        // SAFETY: a connected client handle.
        unsafe { GetNamedPipeServerProcessId(HANDLE(stream.as_raw_handle()), &mut server) }
            .context("reading the pipe server's process id")?;
        ensure!(
            server == expected,
            "pipe {name} is served by process {server}, not the updater service ({expected})"
        );
    }
    write_frame(
        &mut stream,
        &Request {
            protocol: PROTOCOL,
            command,
        },
    )?;
    match read_frame(&mut stream)? {
        Response::Error(message) => bail!("updater: {message}"),
        response => Ok(response),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::updater::protocol::Phase;
    use crate::updater::windows::security::PIPE_SDDL;

    fn test_pipe(tag: &str) -> String {
        format!(r"\\.\pipe\filex-updater-test-{tag}-{}", std::process::id())
    }

    fn start(name: &str) -> (Arc<AtomicBool>, std::thread::JoinHandle<Result<()>>) {
        let stop = Arc::new(AtomicBool::new(false));
        let handler: Handler = Arc::new(|command, session| match command {
            Command::Hello => Response::Hello {
                protocol: PROTOCOL,
                version: format!("session-{session}"),
            },
            _ => Response::Status(Phase::Idle),
        });
        let server = {
            let (name, stop) = (name.to_string(), stop.clone());
            std::thread::spawn(move || serve(&name, PIPE_SDDL, handler, stop))
        };
        (stop, server)
    }

    #[test]
    fn round_trips_and_reports_the_callers_session() {
        let name = test_pipe("roundtrip");
        let (stop, server) = start(&name);
        let me = std::process::id();
        let mut session = 0u32;
        // SAFETY: querying our own session id.
        unsafe { windows::Win32::System::RemoteDesktop::ProcessIdToSessionId(me, &mut session) }
            .unwrap();
        for _ in 0..3 {
            let response = call(&name, Command::Hello, Some(me), Duration::from_secs(5)).unwrap();
            assert_eq!(
                response,
                Response::Hello {
                    protocol: PROTOCOL,
                    version: format!("session-{session}")
                }
            );
        }
        stop.store(true, Ordering::SeqCst);
        wake(&name);
        server.join().unwrap().unwrap();
    }

    #[test]
    fn a_wrong_server_pid_is_refused() {
        let name = test_pipe("pid");
        let (stop, server) = start(&name);
        let error = call(
            &name,
            Command::Status,
            Some(u32::MAX),
            Duration::from_secs(5),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("not the updater service"), "{error}");
        stop.store(true, Ordering::SeqCst);
        wake(&name);
        server.join().unwrap().unwrap();
    }

    /// Regression: a client that connected and left before the server's
    /// `ConnectNamedPipe` (ERROR_NO_DATA) used to end the serve loop — any
    /// local user could take the updater down.
    #[test]
    fn abandoned_connections_do_not_stop_the_server() {
        let name = test_pipe("abandon");
        let (stop, server) = start(&name);
        for _ in 0..5 {
            drop(open(&name, Duration::from_secs(5)).unwrap());
        }
        let response = call(&name, Command::Status, None, Duration::from_secs(5)).unwrap();
        assert_eq!(response, Response::Status(Phase::Idle));
        assert!(!server.is_finished());
        stop.store(true, Ordering::SeqCst);
        wake(&name);
        server.join().unwrap().unwrap();
    }

    #[test]
    fn a_second_server_on_the_same_name_fails() {
        let name = test_pipe("squat");
        let (stop, server) = start(&name);
        // Wait until the first server is listening.
        call(&name, Command::Status, None, Duration::from_secs(5)).unwrap();
        let security = SecurityDescriptor::from_sddl(PIPE_SDDL).unwrap();
        assert!(Instance::create(&name, &security, true).is_err());
        stop.store(true, Ordering::SeqCst);
        wake(&name);
        server.join().unwrap().unwrap();
    }

    #[test]
    fn a_protocol_mismatch_is_answered_with_an_error() {
        let name = test_pipe("protocol");
        let (stop, server) = start(&name);
        let mut stream = open(&name, Duration::from_secs(5)).unwrap();
        write_frame(
            &mut stream,
            &Request {
                protocol: PROTOCOL + 1,
                command: Command::Hello,
            },
        )
        .unwrap();
        let response: Response = read_frame(&mut stream).unwrap();
        assert!(matches!(response, Response::Error(_)));
        stop.store(true, Ordering::SeqCst);
        wake(&name);
        server.join().unwrap().unwrap();
    }
}

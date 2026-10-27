//! One UI process per user: a second `filex <path>` hands its target to
//! the running window instead of opening another app instance.
//!
//! Same transport as the index daemon (`daemon::ipc`): a loopback TCP
//! listener whose port and a random token sit in `endpoint.json` in the
//! per-user data directory. An exclusive lock on `ui.lock` decides which
//! process is primary, so two launches racing at login cannot both become
//! servers. The token keeps other local users from driving the window.

use crate::daemon::ipc::{read_frame, write_frame};
use crate::launch::Browse;
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::Write;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const VERSION: u32 = 1;
/// How long a second launch keeps retrying while the primary is between
/// taking the lock and publishing its endpoint.
const CONNECT_PATIENCE: Duration = Duration::from_millis(1500);

#[derive(Debug, Serialize, Deserialize)]
struct Endpoint {
    version: u32,
    port: u16,
    token: String,
    pid: u32,
}

#[derive(Debug, Serialize, Deserialize)]
struct Message {
    version: u32,
    token: String,
    browse: Browse,
}

#[derive(Debug, Serialize, Deserialize)]
struct Ack {
    ok: bool,
}

/// The outcome of starting up.
pub enum Startup {
    /// This process owns the UI; call [`Primary::serve`] once the app runs.
    Primary(Primary),
    /// The running instance took the request; this process should exit.
    Forwarded,
}

pub struct Primary {
    listener: TcpListener,
    token: String,
    /// Held for the life of the process: the OS drops it on exit or crash.
    _lock: File,
}

pub fn default_directory() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("filex/ui")
}

/// Become the primary instance, or forward `request` to the existing one.
/// `allow_foreground(pid)` runs just before forwarding so the platform can
/// let the primary take focus (Windows only hands focus to processes the
/// foreground app allows).
pub fn claim_or_forward(
    dir: &Path,
    request: &Browse,
    allow_foreground: impl Fn(u32),
) -> Result<Startup> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    // No truncation: the primary holds a lock on this (empty) file.
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("ui.lock"))?;
    if lock.try_lock_exclusive().is_ok() {
        return Ok(Startup::Primary(Primary::start(dir, lock)?));
    }
    forward(dir, request, allow_foreground)?;
    Ok(Startup::Forwarded)
}

impl Primary {
    fn start(dir: &Path, lock: File) -> Result<Self> {
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))?;
        let token = random_token()?;
        let endpoint = Endpoint {
            version: VERSION,
            port: listener.local_addr()?.port(),
            token: token.clone(),
            pid: std::process::id(),
        };
        publish_endpoint(dir, &endpoint)?;
        Ok(Self {
            listener,
            token,
            _lock: lock,
        })
    }

    /// Accept forwarded launches on a background thread for the rest of
    /// the process, calling `on_request` for each one with a valid token.
    pub fn serve(self, on_request: impl Fn(Browse) + Send + 'static) {
        let Primary {
            listener,
            token,
            _lock,
        } = self;
        std::thread::Builder::new()
            .name("filex-single-instance".into())
            .spawn(move || {
                let _lock = _lock;
                for stream in listener.incoming() {
                    let Ok(stream) = stream else { continue };
                    match handle(stream, &token) {
                        Ok(browse) => on_request(browse),
                        Err(err) => tracing::warn!("rejected a forwarded launch: {err:#}"),
                    }
                }
            })
            .map(drop)
            .unwrap_or_else(|err| tracing::warn!("single-instance server not started: {err}"));
    }
}

fn handle(mut stream: TcpStream, token: &str) -> Result<Browse> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let message: Message = read_frame(&mut stream)?;
    let ok = message.version == VERSION && message.token == token;
    write_frame(&mut stream, &Ack { ok })?;
    ensure!(ok, "bad token or version");
    Ok(message.browse)
}

fn forward(dir: &Path, request: &Browse, allow_foreground: impl Fn(u32)) -> Result<()> {
    let deadline = Instant::now() + CONNECT_PATIENCE;
    loop {
        match try_forward(dir, request, &allow_foreground) {
            Ok(()) => return Ok(()),
            Err(err) if Instant::now() >= deadline => return Err(err),
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

fn try_forward(dir: &Path, request: &Browse, allow_foreground: &impl Fn(u32)) -> Result<()> {
    let endpoint: Endpoint = serde_json::from_slice(&std::fs::read(dir.join("endpoint.json"))?)?;
    ensure!(endpoint.version == VERSION, "UI endpoint version mismatch");
    let mut stream = TcpStream::connect_timeout(
        &SocketAddr::from(([127, 0, 0, 1], endpoint.port)),
        Duration::from_millis(300),
    )?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    allow_foreground(endpoint.pid);
    write_frame(
        &mut stream,
        &Message {
            version: VERSION,
            token: endpoint.token,
            browse: request.clone(),
        },
    )?;
    let ack: Ack = read_frame(&mut stream)?;
    ensure!(ack.ok, "running instance refused the launch");
    Ok(())
}

/// Write `endpoint.json` via a temp file + rename, so a reader never sees
/// half a file.
fn publish_endpoint(dir: &Path, endpoint: &Endpoint) -> Result<()> {
    let tmp = dir.join("endpoint.json.tmp");
    let mut file = File::create(&tmp)?;
    file.write_all(&serde_json::to_vec(endpoint)?)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, dir.join("endpoint.json"))?;
    Ok(())
}

fn random_token() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).map_err(|e| anyhow::anyhow!("no OS randomness: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn second_launch_is_forwarded_to_the_primary() {
        let dir = tempfile::tempdir().unwrap();
        let Startup::Primary(primary) =
            claim_or_forward(dir.path(), &Browse::Default, |_| {}).unwrap()
        else {
            panic!("first launch should be primary");
        };
        let (tx, rx) = mpsc::channel();
        primary.serve(move |browse| {
            tx.send(browse).ok();
        });

        let target = Browse::Open(PathBuf::from("somewhere"));
        let seen_pid = std::sync::Mutex::new(None);
        let second = claim_or_forward(dir.path(), &target, |pid| {
            *seen_pid.lock().unwrap() = Some(pid);
        })
        .unwrap();
        assert!(matches!(second, Startup::Forwarded));
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), target);
        assert_eq!(*seen_pid.lock().unwrap(), Some(std::process::id()));
    }

    #[test]
    fn a_wrong_token_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let Startup::Primary(primary) =
            claim_or_forward(dir.path(), &Browse::Default, |_| {}).unwrap()
        else {
            panic!("first launch should be primary");
        };
        let (tx, rx) = mpsc::channel();
        primary.serve(move |browse| {
            tx.send(browse).ok();
        });
        // Tamper with the published token.
        let path = dir.path().join("endpoint.json");
        let mut endpoint: Endpoint =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        endpoint.token = "forged".into();
        std::fs::write(&path, serde_json::to_vec(&endpoint).unwrap()).unwrap();

        let err = try_forward(dir.path(), &Browse::ThisPc, &|_| {}).unwrap_err();
        assert!(err.to_string().contains("refused"), "{err:#}");
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
    }

    #[test]
    fn tokens_are_random_hex() {
        let a = random_token().unwrap();
        let b = random_token().unwrap();
        assert_eq!(a.len(), 32);
        assert_ne!(a, b);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }
}

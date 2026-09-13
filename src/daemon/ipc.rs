//! Versioned authenticated loopback protocol. The endpoint capability is stored
//! in a private per-user data directory; no privileged filesystem operations.
use crate::{catalog::segment::Identity, search::literal::Tier, search_filter::Filter};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    net::{Shutdown, TcpStream},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};
pub const VERSION: u32 = 2;
pub const MAX_FRAME: usize = 8 * 1024 * 1024;
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Query {
    pub text: String,
    pub filters: Vec<Filter>,
    #[serde(with = "crate::catalog::path_codec::optional")]
    pub scope: Option<PathBuf>,
    #[serde(with = "crate::catalog::path_codec::optional_list")]
    pub allowed: Option<Vec<PathBuf>>,
    pub limit: usize,
    pub offset: usize,
    pub fuzzy: bool,
    pub client: u64,
    pub request: u64,
    pub epoch_hint: Option<u64>,
}
impl Default for Query {
    fn default() -> Self {
        Self {
            text: String::new(),
            filters: Vec::new(),
            scope: None,
            allowed: None,
            limit: 100,
            offset: 0,
            fuzzy: true,
            client: 0,
            request: 0,
            epoch_hint: None,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Hit {
    pub id: u64,
    pub name: String,
    #[serde(with = "crate::catalog::path_codec")]
    pub path: PathBuf,
    pub is_dir: bool,
    pub identity: Identity,
    pub tier: Tier,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RootStatus {
    #[serde(with = "crate::catalog::path_codec")]
    pub path: PathBuf,
    pub files: u64,
    pub state: String,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Status {
    pub epoch: u64,
    pub roots: Vec<RootStatus>,
    pub building: bool,
    pub error: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Page {
    pub partial: bool,
    pub epoch: u64,
    pub hits: Vec<Hit>,
    pub more: bool,
    pub examined: usize,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Batch {
    pub epoch: u64,
    pub hits: Vec<Hit>,
    pub scanned: u64,
    pub total: Option<u64>,
}
#[derive(Serialize, Deserialize)]
pub enum Command {
    Status,
    Reconcile,
    Search(Query),
    StreamMatches(Query),
    AddRoot(#[serde(with = "crate::catalog::path_codec")] PathBuf),
    RemoveRoot(#[serde(with = "crate::catalog::path_codec")] PathBuf),
    HintFilesystemChange {
        operation_id: u64,
        #[serde(with = "crate::catalog::path_codec::list")]
        paths: Vec<PathBuf>,
    },
    Cancel {
        client: u64,
        before_request: u64,
    },
    Touch {
        #[serde(with = "crate::catalog::path_codec")]
        path: PathBuf,
    },
    Verify(Hit),
}
#[derive(Serialize, Deserialize)]
pub struct Request {
    pub version: u32,
    pub token: String,
    pub command: Command,
}
#[derive(Serialize, Deserialize)]
pub enum Response {
    Status(Status),
    Page(Page),
    Batch(Batch),
    Ack,
    Error(String),
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Endpoint {
    pub version: u32,
    pub port: u16,
    pub token: String,
}
#[derive(Clone)]
pub struct Client {
    endpoint: Endpoint,
    pub directory: PathBuf,
}
pub fn default_directory() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("filex/index-v2")
}
pub fn write_frame<T: Serialize>(stream: &mut impl Write, message: &T) -> Result<()> {
    let bytes = serde_json::to_vec(message)?;
    ensure!(bytes.len() <= MAX_FRAME, "IPC frame exceeds limit");
    stream.write_all(&(bytes.len() as u32).to_le_bytes())?;
    stream.write_all(&bytes)?;
    stream.flush()?;
    Ok(())
}
pub fn read_frame<T: serde::de::DeserializeOwned>(stream: &mut impl Read) -> Result<T> {
    let mut header = [0u8; 4];
    stream.read_exact(&mut header)?;
    let len = u32::from_le_bytes(header) as usize;
    ensure!(len <= MAX_FRAME, "IPC frame exceeds limit");
    let mut bytes = vec![0; len];
    stream.read_exact(&mut bytes)?;
    Ok(serde_json::from_slice(&bytes)?)
}
impl Client {
    pub fn connect(directory: &Path) -> Result<Self> {
        let endpoint: Endpoint =
            serde_json::from_slice(&std::fs::read(directory.join("endpoint.json"))?)?;
        ensure!(
            endpoint.version == VERSION,
            "index daemon protocol mismatch"
        );
        let client = Self {
            endpoint,
            directory: directory.to_path_buf(),
        };
        client.status()?;
        Ok(client)
    }
    pub fn try_connect() -> Result<Self> {
        Self::connect(&default_directory())
    }
    fn open(&self, command: Command) -> Result<TcpStream> {
        let mut stream = TcpStream::connect_timeout(
            &std::net::SocketAddr::from(([127, 0, 0, 1], self.endpoint.port)),
            Duration::from_millis(300),
        )?;
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(Duration::from_secs(30)))?;
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
        write_frame(
            &mut stream,
            &Request {
                version: VERSION,
                token: self.endpoint.token.clone(),
                command,
            },
        )?;
        Ok(stream)
    }
    pub fn call(&self, command: Command) -> Result<Response> {
        let mut stream = self.open(command)?;
        let response = read_frame(&mut stream)?;
        if let Response::Error(error) = response {
            anyhow::bail!(error);
        }
        Ok(response)
    }
    pub fn status(&self) -> Result<Status> {
        match self.call(Command::Status)? {
            Response::Status(s) => Ok(s),
            _ => anyhow::bail!("invalid status response"),
        }
    }
    pub fn search(&self, query: Query, cancel: &AtomicBool) -> Result<Page> {
        ensure!(!cancel.load(Ordering::Relaxed), "search cancelled");
        match self.call(Command::Search(query))? {
            Response::Page(page) => {
                ensure!(!cancel.load(Ordering::Relaxed), "search cancelled");
                Ok(page)
            }
            _ => anyhow::bail!("invalid search response"),
        }
    }
    pub fn stream_matches(
        &self,
        query: Query,
        cancel: &AtomicBool,
        mut consume: impl FnMut(Batch) -> bool,
    ) -> Result<()> {
        let mut stream = self.open(Command::StreamMatches(query))?;
        loop {
            if cancel.load(Ordering::Relaxed) {
                stream.shutdown(Shutdown::Both).ok();
                anyhow::bail!("stream cancelled");
            }
            match read_frame(&mut stream)? {
                Response::Batch(batch) => {
                    let done = batch.total.is_some();
                    if !consume(batch) || done {
                        stream.shutdown(Shutdown::Both).ok();
                        return Ok(());
                    }
                }
                Response::Error(e) => anyhow::bail!(e),
                _ => anyhow::bail!("invalid stream response"),
            }
        }
    }
    pub fn hint(&self, operation_id: u64, paths: Vec<PathBuf>) -> Result<()> {
        self.call(Command::HintFilesystemChange {
            operation_id,
            paths,
        })?;
        Ok(())
    }
    pub fn verify(&self, hit: Hit) -> Result<()> {
        self.call(Command::Verify(hit))?;
        Ok(())
    }
    pub fn start() -> Result<Self> {
        if let Ok(client) = Self::try_connect() {
            return Ok(client);
        }
        let executable = super::executable::daemon()?;
        let mut command = std::process::Command::new(executable);
        command
            .arg("--user")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let mut child = command.spawn()?;
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        for _ in 0..40 {
            if let Ok(client) = Self::try_connect() {
                return Ok(client);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        anyhow::bail!("search daemon unavailable")
    }
}
/// Successful UI operations enqueue advisory hints without blocking the file op.
pub fn notify_applied(applied: &crate::ops::AppliedOp) {
    use crate::ops::AppliedOp;
    use std::sync::{OnceLock, mpsc};
    static QUEUE: OnceLock<mpsc::SyncSender<Vec<PathBuf>>> = OnceLock::new();
    let queue = QUEUE.get_or_init(|| {
        let (tx, rx) = mpsc::sync_channel::<Vec<PathBuf>>(128);
        std::thread::spawn(move || {
            let mut client = None;
            let mut operation = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(1, |d| d.as_nanos() as u64);
            while let Ok(paths) = rx.recv() {
                if client.is_none() {
                    client = Client::try_connect().ok();
                }
                if let Some(c) = &client {
                    operation = operation.wrapping_add(1);
                    if c.hint(operation, paths).is_err() {
                        client = None;
                    }
                }
            }
        });
        tx
    });
    let paths = match applied {
        AppliedOp::Moved { from, to }
        | AppliedOp::Copied { from, to }
        | AppliedOp::Renamed { from, to } => vec![from.clone(), to.clone()],
        AppliedOp::Deleted { original, .. } => vec![original.clone()],
    };
    let _ = queue.try_send(paths);
}

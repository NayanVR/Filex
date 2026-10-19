//! Versioned authenticated loopback protocol. The endpoint capability is stored
//! in a private per-user data directory; no privileged filesystem operations.
use crate::{catalog::segment::Identity, search::filter::Filter, search::literal::Tier};
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
    /// Set while a full walk runs. Absent from daemons before v0.3.4.
    #[serde(default)]
    pub progress: Option<Progress>,
}
/// How far a full walk (first index, reconcile) has got.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    /// Entries reached so far, across roots.
    pub scanned: u64,
    /// The walk is done and the index is being written.
    pub indexing: bool,
    /// A few names the walk passed, oldest first.
    pub recent: Vec<String>,
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
    /// Exit cleanly (writer flushed, endpoint removed). Sent by the app
    /// before an update replaces the binaries. Daemons before v0.3.6 don't
    /// know it and drop the connection.
    Shutdown,
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
    /// Ask the daemon to exit cleanly and wait (bounded) until its endpoint
    /// is gone. Errors mean it may still be running.
    pub fn shutdown(&self, timeout: Duration) -> Result<()> {
        self.call(Command::Shutdown)?;
        let endpoint = self.directory.join("endpoint.json");
        let deadline = std::time::Instant::now() + timeout;
        while endpoint.exists() {
            ensure!(
                std::time::Instant::now() < deadline,
                "index daemon did not exit within {timeout:?}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
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
        AppliedOp::Created { path } => vec![path.clone()],
    };
    let _ = queue.try_send(paths);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        catalog::segment::Identity,
        listing::FileKind,
        search::filter::{Bound, Filter},
    };
    use std::io::Cursor;

    /// A path no `String` can hold: invalid UTF-8 on Unix, an unpaired
    /// surrogate on Windows. `path_codec` must carry it across untouched.
    fn odd_path() -> PathBuf {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            PathBuf::from(std::ffi::OsStr::from_bytes(b"/tmp/odd\xff\xfe\x80.bin"))
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStringExt;
            let mut units: Vec<u16> = "C:\\odd".encode_utf16().collect();
            units.extend([0xD800, u16::from(b'x')]);
            PathBuf::from(std::ffi::OsString::from_wide(&units))
        }
        #[cfg(not(any(unix, windows)))]
        {
            PathBuf::from("/tmp/odd.bin")
        }
    }

    fn query() -> Query {
        Query {
            text: "report ünï".into(),
            filters: vec![
                Filter::Ext("pdf".into()),
                Filter::Kind(FileKind::Image),
                Filter::Size(Bound::Range(1, u64::MAX)),
                Filter::Modified(Bound::Lt(i64::MIN)),
                Filter::Tag("work".into()),
            ],
            scope: Some(odd_path()),
            allowed: Some(vec![odd_path(), PathBuf::from("plain")]),
            limit: 7,
            offset: 3,
            fuzzy: false,
            client: u64::MAX,
            request: 42,
            epoch_hint: Some(9),
        }
    }

    fn hit() -> Hit {
        Hit {
            id: 5,
            name: "odd.bin".into(),
            path: odd_path(),
            is_dir: false,
            identity: Identity {
                device: u64::MAX,
                key: 1,
                birth: 2,
            },
            tier: Tier::Fuzzy,
        }
    }

    fn commands() -> Vec<Command> {
        vec![
            Command::Status,
            Command::Reconcile,
            Command::Search(query()),
            Command::StreamMatches(Query::default()),
            Command::AddRoot(odd_path()),
            Command::RemoveRoot(odd_path()),
            Command::HintFilesystemChange {
                operation_id: u64::MAX,
                paths: vec![odd_path(), PathBuf::new()],
            },
            Command::Cancel {
                client: 1,
                before_request: 2,
            },
            Command::Touch { path: odd_path() },
            Command::Verify(hit()),
            Command::Shutdown,
        ]
    }

    fn responses() -> Vec<Response> {
        vec![
            Response::Status(Status {
                epoch: 3,
                roots: vec![RootStatus {
                    path: odd_path(),
                    files: 10,
                    state: "ready".into(),
                }],
                building: true,
                error: Some("disk gone".into()),
                progress: Some(Progress {
                    scanned: 99,
                    indexing: true,
                    recent: vec!["a".into(), "b".into()],
                }),
            }),
            Response::Page(Page {
                partial: true,
                epoch: 4,
                hits: vec![hit()],
                more: true,
                examined: 12,
            }),
            Response::Batch(Batch {
                epoch: 5,
                hits: vec![hit(), hit()],
                scanned: 6,
                total: None,
            }),
            Response::Ack,
            Response::Error("boom".into()),
        ]
    }

    /// Wire types carry no `PartialEq`; the JSON value of a re-encoding
    /// is the comparison (it includes the raw path bytes).
    fn roundtrip<T: Serialize + serde::de::DeserializeOwned>(message: &T) -> T {
        let mut wire = Vec::new();
        write_frame(&mut wire, message).expect("write");
        let mut reader = Cursor::new(wire);
        let decoded: T = read_frame(&mut reader).expect("read");
        assert_eq!(reader.position() as usize, reader.get_ref().len());
        assert_eq!(
            serde_json::to_value(&decoded).expect("re-encode"),
            serde_json::to_value(message).expect("encode")
        );
        decoded
    }

    fn frame(body: &[u8]) -> Vec<u8> {
        let mut wire = (body.len() as u32).to_le_bytes().to_vec();
        wire.extend_from_slice(body);
        wire
    }

    #[test]
    fn every_request_command_roundtrips() {
        for command in commands() {
            let request = roundtrip(&Request {
                version: VERSION,
                token: "secret".into(),
                command,
            });
            assert_eq!(request.version, VERSION);
            assert_eq!(request.token, "secret");
        }
    }

    #[test]
    fn non_utf8_paths_survive_the_wire_byte_for_byte() {
        let decoded = roundtrip(&Command::HintFilesystemChange {
            operation_id: 1,
            paths: vec![odd_path()],
        });
        let Command::HintFilesystemChange { paths, .. } = decoded else {
            panic!("wrong variant");
        };
        assert_eq!(paths, vec![odd_path()]);
        let Command::Search(query) = roundtrip(&Command::Search(query())) else {
            panic!("wrong variant");
        };
        assert_eq!(query.scope, Some(odd_path()));
        assert_eq!(
            query.allowed,
            Some(vec![odd_path(), PathBuf::from("plain")])
        );
        assert_eq!(query.filters, self::query().filters);
    }

    #[test]
    fn every_response_roundtrips() {
        for response in responses() {
            roundtrip(&response);
        }
    }

    #[test]
    fn frame_header_is_le_length_of_json_body() {
        let mut wire = Vec::new();
        write_frame(&mut wire, &Response::Ack).expect("write");
        let body = serde_json::to_vec(&Response::Ack).expect("encode");
        assert_eq!(wire, frame(&body));
    }

    #[test]
    fn write_rejects_oversized_message_without_writing() {
        let mut wire = Vec::new();
        let huge = Response::Error("x".repeat(MAX_FRAME));
        assert!(write_frame(&mut wire, &huge).is_err());
        assert!(wire.is_empty());
    }

    /// Fails the test if the body of an oversized frame is ever read.
    struct NoBody;
    impl Read for NoBody {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            panic!("read_frame read past an oversized header");
        }
    }

    #[test]
    fn read_rejects_oversized_header_before_reading_body() {
        let header = ((MAX_FRAME + 1) as u32).to_le_bytes();
        let mut reader = Cursor::new(header).chain(NoBody);
        assert!(read_frame::<Response>(&mut reader).is_err());
        let mut reader = Cursor::new(u32::MAX.to_le_bytes()).chain(NoBody);
        assert!(read_frame::<Response>(&mut reader).is_err());
    }

    #[test]
    fn read_accepts_frame_of_exactly_max_size() {
        // A JSON string of MAX_FRAME bytes: quotes plus filler.
        let mut body = vec![b'"'];
        body.resize(MAX_FRAME - 1, b'a');
        body.push(b'"');
        let value: String = read_frame(&mut Cursor::new(frame(&body))).expect("read");
        assert_eq!(value.len(), MAX_FRAME - 2);
    }

    #[test]
    fn read_rejects_truncated_header() {
        for len in 0..4 {
            let mut reader = Cursor::new(vec![1u8; len]);
            assert!(read_frame::<Response>(&mut reader).is_err(), "{len} bytes");
        }
    }

    #[test]
    fn read_rejects_truncated_body() {
        let body = serde_json::to_vec(&Response::Error("hello".into())).expect("encode");
        let wire = frame(&body);
        for cut in 4..wire.len() {
            let mut reader = Cursor::new(&wire[..cut]);
            assert!(read_frame::<Response>(&mut reader).is_err(), "cut at {cut}");
        }
    }

    #[test]
    fn read_rejects_invalid_json() {
        for body in [&b"{not json"[..], b"", b"\xff\xfe", b"\"Ack\" trailing"] {
            let mut reader = Cursor::new(frame(body));
            assert!(read_frame::<Response>(&mut reader).is_err());
        }
    }

    #[test]
    fn back_to_back_frames_decode_in_order() {
        let mut wire = Vec::new();
        for response in responses() {
            write_frame(&mut wire, &response).expect("write");
        }
        let mut reader = Cursor::new(wire);
        for expected in responses() {
            let decoded: Response = read_frame(&mut reader).expect("read");
            assert_eq!(
                serde_json::to_value(&decoded).expect("encode"),
                serde_json::to_value(&expected).expect("encode")
            );
        }
        assert!(read_frame::<Response>(&mut reader).is_err());
    }

    #[test]
    fn unknown_command_is_an_error_not_a_panic() {
        for command in [r#"{"Bogus":null}"#, r#""Bogus""#, r#"{"Search":"nope"}"#] {
            let body = format!(r#"{{"version":2,"token":"t","command":{command}}}"#);
            let mut reader = Cursor::new(frame(body.as_bytes()));
            assert!(read_frame::<Request>(&mut reader).is_err(), "{command}");
        }
    }

    /// Unit variants are bare strings on the wire; older daemons rely on
    /// unknown ones (e.g. `Shutdown` before v0.3.6) failing to decode.
    #[test]
    fn unit_commands_keep_their_wire_names() {
        for (command, wire) in [
            (Command::Status, r#""Status""#),
            (Command::Reconcile, r#""Reconcile""#),
            (Command::Shutdown, r#""Shutdown""#),
        ] {
            assert_eq!(serde_json::to_string(&command).expect("encode"), wire);
        }
    }

    #[test]
    fn status_without_progress_from_older_daemons_decodes() {
        let body = br#"{"Status":{"epoch":1,"roots":[],"building":false,"error":null}}"#;
        let Response::Status(status) = read_frame(&mut Cursor::new(frame(body))).expect("read")
        else {
            panic!("wrong variant");
        };
        assert_eq!(status.progress, None);
    }
}

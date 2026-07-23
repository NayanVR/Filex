//! IPC between the (elevated) index service and unelevated clients —
//! the "Everything" process split (docs/indexing-architecture.md §1).
//!
//! The protocol is deliberately tiny and hand-rolled, like the snapshot
//! format: length-prefixed frames over any ordered byte stream, so the
//! same server/client code runs over a Windows named pipe in production
//! and over loopback TCP / in-memory pipes in tests on every OS.
//!
//! Wire format (all integers little-endian):
//!   frame   := u32 payload_len, payload           (len capped, see MAX_FRAME)
//!   payload := u8 tag, fields…
//!   string  := u32 len, UTF-8 bytes
//!
//! A connection starts with Hello/HelloOk carrying PROTOCOL_VERSION;
//! mismatches get an Error frame and a closed connection, so old clients
//! fail loudly instead of misparsing.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::PoisonError;

use anyhow::{Context as _, Result, bail, ensure};

use super::manager;
use super::watcher::SharedIndex;

pub const PROTOCOL_VERSION: u32 = 1;

/// The well-known endpoint of the index service on Windows.
pub const PIPE_NAME: &str = r"\\.\pipe\filex-index";

/// Upper bound on a single frame; a corrupt length prefix must not make
/// a client allocate gigabytes. Generous for 500 search results.
pub const MAX_FRAME: usize = 16 * 1024 * 1024;

const TAG_HELLO: u8 = 1;
const TAG_STATUS: u8 = 2;
const TAG_SEARCH: u8 = 3;
const TAG_HELLO_OK: u8 = 128;
const TAG_STATUS_REPLY: u8 = 129;
const TAG_SEARCH_REPLY: u8 = 130;
const TAG_ERROR: u8 = 255;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Hello { version: u32 },
    Status,
    Search { query: String, limit: u32 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    HelloOk { version: u32 },
    Status(HostStatus),
    Search(Vec<RemoteHit>),
    Error { message: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteHit {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootStatus {
    pub path: String,
    pub files: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostStatus {
    pub roots: Vec<RootStatus>,
}

// ---- codec ----

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        ensure!(self.pos + n <= self.buf.len(), "frame truncated");
        let slice = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().expect("4 bytes")))
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().expect("8 bytes")))
    }

    fn string(&mut self) -> Result<String> {
        let len = self.u32()? as usize;
        let bytes = self.take(len)?;
        String::from_utf8(bytes.to_vec()).context("string field not UTF-8")
    }

    fn finished(&self) -> bool {
        self.pos == self.buf.len()
    }
}

fn put_string(buf: &mut Vec<u8>, s: &str) {
    buf.extend_from_slice(&(s.len() as u32).to_le_bytes());
    buf.extend_from_slice(s.as_bytes());
}

pub fn encode_request(request: &Request) -> Vec<u8> {
    let mut buf = Vec::new();
    match request {
        Request::Hello { version } => {
            buf.push(TAG_HELLO);
            buf.extend_from_slice(&version.to_le_bytes());
        }
        Request::Status => buf.push(TAG_STATUS),
        Request::Search { query, limit } => {
            buf.push(TAG_SEARCH);
            put_string(&mut buf, query);
            buf.extend_from_slice(&limit.to_le_bytes());
        }
    }
    buf
}

pub fn decode_request(payload: &[u8]) -> Result<Request> {
    let mut c = Cursor { buf: payload, pos: 0 };
    let request = match c.u8()? {
        TAG_HELLO => Request::Hello { version: c.u32()? },
        TAG_STATUS => Request::Status,
        TAG_SEARCH => Request::Search { query: c.string()?, limit: c.u32()? },
        tag => bail!("unknown request tag {tag}"),
    };
    ensure!(c.finished(), "trailing bytes in request frame");
    Ok(request)
}

pub fn encode_response(response: &Response) -> Vec<u8> {
    let mut buf = Vec::new();
    match response {
        Response::HelloOk { version } => {
            buf.push(TAG_HELLO_OK);
            buf.extend_from_slice(&version.to_le_bytes());
        }
        Response::Status(status) => {
            buf.push(TAG_STATUS_REPLY);
            buf.extend_from_slice(&(status.roots.len() as u32).to_le_bytes());
            for root in &status.roots {
                put_string(&mut buf, &root.path);
                buf.extend_from_slice(&root.files.to_le_bytes());
            }
        }
        Response::Search(hits) => {
            buf.push(TAG_SEARCH_REPLY);
            buf.extend_from_slice(&(hits.len() as u32).to_le_bytes());
            for hit in hits {
                put_string(&mut buf, &hit.name);
                put_string(&mut buf, &hit.path.to_string_lossy());
                buf.push(hit.is_dir as u8);
            }
        }
        Response::Error { message } => {
            buf.push(TAG_ERROR);
            put_string(&mut buf, message);
        }
    }
    buf
}

pub fn decode_response(payload: &[u8]) -> Result<Response> {
    let mut c = Cursor { buf: payload, pos: 0 };
    let response = match c.u8()? {
        TAG_HELLO_OK => Response::HelloOk { version: c.u32()? },
        TAG_STATUS_REPLY => {
            let count = c.u32()? as usize;
            let mut roots = Vec::with_capacity(count.min(1024));
            for _ in 0..count {
                roots.push(RootStatus { path: c.string()?, files: c.u64()? });
            }
            Response::Status(HostStatus { roots })
        }
        TAG_SEARCH_REPLY => {
            let count = c.u32()? as usize;
            let mut hits = Vec::with_capacity(count.min(4096));
            for _ in 0..count {
                hits.push(RemoteHit {
                    name: c.string()?,
                    path: PathBuf::from(c.string()?),
                    is_dir: c.u8()? != 0,
                });
            }
            Response::Search(hits)
        }
        TAG_ERROR => Response::Error { message: c.string()? },
        tag => bail!("unknown response tag {tag}"),
    };
    ensure!(c.finished(), "trailing bytes in response frame");
    Ok(response)
}

/// Read one frame; None on clean EOF (peer closed between frames).
pub fn read_frame(reader: &mut impl Read) -> Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(err) => return Err(err).context("reading frame length"),
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    ensure!(len <= MAX_FRAME, "frame of {len} bytes exceeds limit");
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload).context("reading frame payload")?;
    Ok(Some(payload))
}

pub fn write_frame(writer: &mut impl Write, payload: &[u8]) -> Result<()> {
    ensure!(payload.len() <= MAX_FRAME, "frame too large to send");
    writer.write_all(&(payload.len() as u32).to_le_bytes())?;
    writer.write_all(payload)?;
    writer.flush()?;
    Ok(())
}

// ---- server ----

/// What the service exposes over IPC. Object-safe so transports hold a
/// `&dyn IndexHost`.
pub trait IndexHost: Send + Sync {
    fn search(&self, query: &str, limit: usize) -> Vec<RemoteHit>;
    fn status(&self) -> HostStatus;
}

/// The standard host: a set of (root, shared index) pairs, answered with
/// the same merged ranking the in-process UI uses.
pub struct MultiRootHost {
    pub roots: Vec<(PathBuf, SharedIndex)>,
}

impl IndexHost for MultiRootHost {
    fn search(&self, query: &str, limit: usize) -> Vec<RemoteHit> {
        let indexes: Vec<SharedIndex> =
            self.roots.iter().map(|(_, index)| index.clone()).collect();
        // Filters are applied client-side today; the IPC frame carries no
        // filter payload yet (that is search-chips phase 5).
        manager::search_all(&indexes, query, &[], limit)
            .into_iter()
            .map(|hit| RemoteHit { name: hit.name, path: hit.path, is_dir: hit.is_dir })
            .collect()
    }

    fn status(&self) -> HostStatus {
        HostStatus {
            roots: self
                .roots
                .iter()
                .map(|(path, index)| RootStatus {
                    path: path.display().to_string(),
                    files: index.read().unwrap_or_else(PoisonError::into_inner).len() as u64,
                })
                .collect(),
        }
    }
}

/// Serve one client connection until it closes. The first frame must be
/// a version-matching Hello; anything else is answered with Error and the
/// connection is dropped.
pub fn serve_connection(
    mut reader: impl Read,
    mut writer: impl Write,
    host: &dyn IndexHost,
) -> Result<()> {
    let mut greeted = false;
    while let Some(payload) = read_frame(&mut reader)? {
        let response = match decode_request(&payload) {
            Ok(Request::Hello { version }) if version == PROTOCOL_VERSION => {
                greeted = true;
                Response::HelloOk { version: PROTOCOL_VERSION }
            }
            Ok(Request::Hello { version }) => {
                let message = format!(
                    "protocol version mismatch: client v{version}, service v{PROTOCOL_VERSION}"
                );
                write_frame(&mut writer, &encode_response(&Response::Error { message }))?;
                bail!("client with mismatched protocol version {version}");
            }
            Ok(_) if !greeted => {
                let message = "request before Hello".to_string();
                write_frame(&mut writer, &encode_response(&Response::Error { message }))?;
                bail!("client skipped the Hello handshake");
            }
            Ok(Request::Status) => Response::Status(host.status()),
            Ok(Request::Search { query, limit }) => {
                Response::Search(host.search(&query, limit.min(10_000) as usize))
            }
            Err(err) => Response::Error { message: format!("{err:#}") },
        };
        write_frame(&mut writer, &encode_response(&response))?;
    }
    Ok(())
}

// ---- client ----

/// Blocking client for the index service, generic over the transport.
/// Call from a background thread, never the UI thread.
pub struct RemoteIndex<R: Read, W: Write> {
    reader: R,
    writer: W,
}

impl<R: Read, W: Write> RemoteIndex<R, W> {
    /// Perform the version handshake and return a ready client.
    pub fn connect(reader: R, writer: W) -> Result<Self> {
        let mut client = Self { reader, writer };
        match client.roundtrip(&Request::Hello { version: PROTOCOL_VERSION })? {
            Response::HelloOk { .. } => Ok(client),
            Response::Error { message } => bail!("service rejected connection: {message}"),
            other => bail!("unexpected handshake response {other:?}"),
        }
    }

    pub fn search(&mut self, query: &str, limit: u32) -> Result<Vec<RemoteHit>> {
        match self.roundtrip(&Request::Search { query: query.to_string(), limit })? {
            Response::Search(hits) => Ok(hits),
            Response::Error { message } => bail!("service error: {message}"),
            other => bail!("unexpected search response {other:?}"),
        }
    }

    pub fn status(&mut self) -> Result<HostStatus> {
        match self.roundtrip(&Request::Status)? {
            Response::Status(status) => Ok(status),
            Response::Error { message } => bail!("service error: {message}"),
            other => bail!("unexpected status response {other:?}"),
        }
    }

    fn roundtrip(&mut self, request: &Request) -> Result<Response> {
        write_frame(&mut self.writer, &encode_request(request))?;
        let payload = read_frame(&mut self.reader)?
            .context("service closed the connection mid-request")?;
        decode_response(&payload)
    }
}

/// Thread-safe handle to a running index service over the named pipe —
/// what the UI holds when `filex-indexd` is present. Requests serialize
/// on an internal mutex (each roundtrip is sub-millisecond); call from
/// background threads only.
#[cfg(target_os = "windows")]
pub struct ServiceClient {
    inner: std::sync::Mutex<RemoteIndex<std::fs::File, std::fs::File>>,
}

#[cfg(target_os = "windows")]
impl ServiceClient {
    /// Fast liveness probe: one connection attempt + handshake. Not-found
    /// simply means no service is installed — callers fall back to
    /// in-process indexing.
    pub fn try_connect() -> Result<Self> {
        let stream = super::windows::connect_index_pipe(PIPE_NAME, 1)?;
        let reader = stream.try_clone().context("cloning pipe stream")?;
        let client = RemoteIndex::connect(reader, stream)?;
        Ok(Self { inner: std::sync::Mutex::new(client) })
    }

    pub fn search(&self, query: &str, limit: u32) -> Result<Vec<RemoteHit>> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .search(query, limit)
    }

    pub fn status(&self) -> Result<HostStatus> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .status()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{ROOT, VolumeIndex};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, RwLock};

    #[test]
    fn requests_and_responses_roundtrip() {
        let requests = [
            Request::Hello { version: 7 },
            Request::Status,
            Request::Search { query: "Übersicht μ".into(), limit: 500 },
        ];
        for request in requests {
            assert_eq!(decode_request(&encode_request(&request)).unwrap(), request);
        }

        let responses = [
            Response::HelloOk { version: PROTOCOL_VERSION },
            Response::Status(HostStatus {
                roots: vec![RootStatus { path: "/vol".into(), files: 42 }],
            }),
            Response::Search(vec![
                RemoteHit { name: "a.txt".into(), path: "/vol/a.txt".into(), is_dir: false },
                RemoteHit { name: "dir".into(), path: "/vol/dir".into(), is_dir: true },
            ]),
            Response::Search(Vec::new()),
            Response::Error { message: "nope".into() },
        ];
        for response in responses {
            assert_eq!(decode_response(&encode_response(&response)).unwrap(), response);
        }
    }

    #[test]
    fn decode_rejects_garbage_and_truncation() {
        assert!(decode_request(&[99]).is_err()); // unknown tag
        assert!(decode_request(&[]).is_err()); // empty
        let mut valid = encode_request(&Request::Search { query: "x".into(), limit: 5 });
        valid.truncate(valid.len() - 1);
        assert!(decode_request(&valid).is_err());
        // Trailing junk is rejected, not silently ignored.
        let mut padded = encode_request(&Request::Status);
        padded.push(0);
        assert!(decode_request(&padded).is_err());
    }

    #[test]
    fn frames_cap_length_and_signal_clean_eof() {
        let mut buf = Vec::new();
        write_frame(&mut buf, b"hello").unwrap();
        let mut reader = &buf[..];
        assert_eq!(read_frame(&mut reader).unwrap().as_deref(), Some(&b"hello"[..]));
        assert_eq!(read_frame(&mut reader).unwrap(), None); // clean EOF

        let huge = (MAX_FRAME as u32 + 1).to_le_bytes();
        assert!(read_frame(&mut &huge[..]).is_err());
    }

    fn test_host() -> MultiRootHost {
        let mut a = VolumeIndex::new("/vol-a");
        a.insert(ROOT, "main.rs", false).unwrap();
        let mut b = VolumeIndex::new("/vol-b");
        b.insert(ROOT, "main", true).unwrap();
        MultiRootHost {
            roots: vec![
                (PathBuf::from("/vol-a"), Arc::new(RwLock::new(a))),
                (PathBuf::from("/vol-b"), Arc::new(RwLock::new(b))),
            ],
        }
    }

    /// Full client/server cycle over a real byte stream (loopback TCP —
    /// transport-agnostic by design; Windows named pipes reuse the exact
    /// same server and client code).
    #[test]
    fn end_to_end_over_a_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let reader = stream.try_clone().unwrap();
            serve_connection(reader, stream, &test_host())
        });

        let stream = TcpStream::connect(addr).unwrap();
        let reader = stream.try_clone().unwrap();
        let mut client = RemoteIndex::connect(reader, stream).unwrap();

        let status = client.status().unwrap();
        assert_eq!(status.roots.len(), 2);
        assert_eq!(status.roots[0].files, 1);

        let hits = client.search("main", 100).unwrap();
        // Merged ranking: exact "main" (dir, vol-b) before prefix "main.rs".
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].name, "main");
        assert!(hits[0].is_dir);
        assert_eq!(hits[1].name, "main.rs");

        drop(client); // close -> server sees clean EOF
        server.join().unwrap().unwrap();
    }

    #[test]
    fn version_mismatch_is_rejected_loudly() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let reader = stream.try_clone().unwrap();
            serve_connection(reader, stream, &test_host())
        });

        let stream = TcpStream::connect(addr).unwrap();
        let mut reader = stream.try_clone().unwrap();
        let mut writer = stream;
        write_frame(&mut writer, &encode_request(&Request::Hello { version: 999 })).unwrap();
        let payload = read_frame(&mut reader).unwrap().unwrap();
        let Response::Error { message } = decode_response(&payload).unwrap() else {
            panic!("expected an error response");
        };
        assert!(message.contains("version"));
        assert!(server.join().unwrap().is_err()); // server side logged + bailed
    }
}

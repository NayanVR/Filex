//! Prepare immutable generations in a disposable process.
//!
//! The owner assigns file IDs and spools records. The worker sorts and builds
//! the segment, then exits, returning all construction allocations to the OS.
//! Only the catalog writer can publish the completed file in a manifest.
use super::view::View;
use crate::{
    catalog::segment::{Identity, Record, Root, Segment, raw_name},
    ingest,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs::File,
    io::{BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const REQUEST_VERSION: u32 = 2;
const RECORD_HEADER_BYTES: usize = 66;
const MAX_RECORD_BYTES: u64 = 1024 * 1024;
const MAX_ERROR_BYTES: u64 = 16 * 1024;
const WORKER_POLL_INTERVAL: Duration = Duration::from_millis(25);

pub(super) struct BuildRequest {
    pub view: Arc<View>,
    pub reconcile: bool,
    pub directory: PathBuf,
    pub next: Arc<AtomicU64>,
}

pub(super) struct Built {
    pub(super) segment: Segment,
    pub(super) path: PathBuf,
    pub(super) sequence: u64,
    pub(super) skipped: usize,
}

#[derive(Serialize, Deserialize)]
struct Header {
    version: u32,
    sequence: u64,
    roots: Vec<Root>,
}

/// Scratch files belong to a build until the writer accepts its output.
struct ScratchFiles(Vec<PathBuf>);
impl Drop for ScratchFiles {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Always reap a worker, including cancellation and parent-side I/O failures.
struct Worker(Child);
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub(super) fn build(request: BuildRequest, stop: &AtomicBool) -> Result<Built> {
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let output = request.directory.join(format!("build-{stamp}.tmp"));
    let input = request.directory.join(format!("build-{stamp}-input.tmp"));
    let errors = request.directory.join(format!("build-{stamp}-errors.tmp"));
    let mut scratch = ScratchFiles(vec![input.clone(), output.clone(), errors.clone()]);
    let file = File::options().write(true).create_new(true).open(&input)?;
    let mut spool = BufWriter::new(file);
    write_line(
        &mut spool,
        &Header {
            version: REQUEST_VERSION,
            sequence: request.view.epoch,
            roots: request.view.roots.clone(),
        },
    )?;

    let skipped = if request.reconcile {
        enumerate(&request, &mut spool, stop)?
    } else {
        for record in request.view.records() {
            ensure!(!stop.load(Ordering::Relaxed), "segment build cancelled");
            write_record(&mut spool, &record)?;
        }
        0
    };
    spool.flush()?;
    drop(spool);
    ensure!(!stop.load(Ordering::Relaxed), "segment build cancelled");

    run_process(&input, &output, &errors, stop)?;
    // SAFETY: the worker has exited. This output is immutable, and the writer
    // keeps the generation alive until all query views release their mappings.
    let segment = unsafe { Segment::open(&output) }.context("loading the completed segment")?;
    ensure!(
        segment.sequence == request.view.epoch && segment.roots == request.view.roots,
        "segment worker returned a different generation"
    );
    scratch.0.retain(|path| path != &output);
    Ok(Built {
        segment,
        path: output,
        sequence: request.view.epoch,
        skipped,
    })
}

fn enumerate(request: &BuildRequest, spool: &mut impl Write, stop: &AtomicBool) -> Result<usize> {
    let mut allocated = HashSet::new();
    let mut skipped = 0;
    for root in &request.view.roots {
        skipped += ingest::walk(
            root,
            &request.directory,
            |path, parent, identity| {
                let existing = unchanged_id(&request.view, path, parent, identity).or_else(|| {
                    request.view.find_native(root.id, *identity).filter(|&id| {
                        identity.birth != 0 && request.view.path(id).is_none_or(|old| !old.exists())
                    })
                });
                let id = existing
                    .filter(|id| allocated.insert(*id))
                    .unwrap_or_else(|| request.next.fetch_add(1, Ordering::Relaxed));
                allocated.insert(id);
                id
            },
            |record| write_record(spool, &record),
            stop,
        )?;
    }
    Ok(skipped)
}

/// The ID of the record at `path` whose native identity still matches.
///
/// The walk already holds the parent's ID, so look one level down instead of
/// resolving the path from the root for every entry: that per-entry
/// root-to-leaf walk, a name-page decode per binary-search probe, was ~94% of
/// a rebuild's CPU (FIL-27). A miss still tries the full path, which covers a
/// parent that was replaced under its children; it costs only for changed
/// entries.
fn unchanged_id(view: &View, path: &Path, parent: u64, identity: &Identity) -> Option<u64> {
    let unchanged = |id| {
        view.record(id)
            .filter(|record| record.identity == *identity)
            .map(|record| record.id)
    };
    path.file_name()
        .filter(|_| parent != 0)
        .and_then(|name| view.child(parent, &raw_name(name)))
        .and_then(unchanged)
        .or_else(|| view.resolve(path).and_then(unchanged))
}

fn write_line(writer: &mut impl Write, value: &impl Serialize) -> Result<()> {
    serde_json::to_writer(&mut *writer, value)?;
    writer.write_all(b"\n")?;
    Ok(())
}

// Private worker spool: a JSON header followed by fixed-width metadata and raw
// name bytes. This is scratch, not the durable WAL or segment format.
fn write_record(writer: &mut impl Write, record: &Record) -> Result<()> {
    ensure!(
        record.name.len() <= MAX_RECORD_BYTES as usize - RECORD_HEADER_BYTES,
        "segment build record exceeds size limit"
    );
    let mut header = [0u8; RECORD_HEADER_BYTES];
    header[0..8].copy_from_slice(&record.id.to_le_bytes());
    header[8..16].copy_from_slice(&record.parent.to_le_bytes());
    header[16..20].copy_from_slice(&record.root.to_le_bytes());
    header[20] = record.flags;
    header[21] = u8::from(record.size.is_some()) | (u8::from(record.mtime.is_some()) << 1);
    header[22..30].copy_from_slice(&record.identity.device.to_le_bytes());
    header[30..38].copy_from_slice(&record.identity.key.to_le_bytes());
    header[38..46].copy_from_slice(&record.identity.birth.to_le_bytes());
    header[46..54].copy_from_slice(&record.size.unwrap_or(0).to_le_bytes());
    header[54..62].copy_from_slice(&record.mtime.unwrap_or(0).to_le_bytes());
    header[62..66].copy_from_slice(&(record.name.len() as u32).to_le_bytes());
    writer.write_all(&header)?;
    writer.write_all(&record.name)?;
    Ok(())
}

fn read_record(
    reader: &mut impl BufRead,
    version: u32,
    line: &mut String,
) -> Result<Option<Record>> {
    if version == 1 {
        return if read_line(reader, line)? == 0 {
            Ok(None)
        } else {
            Ok(Some(
                serde_json::from_str(line).context("invalid segment build record")?,
            ))
        };
    }
    let mut header = [0u8; RECORD_HEADER_BYTES];
    if reader.read(&mut header[..1])? == 0 {
        return Ok(None);
    }
    reader
        .read_exact(&mut header[1..])
        .context("truncated segment build record")?;
    ensure!(header[21] & !3 == 0, "invalid record presence bits");
    let size = u32::from_le_bytes(header[62..66].try_into()?) as usize;
    ensure!(
        size <= MAX_RECORD_BYTES as usize - RECORD_HEADER_BYTES,
        "segment build record exceeds size limit"
    );
    let mut name = vec![0; size];
    reader
        .read_exact(&mut name)
        .context("truncated segment build name")?;
    Ok(Some(Record {
        id: u64::from_le_bytes(header[0..8].try_into()?),
        parent: u64::from_le_bytes(header[8..16].try_into()?),
        root: u32::from_le_bytes(header[16..20].try_into()?),
        flags: header[20],
        identity: crate::catalog::segment::Identity {
            device: u64::from_le_bytes(header[22..30].try_into()?),
            key: u64::from_le_bytes(header[30..38].try_into()?),
            birth: u64::from_le_bytes(header[38..46].try_into()?),
        },
        size: (header[21] & 1 != 0).then(|| u64::from_le_bytes(header[46..54].try_into().unwrap())),
        mtime: (header[21] & 2 != 0)
            .then(|| i64::from_le_bytes(header[54..62].try_into().unwrap())),
        name,
    }))
}

fn run_process(input: &Path, output: &Path, errors: &Path, stop: &AtomicBool) -> Result<()> {
    let error_file = File::options().write(true).create_new(true).open(errors)?;
    let mut command = Command::new(super::executable::daemon()?);
    command
        .arg("--build-segment")
        .arg(input)
        .arg(output)
        // The open stdin pipe is a lifetime signal. If the owner crashes, EOF
        // terminates the worker before a replacement owner cleans its scratch.
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(error_file);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let mut worker = Worker(command.spawn().context("starting the segment worker")?);
    loop {
        ensure!(!stop.load(Ordering::Relaxed), "segment build cancelled");
        if let Some(status) = worker.0.try_wait()? {
            if !status.success() {
                let mut message = String::new();
                File::open(errors)?
                    .take(MAX_ERROR_BYTES)
                    .read_to_string(&mut message)?;
                anyhow::bail!("segment worker failed ({status}): {}", message.trim());
            }
            return Ok(());
        }
        std::thread::sleep(WORKER_POLL_INTERVAL);
    }
}

/// Internal companion-daemon entry point; does not open the live database.
pub fn run_worker(input: &Path, output: &Path) -> Result<()> {
    std::thread::Builder::new()
        .name("filex-builder-owner-watch".into())
        .spawn(|| {
            let mut byte = [0];
            loop {
                match std::io::stdin().read(&mut byte) {
                    Ok(0) | Err(_) => std::process::exit(1),
                    Ok(_) => {}
                }
            }
        })?;
    compile(input, output)
}

fn read_line(reader: &mut impl BufRead, line: &mut String) -> Result<usize> {
    line.clear();
    let bytes = reader.take(MAX_RECORD_BYTES + 1).read_line(line)?;
    ensure!(
        bytes as u64 <= MAX_RECORD_BYTES,
        "segment build record exceeds size limit"
    );
    Ok(bytes)
}

fn compile(input: &Path, output: &Path) -> Result<()> {
    let mut reader = BufReader::new(File::open(input)?);
    let mut line = String::new();
    let header_bytes = read_line(&mut reader, &mut line)?;
    ensure!(header_bytes != 0, "missing segment build header");
    let header: Header = serde_json::from_str(&line).context("invalid segment build header")?;
    ensure!(
        matches!(header.version, 1 | REQUEST_VERSION),
        "unsupported segment build request"
    );

    // A move can preserve an ID smaller than its newly enumerated parent.
    // Sort compact (ID, offset) pairs, never a second arena of complete records.
    let mut order = Vec::new();
    let mut position = header_bytes as u64;
    loop {
        let Some(record) = read_record(&mut reader, header.version, &mut line)? else {
            break;
        };
        order.push((record.id, position));
        position += if header.version == 1 {
            line.len()
        } else {
            RECORD_HEADER_BYTES + record.name.len()
        } as u64;
    }
    order.sort_unstable();
    ensure!(
        order.windows(2).all(|pair| pair[0].0 != pair[1].0),
        "duplicate file ID in build input"
    );
    let mut failure = None;
    let records = order.into_iter().map_while(|(_, offset)| {
        let record = (|| -> Result<Record> {
            // Compaction spools are already ID-ordered. Preserve the reader's
            // buffer across adjacent records; enumeration may still need seeks.
            if position != offset {
                reader.seek(SeekFrom::Start(offset))?;
            }
            let record = read_record(&mut reader, header.version, &mut line)?
                .context("truncated segment build input")?;
            position = offset
                + if header.version == 1 {
                    line.len()
                } else {
                    RECORD_HEADER_BYTES + record.name.len()
                } as u64;
            Ok(record)
        })();
        match record {
            Ok(record) => Some(record),
            Err(error) => {
                failure = Some(error);
                None
            }
        }
    });
    let segment = Segment::build(records, header.roots, header.sequence);
    if let Some(error) = failure {
        return Err(error);
    }
    segment?.save(output)
}

/// Laboratory helper that uses the same isolated compaction path as the writer.
#[cfg(feature = "index-v2-lab")]
pub fn compact_to(input: &Path, output: &Path) -> Result<()> {
    // SAFETY: laboratory input is an immutable generation owned by the caller.
    let base = Arc::new(unsafe { Segment::open(input) }?);
    let view = Arc::new(View {
        roots: base.roots.clone(),
        epoch: base.sequence,
        base,
        layers: vec![],
    });
    compact_view_to(view, output)?;
    Ok(())
}

/// Keep the owner process and its old view alive for lifecycle memory probes.
#[cfg(feature = "index-v2-lab")]
pub fn compact_view_to(view: Arc<View>, output: &Path) -> Result<Segment> {
    ensure!(!output.exists(), "compaction output already exists");
    let built = build(
        BuildRequest {
            view,
            reconcile: false,
            directory: output
                .parent()
                .context("output has no parent directory")?
                .to_owned(),
            next: Arc::new(AtomicU64::new(0)),
        },
        &AtomicBool::new(false),
    )?;
    if let Err(error) = std::fs::rename(&built.path, output) {
        drop(built.segment);
        let _ = std::fs::remove_file(built.path);
        return Err(error.into());
    }
    Ok(built.segment)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_spool_preserves_native_bytes_and_rejects_every_truncation() {
        let record = Record {
            id: u64::MAX,
            parent: 19,
            root: u32::MAX,
            name: vec![255, 254, 13, 10, 0, 128],
            flags: 2,
            identity: crate::catalog::segment::Identity {
                device: u64::MAX,
                key: 123,
                birth: 987,
            },
            size: Some(u64::MAX),
            mtime: Some(i64::MIN),
        };
        for record in [
            record.clone(),
            Record {
                size: None,
                mtime: None,
                ..record
            },
        ] {
            let mut encoded = Vec::new();
            write_record(&mut encoded, &record).unwrap();
            let mut reader = encoded.as_slice();
            let mut line = String::new();
            assert_eq!(
                read_record(&mut reader, REQUEST_VERSION, &mut line).unwrap(),
                Some(record)
            );
            assert!(
                read_record(&mut reader, REQUEST_VERSION, &mut line)
                    .unwrap()
                    .is_none()
            );
            for end in 1..encoded.len() {
                assert!(read_record(&mut &encoded[..end], REQUEST_VERSION, &mut line).is_err());
            }
            encoded[62..66].copy_from_slice(&u32::MAX.to_le_bytes());
            assert!(read_record(&mut encoded.as_slice(), REQUEST_VERSION, &mut line).is_err());
        }
    }

    #[test]
    fn binary_spool_reorders_records_and_reuses_adjacent_input() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("input");
        let output = directory.path().join("output");
        let mut file = File::create(&input).unwrap();
        write_line(
            &mut file,
            &Header {
                version: REQUEST_VERSION,
                sequence: 9,
                roots: vec![Root {
                    id: 1,
                    path: "/fixture".into(),
                    device: 1,
                }],
            },
        )
        .unwrap();
        for id in [4, 5, 1, 3, 2, 6] {
            write_record(
                &mut file,
                &Record {
                    id,
                    parent: if id == 1 { 0 } else { 1 },
                    root: 1,
                    name: format!("東京-{}", "x".repeat(id as usize * 2000)).into_bytes(),
                    flags: u8::from(id == 1),
                    identity: Default::default(),
                    size: Some(id),
                    mtime: Some(-(id as i64)),
                },
            )
            .unwrap();
        }
        drop(file);
        compile(&input, &output).unwrap();
        let segment = unsafe { Segment::open(&output) }.unwrap();
        assert_eq!(segment.sequence, 9);
        assert_eq!(segment.len(), 6);
        for slot in 0..segment.len() {
            let record = segment.record(slot);
            assert_eq!(record.id, slot as u64 + 1);
            assert_eq!(record.size, Some(record.id));
            assert_eq!(record.mtime, Some(-(record.id as i64)));
            assert_eq!(record.name.len(), 7 + record.id as usize * 2000);
        }
    }

    #[test]
    fn worker_rejects_duplicate_ids_and_does_not_publish_partial_output() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("input");
        let output = directory.path().join("output");
        let mut file = File::create(&input).unwrap();
        write_line(
            &mut file,
            &Header {
                version: REQUEST_VERSION,
                sequence: 1,
                roots: vec![],
            },
        )
        .unwrap();
        let record = Record {
            id: 1,
            parent: 0,
            root: 1,
            name: b"root".to_vec(),
            flags: 1,
            identity: Default::default(),
            size: None,
            mtime: None,
        };
        write_record(&mut file, &record).unwrap();
        write_record(&mut file, &record).unwrap();
        assert!(
            compile(&input, &output)
                .unwrap_err()
                .to_string()
                .contains("duplicate file ID")
        );
        assert!(!output.exists());
    }

    /// FIL-27 regression: a stopping daemon cancels a reconcile build without
    /// leaving scratch files or anything a writer could publish.
    #[test]
    fn stopped_build_fails_and_leaves_nothing_behind() {
        let directory = tempfile::tempdir().unwrap();
        let files = directory.path().join("files");
        let data = directory.path().join("index");
        std::fs::create_dir_all(files.join("nested")).unwrap();
        std::fs::create_dir(&data).unwrap();
        for i in 0..200 {
            std::fs::write(files.join(format!("nested/{i}.txt")), b"x").unwrap();
        }
        let roots = vec![Root {
            id: 1,
            path: files,
            device: 0,
        }];
        let request = BuildRequest {
            view: Arc::new(View {
                base: Arc::new(Segment::build([], roots.clone(), 0).unwrap()),
                layers: vec![],
                roots,
                epoch: 0,
            }),
            reconcile: true,
            directory: data.clone(),
            next: Arc::new(AtomicU64::new(1)),
        };
        let error = build(request, &AtomicBool::new(true)).err().unwrap();
        assert!(error.to_string().contains("cancelled"), "{error}");
        assert_eq!(std::fs::read_dir(&data).unwrap().count(), 0);
    }

    #[test]
    fn malformed_record_is_not_silently_omitted() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("input");
        let output = directory.path().join("output");
        std::fs::write(
            &input,
            b"{\"version\":1,\"sequence\":0,\"roots\":[]}\nnot-json\n",
        )
        .unwrap();
        assert!(compile(&input, &output).is_err());
        assert!(!output.exists());
    }
}

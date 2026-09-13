//! Prepare immutable generations in a disposable process.
//!
//! The owner assigns file IDs and spools records. The worker sorts and builds
//! the segment, then exits, returning all construction allocations to the OS.
//! Only the catalog writer can publish the completed file in a manifest.
use super::view::View;
use crate::{
    catalog::segment::{Record, Root, Segment},
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

const REQUEST_VERSION: u32 = 1;
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
            write_line(&mut spool, &record)?;
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
            |path, identity| {
                let existing = request
                    .view
                    .resolve(path)
                    .and_then(|id| request.view.record(id))
                    .filter(|record| record.identity == *identity)
                    .map(|record| record.id)
                    .or_else(|| {
                        request.view.find_native(root.id, *identity).filter(|&id| {
                            identity.birth != 0
                                && request.view.path(id).is_none_or(|old| !old.exists())
                        })
                    });
                let id = existing
                    .filter(|id| allocated.insert(*id))
                    .unwrap_or_else(|| request.next.fetch_add(1, Ordering::Relaxed));
                allocated.insert(id);
                id
            },
            |record| write_line(spool, &record),
            stop,
        )?;
    }
    Ok(skipped)
}

fn write_line(writer: &mut impl Write, value: &impl Serialize) -> Result<()> {
    serde_json::to_writer(&mut *writer, value)?;
    writer.write_all(b"\n")?;
    Ok(())
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
    ensure!(
        read_line(&mut reader, &mut line)? != 0,
        "missing segment build header"
    );
    let header: Header = serde_json::from_str(&line).context("invalid segment build header")?;
    ensure!(
        header.version == REQUEST_VERSION,
        "unsupported segment build request"
    );

    // A move can preserve an ID smaller than its newly enumerated parent.
    // Sort compact (ID, offset) pairs, never a second arena of complete records.
    let mut order = Vec::new();
    loop {
        let offset = reader.stream_position()?;
        if read_line(&mut reader, &mut line)? == 0 {
            break;
        }
        let record: Record = serde_json::from_str(&line).context("invalid segment build record")?;
        order.push((record.id, offset));
    }
    order.sort_unstable();
    ensure!(
        order.windows(2).all(|pair| pair[0].0 != pair[1].0),
        "duplicate file ID in build input"
    );
    let mut failure = None;
    let records = order.into_iter().map_while(|(_, offset)| {
        let record = (|| -> Result<Record> {
            reader.seek(SeekFrom::Start(offset))?;
            ensure!(
                read_line(&mut reader, &mut line)? != 0,
                "truncated segment build input"
            );
            Ok(serde_json::from_str(&line)?)
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
        write_line(&mut file, &record).unwrap();
        write_line(&mut file, &record).unwrap();
        assert!(
            compile(&input, &output)
                .unwrap_err()
                .to_string()
                .contains("duplicate file ID")
        );
        assert!(!output.exists());
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

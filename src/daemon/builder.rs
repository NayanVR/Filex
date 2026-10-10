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

/// Version 3 adds delta tombstones; an older worker must reject it rather
/// than silently build a delta without them.
const REQUEST_VERSION: u32 = 3;
const RECORD_HEADER_BYTES: usize = 66;
const MAX_RECORD_BYTES: u64 = 1024 * 1024;
/// The header carries a delta's tombstones: ~21 bytes each in JSON.
const MAX_HEADER_BYTES: u64 = 64 * 1024 * 1024;
const MAX_ERROR_BYTES: u64 = 16 * 1024;
const WORKER_POLL_INTERVAL: Duration = Duration::from_millis(25);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Mode {
    /// Walk every root into a new base.
    Reconcile,
    /// Merge every level of the view into a new base.
    Merge,
    /// Write the overlays as one new delta on top of the existing ones.
    Flush,
    /// Merge every delta and overlay into one delta.
    MergeDeltas,
}

pub(super) struct BuildRequest {
    pub view: Arc<View>,
    pub mode: Mode,
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
    tombstones: Option<Vec<u64>>,
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
    let delta = matches!(request.mode, Mode::Flush | Mode::MergeDeltas)
        .then(|| delta_changes(&request.view, request.mode == Mode::MergeDeltas));
    write_line(
        &mut spool,
        &Header {
            version: REQUEST_VERSION,
            sequence: request.view.epoch,
            roots: request.view.roots.clone(),
            tombstones: delta.as_ref().map(|(_, deleted)| deleted.clone()),
        },
    )?;

    let skipped = match (request.mode, delta) {
        (Mode::Reconcile, _) => enumerate(&request, &mut spool, stop)?,
        (_, Some((records, _))) => {
            for record in &records {
                ensure!(!stop.load(Ordering::Relaxed), "segment build cancelled");
                write_record(&mut spool, record)?;
            }
            0
        }
        _ => {
            for record in request.view.records() {
                ensure!(!stop.load(Ordering::Relaxed), "segment build cancelled");
                write_record(&mut spool, &record)?;
            }
            0
        }
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

/// The records and sorted tombstones of a new delta, newest version winning.
///
/// A flush takes only the overlays; `merge_deltas` also folds in every
/// existing delta, which the result then replaces. A tombstone is kept only
/// while an older surviving level still holds the ID.
// ponytail: materializes the merged changes in memory; bounded by the delta
// budget (1/8 of the base), stream a k-way merge if that budget grows.
pub(super) fn delta_changes(view: &View, merge_deltas: bool) -> (Vec<Record>, Vec<u64>) {
    let mut changes = std::collections::BTreeMap::<u64, Option<Record>>::new();
    if merge_deltas {
        for delta in &view.deltas {
            for slot in 0..delta.len() {
                changes.insert(delta.id(slot), Some(delta.record(slot)));
            }
            for &id in delta.tombstones.iter().flatten() {
                changes.insert(id, None);
            }
        }
    }
    for layer in &view.layers {
        changes.extend(layer.records.iter().map(|(&id, r)| (id, r.clone())));
    }
    let older: &[Arc<Segment>] = if merge_deltas { &[] } else { &view.deltas };
    let mut records = Vec::new();
    let mut tombstones = Vec::new();
    for (id, record) in changes {
        match record {
            Some(record) => records.push(record),
            None if view.base.slot(id).is_some() || older.iter().any(|d| d.slot(id).is_some()) => {
                tombstones.push(id)
            }
            None => {}
        }
    }
    (records, tombstones)
}

fn enumerate(request: &BuildRequest, spool: &mut impl Write, stop: &AtomicBool) -> Result<usize> {
    let mut allocated = HashSet::new();
    let mut skipped = 0;
    let filter = ingest::IndexFilter::load();
    for root in &request.view.roots {
        skipped += ingest::walk(
            root,
            &request.directory,
            &filter,
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
// Public only for the worker-process integration tests.
#[doc(hidden)]
pub fn write_record(writer: &mut impl Write, record: &Record) -> Result<()> {
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

fn read_record(reader: &mut impl BufRead) -> Result<Option<Record>> {
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

fn read_line(reader: &mut impl BufRead, line: &mut String, limit: u64) -> Result<usize> {
    line.clear();
    let bytes = reader.take(limit + 1).read_line(line)?;
    ensure!(
        bytes as u64 <= limit,
        "segment build record exceeds size limit"
    );
    Ok(bytes)
}

fn compile(input: &Path, output: &Path) -> Result<()> {
    let mut reader = BufReader::new(File::open(input)?);
    let mut line = String::new();
    let header_bytes = read_line(&mut reader, &mut line, MAX_HEADER_BYTES)?;
    ensure!(header_bytes != 0, "missing segment build header");
    let header: Header = serde_json::from_str(&line).context("invalid segment build header")?;
    ensure!(
        header.version == REQUEST_VERSION,
        "unsupported segment build request"
    );

    // A move can preserve an ID smaller than its newly enumerated parent.
    // Sort compact (ID, offset) pairs, never a second arena of complete records.
    let mut order = Vec::new();
    let mut position = header_bytes as u64;
    loop {
        let Some(record) = read_record(&mut reader)? else {
            break;
        };
        order.push((record.id, position));
        position += (RECORD_HEADER_BYTES + record.name.len()) as u64;
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
            let record = read_record(&mut reader)?.context("truncated segment build input")?;
            position = offset + (RECORD_HEADER_BYTES + record.name.len()) as u64;
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
    let tombstones = header.tombstones.clone();
    let segment = Segment::build(records, header.roots, header.sequence);
    if let Some(error) = failure {
        return Err(error);
    }
    let mut segment = segment?;
    segment.tombstones = tombstones;
    segment.save(output)
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
        deltas: vec![],
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
            mode: Mode::Merge,
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
            assert_eq!(read_record(&mut reader).unwrap(), Some(record));
            assert!(read_record(&mut reader).unwrap().is_none());
            for end in 1..encoded.len() {
                assert!(read_record(&mut &encoded[..end]).is_err());
            }
            encoded[62..66].copy_from_slice(&u32::MAX.to_le_bytes());
            assert!(read_record(&mut encoded.as_slice()).is_err());
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
                tombstones: None,
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
                tombstones: None,
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
                deltas: vec![],
                layers: vec![],
                roots,
                epoch: 0,
            }),
            mode: Mode::Reconcile,
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
            b"{\"version\":3,\"sequence\":0,\"roots\":[],\"tombstones\":null}\nnot-a-record\n",
        )
        .unwrap();
        assert!(compile(&input, &output).is_err());
        assert!(!output.exists());
    }

    fn entry(id: u64, parent: u64, name: &str, directory: bool) -> Record {
        Record {
            id,
            parent,
            root: 1,
            name: name.as_bytes().to_vec(),
            flags: if directory { Record::DIRECTORY } else { 0 },
            identity: Identity {
                device: 1,
                key: id,
                birth: 1,
            },
            size: Some(id * 10),
            mtime: Some(id as i64),
        }
    }

    fn overlay(changes: &[(u64, Option<Record>)]) -> Arc<super::super::view::Overlay> {
        let mut overlay = super::super::view::Overlay::default();
        for (id, record) in changes {
            overlay.put(*id, record.clone());
        }
        Arc::new(overlay)
    }

    /// Build `view`'s pending changes into a delta the way the worker does,
    /// through a save and a mapped reopen.
    fn flush(view: &View, merge_deltas: bool, directory: &Path, name: &str) -> Arc<Segment> {
        let (records, tombstones) = delta_changes(view, merge_deltas);
        let mut segment = Segment::build(records, view.roots.clone(), view.epoch).unwrap();
        segment.tombstones = Some(tombstones);
        let path = directory.join(name);
        segment.save(&path).unwrap();
        Arc::new(unsafe { Segment::open(&path) }.unwrap())
    }

    /// Everything a client can observe from a view, for comparing levels.
    fn observe(view: &View) -> Vec<String> {
        use super::super::{ipc::Query, query};
        let cancel = AtomicBool::new(false);
        let mut seen = Vec::new();
        for record in view.records() {
            let path = view.path(record.id).unwrap();
            assert_eq!(view.resolve(&path), Some(record.id), "{path:?}");
            seen.push(format!("record {record:?} at {path:?}"));
        }
        let mut scoped = view.scoped_ids(Path::new("/fixture"), 10_000, &cancel);
        scoped.sort_unstable();
        seen.push(format!("scope {scoped:?}"));
        for text in ["", "report", "txt", "summary", "deep", "fresh", "zz"] {
            for (offset, filters) in [
                (0, vec![]),
                (40, vec![]),
                (0, vec![crate::search::filter::Filter::Ext("md".into())]),
            ] {
                let query = Query {
                    text: text.into(),
                    filters,
                    limit: 40,
                    offset,
                    fuzzy: false,
                    ..Default::default()
                };
                let page = query::search(view, &query, &cancel, &Default::default()).unwrap();
                let hits: Vec<_> = page.hits.iter().map(|h| (h.id, h.tier, &h.path)).collect();
                seen.push(format!(
                    "search {text:?} {offset}: {hits:?} more={}",
                    page.more
                ));
                let mut streamed = Vec::new();
                query::stream(view, &query, &cancel, |batch| {
                    streamed.extend(batch.hits.into_iter().map(|h| (h.id, h.tier, h.path)));
                    Ok(())
                })
                .unwrap();
                seen.push(format!("stream {text:?}: {streamed:?}"));
            }
        }
        seen
    }

    /// FIL-30: changes answer every query identically whether they sit in
    /// overlays, in flushed deltas, or in merged deltas.
    #[test]
    fn deltas_answer_exactly_like_the_overlays_they_replace() {
        let directory = tempfile::tempdir().unwrap();
        let roots = vec![Root {
            id: 1,
            path: "/fixture".into(),
            device: 1,
        }];
        let mut base = vec![
            entry(1, 0, "fixture", true),
            entry(2, 1, "a", true),
            entry(3, 1, "b", true),
            entry(4, 2, "report.txt", false),
            entry(5, 2, "notes.md", false),
            entry(6, 3, "report-final.txt", false),
            entry(7, 3, "c", true),
            entry(8, 7, "deep.txt", false),
        ];
        base.extend((100..200).map(|id| entry(id, 2, &format!("report-{id}.txt"), false)));
        let base = Arc::new(Segment::build(base, roots.clone(), 1).unwrap());
        let first = overlay(&[
            (4, Some(entry(4, 2, "summary.txt", false))),
            (5, None),
            // Deleting a directory hides its base descendants.
            (3, None),
            (9, Some(entry(9, 2, "report-new.txt", false))),
            (10, Some(entry(10, 2, "fresh", true))),
            (11, Some(entry(11, 10, "report-deep.md", false))),
            (150, None),
            (151, Some(entry(151, 10, "report-moved.txt", false))),
        ]);
        let second = overlay(&[
            // Rename a delta record, delete another, recreate a deleted name.
            (9, Some(entry(9, 2, "renamed.md", false))),
            (11, None),
            (12, Some(entry(12, 2, "notes.md", false))),
            (160, None),
            (13, Some(entry(13, 10, "zz-only-new.txt", false))),
        ]);
        let view = |deltas: Vec<Arc<Segment>>, layers, epoch| View {
            base: base.clone(),
            deltas,
            layers,
            roots: roots.clone(),
            epoch,
        };

        let overlays_only = view(vec![], vec![first.clone()], 2);
        let expected = observe(&overlays_only);
        let one = flush(&overlays_only, false, directory.path(), "one");
        assert_eq!(one.tombstones.as_deref(), Some(&[3, 5, 150][..]));
        assert_eq!(observe(&view(vec![one.clone()], vec![], 2)), expected);

        let overlays_only = view(vec![], vec![first, second.clone()], 3);
        let expected = observe(&overlays_only);
        let pending = view(vec![one.clone()], vec![second], 3);
        assert_eq!(observe(&pending), expected);
        // 11 existed only in the first delta: the flush keeps its tombstone
        // to hide that copy, but a merge of both deltas drops it.
        let two = flush(&pending, false, directory.path(), "two");
        assert_eq!(two.tombstones.as_deref(), Some(&[11, 160][..]));
        assert_eq!(
            observe(&view(vec![one.clone(), two.clone()], vec![], 3)),
            expected
        );
        let merged = flush(&pending, true, directory.path(), "merged");
        assert_eq!(merged.tombstones.as_deref(), Some(&[3, 5, 150, 160][..]));
        let merged = view(vec![merged], vec![], 3);
        assert_eq!(observe(&merged), expected);

        // Status counts follow the deltas without a base rewrite.
        let counted = |view: &View| {
            let mut counts = std::collections::BTreeMap::new();
            for record in view.records().filter(|r| r.parent != 0) {
                *counts.entry(record.root).or_insert(0) += 1;
            }
            counts
        };
        let only = view(vec![one.clone()], vec![], 2);
        let split = view(vec![one, two], vec![], 3);
        for view in [&only, &split, &merged] {
            assert_eq!(view.root_counts(), counted(view));
        }
        assert_ne!(only.root_counts(), counted(&view(vec![], vec![], 1)));
    }
}

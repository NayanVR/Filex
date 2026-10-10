//! The builder is a real child process so allocator lifetime and owner death
//! behave the same way here as in an installed daemon.
use filex::catalog::segment::{Identity, Record, Root, Segment};
use std::{
    fs::File,
    io::Write,
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct Worker(Child);
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn(input: &Path, output: &Path) -> Worker {
    Worker(
        Command::new(env!("CARGO_BIN_EXE_filex-indexd"))
            .arg("--build-segment")
            .arg(input)
            .arg(output)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    )
}

fn input(path: &Path, records: impl IntoIterator<Item = Record>) {
    let mut file = std::io::BufWriter::new(File::create(path).unwrap());
    let roots = vec![Root {
        id: 1,
        path: "/fixture".into(),
        device: 0,
    }];
    serde_json::to_writer(
        &mut file,
        &serde_json::json!({"version":3,"sequence":7,"roots":roots,"tombstones":null}),
    )
    .unwrap();
    writeln!(file).unwrap();
    for record in records {
        filex::daemon::builder::write_record(&mut file, &record).unwrap();
    }
    file.flush().unwrap();
}

fn record(id: u64) -> Record {
    Record {
        id,
        parent: if id == 1 { 0 } else { 1 },
        root: 1,
        name: format!("file-{id}.txt").into_bytes(),
        flags: u8::from(id == 1),
        identity: Identity {
            key: id,
            ..Default::default()
        },
        size: Some(id),
        mtime: Some(0),
    }
}

#[test]
fn child_build_sorts_preserved_ids_and_produces_a_valid_segment() {
    let directory = tempfile::tempdir().unwrap();
    let request = directory.path().join("input with spaces");
    let output = directory.path().join("output.fx2");
    input(&request, [record(3), record(1), record(2)]);
    let mut worker = spawn(&request, &output);
    // Child::wait closes its own stdin handle; keep the owner's lifetime pipe
    // outside Child while waiting for a successful build.
    let _owner_lifetime = worker.0.stdin.take();
    assert!(worker.0.wait().unwrap().success());
    let segment = unsafe { Segment::open(&output) }.unwrap();
    assert_eq!(segment.sequence, 7);
    assert_eq!(segment.len(), 3);
    assert_eq!(segment.child(1, b"file-2.txt"), Some(1));
}

#[test]
fn child_exits_when_the_owner_lifetime_pipe_closes() {
    let directory = tempfile::tempdir().unwrap();
    let request = directory.path().join("input");
    let output = directory.path().join("output.fx2");
    input(&request, (1..=20_000).map(record));
    let mut worker = spawn(&request, &output);
    drop(worker.0.stdin.take());
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = worker.0.try_wait().unwrap() {
            assert!(!status.success());
            break;
        }
        assert!(
            Instant::now() < deadline,
            "orphaned segment worker did not exit"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

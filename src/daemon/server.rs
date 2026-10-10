//! One per-user index owner, two query workers, one writer, one builder.
use super::{
    ipc::{self, Command, Endpoint, Request, Response, VERSION},
    query, writer,
};
use anyhow::{Result, ensure};
use notify::Watcher;
use std::{
    collections::{HashMap, VecDeque},
    fs::{File, OpenOptions},
    io::Write,
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};
struct Job {
    stream: TcpStream,
    command: Command,
    cancel: Arc<AtomicBool>,
}
pub fn run(directory: &Path, roots: Vec<PathBuf>, stop: Arc<AtomicBool>) -> Result<()> {
    std::fs::create_dir_all(directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join("owner.lock"))?;
    fs2::FileExt::try_lock_exclusive(&lock)?;
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
    let mut random = [0u8; 32];
    getrandom::getrandom(&mut random).map_err(|e| anyhow::anyhow!("randomness failed: {e}"))?;
    let token: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    let endpoint = Endpoint {
        version: VERSION,
        port: listener.local_addr()?.port(),
        token: token.clone(),
    };
    let handle = writer::start(directory, roots)?;
    let mut endpoint_file = File::create(directory.join("endpoint.tmp"))?;
    endpoint_file.write_all(&serde_json::to_vec(&endpoint)?)?;
    endpoint_file.sync_all()?;
    drop(endpoint_file);
    let endpoint_path = directory.join("endpoint.json");
    crate::catalog::manifest::replace_file(&directory.join("endpoint.tmp"), &endpoint_path)?;
    let ignored = directory.canonicalize()?;
    let shared = handle.shared.clone();
    let events = handle.commands.clone();
    let event_owner = shared.clone();
    let filter = Arc::new(RwLock::new(crate::ingest::IndexFilter::load()));
    let event_filter = filter.clone();
    let mut watcher =
        notify::recommended_watcher(move |event: notify::Result<notify::Event>| match event {
            Ok(mut event) => {
                if matches!(event.kind, notify::EventKind::Access(_)) {
                    return;
                }
                if event.need_rescan() {
                    event_owner.overflow.store(true, Ordering::Relaxed);
                }
                {
                    let view = event_owner.view.read().unwrap();
                    let filter = event_filter.read().unwrap();
                    event.paths.retain(|p| {
                        !p.starts_with(&ignored)
                            && view
                                .roots
                                .iter()
                                .any(|r| p.starts_with(&r.path) && !filter.excludes(&r.path, p))
                    });
                }
                if event.paths.is_empty() {
                    return;
                }
                let remove = matches!(event.kind, notify::EventKind::Remove(_));
                if events
                    .try_send(writer::Command::Paths {
                        paths: event.paths,
                        remove,
                    })
                    .is_err()
                {
                    event_owner.overflow.store(true, Ordering::Relaxed);
                }
            }
            Err(error) => {
                *event_owner.coverage_error.write().unwrap() =
                    Some(format!("watch coverage incomplete: {error}"));
                event_owner.overflow.store(true, Ordering::Relaxed);
            }
        })?;
    let watched_roots = shared.view.read().unwrap().roots.clone();
    for root in &watched_roots {
        if let Err(e) = watcher.watch(&root.path, notify::RecursiveMode::Recursive) {
            *shared.coverage_error.write().unwrap() =
                Some(format!("watch coverage incomplete: {e}"));
        }
    }
    handle.commands.send(writer::Command::Reconcile)?;
    let watcher = Arc::new(Mutex::new(watcher));
    let (jobs, interactive) = mpsc::sync_channel::<Job>(16);
    let (streams, exhaustive) = mpsc::sync_channel::<Job>(4);
    let hot = Arc::new(Mutex::new(HashMap::<u64, u64>::new()));
    let cancellations = Arc::new(Mutex::new(HashMap::<u64, (u64, Arc<AtomicBool>)>::new()));
    let mut workers = Vec::new();
    for (index, rx) in [interactive, exhaustive].into_iter().enumerate() {
        let shared = shared.clone();
        let hot = hot.clone();
        workers.push(
            std::thread::Builder::new()
                .name(format!("filex-query-{index}"))
                .spawn(move || {
                    loop {
                        let job = match rx.recv() {
                            Ok(j) => j,
                            Err(_) => break,
                        };
                        let mut stream = job.stream;
                        let view = shared.view.read().unwrap().clone();
                        let result = match job.command {
                            Command::Search(q) => {
                                if view.base.is_empty() && shared.status.read().unwrap().building {
                                    Err(anyhow::anyhow!("Building index"))
                                } else {
                                    query::search(
                                        &view,
                                        &q,
                                        &job.cancel,
                                        &hot.lock().unwrap().clone(),
                                    )
                                    .and_then(|page| {
                                        ipc::write_frame(&mut stream, &Response::Page(page))
                                    })
                                }
                            }
                            Command::StreamMatches(q) => {
                                query::stream(&view, &q, &job.cancel, |batch| {
                                    ipc::write_frame(&mut stream, &Response::Batch(batch))
                                })
                            }
                            _ => Err(anyhow::anyhow!("invalid worker request")),
                        };
                        if let Err(error) = result {
                            let _ =
                                ipc::write_frame(&mut stream, &Response::Error(error.to_string()));
                        }
                    }
                })?,
        );
    }
    // Wake a blocking accept on shutdown; no idle accept polling loop.
    let wake_stop = stop.clone();
    let writer_stop = shared.stop.clone();
    let port = endpoint.port;
    let wake = std::thread::spawn(move || {
        while !wake_stop.load(Ordering::Relaxed) && !writer_stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(100));
        }
        // Cancel an in-flight build now. Teardown reaches the writer only after
        // joining query workers and the watcher (FIL-27: SIGTERM mid-build).
        writer_stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port));
    });
    let mut operations = VecDeque::<u64>::new();
    for incoming in listener.incoming() {
        if stop.load(Ordering::Relaxed) || shared.stop.load(Ordering::Relaxed) {
            break;
        }
        let mut stream = match incoming {
            Ok(s) => s,
            Err(_) => continue,
        };
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
        let mut request = match ipc::read_frame::<Request>(&mut stream) {
            Ok(r) => r,
            Err(_) => continue,
        };
        if request.version != VERSION || !same_token(&request.token, &token) {
            let _ = ipc::write_frame(
                &mut stream,
                &Response::Error("unauthorized or incompatible daemon client".into()),
            );
            continue;
        }
        if let Command::Search(q) | Command::StreamMatches(q) = &mut request.command {
            q.scope = q
                .scope
                .as_ref()
                .map(|p| crate::ingest::canonical_event_path(p));
            if let Some(paths) = &mut q.allowed {
                for path in paths {
                    *path = crate::ingest::canonical_event_path(path);
                }
            }
        }
        let result = (|| -> Result<Option<Response>> {
            match request.command {
                command @ (Command::Search(_) | Command::StreamMatches(_)) => {
                    let (client, q) = match &command {
                        Command::Search(q) | Command::StreamMatches(q) => (q.client, q),
                        _ => unreachable!(),
                    };
                    ensure!(
                        q.text.len() <= 4096
                            && q.filters.len() <= 64
                            && q.allowed.as_ref().is_none_or(|v| v.len() <= 100_000),
                        "query exceeds protocol bounds"
                    );
                    let cancel = Arc::new(AtomicBool::new(false));
                    let mut map = cancellations.lock().unwrap();
                    ensure!(
                        map.get(&client)
                            .is_none_or(|(latest, _)| *latest <= q.request),
                        "superseded search request"
                    );
                    if let Some(old) = map.insert(client, (q.request, cancel.clone())) {
                        old.1.store(true, Ordering::Relaxed);
                    }
                    if map.len() > 1024 {
                        map.retain(|_, v| Arc::strong_count(&v.1) > 1);
                    }
                    let clone = stream.try_clone()?;
                    let queue = if matches!(&command, Command::StreamMatches(_)) {
                        &streams
                    } else {
                        &jobs
                    };
                    queue
                        .try_send(Job {
                            stream: clone,
                            command,
                            cancel,
                        })
                        .map_err(|_| anyhow::anyhow!("search daemon busy; retry"))?;
                    Ok(None)
                }
                Command::Status => {
                    let mut status = shared.status.read().unwrap().clone();
                    if let Some(error) = shared.coverage_error.read().unwrap().clone() {
                        status.error = Some(error);
                        for root in &mut status.roots {
                            root.state = "Watch coverage incomplete".into();
                        }
                    }
                    Ok(Some(Response::Status(status)))
                }
                Command::Reconcile => {
                    *filter.write().unwrap() = crate::ingest::IndexFilter::load();
                    handle.commands.try_send(writer::Command::Reconcile)?;
                    Ok(Some(Response::Ack))
                }
                Command::Cancel {
                    client,
                    before_request,
                } => {
                    if let Some((request, flag)) = cancellations.lock().unwrap().get(&client)
                        && *request < before_request
                    {
                        flag.store(true, Ordering::Relaxed);
                    }
                    Ok(Some(Response::Ack))
                }
                Command::AddRoot(path) => {
                    let roots: Vec<_> = shared
                        .view
                        .read()
                        .unwrap()
                        .roots
                        .iter()
                        .map(|r| r.path.clone())
                        .collect();
                    let path = crate::ingest::validate_new_root(&roots, &path)?;
                    watcher
                        .lock()
                        .unwrap()
                        .watch(&path, notify::RecursiveMode::Recursive)?;
                    handle.commands.send(writer::Command::AddRoot(path))?;
                    Ok(Some(Response::Ack))
                }
                Command::RemoveRoot(path) => {
                    let path = crate::ingest::canonical_event_path(&path);
                    let _ = watcher.lock().unwrap().unwatch(&path);
                    handle.commands.send(writer::Command::RemoveRoot(path))?;
                    Ok(Some(Response::Ack))
                }
                Command::HintFilesystemChange {
                    operation_id,
                    paths,
                } => {
                    ensure!(paths.len() <= 4096, "too many hint paths");
                    if !operations.contains(&operation_id) {
                        if handle
                            .commands
                            .try_send(writer::Command::Paths {
                                paths,
                                remove: false,
                            })
                            .is_err()
                        {
                            shared.overflow.store(true, Ordering::Relaxed);
                        }
                        operations.push_back(operation_id);
                        if operations.len() > 4096 {
                            operations.pop_front();
                        }
                    }
                    Ok(Some(Response::Ack))
                }
                Command::Touch { path } => {
                    if let Some(id) = shared.view.read().unwrap().resolve(&path) {
                        let mut hot = hot.lock().unwrap();
                        let stamp = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)?
                            .as_secs();
                        hot.insert(id, stamp);
                        if hot.len() > 512
                            && let Some(oldest) =
                                hot.iter().min_by_key(|(_, t)| **t).map(|(&id, _)| id)
                        {
                            hot.remove(&oldest);
                        }
                    }
                    Ok(Some(Response::Ack))
                }
                Command::Verify(hit) => {
                    query::verify(&hit)?;
                    let view = shared.view.read().unwrap();
                    ensure!(
                        view.record(hit.id)
                            .is_some_and(|r| r.identity == hit.identity)
                            && view.path(hit.id).as_ref() == Some(&hit.path),
                        "search target changed"
                    );
                    Ok(Some(Response::Ack))
                }
            }
        })();
        match result {
            Ok(Some(response)) => {
                let _ = ipc::write_frame(&mut stream, &response);
            }
            Ok(None) => {}
            Err(e) => {
                let _ = ipc::write_frame(&mut stream, &Response::Error(e.to_string()));
            }
        }
    }
    stop.store(true, Ordering::Relaxed);
    for (_, flag) in cancellations.lock().unwrap().values() {
        flag.store(true, Ordering::Relaxed);
    }
    drop(jobs);
    drop(streams);
    for worker in workers {
        let _ = worker.join();
    }
    drop(watcher);
    drop(handle);
    let _ = wake.join();
    let _ = std::fs::remove_file(endpoint_path);
    Ok(())
}
fn same_token(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0u8, |diff, (a, b)| diff | (a ^ b))
        == 0
}

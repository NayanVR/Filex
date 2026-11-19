//! One per-user index owner, two query workers, one writer, one builder.
//!
//! The accept thread only accepts: each connection gets its own short-lived
//! `filex-conn` thread, so a silent client, a stalled network-mount syscall
//! or a big Linux root never holds up anyone else's Status, Cancel or hint.
//! Two caps bound the threads: [`READ_SLOTS`] for connections still reading
//! their request, [`FS_SLOTS`] for requests doing filesystem syscalls.
use super::{
    ipc::{self, BUSY, Command, Endpoint, Query, Request, Response, VERSION},
    query,
    watch::RootWatches,
    writer,
};
use crate::ingest::IndexFilter;
use anyhow::{Result, anyhow, ensure};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs::{File, OpenOptions},
    io::Write,
    net::{Shutdown, TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex, RwLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

/// Connections between accept and an authenticated request.
const READ_SLOTS: usize = 64;
/// Requests inside a filesystem syscall (canonicalize, lstat, watch walk).
const FS_SLOTS: usize = 8;
/// Connection threads alive at once, whatever they're doing; past this a
/// new connection is refused at once instead of spawning another thread.
const LIVE_SLOTS: usize = READ_SLOTS + FS_SLOTS + 16;
const FRAME_TIMEOUT: Duration = Duration::from_secs(2);
const BUSY_WRITE_TIMEOUT: Duration = Duration::from_millis(100);
/// How long shutdown waits for in-flight connections before abandoning them.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
const WORKER_POLL: Duration = Duration::from_millis(100);
const SCOPE_CACHE_LIMIT: usize = 64;
/// A renamed or remounted scope folder resolves afresh after this.
const SCOPE_TTL: Duration = Duration::from_secs(30);
/// A request whose scope another request is already resolving waits at most
/// this long, so a dead mount can't pile up waiting threads.
const SCOPE_WAIT: Duration = Duration::from_secs(1);
const CANCELLATION_LIMIT: usize = 1024;
const OPERATION_LIMIT: usize = 4096;

struct Job {
    stream: TcpStream,
    command: Command,
    cancel: Arc<AtomicBool>,
}

/// Everything a connection thread needs, shared behind one `Arc`.
struct Control {
    shared: Arc<writer::Shared>,
    commands: SyncSender<writer::Command>,
    watches: RootWatches,
    filter: Arc<RwLock<IndexFilter>>,
    token: String,
    jobs: SyncSender<Job>,
    streams: SyncSender<Job>,
    hot: Arc<Mutex<HashMap<u64, u64>>>,
    cancellations: Mutex<Cancellations>,
    operations: Mutex<VecDeque<u64>>,
    stop: Arc<AtomicBool>,
    reading: Arc<Slots>,
    filesystem: Arc<Slots>,
    /// Every live connection thread, capped at [`LIVE_SLOTS`]; shutdown
    /// drains it.
    live: Arc<Slots>,
    scopes: ScopeCache,
}

impl Control {
    fn new(
        handle: &writer::Handle,
        watches: RootWatches,
        token: String,
        jobs: SyncSender<Job>,
        streams: SyncSender<Job>,
        stop: Arc<AtomicBool>,
    ) -> Self {
        Self {
            shared: handle.shared.clone(),
            commands: handle.commands.clone(),
            watches,
            filter: handle.shared.filter.clone(),
            token,
            jobs,
            streams,
            hot: Default::default(),
            cancellations: Default::default(),
            operations: Default::default(),
            stop,
            reading: Slots::new(READ_SLOTS),
            filesystem: Slots::new(FS_SLOTS),
            live: Slots::new(LIVE_SLOTS),
            scopes: ScopeCache::new(SCOPE_TTL, SCOPE_WAIT),
        }
    }

    fn stopping(&self) -> bool {
        self.stop.load(Ordering::Relaxed) || self.shared.stop.load(Ordering::Relaxed)
    }
}

pub fn run(directory: &Path, roots: Vec<PathBuf>, stop: Arc<AtomicBool>) -> Result<()> {
    let _lock = lock_directory(directory)?;
    super::executable::note_startup();
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
    let endpoint = Endpoint {
        version: VERSION,
        port: listener.local_addr()?.port(),
        token: new_token()?,
    };
    let handle = writer::start(directory, roots)?;
    let endpoint_path = publish_endpoint(directory, &endpoint)?;
    let filter = handle.shared.filter.clone();
    let handler = watch_handler(&handle, filter.clone(), directory.canonicalize()?);
    let watches = RootWatches::new(Arc::new(handler), filter)?;
    let (jobs, interactive) = mpsc::sync_channel::<Job>(16);
    let (streams, exhaustive) = mpsc::sync_channel::<Job>(4);
    let control = Arc::new(Control::new(
        &handle,
        watches,
        endpoint.token.clone(),
        jobs,
        streams,
        stop,
    ));
    let mut workers = Vec::new();
    for (index, rx) in [interactive, exhaustive].into_iter().enumerate() {
        workers.push(spawn_worker(index, rx, &control)?);
    }
    spawn_watch_setup(control.clone())?;
    let wake = spawn_wake(&control, endpoint.port);
    accept_loop(&listener, &control);
    shut_down(control, workers, handle, wake, &endpoint_path)
}

fn lock_directory(directory: &Path) -> Result<File> {
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
    Ok(lock)
}

fn new_token() -> Result<String> {
    let mut random = [0u8; 32];
    getrandom::getrandom(&mut random).map_err(|e| anyhow!("randomness failed: {e}"))?;
    Ok(random.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn publish_endpoint(directory: &Path, endpoint: &Endpoint) -> Result<PathBuf> {
    let mut endpoint_file = File::create(directory.join("endpoint.tmp"))?;
    endpoint_file.write_all(&serde_json::to_vec(endpoint)?)?;
    endpoint_file.sync_all()?;
    drop(endpoint_file);
    let endpoint_path = directory.join("endpoint.json");
    crate::catalog::manifest::replace_file(&directory.join("endpoint.tmp"), &endpoint_path)?;
    Ok(endpoint_path)
}

/// Turns notify events into writer path batches. Assumes notify reports
/// absolute paths under the watched roots; anything under the daemon's own
/// data directory (`ignored`, canonical) or outside every root is dropped.
fn watch_handler(
    handle: &writer::Handle,
    filter: Arc<RwLock<IndexFilter>>,
    ignored: PathBuf,
) -> impl Fn(notify::Result<notify::Event>) + Send + Sync + 'static {
    let owner = handle.shared.clone();
    let events = handle.commands.clone();
    move |event| match event {
        Ok(mut event) => {
            if matches!(event.kind, notify::EventKind::Access(_)) {
                return;
            }
            if event.need_rescan() {
                owner.overflowed(&event.paths);
                // A dropped root is watched again; its error is resolved.
                // ponytail: with several broken roots this clears early,
                // track errors per root if that ever matters.
                if event.info() == Some(super::watch::REARMED) {
                    *owner.coverage_error.write().unwrap() = None;
                }
            }
            {
                let view = owner.view.read().unwrap();
                let filter = filter.read().unwrap();
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
            let paths = event.paths;
            if let Some(paths) =
                refused_paths(events.try_send(writer::Command::Paths { paths, remove }))
            {
                owner.overflowed(&paths);
            }
        }
        Err(error) => {
            *owner.coverage_error.write().unwrap() =
                Some(format!("watch coverage incomplete: {error}"));
            // macOS/Windows re-arm the failed root and send a `REARMED`
            // rescan once it is watched again; reconciling now, with the
            // watch still dead, would only walk it twice.
            if !super::watch::REARMS_ON_ERROR {
                owner.overflowed(&error.paths);
            }
        }
    }
}

/// The paths of a `Paths` command the writer's queue refused.
fn refused_paths(sent: Result<(), mpsc::TrySendError<writer::Command>>) -> Option<Vec<PathBuf>> {
    match sent {
        Err(
            mpsc::TrySendError::Full(writer::Command::Paths { paths, .. })
            | mpsc::TrySendError::Disconnected(writer::Command::Paths { paths, .. }),
        ) => Some(paths),
        _ => None,
    }
}

/// Registers root watches off the accept path (a big Linux root walks its
/// whole tree), then lets the writer reconcile. The writer reports
/// `building` until that `Reconcile` arrives (`awaiting_watch`).
fn spawn_watch_setup(control: Arc<Control>) -> Result<()> {
    std::thread::Builder::new()
        .name("filex-watch-setup".into())
        .spawn(move || {
            let roots = control.shared.view.read().unwrap().roots.clone();
            for root in &roots {
                if let Err(e) = control.watches.add_root(&root.path) {
                    *control.shared.coverage_error.write().unwrap() =
                        Some(format!("watch coverage incomplete: {e}"));
                }
            }
            // Fails only once the writer is gone, i.e. during shutdown.
            let _ = control.commands.send(writer::Command::Reconcile);
        })?;
    Ok(())
}

fn spawn_worker(index: usize, rx: Receiver<Job>, control: &Control) -> Result<JoinHandle<()>> {
    let shared = control.shared.clone();
    let hot = control.hot.clone();
    let stop = control.stop.clone();
    Ok(std::thread::Builder::new()
        .name(format!("filex-query-{index}"))
        .spawn(move || {
            // Connection threads hold job senders, so a closed channel alone
            // can't end this loop; the stop flag does.
            loop {
                match rx.recv_timeout(WORKER_POLL) {
                    Ok(job) => run_job(&shared, &hot, job),
                    Err(RecvTimeoutError::Timeout) if !stop.load(Ordering::Relaxed) => {}
                    Err(_) => break,
                }
            }
        })?)
}

fn run_job(shared: &writer::Shared, hot: &Mutex<HashMap<u64, u64>>, job: Job) {
    let mut stream = job.stream;
    let view = shared.view.read().unwrap().clone();
    let result = match job.command {
        Command::Search(q) => {
            if view.base.is_empty() && shared.status.read().unwrap().building {
                Err(anyhow!("Building index"))
            } else {
                let hot = hot.lock().unwrap().clone();
                query::search(&view, &q, &job.cancel, &hot)
                    .and_then(|page| ipc::write_frame(&mut stream, &Response::Page(page)))
            }
        }
        Command::StreamMatches(q) => query::stream(&view, &q, &job.cancel, |batch| {
            ipc::write_frame(&mut stream, &Response::Batch(batch))
        }),
        _ => Err(anyhow!("invalid worker request")),
    };
    if let Err(error) = result {
        let _ = ipc::write_frame(&mut stream, &Response::Error(error.to_string()));
    }
}

/// Wakes the blocking accept on shutdown; no idle accept polling loop.
fn spawn_wake(control: &Control, port: u16) -> JoinHandle<()> {
    let stop = control.stop.clone();
    let writer_stop = control.shared.stop.clone();
    std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) && !writer_stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(100));
        }
        // Cancel an in-flight build now. Teardown reaches the writer only after
        // joining query workers and the watcher (FIL-27: SIGTERM mid-build).
        writer_stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port));
    })
}

fn accept_loop(listener: &TcpListener, control: &Arc<Control>) {
    let mut refused = VecDeque::new();
    for incoming in listener.incoming() {
        if control.stopping() {
            break;
        }
        let Ok(stream) = incoming else { continue };
        let (Ok(reading), Ok(live)) = (control.reading.take(), control.live.take()) else {
            refuse_busy(stream, &mut refused);
            continue;
        };
        let connection = control.clone();
        // A failed spawn drops the closure: the stream closes, the slots free.
        let _ = std::thread::Builder::new()
            .name("filex-conn".into())
            .spawn(move || serve(&connection, stream, reading, live));
    }
}

/// How long a refused connection stays open after its "busy" reply, and
/// how many may wait at once.
const REFUSED_LINGER: Duration = Duration::from_secs(1);
const REFUSED_MAX: usize = 256;

/// Answers "busy" without waiting on the client (this runs on the accept
/// thread). The socket is only half-closed, then kept open in `refused`
/// for [`REFUSED_LINGER`]: closing a socket whose request hasn't been read
/// makes the kernel send an RST, and a request that arrives after the
/// close does too. Either way the client gets "broken pipe" or "reset"
/// instead of reading the reply, and the UI took that for a crash.
// ponytail: refused sockets are pruned only on the next accept, so up to
// REFUSED_MAX descriptors can sit idle until then; a timer if that matters.
fn refuse_busy(mut stream: TcpStream, refused: &mut VecDeque<(Instant, TcpStream)>) {
    let _ = stream.set_write_timeout(Some(BUSY_WRITE_TIMEOUT));
    let _ = ipc::write_frame(&mut stream, &Response::Error(BUSY.into()));
    let _ = stream.shutdown(Shutdown::Write);
    let now = Instant::now();
    while refused
        .front()
        .is_some_and(|(at, _)| now.duration_since(*at) >= REFUSED_LINGER)
        || refused.len() >= REFUSED_MAX
    {
        refused.pop_front();
    }
    refused.push_back((now, stream));
}

fn serve(control: &Control, mut stream: TcpStream, reading: Slot, _live: Slot) {
    let command = read_request(control, &mut stream);
    drop(reading);
    let Some(command) = command else { return };
    let response = match dispatch(control, &mut stream, command) {
        Ok(Some(response)) => response,
        Ok(None) => return,
        Err(e) => Response::Error(e.to_string()),
    };
    let _ = ipc::write_frame(&mut stream, &response);
}

/// Reads one frame and checks it's from a client holding the endpoint token.
fn read_request(control: &Control, stream: &mut TcpStream) -> Option<Command> {
    stream.set_nodelay(true).ok()?;
    stream.set_read_timeout(Some(FRAME_TIMEOUT)).ok()?;
    stream.set_write_timeout(Some(FRAME_TIMEOUT)).ok()?;
    let request = ipc::read_frame::<Request>(stream).ok()?;
    if request.version != VERSION || !same_token(&request.token, &control.token) {
        let _ = ipc::write_frame(
            stream,
            &Response::Error("unauthorized or incompatible daemon client".into()),
        );
        return None;
    }
    Some(request.command)
}

/// `Ok(None)` means a query worker now owns the reply.
fn dispatch(
    control: &Control,
    stream: &mut TcpStream,
    command: Command,
) -> Result<Option<Response>> {
    match command {
        command @ (Command::Search(_) | Command::StreamMatches(_)) => {
            submit(control, stream, command)
        }
        Command::Status => Ok(Some(Response::Status(status(&control.shared)))),
        Command::Reconcile => {
            *control.filter.write().unwrap() = IndexFilter::load();
            // Let watches heal: a resync re-reports anything still failing.
            *control.shared.coverage_error.write().unwrap() = None;
            control.watches.resync();
            control.commands.try_send(writer::Command::Reconcile)?;
            Ok(Some(Response::Ack))
        }
        Command::Cancel {
            client,
            before_request,
        } => {
            let mut cancellations = control.cancellations.lock().unwrap();
            cancellations.cancel(client, before_request);
            Ok(Some(Response::Ack))
        }
        Command::AddRoot(path) => {
            let _slot = control.filesystem.take()?;
            let roots: Vec<_> = (control.shared.view.read().unwrap().roots.iter())
                .map(|r| r.path.clone())
                .collect();
            let path = crate::ingest::validate_new_root(&roots, &path)?;
            control.watches.add_root(&path)?;
            control.commands.send(writer::Command::AddRoot(path))?;
            Ok(Some(Response::Ack))
        }
        Command::RemoveRoot(path) => {
            let path = {
                let _slot = control.filesystem.take()?;
                crate::ingest::canonical_event_path(&path)
            };
            control.watches.remove_root(&path);
            control.commands.send(writer::Command::RemoveRoot(path))?;
            Ok(Some(Response::Ack))
        }
        Command::HintFilesystemChange {
            operation_id,
            paths,
        } => {
            // No filesystem I/O here: the writer inspects the paths later.
            ensure!(paths.len() <= 4096, "too many hint paths");
            if first_sighting(&control.operations, operation_id)
                && let Some(paths) =
                    refused_paths(control.commands.try_send(writer::Command::Paths {
                        paths,
                        remove: false,
                    }))
            {
                control.shared.overflowed(&paths);
            }
            Ok(Some(Response::Ack))
        }
        Command::Touch { path } => {
            touch(control, &path)?;
            Ok(Some(Response::Ack))
        }
        Command::Shutdown => {
            // The wake thread sees the flag and unblocks `incoming`.
            control.stop.store(true, Ordering::Relaxed);
            Ok(Some(Response::Ack))
        }
        Command::Verify(hit) => {
            let _slot = control.filesystem.take()?;
            query::verify(&hit)?;
            let view = control.shared.view.read().unwrap();
            ensure!(
                view.record(hit.id)
                    .is_some_and(|r| r.identity == hit.identity)
                    && view.path(hit.id).as_ref() == Some(&hit.path),
                "search target changed"
            );
            Ok(Some(Response::Ack))
        }
    }
}

/// Validates and canonicalizes a query, registers it for cancellation and
/// hands it to a query worker.
fn submit(control: &Control, stream: &TcpStream, mut command: Command) -> Result<Option<Response>> {
    let (Command::Search(q) | Command::StreamMatches(q)) = &mut command else {
        anyhow::bail!("invalid search request");
    };
    ensure!(
        q.text.len() <= 4096
            && q.filters.len() <= 64
            && q.allowed.as_ref().is_none_or(|v| v.len() <= 100_000),
        "query exceeds protocol bounds"
    );
    canonicalize(control, q)?;
    let (client, request) = (q.client, q.request);
    let cancel = control
        .cancellations
        .lock()
        .unwrap()
        .register(client, request)?;
    let queue = if matches!(command, Command::StreamMatches(_)) {
        &control.streams
    } else {
        &control.jobs
    };
    queue
        .try_send(Job {
            stream: stream.try_clone()?,
            command,
            cancel,
        })
        .map_err(|_| anyhow!(BUSY))?;
    Ok(None)
}

/// Resolves the scope (cached) and any tag-filter `allowed` paths (not
/// cached), taking one filesystem slot for whichever syscalls actually run.
fn canonicalize(control: &Control, q: &mut Query) -> Result<()> {
    let mut slot = None;
    if let Some(scope) = q.scope.take() {
        q.scope = Some(control.scopes.resolve(&scope, |raw| {
            slot = Some(control.filesystem.take()?);
            Ok(crate::ingest::canonical_event_path(raw))
        })?);
    }
    if let Some(paths) = &mut q.allowed {
        if slot.is_none() {
            slot = Some(control.filesystem.take()?);
        }
        for path in paths {
            *path = crate::ingest::canonical_event_path(path);
        }
    }
    drop(slot);
    Ok(())
}

fn status(shared: &writer::Shared) -> ipc::Status {
    let mut status = shared.status.read().unwrap().clone();
    status.progress = shared.progress.snapshot();
    if let Some(error) = shared.coverage_error.read().unwrap().clone() {
        status.error = Some(error);
        for root in &mut status.roots {
            root.state = "Watch coverage incomplete".into();
        }
    }
    status
}

/// Marks an indexed path recently used. Index lookup only, no filesystem I/O.
fn touch(control: &Control, path: &Path) -> Result<()> {
    if let Some(id) = control.shared.view.read().unwrap().resolve(path) {
        let mut hot = control.hot.lock().unwrap();
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();
        hot.insert(id, stamp);
        if hot.len() > 512
            && let Some(oldest) = hot.iter().min_by_key(|(_, t)| **t).map(|(&id, _)| id)
        {
            hot.remove(&oldest);
        }
    }
    Ok(())
}

/// Whether `operation_id` is new; remembers it. Check and insert happen
/// under one lock so concurrent duplicate hints apply once.
fn first_sighting(operations: &Mutex<VecDeque<u64>>, operation_id: u64) -> bool {
    let mut operations = operations.lock().unwrap();
    if operations.contains(&operation_id) {
        return false;
    }
    operations.push_back(operation_id);
    if operations.len() > OPERATION_LIMIT {
        operations.pop_front();
    }
    true
}

fn shut_down(
    control: Arc<Control>,
    workers: Vec<JoinHandle<()>>,
    handle: writer::Handle,
    wake: JoinHandle<()>,
    endpoint_path: &Path,
) -> Result<()> {
    control.stop.store(true, Ordering::Relaxed);
    control.cancellations.lock().unwrap().cancel_all();
    // Stragglers (a silent client, a hung syscall) are abandoned; they hold
    // their own `Arc<Control>` and end with the process.
    let deadline = Instant::now() + DRAIN_TIMEOUT;
    while control.live.in_use() > 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    for worker in workers {
        let _ = worker.join();
    }
    let shared = control.shared.clone();
    drop(control);
    drop(handle);
    let _ = wake.join();
    let _ = std::fs::remove_file(endpoint_path);
    // A failure exit, so launchd (KeepAlive SuccessfulExit=false), systemd
    // (Restart=on-failure) or the app's reconnect starts the new binary.
    ensure!(
        !shared.restart.load(Ordering::Relaxed),
        "filex-indexd was replaced on disk; exiting so the new build starts"
    );
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

/// A counting semaphore that never blocks: [`Slots::take`] fails at the cap.
struct Slots {
    used: AtomicUsize,
    limit: usize,
}

/// Holds one slot; dropping it (including on unwind) gives it back.
struct Slot(Arc<Slots>);

impl Slots {
    fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            used: AtomicUsize::new(0),
            limit,
        })
    }

    fn take(self: &Arc<Self>) -> Result<Slot> {
        self.used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < self.limit).then_some(n + 1)
            })
            .map_err(|_| anyhow!(BUSY))?;
        Ok(Slot(self.clone()))
    }

    fn in_use(&self) -> usize {
        self.used.load(Ordering::Acquire)
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.used.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Which searches each client still wants. Requests arrive on concurrent
/// connections, so a `Cancel` can land before the search it cancels; the
/// per-client watermark catches that one when it registers.
#[derive(Default)]
struct Cancellations {
    latest: HashMap<u64, (u64, Arc<AtomicBool>)>,
    cancelled_before: HashMap<u64, u64>,
}

impl Cancellations {
    fn register(&mut self, client: u64, request: u64) -> Result<Arc<AtomicBool>> {
        let superseded = self
            .latest
            .get(&client)
            .is_some_and(|(latest, _)| *latest > request)
            || self
                .cancelled_before
                .get(&client)
                .is_some_and(|before| request < *before);
        ensure!(!superseded, "superseded search request");
        let cancel = Arc::new(AtomicBool::new(false));
        if let Some((_, old)) = self.latest.insert(client, (request, cancel.clone())) {
            old.store(true, Ordering::Relaxed);
        }
        if self.latest.len() > CANCELLATION_LIMIT {
            // Only queued or running searches hold a second reference.
            self.latest
                .retain(|_, (_, flag)| Arc::strong_count(flag) > 1);
        }
        Ok(cancel)
    }

    fn cancel(&mut self, client: u64, before: u64) {
        if let Some((request, flag)) = self.latest.get(&client)
            && *request < before
        {
            flag.store(true, Ordering::Relaxed);
        }
        if self.cancelled_before.len() >= CANCELLATION_LIMIT
            && !self.cancelled_before.contains_key(&client)
        {
            let latest = &self.latest;
            self.cancelled_before.retain(|c, _| latest.contains_key(c));
        }
        let mark = self.cancelled_before.entry(client).or_default();
        *mark = (*mark).max(before);
    }

    fn cancel_all(&self) {
        for (_, flag) in self.latest.values() {
            flag.store(true, Ordering::Relaxed);
        }
    }
}

/// Raw scope path -> canonical path, bounded, expiring, single-flight: the
/// UI sends the same scope on every keystroke, and concurrent misses for one
/// path wait on a single resolution instead of each entering the syscall.
struct ScopeCache {
    state: Mutex<ScopeState>,
    ready: Condvar,
    ttl: Duration,
    wait: Duration,
}

#[derive(Default)]
struct ScopeState {
    resolved: HashMap<PathBuf, (PathBuf, Instant)>,
    pending: HashSet<PathBuf>,
}

/// Clears a pending resolution even if the resolver errors or panics.
struct Pending<'a> {
    cache: &'a ScopeCache,
    key: PathBuf,
}

impl Drop for Pending<'_> {
    fn drop(&mut self) {
        let mut state = self.cache.state.lock().unwrap_or_else(|e| e.into_inner());
        state.pending.remove(&self.key);
        self.cache.ready.notify_all();
    }
}

impl ScopeCache {
    fn new(ttl: Duration, wait: Duration) -> Self {
        Self {
            state: Default::default(),
            ready: Condvar::new(),
            ttl,
            wait,
        }
    }

    fn resolve(
        &self,
        raw: &Path,
        resolver: impl FnOnce(&Path) -> Result<PathBuf>,
    ) -> Result<PathBuf> {
        let deadline = Instant::now() + self.wait;
        let mut state = self.state.lock().unwrap();
        loop {
            if let Some((path, at)) = state.resolved.get(raw)
                && at.elapsed() < self.ttl
            {
                return Ok(path.clone());
            }
            if !state.pending.contains(raw) {
                break;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            ensure!(!left.is_zero(), BUSY);
            state = self.ready.wait_timeout(state, left).unwrap().0;
        }
        state.pending.insert(raw.to_path_buf());
        drop(state);
        let _pending = Pending {
            cache: self,
            key: raw.to_path_buf(),
        };
        let path = resolver(raw)?;
        let mut state = self.state.lock().unwrap();
        if state.resolved.len() >= SCOPE_CACHE_LIMIT {
            let ttl = self.ttl;
            state.resolved.retain(|_, (_, at)| at.elapsed() < ttl);
            if state.resolved.len() >= SCOPE_CACHE_LIMIT {
                state.resolved.clear();
            }
        }
        state
            .resolved
            .insert(raw.to_path_buf(), (path.clone(), Instant::now()));
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;

    #[test]
    fn cancel_arriving_before_its_search_still_cancels() {
        let mut c = Cancellations::default();
        c.cancel(7, 5);
        assert!(c.register(7, 4).is_err(), "below the watermark");
        assert!(
            c.register(7, 5).is_ok(),
            "the request the cancel made way for"
        );
        // Another client is unaffected.
        assert!(c.register(8, 1).is_ok());
        // A cancel after registration flags the running search.
        let flag = c.register(7, 6).unwrap();
        c.cancel(7, 7);
        assert!(flag.load(Ordering::Relaxed));
    }

    #[test]
    fn superseded_request_rejected() {
        let mut c = Cancellations::default();
        let first = c.register(1, 10).unwrap();
        let second = c.register(1, 11).unwrap();
        assert!(
            first.load(Ordering::Relaxed),
            "a newer request cancels the older"
        );
        assert!(!second.load(Ordering::Relaxed));
        assert!(
            c.register(1, 10).is_err(),
            "late arrival of an older request"
        );
        assert!(
            c.register(1, 11).is_ok(),
            "same request id (paging) still allowed"
        );
    }

    #[test]
    fn cancellations_prune_keeps_live() {
        let mut c = Cancellations::default();
        let live: Vec<_> = (0..4)
            .map(|client| c.register(client, 1).unwrap())
            .collect();
        for client in 4..CANCELLATION_LIMIT as u64 + 1 {
            drop(c.register(client, 1).unwrap());
        }
        assert!(c.latest.len() <= 5, "finished searches pruned");
        for (client, flag) in live.iter().enumerate() {
            assert!(c.latest.contains_key(&(client as u64)));
            c.cancel(client as u64, 2);
            assert!(flag.load(Ordering::Relaxed));
        }
        for client in 0..CANCELLATION_LIMIT as u64 * 2 {
            c.cancel(client, 1);
        }
        assert!(c.cancelled_before.len() <= CANCELLATION_LIMIT + 1);
    }

    #[test]
    fn hint_dedup_once_per_operation() {
        let operations = Arc::new(Mutex::new(VecDeque::new()));
        let barrier = Arc::new(Barrier::new(8));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let (operations, barrier) = (operations.clone(), barrier.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    first_sighting(&operations, 42)
                })
            })
            .collect();
        let applied = threads
            .into_iter()
            .map(|t| t.join().unwrap())
            .filter(|&first| first)
            .count();
        assert_eq!(applied, 1, "concurrent duplicates apply once");
        assert!(first_sighting(&operations, 43));
        for id in 1000..1000 + OPERATION_LIMIT as u64 {
            first_sighting(&operations, id);
        }
        assert!(first_sighting(&operations, 42), "oldest ids age out");
    }

    #[test]
    fn slot_guard_releases_on_drop() {
        let slots = Slots::new(2);
        let a = slots.take().unwrap();
        let _b = slots.take().unwrap();
        assert!(slots.take().is_err());
        drop(a);
        let c = slots.take().unwrap();
        let held = slots.clone();
        let panicked = std::thread::spawn(move || {
            let _slot = c;
            panic!("connection thread died");
        })
        .join();
        assert!(panicked.is_err());
        assert_eq!(held.in_use(), 1, "unwind released the slot");
        assert!(slots.take().is_ok());
    }

    /// Counts calls and blocks until `open` is set.
    struct Gate {
        calls: AtomicUsize,
        open: AtomicBool,
    }

    impl Gate {
        fn new(open: bool) -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicUsize::new(0),
                open: AtomicBool::new(open),
            })
        }

        fn resolve(&self, raw: &Path) -> Result<PathBuf> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            while !self.open.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(2));
            }
            Ok(raw.join("canonical"))
        }
    }

    fn wait_for(condition: impl Fn() -> bool) {
        let start = Instant::now();
        while !condition() {
            assert!(start.elapsed() < Duration::from_secs(5), "timed out");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn scope_cache_single_flight() {
        let cache = Arc::new(ScopeCache::new(SCOPE_TTL, Duration::from_secs(5)));
        let gate = Gate::new(false);
        let threads: Vec<_> = (0..6)
            .map(|_| {
                let (cache, gate) = (cache.clone(), gate.clone());
                std::thread::spawn(move || cache.resolve(Path::new("/scope"), |p| gate.resolve(p)))
            })
            .collect();
        wait_for(|| gate.calls.load(Ordering::SeqCst) == 1);
        std::thread::sleep(Duration::from_millis(30));
        gate.open.store(true, Ordering::SeqCst);
        for thread in threads {
            assert_eq!(
                thread.join().unwrap().unwrap(),
                Path::new("/scope/canonical")
            );
        }
        assert_eq!(gate.calls.load(Ordering::SeqCst), 1);
        // A failed resolution is not cached and doesn't wedge the key.
        let failed = cache.resolve(Path::new("/bad"), |_| Err(anyhow!("gone")));
        assert!(failed.is_err());
        assert!(
            cache
                .resolve(Path::new("/bad"), |p| gate.resolve(p))
                .is_ok()
        );
    }

    #[test]
    fn scope_cache_waiter_gives_up_when_resolution_stalls() {
        let cache = Arc::new(ScopeCache::new(SCOPE_TTL, Duration::from_millis(50)));
        let gate = Gate::new(false);
        let leader = {
            let (cache, gate) = (cache.clone(), gate.clone());
            std::thread::spawn(move || cache.resolve(Path::new("/dead"), |p| gate.resolve(p)))
        };
        wait_for(|| gate.calls.load(Ordering::SeqCst) == 1);
        let start = Instant::now();
        let waiter = cache.resolve(Path::new("/dead"), |p| gate.resolve(p));
        assert!(waiter.unwrap_err().to_string().contains("busy"));
        assert!(start.elapsed() < Duration::from_millis(500));
        gate.open.store(true, Ordering::SeqCst);
        assert!(leader.join().unwrap().is_ok());
    }

    #[test]
    fn scope_cache_expires() {
        let cache = ScopeCache::new(Duration::from_millis(40), SCOPE_WAIT);
        let gate = Gate::new(true);
        let scope = Path::new("/scope");
        cache.resolve(scope, |p| gate.resolve(p)).unwrap();
        cache.resolve(scope, |p| gate.resolve(p)).unwrap();
        assert_eq!(gate.calls.load(Ordering::SeqCst), 1, "second hit cached");
        std::thread::sleep(Duration::from_millis(60));
        cache.resolve(scope, |p| gate.resolve(p)).unwrap();
        assert_eq!(
            gate.calls.load(Ordering::SeqCst),
            2,
            "expired entry re-resolved"
        );
        for i in 0..SCOPE_CACHE_LIMIT * 2 {
            cache
                .resolve(&PathBuf::from(format!("/s{i}")), |p| gate.resolve(p))
                .unwrap();
        }
        assert!(cache.state.lock().unwrap().resolved.len() <= SCOPE_CACHE_LIMIT);
    }

    fn loopback() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        (listener.accept().unwrap().0, client)
    }

    /// A `Control` over a writer with no roots. Keep the handle and both
    /// receivers alive for as long as the control is used.
    fn test_control(dir: &Path) -> (Arc<Control>, writer::Handle, [Receiver<Job>; 2]) {
        let handle = writer::start(dir, Vec::new()).unwrap();
        let filter = Arc::new(RwLock::new(IndexFilter::default()));
        let watches = RootWatches::new(Arc::new(|_| {}), filter).unwrap();
        let (jobs, interactive) = mpsc::sync_channel(16);
        let (streams, exhaustive) = mpsc::sync_channel(4);
        let stop = Arc::new(AtomicBool::new(false));
        let control = Arc::new(Control::new(
            &handle,
            watches,
            "token".into(),
            jobs,
            streams,
            stop,
        ));
        (control, handle, [interactive, exhaustive])
    }

    /// Past [`LIVE_SLOTS`] connection threads, a new connection is refused
    /// on the accept thread instead of spawning one more.
    #[test]
    fn live_connection_cap_refuses_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let (control, _handle, _queues) = test_control(dir.path());
        let held: Vec<_> = (0..LIVE_SLOTS)
            .map(|_| control.live.take().unwrap())
            .collect();
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let accept = {
            let control = control.clone();
            std::thread::spawn(move || accept_loop(&listener, &control))
        };
        let mut client = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let reply = ipc::read_frame::<Response>(&mut client).unwrap();
        assert!(matches!(reply, Response::Error(e) if e.contains("busy")));
        assert_eq!(control.reading.in_use(), 0);
        drop(held);
        control.stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port));
        accept.join().unwrap();
    }

    /// Regression: a refused client whose request arrived after the
    /// refusal got "broken pipe" or "reset" instead of the busy reply (the
    /// socket was closed with the request unread), and the UI counted that
    /// as a daemon crash.
    #[test]
    fn a_late_request_still_reads_the_busy_reply() {
        let dir = tempfile::tempdir().unwrap();
        let (control, _handle, _queues) = test_control(dir.path());
        let held: Vec<_> = (0..LIVE_SLOTS)
            .map(|_| control.live.take().unwrap())
            .collect();
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let accept = {
            let control = control.clone();
            std::thread::spawn(move || accept_loop(&listener, &control))
        };
        let mut client = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        // The refusal has happened before the request is written.
        std::thread::sleep(Duration::from_millis(100));
        // A frame goes out as two writes (length, then body); the first
        // reaching a closed socket draws an RST, failing the second with
        // EPIPE, which is what the client saw.
        let body = serde_json::to_vec(&Response::Ack).unwrap();
        client
            .write_all(&(body.len() as u32).to_le_bytes())
            .unwrap();
        std::thread::sleep(Duration::from_millis(50));
        client.write_all(&body).unwrap();
        let reply = ipc::read_frame::<Response>(&mut client).unwrap();
        assert!(matches!(reply, Response::Error(e) if e.contains("busy")));
        drop(held);
        control.stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port));
        accept.join().unwrap();
    }

    /// One watch error used to reconcile twice on macOS/Windows: once at
    /// the error, with the watch still dead, and again at the `REARMED`
    /// rescan. Now only the rescan does, and only for that root.
    #[test]
    fn watch_error_reconciles_once_the_root_is_rearmed() {
        use crate::daemon::builder::{Scope, Subtrees};
        use notify::{Event, EventKind, event::Flag};
        let dir = tempfile::tempdir().unwrap();
        let tree = tempfile::tempdir().unwrap();
        let root = tree.path().canonicalize().unwrap();
        let handle = writer::start(dir.path(), vec![root.clone()]).unwrap();
        let shared = handle.shared.clone();
        let handler = watch_handler(
            &handle,
            shared.filter.clone(),
            dir.path().canonicalize().unwrap(),
        );
        // Stop the writer so nothing else takes the overflow.
        drop(handle);
        let id = shared.view.read().unwrap().roots[0].id;

        handler(Err(
            notify::Error::generic("read failed").add_path(root.join("a"))
        ));
        assert!(shared.coverage_error.read().unwrap().is_some());
        let pending = shared.overflow.lock().unwrap().take();
        if super::super::watch::REARMS_ON_ERROR {
            assert_eq!(pending, None, "the rescan after re-arming reconciles");
        } else {
            // Linux names the directory it couldn't watch: only that subtree.
            assert_eq!(
                pending,
                Some(Scope::Roots(
                    [(id, Subtrees::Dirs([root.join("a")].into()))].into()
                ))
            );
        }

        handler(Ok(Event::new(EventKind::Other)
            .set_flag(Flag::Rescan)
            .set_info(super::super::watch::REARMED)
            .add_path(root)));
        assert!(shared.coverage_error.read().unwrap().is_none());
        assert_eq!(
            shared.overflow.lock().unwrap().take(),
            Some(Scope::roots([id]))
        );
        // A rescan naming no path (native FSEvents/RDCW) covers every root.
        handler(Ok(Event::new(EventKind::Other).set_flag(Flag::Rescan)));
        assert_eq!(shared.overflow.lock().unwrap().take(), Some(Scope::All));
    }

    #[test]
    fn fs_pool_exhaustion_does_not_block_status() {
        let dir = tempfile::tempdir().unwrap();
        let (control, _handle, _queues) = test_control(dir.path());
        // Every filesystem slot is stuck in a scope resolution (a dead mount).
        let gate = Gate::new(false);
        let stalled: Vec<_> = (0..FS_SLOTS)
            .map(|i| {
                let (control, gate) = (control.clone(), gate.clone());
                std::thread::spawn(move || {
                    control
                        .scopes
                        .resolve(&PathBuf::from(format!("/dead/{i}")), |p| {
                            let _slot = control.filesystem.take()?;
                            gate.resolve(p)
                        })
                })
            })
            .collect();
        wait_for(|| control.filesystem.in_use() == FS_SLOTS);
        let (mut stream, _client) = loopback();
        let start = Instant::now();
        assert!(matches!(
            dispatch(&control, &mut stream, Command::Status),
            Ok(Some(Response::Status(_)))
        ));
        let cancel = Command::Cancel {
            client: 1,
            before_request: 3,
        };
        assert!(matches!(
            dispatch(&control, &mut stream, cancel),
            Ok(Some(Response::Ack))
        ));
        let touch = Command::Touch { path: "/x".into() };
        assert!(matches!(
            dispatch(&control, &mut stream, touch),
            Ok(Some(Response::Ack))
        ));
        // Requests that need the filesystem get a fast "busy" instead.
        let scoped = Command::Search(Query {
            scope: Some("/elsewhere".into()),
            client: 1,
            request: 3,
            ..Default::default()
        });
        let busy = dispatch(&control, &mut stream, scoped)
            .err()
            .map(|e| e.to_string());
        assert!(busy.is_some_and(|e| e.contains("busy")));
        let remove = Command::RemoveRoot("/nowhere".into());
        assert!(dispatch(&control, &mut stream, remove).is_err());
        // An unscoped search needs no filesystem slot at all.
        let plain = Command::Search(Query {
            client: 1,
            request: 3,
            ..Default::default()
        });
        assert!(matches!(dispatch(&control, &mut stream, plain), Ok(None)));
        assert!(
            start.elapsed() < Duration::from_millis(200),
            "{:?}",
            start.elapsed()
        );
        gate.open.store(true, Ordering::SeqCst);
        for thread in stalled {
            assert!(thread.join().unwrap().is_ok());
        }
        assert_eq!(control.filesystem.in_use(), 0);
    }

    #[test]
    fn token_compare() {
        assert!(same_token("abc", "abc"));
        assert!(!same_token("abc", "abd"));
        assert!(!same_token("abc", "abcd"));
    }
}

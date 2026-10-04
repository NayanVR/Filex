//! Bounded IPC and watchdogs. No native provider call runs in this process.
use super::protocol::{self, Event, Request};
use anyhow::{Context, Result, bail, ensure};
use std::{
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        io::AsRawHandle,
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Condvar, Mutex, OnceLock, mpsc},
    time::{Duration, Instant},
};
use windows::Win32::{
    Foundation::*,
    System::{JobObjects::*, Threading::CREATE_NO_WINDOW},
};
const DEADLINE: Duration = Duration::from_secs(10);
struct Job(HANDLE);
// A job handle is a kernel object; it has no COM apartment affinity.
unsafe impl Send for Job {}
impl Drop for Job {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
struct Helper {
    child: Child,
    _job: Job,
    sender: mpsc::SyncSender<Request>,
    receiver: mpsc::Receiver<Result<Event, String>>,
}
impl Helper {
    fn spawn(mode: &str) -> Result<Self> {
        // Helper waits for its first command before loading providers. Attach
        // it to the lifetime job before sending anything through the pipe.
        let job = unsafe {
            let job = Job(CreateJobObjectW(None, None)?);
            let info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
                BasicLimitInformation: JOBOBJECT_BASIC_LIMIT_INFORMATION {
                    LimitFlags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
                        | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION
                        | JOB_OBJECT_LIMIT_PROCESS_MEMORY,
                    ..Default::default()
                },
                ProcessMemoryLimit: 512 * 1024 * 1024,
                ..Default::default()
            };
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&info) as u32,
            )?;
            job
        };
        let mut child = Command::new(std::env::current_exe()?)
            .arg(mode)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW.0)
            .spawn()
            .context("starting Windows preview helper")?;
        if let Err(error) =
            unsafe { AssignProcessToJobObject(job.0, HANDLE(child.as_raw_handle())) }
        {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error.into());
        }
        let mut input = child.stdin.take().context("preview input pipe missing")?;
        let mut output = child.stdout.take().context("preview output pipe missing")?;
        let (sender, requests) = mpsc::sync_channel::<Request>(1);
        let (events, receiver) = mpsc::sync_channel(4);
        let helper = Self {
            child,
            _job: job,
            sender,
            receiver,
        };
        let errors = events.clone();
        std::thread::Builder::new()
            .name("preview-pipe-writer".into())
            .spawn(move || {
                for request in requests {
                    if let Err(error) = protocol::write(&mut input, &request) {
                        let _ = errors.send(Err(error.to_string()));
                        break;
                    }
                }
            })?;
        std::thread::Builder::new()
            .name("preview-pipe-reader".into())
            .spawn(move || {
                loop {
                    let event = protocol::read(&mut output).map_err(|e| e.to_string());
                    let failed = event.is_err();
                    if events.send(event).is_err() || failed {
                        break;
                    }
                }
            })?;
        Ok(helper)
    }
    fn request(&self, request: Request) -> Result<Event> {
        request.validate()?;
        self.sender
            .try_send(request)
            .context("preview helper busy")?;
        self.receiver
            .recv_timeout(DEADLINE)
            .context("Windows thumbnail helper timed out or stopped")?
            .map_err(anyhow::Error::msg)
    }
}
impl Drop for Helper {
    fn drop(&mut self) {
        // Also closes pipes and releases any waiting reader/writer threads.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().collect()
}

/// Blocking, at most two warm helper processes. Call off the foreground thread.
pub fn thumbnail(path: &Path) -> Result<image::RgbaImage> {
    static POOL: OnceLock<[Mutex<Option<Helper>>; 2]> = OnceLock::new();
    let pool = POOL.get_or_init(|| [Mutex::new(None), Mutex::new(None)]);
    let mut slot = pool
        .iter()
        .find_map(|s| s.try_lock().ok())
        .context("native thumbnail capacity reached")?;
    if slot.is_none() {
        *slot = Some(Helper::spawn("--filex-thumbnail-helper")?);
    }
    let result = slot
        .as_ref()
        .context("thumbnail helper unavailable")?
        .request(Request::Thumbnail(wide(path)));
    match result {
        Ok(Event::Pixels {
            width,
            height,
            bgra,
        }) => {
            ensure!(
                width > 0 && height > 0 && width <= 128 && height <= 128,
                "invalid thumbnail dimensions"
            );
            image::RgbaImage::from_raw(width, height, bgra).context("invalid thumbnail buffer")
        }
        Ok(Event::Error(error)) => bail!("{error}"), // unsupported file; healthy worker
        other => {
            *slot = None; // next request starts a fresh process after crash/hang
            match other {
                Err(error) => Err(error),
                _ => bail!("invalid thumbnail response"),
            }
        }
    }
}

enum Control {
    Present(Vec<PathBuf>, usize, u64),
    Close,
}
#[derive(Default)]
struct State {
    pending: Option<Control>,
    visible: bool,
    shutdown: bool,
    kind: Option<String>,
    request_id: u64,
}
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}
/// A single Space-bar viewer. Native state lives only in its disposable child.
pub struct Preview {
    shared: Arc<Shared>,
}
impl Preview {
    pub fn new(owner: isize) -> Result<(Self, futures::channel::mpsc::UnboundedReceiver<String>)> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
        });
        let worker = shared.clone();
        let (errors, receiver) = futures::channel::mpsc::unbounded();
        std::thread::Builder::new()
            .name("preview-watchdog".into())
            .spawn(move || supervise(worker, owner, errors))?;
        Ok((Self { shared }, receiver))
    }
    pub fn is_visible(&self) -> bool {
        self.shared.state.lock().is_ok_and(|s| s.visible)
    }
    /// Reports API initialization, not verified first paint. Useful for smoke tests.
    pub fn ready_kind(&self) -> Option<String> {
        self.shared.state.lock().ok().and_then(|s| s.kind.clone())
    }
    pub fn show(&self, paths: Vec<PathBuf>, selected: usize) {
        if paths.is_empty() {
            self.close();
            return;
        }
        let selected = selected.min(paths.len() - 1);
        // Bound selection IPC while retaining the lead item and nearby items.
        let start = selected
            .saturating_sub(protocol::MAX_PATHS / 2)
            .min(paths.len().saturating_sub(protocol::MAX_PATHS));
        let paths = paths
            .into_iter()
            .skip(start)
            .take(protocol::MAX_PATHS)
            .collect();
        if let Ok(mut state) = self.shared.state.lock() {
            state.request_id = state.request_id.wrapping_add(1);
            state.pending = Some(Control::Present(paths, selected - start, state.request_id));
            state.visible = true;
            state.kind = None;
            self.shared.changed.notify_one();
        }
    }
    pub fn close(&self) {
        if let Ok(mut state) = self.shared.state.lock() {
            state.pending = Some(Control::Close);
            state.visible = false;
            state.kind = None;
            self.shared.changed.notify_one();
        }
    }
}
impl Drop for Preview {
    fn drop(&mut self) {
        if let Ok(mut state) = self.shared.state.lock() {
            state.shutdown = true;
            state.visible = false;
            self.shared.changed.notify_one();
        }
    }
}
fn supervise(
    shared: Arc<Shared>,
    owner: isize,
    errors: futures::channel::mpsc::UnboundedSender<String>,
) {
    let mut helper: Option<Helper> = None;
    let mut outgoing = None;
    let mut allowed_paths = Vec::<PathBuf>::new();
    let mut closing: Option<Instant> = None;
    let mut last_event = Instant::now();
    loop {
        let Ok(mut state) = shared.state.lock() else {
            break;
        };
        if state.shutdown {
            break;
        }
        let control = state.pending.take();
        drop(state);
        let mut failure = None;
        let mut finished = false;
        match control {
            Some(Control::Close) => {
                outgoing = Some(Request::Close);
                closing = Some(Instant::now());
            }
            Some(Control::Present(paths, selected, request_id)) => {
                if closing.take().is_some() {
                    helper = None;
                }
                allowed_paths.clone_from(&paths);
                let request = Request::Present {
                    paths: paths.iter().map(|p| wide(p)).collect(),
                    selected,
                    owner,
                    request_id,
                };
                if let Err(error) = request.validate() {
                    failure = Some(error.to_string());
                } else {
                    if helper.is_none() {
                        match Helper::spawn("--filex-preview-helper") {
                            Ok(child) => {
                                helper = Some(child);
                                last_event = Instant::now();
                            }
                            Err(error) => failure = Some(error.to_string()),
                        }
                    }
                    outgoing = Some(request);
                }
            }
            None => {}
        }
        if let Some(child) = helper.as_mut() {
            if let Some(request) = outgoing.take() {
                match child.sender.try_send(request) {
                    Ok(()) => {}
                    Err(mpsc::TrySendError::Full(request)) => outgoing = Some(request),
                    Err(_) => failure = Some("Windows preview helper stopped".into()),
                }
            }
            while let Ok(event) = child.receiver.try_recv() {
                match event {
                    Ok(Event::Heartbeat) => last_event = Instant::now(),
                    Ok(Event::Ready { kind, request_id }) => {
                        last_event = Instant::now();
                        if let Ok(mut state) = shared.state.lock() {
                            if state.visible && state.request_id == request_id {
                                state.kind = Some(kind);
                            }
                        }
                    }
                    Ok(Event::Closed) => {
                        finished = true;
                        break;
                    }
                    Ok(Event::Open(path)) => {
                        let path = PathBuf::from(std::ffi::OsString::from_wide(&path));
                        if allowed_paths.contains(&path) {
                            // Launch outside the helper's kill-on-close job. Explorer
                            // delegates to the registered app, with no command shell.
                            if let Err(error) = Command::new("explorer.exe").arg(path).spawn() {
                                let _ = errors
                                    .unbounded_send(format!("Could not open default app: {error}"));
                            }
                        }
                    }
                    Ok(Event::Error(error)) | Err(error) => {
                        failure = Some(error);
                        break;
                    }
                    Ok(_) => failure = Some("Invalid preview helper response".into()),
                }
            }
            if last_event.elapsed() > DEADLINE {
                failure =
                    Some("Windows preview handler stopped responding (10 second limit)".into());
            }
            // Only an explicit normal-close response counts as success.
            if finished {
                helper = None;
                outgoing = None;
                failure = None;
                if let Ok(mut state) = shared.state.lock() {
                    if state.pending.is_none() {
                        state.visible = false;
                        state.kind = None;
                    }
                }
            }
        }
        if closing.is_some_and(|start| start.elapsed() > Duration::from_secs(1)) {
            helper = None;
            outgoing = None;
            closing = None;
        }
        if closing.is_some() {
            failure = None;
        }
        if let Some(error) = failure {
            helper = None;
            outgoing = None;
            if let Ok(mut state) = shared.state.lock() {
                if state.pending.is_none() {
                    state.visible = false;
                    state.kind = None;
                }
            }
            let _ = errors.unbounded_send(error);
        }
        let Ok(state) = shared.state.lock() else {
            break;
        };
        if state.pending.is_none() && !state.shutdown {
            let _ = shared
                .changed
                .wait_timeout(state, Duration::from_millis(50));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selection_updates_are_bounded_coalesced_and_close_cancels_pending_open() {
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
        });
        let viewer = Preview {
            shared: shared.clone(),
        };
        for _ in 0..100 {
            viewer.show(
                (0..1000)
                    .map(|n| PathBuf::from(format!("C:\\{n}.txt")))
                    .collect(),
                999,
            );
        }
        {
            let state = shared.state.lock().unwrap();
            let Some(Control::Present(paths, selected, id)) = &state.pending else {
                panic!("missing presentation");
            };
            assert_eq!(paths.len(), protocol::MAX_PATHS);
            assert_eq!(paths[*selected], PathBuf::from("C:\\999.txt"));
            assert_eq!(*id, 100);
        }
        viewer.close();
        assert!(!viewer.is_visible());
        assert!(matches!(
            shared.state.lock().unwrap().pending,
            Some(Control::Close)
        ));
        drop(viewer);
        assert!(shared.state.lock().unwrap().shutdown);
    }
}

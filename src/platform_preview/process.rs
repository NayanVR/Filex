//! Bounded IPC and watchdogs. No native provider call runs in this process.
use super::protocol::{self, Event, Key, Rect, Request};
use anyhow::{Context, Result, bail, ensure};
use std::{
    os::windows::{ffi::OsStrExt, io::AsRawHandle, process::CommandExt},
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

/// What the embedded handler surface reports back to the GPUI viewer.
#[derive(Debug, Clone, PartialEq)]
pub enum PreviewEvent {
    /// The presentation `request_id` finished loading. `native`: a handler
    /// is showing; otherwise the file has no usable preview handler.
    Ready { native: bool, request_id: u64 },
    /// A viewer key pressed while the handler had keyboard focus.
    Key(Key),
    /// The helper failed, hung or crashed; it has been stopped.
    Error(String),
}
enum Control {
    Present {
        path: PathBuf,
        parent: isize,
        rect: Rect,
        background: u32,
        request_id: u64,
    },
    Hide,
    Close,
}
#[derive(Default)]
struct State {
    pending: Option<Control>,
    resize: Option<Rect>,
    return_focus: bool,
    shutdown: bool,
    request_id: u64,
}
impl State {
    /// Takes the next control and resize. A resize that arrived while a
    /// presentation was still pending is folded into that presentation.
    fn take(&mut self) -> (Option<Control>, Option<Rect>) {
        let mut control = self.pending.take();
        let mut resize = self.resize.take();
        if let (Some(Control::Present { rect, .. }), Some(latest)) = (control.as_mut(), resize) {
            *rect = latest;
            resize = None;
        }
        if matches!(control, Some(Control::Hide | Control::Close)) {
            resize = None;
        }
        (control.take(), resize)
    }
}
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}
/// The isolated preview-handler host behind one GPUI preview window.
/// Native state lives only in its disposable child process; this side only
/// queues commands, and never blocks the caller.
pub struct Preview {
    shared: Arc<Shared>,
}
impl Preview {
    pub fn new() -> Result<(
        Self,
        futures::channel::mpsc::UnboundedReceiver<PreviewEvent>,
    )> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
        });
        let worker = shared.clone();
        let (events, receiver) = futures::channel::mpsc::unbounded();
        std::thread::Builder::new()
            .name("preview-watchdog".into())
            .spawn(move || supervise(worker, events))?;
        Ok((Self { shared }, receiver))
    }
    fn send(&self, update: impl FnOnce(&mut State)) {
        if let Ok(mut state) = self.shared.state.lock() {
            update(&mut state);
            self.shared.changed.notify_one();
        }
    }
    /// Shows `path`'s preview handler in a child of `parent` (an HWND) at
    /// `rect`. Replaces any pending presentation; returns its request id.
    pub fn present(&self, path: PathBuf, parent: isize, rect: Rect, background: u32) -> u64 {
        let mut id = 0;
        self.send(|state| {
            state.request_id = state.request_id.wrapping_add(1);
            id = state.request_id;
            state.pending = Some(Control::Present {
                path,
                parent,
                rect,
                background,
                request_id: id,
            });
        });
        id
    }
    pub fn resize(&self, rect: Rect) {
        self.send(|state| state.resize = Some(rect));
    }
    /// Unloads the handler and hides its surface. The helper stays warm.
    pub fn hide(&self) {
        self.send(|state| state.pending = Some(Control::Hide));
    }
    /// Takes keyboard focus back from the handler, if it has it.
    pub fn return_focus(&self) {
        self.send(|state| state.return_focus = true);
    }
    /// Stops the helper, which destroys its surface.
    pub fn close(&self) {
        self.send(|state| state.pending = Some(Control::Close));
    }
}
impl Drop for Preview {
    fn drop(&mut self) {
        self.send(|state| state.shutdown = true);
    }
}
fn supervise(shared: Arc<Shared>, events: futures::channel::mpsc::UnboundedSender<PreviewEvent>) {
    let mut helper: Option<Helper> = None;
    let mut outgoing: Option<Request> = None;
    let mut outgoing_resize: Option<Rect> = None;
    let mut closing: Option<Instant> = None;
    let mut last_event = Instant::now();
    loop {
        let Ok(mut state) = shared.state.lock() else {
            break;
        };
        if state.shutdown {
            break;
        }
        let (control, resize) = state.take();
        let return_focus = std::mem::take(&mut state.return_focus);
        let current_request = state.request_id;
        drop(state);
        let mut failure = None;
        let mut finished = false;
        match control {
            Some(Control::Close) => {
                outgoing = helper.is_some().then_some(Request::Close);
                outgoing_resize = None;
                closing = helper.is_some().then(Instant::now);
            }
            Some(Control::Hide) => {
                if helper.is_some() && closing.is_none() {
                    outgoing = Some(Request::Hide);
                    outgoing_resize = None;
                }
            }
            Some(Control::Present {
                path,
                parent,
                rect,
                background,
                request_id,
            }) => {
                if closing.take().is_some() {
                    helper = None;
                }
                let request = Request::Present {
                    path: wide(&path),
                    parent,
                    rect,
                    background,
                    request_id,
                };
                outgoing_resize = None;
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
        if resize.is_some() && closing.is_none() {
            outgoing_resize = resize;
        }
        if let Some(child) = helper.as_mut() {
            if let Some(request) = outgoing.take() {
                match child.sender.try_send(request) {
                    Ok(()) => {}
                    Err(mpsc::TrySendError::Full(request)) => outgoing = Some(request),
                    Err(_) => failure = Some("Windows preview helper stopped".into()),
                }
            }
            // Best effort: a click during a busy pipe keeps the old focus.
            if return_focus && outgoing.is_none() && closing.is_none() {
                let _ = child.sender.try_send(Request::ReturnFocus);
            }
            // A resize never overtakes the presentation it applies to.
            if outgoing.is_none()
                && let Some(rect) = outgoing_resize.take()
            {
                match child.sender.try_send(Request::Resize(rect)) {
                    Ok(()) => {}
                    Err(mpsc::TrySendError::Full(_)) => outgoing_resize = Some(rect),
                    Err(_) => failure = Some("Windows preview helper stopped".into()),
                }
            }
            while let Ok(event) = child.receiver.try_recv() {
                match event {
                    Ok(Event::Heartbeat) => last_event = Instant::now(),
                    Ok(Event::Ready { kind, request_id }) => {
                        last_event = Instant::now();
                        // Stale completions from rapid selection changes are dropped.
                        if request_id == current_request && closing.is_none() {
                            let native = kind == "native";
                            let _ =
                                events.unbounded_send(PreviewEvent::Ready { native, request_id });
                        }
                    }
                    Ok(Event::Key(key)) => {
                        last_event = Instant::now();
                        if closing.is_none() {
                            let _ = events.unbounded_send(PreviewEvent::Key(key));
                        }
                    }
                    Ok(Event::Closed) => {
                        finished = true;
                        break;
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
                outgoing_resize = None;
                failure = None;
                closing = None;
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
            outgoing_resize = None;
            let _ = events.unbounded_send(PreviewEvent::Error(error));
        }
        let Ok(state) = shared.state.lock() else {
            break;
        };
        if state.pending.is_none()
            && state.resize.is_none()
            && !state.return_focus
            && !state.shutdown
        {
            let _ = shared
                .changed
                .wait_timeout(state, Duration::from_millis(50));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn shared() -> (Arc<Shared>, Preview) {
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
        });
        let preview = Preview {
            shared: shared.clone(),
        };
        (shared, preview)
    }
    fn rect(width: i32) -> Rect {
        Rect {
            x: 0,
            y: 48,
            width,
            height: 300,
        }
    }
    #[test]
    fn presentations_coalesce_and_close_cancels_a_pending_open() {
        let (shared, preview) = shared();
        for n in 0..100 {
            preview.present(PathBuf::from(format!("C:\\{n}.txt")), 7, rect(400), 0);
        }
        {
            let state = shared.state.lock().unwrap();
            let Some(Control::Present {
                path, request_id, ..
            }) = &state.pending
            else {
                panic!("missing presentation");
            };
            assert_eq!(path, &PathBuf::from("C:\\99.txt"));
            assert_eq!(*request_id, 100);
        }
        preview.close();
        assert!(matches!(
            shared.state.lock().unwrap().pending,
            Some(Control::Close)
        ));
        drop(preview);
        assert!(shared.state.lock().unwrap().shutdown);
    }
    #[test]
    fn a_resize_folds_into_its_pending_presentation() {
        let (shared, preview) = shared();
        preview.present(PathBuf::from("C:\\a.txt"), 7, rect(400), 0);
        preview.resize(rect(500));
        preview.resize(rect(600));
        let (control, resize) = shared.state.lock().unwrap().take();
        let Some(Control::Present { rect: folded, .. }) = control else {
            panic!("missing presentation");
        };
        assert_eq!(folded.width, 600);
        assert_eq!(resize, None);
        // Without a pending presentation the latest resize goes on its own.
        preview.resize(rect(700));
        let (control, resize) = shared.state.lock().unwrap().take();
        assert!(control.is_none());
        assert_eq!(resize.map(|r| r.width), Some(700));
        // Hiding discards geometry for a surface about to disappear.
        preview.resize(rect(800));
        preview.hide();
        let (control, resize) = shared.state.lock().unwrap().take();
        assert!(matches!(control, Some(Control::Hide)));
        assert_eq!(resize, None);
    }
}

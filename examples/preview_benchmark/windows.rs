//! Windows-only experimental adapters. All handles/COM objects stay in the
//! watchdog-controlled measurement process, not in Filex's production UI.
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::{
    mem::size_of,
    path::Path,
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::{
            Com::*,
            Ole::{IObjectWithSite, OleInitialize, OleUninitialize},
            ProcessStatus::*,
            Threading::*,
        },
        UI::{
            Shell::{PropertiesSystem::*, *},
            WindowsAndMessaging::*,
        },
    },
    core::{GUID, HRESULT, HSTRING, IUnknown, Interface, PCWSTR, PWSTR, implement},
};
struct Com;
impl Com {
    fn new() -> Result<Self> {
        unsafe {
            OleInitialize(None)?;
        }
        Ok(Self)
    }
}
impl Drop for Com {
    fn drop(&mut self) {
        unsafe {
            OleUninitialize();
        }
    }
}
struct Bitmap(HBITMAP);
impl Drop for Bitmap {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(self.0.into());
        }
    }
}
struct Dc(HDC);
impl Drop for Dc {
    fn drop(&mut self) {
        unsafe {
            ReleaseDC(None, self.0);
        }
    }
}
struct Host(HWND);
impl Drop for Host {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.0);
        }
    }
}

fn process_memory(handle: HANDLE) -> Result<Value> {
    unsafe {
        let mut counters = PROCESS_MEMORY_COUNTERS_EX {
            cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
            ..Default::default()
        };
        GetProcessMemoryInfo(
            handle,
            (&mut counters as *mut PROCESS_MEMORY_COUNTERS_EX).cast(),
            counters.cb,
        )?;
        let mut created = FILETIME::default();
        let mut exited = created;
        let mut kernel = created;
        let mut user = created;
        GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user)?;
        let ticks = |t: FILETIME| (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime);
        Ok(
            json!({"working_set_bytes": counters.WorkingSetSize, "peak_working_set_bytes": counters.PeakWorkingSetSize,
            "private_bytes": counters.PrivateUsage, "peak_commit_bytes": counters.PeakPagefileUsage,
            "cpu_ms": (ticks(kernel)+ticks(user)) as f64 / 10000.,
            "gdi_handles": GetGuiResources(handle, GR_GDIOBJECTS), "user_handles": GetGuiResources(handle, GR_USEROBJECTS)}),
        )
    }
}
pub fn memory() -> Value {
    process_memory(unsafe { GetCurrentProcess() })
        .unwrap_or_else(|e| json!({"error": e.to_string()}))
}

/// Copies a Shell HBITMAP into CPU BGRA, matching the production decoder's output.
fn pixels(bitmap: HBITMAP) -> Result<image::RgbaImage> {
    unsafe {
        let mut bm = BITMAP::default();
        anyhow::ensure!(
            GetObjectW(
                bitmap.into(),
                size_of::<BITMAP>() as i32,
                Some((&mut bm as *mut BITMAP).cast())
            ) != 0,
            "GetObjectW failed"
        );
        let (width, height) = (bm.bmWidth, bm.bmHeight.abs());
        anyhow::ensure!(
            width > 0 && height > 0 && width <= 8192 && height <= 8192,
            "invalid bitmap dimensions"
        );
        let mut info = BITMAPINFO::default();
        info.bmiHeader = BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        };
        let dc = Dc(GetDC(None));
        anyhow::ensure!(!dc.0.is_invalid(), "GetDC failed");
        let mut bytes = vec![0u8; width as usize * height as usize * 4];
        anyhow::ensure!(
            GetDIBits(
                dc.0,
                bitmap,
                0,
                height as u32,
                Some(bytes.as_mut_ptr().cast()),
                &mut info,
                DIB_RGB_COLORS
            ) == height,
            "GetDIBits failed"
        );
        // Some opaque DDB providers don't supply alpha at all.
        if bytes.chunks_exact(4).all(|p| p[3] == 0) {
            for p in bytes.chunks_exact_mut(4) {
                p[3] = 255;
            }
        }
        image::RgbaImage::from_raw(width as u32, height as u32, bytes)
            .context("invalid bitmap buffer")
    }
}
thread_local! { static THUMBNAIL_COM: std::cell::RefCell<Option<Com>> = const { std::cell::RefCell::new(None) }; }
pub fn init_thumbnail_thread() -> Result<()> {
    THUMBNAIL_COM.with(|state| {
        if state.borrow().is_none() {
            *state.borrow_mut() = Some(Com::new()?);
        }
        Ok(())
    })
}
pub fn thumbnail(path: &Path, cache_only: bool) -> Result<image::RgbaImage> {
    init_thumbnail_thread()?;
    thumbnail_in_apartment(path, cache_only)
}
fn thumbnail_in_apartment(path: &Path, cache_only: bool) -> Result<image::RgbaImage> {
    unsafe {
        let factory: IShellItemImageFactory =
            SHCreateItemFromParsingName(&HSTRING::from(path.as_os_str()), None)?;
        let flags = if cache_only {
            SIIGBF_THUMBNAILONLY | SIIGBF_INCACHEONLY
        } else {
            SIIGBF_THUMBNAILONLY
        };
        let bitmap = Bitmap(factory.GetImage(SIZE { cx: 128, cy: 128 }, flags)?);
        pixels(bitmap.0)
    }
}
/// A bounded pool of dedicated STA threads. GPUI's Windows background executor
/// uses threads whose COM apartment is already MTA; OleInitialize cannot change
/// that apartment. Only owned paths and pixel buffers cross this boundary.
pub struct ThumbnailWorkers {
    sender: std::sync::mpsc::SyncSender<ThumbnailJob>,
}
struct ThumbnailJob {
    path: std::path::PathBuf,
    result: futures::channel::oneshot::Sender<Result<image::RgbaImage>>,
}
impl ThumbnailWorkers {
    pub fn new(count: usize) -> Result<Self> {
        anyhow::ensure!(count > 0, "thumbnail worker count must be positive");
        let (sender, receiver) = std::sync::mpsc::sync_channel::<ThumbnailJob>(count);
        let receiver = std::sync::Arc::new(std::sync::Mutex::new(receiver));
        for index in 0..count {
            let receiver = receiver.clone();
            let (ready, initialized) = std::sync::mpsc::sync_channel(1);
            std::thread::Builder::new()
                .name(format!("shell-thumbnail-sta-{index}"))
                .spawn(move || {
                    // Explicitly owned on this OS thread, including final release.
                    let apartment = Com::new();
                    let _ =
                        ready.send(apartment.as_ref().map(|_| ()).map_err(|e| format!("{e:#}")));
                    let Ok(_apartment) = apartment else {
                        return;
                    };
                    loop {
                        pump_pending_messages();
                        // No COM call or decode occurs while the receiver is locked.
                        let job = match receiver.lock() {
                            Ok(receiver) => receiver.recv_timeout(Duration::from_millis(10)),
                            Err(_) => return,
                        };
                        match job {
                            Ok(job) => {
                                let _ = job.result.send(thumbnail_in_apartment(&job.path, false));
                            }
                            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                        }
                    }
                })?;
            initialized
                .recv()
                .context("Shell thumbnail thread stopped during initialization")?
                .map_err(anyhow::Error::msg)?;
        }
        Ok(Self { sender })
    }

    pub fn request(
        &self,
        path: std::path::PathBuf,
    ) -> futures::channel::oneshot::Receiver<Result<image::RgbaImage>> {
        let (result, receiver) = futures::channel::oneshot::channel();
        let job = ThumbnailJob { path, result };
        if let Err(error) = self.sender.try_send(job) {
            let (job, reason) = match error {
                std::sync::mpsc::TrySendError::Full(job) => (job, "Shell thumbnail queue is full"),
                std::sync::mpsc::TrySendError::Disconnected(job) => {
                    (job, "Shell thumbnail workers stopped")
                }
            };
            let _ = job.result.send(Err(anyhow::anyhow!(reason)));
        }
        receiver
    }
}

fn pump_pending_messages() {
    unsafe {
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

#[cfg(test)]
mod thumbnail_thread_tests {
    use super::*;

    #[test]
    fn shell_workers_decode_from_an_existing_mta_and_survive_a_bad_file() {
        // Match GPUI's Windows pool rather than the CLI worker's fresh STA.
        std::thread::spawn(|| {
            unsafe {
                CoInitializeEx(None, COINIT_MULTITHREADED).ok().unwrap();
            }
            struct Mta;
            impl Drop for Mta {
                fn drop(&mut self) {
                    unsafe {
                        CoUninitialize();
                    }
                }
            }
            let _mta = Mta;
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("fixture.png");
            image::RgbImage::from_pixel(256, 128, image::Rgb([40, 100, 220]))
                .save(&path)
                .unwrap();
            let error = thumbnail(&path, false).unwrap_err();
            assert_eq!(
                error.downcast_ref::<windows::core::Error>().unwrap().code(),
                RPC_E_CHANGED_MODE
            );
            let workers = ThumbnailWorkers::new(2).unwrap();
            let a = workers.request(path.clone());
            let b = workers.request(path.clone());
            let (a, b) = futures::executor::block_on(async { futures::join!(a, b) });
            for result in [a, b] {
                let image = result.unwrap().unwrap();
                assert_eq!(image.dimensions(), (128, 64));
                assert_eq!(&image.get_pixel(64, 32).0[..3], &[220, 100, 40]); // BGRA
            }
            assert!(
                futures::executor::block_on(workers.request(dir.path().join("missing.png")))
                    .unwrap()
                    .is_err()
            );
            assert!(
                futures::executor::block_on(workers.request(path))
                    .unwrap()
                    .is_ok()
            );
            // Calling the pool must not alter or uninitialize the caller's MTA.
            let mut apartment = APTTYPE::default();
            let mut qualifier = APTTYPEQUALIFIER::default();
            unsafe {
                CoGetApartmentType(&mut apartment, &mut qualifier).unwrap();
            }
            assert_eq!(apartment, APTTYPE_MTA);
        })
        .join()
        .unwrap();
    }
}

#[implement(IPreviewHandlerFrame)]
struct Frame;
impl IPreviewHandlerFrame_Impl for Frame_Impl {
    fn GetWindowContext(&self) -> windows::core::Result<PREVIEWHANDLERFRAMEINFO> {
        Ok(PREVIEWHANDLERFRAMEINFO::default())
    }
    fn TranslateAccelerator(&self, _: *const MSG) -> windows::core::Result<()> {
        Err(windows::core::Error::from_hresult(HRESULT(1))) // S_FALSE: host did not handle it.
    }
}
fn pump(duration: Duration) {
    let end = Instant::now() + duration;
    while Instant::now() < end {
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn handler_id(path: &Path) -> Result<GUID> {
    let ext = HSTRING::from(format!(
        ".{}",
        path.extension().context("no extension")?.to_string_lossy()
    ));
    let iid = HSTRING::from("{8895b1c6-b41f-4c1c-a562-0d564250836f}");
    let mut buf = vec![0u16; 256];
    let mut len = buf.len() as u32;
    unsafe {
        AssocQueryStringW(
            ASSOCF_INIT_DEFAULTTOSTAR,
            ASSOCSTR_SHELLEXTENSION,
            &ext,
            &iid,
            Some(PWSTR(buf.as_mut_ptr())),
            &mut len,
        )
        .ok()?;
        CLSIDFromString(PCWSTR(buf.as_ptr())).map_err(Into::into)
    }
}
fn save_window(hwnd: HWND, path: &Path) -> Result<()> {
    unsafe {
        let mut r = RECT::default();
        GetClientRect(hwnd, &mut r)?;
        let source = GetDC(Some(hwnd));
        anyhow::ensure!(!source.is_invalid(), "window DC unavailable");
        let target = CreateCompatibleDC(Some(source));
        let bitmap = Bitmap(CreateCompatibleBitmap(source, r.right, r.bottom));
        let old = SelectObject(target, bitmap.0.into());
        // Capture the actual client pixels after pumping. Not proof of semantic correctness.
        let copied = BitBlt(target, 0, 0, r.right, r.bottom, Some(source), 0, 0, SRCCOPY);
        SelectObject(target, old);
        let _ = DeleteDC(target);
        ReleaseDC(Some(hwnd), source);
        copied?;
        let mut image = pixels(bitmap.0)?;
        for pixel in image.pixels_mut() {
            pixel.0.swap(0, 2);
            pixel.0[3] = 255;
        }
        image.save(path)?;
        Ok(())
    }
}
/// Exercises a registered preview handler via the documented out-of-process
/// COM server model, with a real parent HWND and message pump. API completion
/// and child-window appearance are measured separately; neither means first paint.
pub fn preview(path: &Path, output: &Path) -> Result<()> {
    let _com = Com::new()?;
    let clsid = match handler_id(path) {
        Ok(id) => id,
        Err(error) => {
            return super::write_json(
                output,
                &json!({"status":"unsupported","reason":"no associated preview handler","error":format!("{error:#}")}),
            );
        }
    };
    let before = memory();
    unsafe {
        let host = Host(CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            windows::core::w!("STATIC"),
            windows::core::w!("Filex preview benchmark"),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            40,
            40,
            900,
            700,
            None,
            None,
            None,
            None,
        )?);
        let mut cycles = Vec::new();
        for cycle in 0..10 {
            let start = Instant::now();
            // Do not load a third-party preview DLL in-process.
            let handler: IPreviewHandler = CoCreateInstance(&clsid, None, CLSCTX_LOCAL_SERVER)?;
            let site: IPreviewHandlerFrame = Frame.into();
            let object_site = handler.cast::<IObjectWithSite>()?;
            object_site.SetSite(&site)?;
            let init = if let Ok(init) = handler.cast::<IInitializeWithStream>() {
                let stream = SHCreateStreamOnFileEx(
                    &HSTRING::from(path.as_os_str()),
                    (STGM_READ | STGM_SHARE_DENY_NONE).0,
                    0,
                    false,
                    None,
                )?;
                init.Initialize(&stream, STGM_READ.0)?;
                "stream"
            } else if let Ok(init) = handler.cast::<IInitializeWithItem>() {
                let item: IShellItem =
                    SHCreateItemFromParsingName(&HSTRING::from(path.as_os_str()), None)?;
                init.Initialize(&item, STGM_READ.0)?;
                "item"
            } else {
                handler
                    .cast::<IInitializeWithFile>()?
                    .Initialize(&HSTRING::from(path.as_os_str()), STGM_READ.0)?;
                "file"
            };
            let mut rect = RECT {
                left: 0,
                top: 0,
                right: 840,
                bottom: 620,
            };
            handler.SetWindow(host.0, &rect)?;
            handler.DoPreview()?;
            let open_ms = start.elapsed().as_secs_f64() * 1000.;
            let mut child = None;
            let wait = Instant::now();
            while wait.elapsed() < Duration::from_secs(2) {
                pump(Duration::from_millis(20));
                if let Ok(hwnd) = GetWindow(host.0, GW_CHILD) {
                    child = Some(hwnd);
                    break;
                }
            }
            let child_window_ms = child.map(|_| start.elapsed().as_secs_f64() * 1000.);
            pump(Duration::from_millis(300));
            let mut handler_memory = Value::Null;
            let mut pid = 0;
            if let Some(child) = child {
                GetWindowThreadProcessId(child, Some(&mut pid));
                if let Ok(process) =
                    OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, false, pid)
                {
                    handler_memory =
                        process_memory(process).unwrap_or_else(|e| json!({"error":e.to_string()}));
                    let _ = CloseHandle(process);
                }
            }
            let screenshot = if cycle == 0 {
                Some(
                    save_window(host.0, &output.with_extension("png"))
                        .map(|_| "saved".to_owned())
                        .unwrap_or_else(|e| e.to_string()),
                )
            } else {
                None
            };
            let resize = Instant::now();
            rect.right = 640;
            rect.bottom = 480;
            handler.SetRect(&rect)?;
            let resize_ms = resize.elapsed().as_secs_f64() * 1000.;
            pump(Duration::from_millis(50));
            let close = Instant::now();
            handler.Unload()?;
            object_site.SetSite(None::<&IUnknown>)?;
            drop(object_site);
            drop(handler);
            drop(site);
            let unload_ms = close.elapsed().as_secs_f64() * 1000.;
            pump(Duration::from_millis(50));
            cycles.push(json!({"cycle":cycle,"initialization":init,"open_api_ms":open_ms,"child_window_ms":child_window_ms,
                "resize_api_ms":resize_ms,"unload_and_release_ms":unload_ms,
                "child_remaining":GetWindow(host.0,GW_CHILD).is_ok(),"handler_pid":pid,"handler_memory":handler_memory,
                "host_memory":memory(),"screenshot":screenshot}));
            // Checkpoint survives a later crash/hang; parent keeps the failure status.
            super::write_json(
                output,
                &json!({"status":"running","handler":format!("{clsid:?}"),"cycles":cycles,"memory_before":before}),
            )?;
        }
        let all_windows = cycles
            .iter()
            .all(|v| v["child_window_ms"].is_number() && v["child_remaining"] == false);
        drop(host);
        super::write_json(
            output,
            &json!({"status":if all_windows {"ok"}else{"partial"},"handler":format!("{clsid:?}"),
            "cycles":cycles,"memory_before":before,"memory_after":memory(),
            "timing_scope":"API completion and child HWND creation; screenshot after fixed settle, not verified first paint"}),
        )
    }
}

pub fn capture_grid(path: &Path) -> Result<()> {
    let hwnd = unsafe { FindWindowW(None, windows::core::w!("Filex scrolling benchmark"))? };
    save_window(hwnd, path)
}

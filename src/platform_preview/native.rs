//! All COM, HWND and GDI ownership stays on this helper's main STA thread.
use super::protocol::{self, Event, Request};
use anyhow::{Context, Result};
use std::{
    cell::Cell,
    mem::size_of,
    os::windows::ffi::OsStringExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::{
            Com::*,
            Ole::{IObjectWithSite, OleInitialize, OleUninitialize},
        },
        UI::{
            Shell::{PropertiesSystem::*, *},
            WindowsAndMessaging::*,
        },
    },
    core::{GUID, HRESULT, HSTRING, IUnknown, Interface, PCWSTR, PWSTR, implement, w},
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
            width > 0 && height > 0 && width <= 128 && height <= 128,
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
#[implement(IPreviewHandlerFrame)]
struct Frame;
impl IPreviewHandlerFrame_Impl for Frame_Impl {
    fn GetWindowContext(&self) -> windows::core::Result<PREVIEWHANDLERFRAMEINFO> {
        Ok(PREVIEWHANDLERFRAMEINFO::default())
    }
    fn TranslateAccelerator(&self, message: *const MSG) -> windows::core::Result<()> {
        if let Some(message) = unsafe { message.as_ref() } {
            if message.message == WM_KEYDOWN && key_action(message.wParam.0, true) {
                return Ok(());
            }
        }
        Err(windows::core::Error::from_hresult(HRESULT(1))) // S_FALSE: host did not handle it.
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
const CLOSE: u32 = 1;
const NEXT: u32 = 2;
const PREVIOUS: u32 = 4;
const OPEN: u32 = 8;
const PAINT: u32 = 16;
thread_local! { static ACTIONS: Cell<u32> = const { Cell::new(0) }; }
fn action(flag: u32) {
    ACTIONS.with(|a| a.set(a.get() | flag));
}
fn key_action(key: usize, multiple: bool) -> bool {
    let flag = match key {
        0x1b | 0x20 => CLOSE,
        0x25 if multiple => PREVIOUS,
        0x27 if multiple => NEXT,
        _ => return false,
    };
    action(flag);
    true
}
unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // No borrowed Rust window state across a native call: COM and window APIs
    // may synchronously reenter this procedure.
    match message {
        WM_CLOSE => {
            action(CLOSE);
            LRESULT(0)
        }
        WM_COMMAND => {
            match wparam.0 & 0xffff {
                101 => action(PREVIOUS),
                102 => action(NEXT),
                103 => action(OPEN),
                _ => {}
            }
            LRESULT(0)
        }
        WM_SIZE | WM_PAINT => {
            action(PAINT);
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}
struct Handler(IPreviewHandler);
impl Drop for Handler {
    fn drop(&mut self) {
        unsafe {
            let _ = self.0.Unload();
            if let Ok(site) = self.0.cast::<IObjectWithSite>() {
                let _ = site.SetSite(None::<&IUnknown>);
            }
        }
    }
}
fn load_handler(path: &Path, hwnd: HWND, rect: &RECT) -> Result<Handler> {
    unsafe {
        let handler = Handler(CoCreateInstance(
            &handler_id(path)?,
            None,
            CLSCTX_LOCAL_SERVER,
        )?);
        let frame: IPreviewHandlerFrame = Frame.into();
        handler.0.cast::<IObjectWithSite>()?.SetSite(&frame)?;
        if let Ok(init) = handler.0.cast::<IInitializeWithStream>() {
            let stream = SHCreateStreamOnFileEx(
                &HSTRING::from(path.as_os_str()),
                (STGM_READ | STGM_SHARE_DENY_NONE).0,
                0,
                false,
                None,
            )?;
            init.Initialize(&stream, STGM_READ.0)?;
        } else if let Ok(init) = handler.0.cast::<IInitializeWithItem>() {
            let item: IShellItem =
                SHCreateItemFromParsingName(&HSTRING::from(path.as_os_str()), None)?;
            init.Initialize(&item, STGM_READ.0)?;
        } else {
            handler
                .0
                .cast::<IInitializeWithFile>()?
                .Initialize(&HSTRING::from(path.as_os_str()), STGM_READ.0)?;
        }
        handler.0.SetWindow(hwnd, rect)?;
        handler.0.DoPreview()?;
        Ok(handler)
    }
}
fn image_fallback(path: &Path) -> Result<image::RgbaImage> {
    let metadata = std::fs::metadata(path)?;
    anyhow::ensure!(
        metadata.is_file() && metadata.len() <= 64 * 1024 * 1024,
        "image exceeds preview size limit"
    );
    let mut reader = image::ImageReader::open(path)?.with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(128 * 1024 * 1024);
    limits.max_image_width = Some(32768);
    limits.max_image_height = Some(32768);
    reader.limits(limits);
    let mut image = reader.decode()?.thumbnail(2048, 2048).to_rgba8();
    // Composite transparency onto the native window background; GDI BI_RGB
    // ignores alpha. No opaque black boxes around transparent source images.
    for pixel in image.chunks_exact_mut(4) {
        let alpha = u32::from(pixel[3]);
        for channel in &mut pixel[..3] {
            *channel = ((u32::from(*channel) * alpha + 245 * (255 - alpha)) / 255) as u8;
        }
        pixel.swap(0, 2);
        pixel[3] = 255;
    }
    Ok(image)
}
enum Content {
    Native(Handler),
    Image(image::RgbaImage),
    Missing(Host),
}
struct Viewer {
    content: Option<Content>,
    previous: Host,
    next: Host,
    _open: Host,
    host: Host,
    paths: Vec<PathBuf>,
    selected: usize,
    request_id: u64,
    rect: RECT,
}
impl Viewer {
    fn new(owner: isize) -> Result<Self> {
        unsafe {
            let instance: HINSTANCE =
                windows::Win32::System::LibraryLoader::GetModuleHandleW(None)?.into();
            let class = WNDCLASSW {
                hInstance: instance,
                lpfnWndProc: Some(window_proc),
                lpszClassName: w!("FilexIsolatedPreview"),
                hCursor: LoadCursorW(None, IDC_ARROW)?,
                hbrBackground: HBRUSH((COLOR_WINDOW.0 + 1) as *mut _),
                ..Default::default()
            };
            anyhow::ensure!(
                RegisterClassW(&class) != 0,
                "registering preview window failed"
            );
            let owner = if owner == 0 {
                None
            } else {
                Some(HWND(owner as *mut _))
            };
            let host = Host(CreateWindowExW(
                WS_EX_TOOLWINDOW,
                w!("FilexIsolatedPreview"),
                w!("Filex Preview"),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE | WS_CLIPCHILDREN,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                960,
                760,
                owner,
                None,
                None,
                None,
            )?);
            let button = |label: PCWSTR, x, width, id: usize| -> Result<Host> {
                Ok(Host(CreateWindowExW(
                    WINDOW_EX_STYLE::default(),
                    w!("BUTTON"),
                    label,
                    WS_CHILD | WS_VISIBLE | WS_TABSTOP,
                    x,
                    8,
                    width,
                    30,
                    Some(host.0),
                    Some(HMENU(id as *mut _)),
                    None,
                    None,
                )?))
            };
            let previous = button(w!("Previous"), 12, 85, 101)?;
            let next = button(w!("Next"), 105, 85, 102)?;
            let open = button(w!("Open in default app"), 200, 180, 103)?;
            Ok(Self {
                content: None,
                host,
                previous,
                next,
                _open: open,
                paths: Vec::new(),
                selected: 0,
                request_id: 0,
                rect: RECT::default(),
            })
        }
    }
    fn bounds(&self) -> Result<RECT> {
        let mut rect = RECT::default();
        unsafe {
            GetClientRect(self.host.0, &mut rect)?;
        }
        rect.top = 48;
        rect.right = rect.right.max(1);
        rect.bottom = rect.bottom.max(49);
        Ok(rect)
    }
    fn present(&mut self, paths: Vec<Vec<u16>>, selected: usize, request_id: u64) -> Result<()> {
        self.request_id = request_id;
        let paths: Vec<PathBuf> = paths
            .into_iter()
            .map(|p| std::ffi::OsString::from_wide(&p).into())
            .collect();
        if self.paths == paths && self.selected == selected {
            return self.ready();
        }
        self.paths = paths;
        self.selected = selected;
        self.load()
    }
    fn load(&mut self) -> Result<()> {
        self.content = None; // Unload before replacing, on the same STA.
        let rect = self.bounds()?;
        let path = &self.paths[self.selected];
        let name = path
            .file_name()
            .unwrap_or(path.as_os_str())
            .to_string_lossy();
        let title = HSTRING::from(format!(
            "{name} — Filex Preview ({}/{})",
            self.selected + 1,
            self.paths.len()
        ));
        unsafe {
            SetWindowTextW(self.host.0, &title)?;
            let _ = windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow(
                self.previous.0,
                self.selected > 0,
            );
            let _ = windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow(
                self.next.0,
                self.selected + 1 < self.paths.len(),
            );
        }
        self.content = Some(if super::policy::rust_image(path) {
            match image_fallback(path) {
                Ok(image) => Content::Image(image),
                Err(_) => self.missing(path, &rect)?,
            }
        } else {
            match load_handler(path, self.host.0, &rect) {
                Ok(handler) => Content::Native(handler),
                Err(_) => match image_fallback(path) {
                    Ok(image) => Content::Image(image),
                    Err(_) => self.missing(path, &rect)?,
                },
            }
        });
        self.rect = rect;
        self.draw()?;
        self.ready()
    }
    fn missing(&self, path: &Path, rect: &RECT) -> Result<Content> {
        let metadata = std::fs::metadata(path).ok();
        let detail = metadata
            .map(|m| {
                if m.is_dir() {
                    "Folder".into()
                } else {
                    format!("{} bytes", m.len())
                }
            })
            .unwrap_or_else(|| "File is unavailable".into());
        let text = HSTRING::from(format!(
            "No preview available\r\n\r\n{}\r\n\r\n{detail}\r\n\r\nUse Open in default app to view this file.",
            path.file_name()
                .unwrap_or(path.as_os_str())
                .to_string_lossy()
        ));
        unsafe {
            Ok(Content::Missing(Host(CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("STATIC"),
                &text,
                WS_CHILD
                    | WS_VISIBLE
                    | WINDOW_STYLE(windows::Win32::System::SystemServices::SS_CENTER.0),
                20,
                rect.top + 30,
                (rect.right - 40).max(1),
                (rect.bottom - rect.top - 60).max(1),
                Some(self.host.0),
                None,
                None,
                None,
            )?)))
        }
    }
    fn ready(&self) -> Result<()> {
        let kind = match self.content {
            Some(Content::Native(_)) => "native",
            Some(Content::Image(_)) => "image",
            _ => "unavailable",
        };
        emit(&Event::Ready {
            kind: kind.into(),
            request_id: self.request_id,
        })
    }
    fn draw(&mut self) -> Result<()> {
        let rect = self.bounds()?;
        let resized = rect != self.rect;
        self.rect = rect;
        match self.content.as_ref() {
            Some(Content::Native(handler)) => {
                if resized {
                    unsafe {
                        handler.0.SetRect(&rect)?;
                    }
                }
            }
            Some(Content::Missing(text)) => unsafe {
                MoveWindow(
                    text.0,
                    20,
                    rect.top + 30,
                    (rect.right - 40).max(1),
                    (rect.bottom - rect.top - 60).max(1),
                    true,
                )?;
            },
            Some(Content::Image(image)) => unsafe {
                let dc = GetDC(Some(self.host.0));
                if dc.is_invalid() {
                    return Ok(());
                }
                let brush = CreateSolidBrush(COLORREF(0x00f5f5f5));
                FillRect(dc, &rect, brush);
                let _ = DeleteObject(brush.into());
                let available_w = (rect.right - 24).max(1);
                let available_h = (rect.bottom - rect.top - 24).max(1);
                let scale = (available_w as f64 / image.width() as f64)
                    .min(available_h as f64 / image.height() as f64)
                    .min(1.0);
                let width = (image.width() as f64 * scale).max(1.0) as i32;
                let height = (image.height() as f64 * scale).max(1.0) as i32;
                let info = BITMAPINFO {
                    bmiHeader: BITMAPINFOHEADER {
                        biSize: size_of::<BITMAPINFOHEADER>() as u32,
                        biWidth: image.width() as i32,
                        biHeight: -(image.height() as i32),
                        biPlanes: 1,
                        biBitCount: 32,
                        biCompression: BI_RGB.0,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                SetStretchBltMode(dc, HALFTONE);
                StretchDIBits(
                    dc,
                    (rect.right - width) / 2,
                    rect.top + (rect.bottom - rect.top - height) / 2,
                    width,
                    height,
                    0,
                    0,
                    image.width() as i32,
                    image.height() as i32,
                    Some(image.as_ptr().cast()),
                    &info,
                    DIB_RGB_COLORS,
                    SRCCOPY,
                );
                ReleaseDC(Some(self.host.0), dc);
            },
            None => {}
        }
        Ok(())
    }
    fn actions(&mut self, flags: u32) -> Result<bool> {
        if flags & CLOSE != 0 {
            return Ok(false);
        }
        if flags & PREVIOUS != 0 && self.selected > 0 {
            self.selected -= 1;
            self.load()?;
        }
        if flags & NEXT != 0 && self.selected + 1 < self.paths.len() {
            self.selected += 1;
            self.load()?;
        }
        if flags & OPEN != 0 {
            if let Some(path) = self.paths.get(self.selected) {
                use std::os::windows::ffi::OsStrExt;
                emit(&Event::Open(path.as_os_str().encode_wide().collect()))?;
            }
        }
        if flags & PAINT != 0 {
            self.draw()?;
        }
        Ok(true)
    }
}
fn emit(event: &Event) -> Result<()> {
    protocol::write(&mut std::io::stdout().lock(), event)
}
#[derive(Default)]
struct Incoming {
    latest: Option<Request>,
    stopped: bool,
}
pub fn run(mode: &str) -> Result<()> {
    let first: Request = protocol::read(&mut std::io::stdin().lock())?;
    first.validate()?; // parent has attached the job before this point
    let _com = Com::new()?;
    let incoming = Arc::new(Mutex::new(Incoming {
        latest: Some(first),
        stopped: false,
    }));
    let reader = incoming.clone();
    std::thread::Builder::new()
        .name("preview-command-reader".into())
        .spawn(move || {
            loop {
                let request: Result<Request> = protocol::read(&mut std::io::stdin().lock());
                let Ok(mut state) = reader.lock() else {
                    return;
                };
                match request {
                    Ok(request) if request.validate().is_ok() => state.latest = Some(request),
                    _ => {
                        state.stopped = true;
                        return;
                    }
                }
            }
        })?;
    let mut viewer: Option<Viewer> = None;
    let mut heartbeat = Instant::now();
    loop {
        let (request, stopped) = {
            let mut state = incoming
                .lock()
                .map_err(|_| anyhow::anyhow!("preview command reader failed"))?;
            (state.latest.take(), state.stopped)
        };
        if stopped {
            break;
        }
        match request {
            Some(Request::Close) => break,
            Some(Request::Thumbnail(path)) if mode == "--filex-thumbnail-helper" => {
                let path = PathBuf::from(std::ffi::OsString::from_wide(&path));
                let result = thumbnail_in_apartment(&path, false);
                let event = match result {
                    Ok(image) => Event::Pixels {
                        width: image.width(),
                        height: image.height(),
                        bgra: image.into_raw(),
                    },
                    Err(error) => Event::Error(format!("{error:#}")),
                };
                emit(&event)?;
            }
            Some(Request::Present {
                paths,
                selected,
                owner,
                request_id,
            }) if mode == "--filex-preview-helper" => {
                if viewer.is_none() {
                    viewer = Some(Viewer::new(owner)?);
                }
                if let Some(viewer) = viewer.as_mut() {
                    viewer.present(paths, selected, request_id)?;
                }
            }
            Some(_) => anyhow::bail!("wrong preview helper mode"),
            None => {}
        }
        unsafe {
            let mut message = MSG::default();
            while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                if message.message == WM_KEYDOWN
                    && key_action(
                        message.wParam.0,
                        viewer.as_ref().is_some_and(|v| v.paths.len() > 1),
                    )
                {
                    continue;
                }
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        if let Some(viewer) = viewer.as_mut() {
            let flags = ACTIONS.with(|a| a.replace(0));
            if !viewer.actions(flags)? {
                break;
            }
            if heartbeat.elapsed() >= Duration::from_millis(500) {
                emit(&Event::Heartbeat)?;
                heartbeat = Instant::now();
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(viewer); // unload native content before reporting a normal close
    if mode == "--filex-preview-helper" {
        emit(&Event::Closed)?;
    }
    Ok(())
}

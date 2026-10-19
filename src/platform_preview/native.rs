//! Helper-process side: Shell thumbnails, and preview handlers hosted in a
//! surface embedded in Filex's GPUI preview window. All COM, HWND and GDI
//! ownership stays on this helper's main STA thread.
use super::protocol::{self, Event, Key, Rect, Request};
use anyhow::{Context, Result};
use std::{
    cell::{Cell, RefCell},
    mem::size_of,
    os::windows::ffi::OsStringExt,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex},
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
            Input::KeyboardAndMouse::{GetFocus, SetFocus},
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
            if message.message == WM_KEYDOWN && queue_key(message.wParam.0) {
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
thread_local! {
    /// Viewer keys a handler forwarded, drained after each message pump.
    static KEYS: RefCell<Vec<Key>> = const { RefCell::new(Vec::new()) };
    /// Surface fill (0x00BBGGRR), painted until the handler covers it.
    static BACKGROUND: Cell<u32> = const { Cell::new(0) };
}
/// Viewer keys belong to Filex even while a handler has focus.
fn viewer_key(key: usize) -> Option<Key> {
    match key {
        0x1b | 0x20 => Some(Key::Close),
        0x25 => Some(Key::Previous),
        0x27 => Some(Key::Next),
        _ => None,
    }
}
fn queue_key(key: usize) -> bool {
    let Some(key) = viewer_key(key) else {
        return false;
    };
    KEYS.with(|keys| keys.borrow_mut().push(key));
    true
}
unsafe extern "system" fn surface_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // No borrowed Rust window state across a native call: COM and window APIs
    // may synchronously reenter this procedure.
    match message {
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => unsafe {
            let mut paint = PAINTSTRUCT::default();
            let dc = BeginPaint(hwnd, &mut paint);
            let brush = CreateSolidBrush(COLORREF(BACKGROUND.with(Cell::get)));
            FillRect(dc, &paint.rcPaint, brush);
            let _ = DeleteObject(brush.into());
            let _ = EndPaint(hwnd, &paint);
            LRESULT(0)
        },
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
/// Activates the extension's registered `IPreviewHandler` out of process
/// (`CLSCTX_LOCAL_SERVER`, so its DLL never loads here) and points it at
/// `hwnd`. Assumes the calling thread is an OLE STA that pumps messages:
/// handlers call back through `IPreviewHandlerFrame` on this thread.
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
fn client(rect: &Rect) -> RECT {
    RECT {
        left: 0,
        top: 0,
        right: rect.width,
        bottom: rect.height,
    }
}
/// The handler's window: a child of Filex's GPUI preview window, owned by
/// this helper's thread.
///
/// Assumes the parent is a DirectComposition window created with
/// `WS_EX_NOREDIRECTIONBITMAP` and a topmost composition target (gpui-ce's
/// DirectX renderer). Such a parent has no GDI surface for ordinary
/// children to draw into, so the surface is `WS_EX_LAYERED` (child
/// layering, Windows 8+), which gives it its own. GPUI composites above it
/// and leaves that region transparent to reveal it. The parent lives in
/// another process, so Windows attaches the two threads' input queues; the
/// supervisor kills a helper that stops pumping messages.
///
/// Layered *child* windows also need the creating executable to declare
/// Windows 8+ in its manifest. `filex.exe` (which is also the helper) gets
/// that from gpui-ce's embedded `gpui.manifest.xml`; any other executable
/// that runs the helper must link gpui too (see `windows_preview_smoke`).
struct Surface {
    // Field order is drop order: unload the handler before its window goes.
    handler: Option<Handler>,
    window: Host,
    parent: isize,
    rect: Rect,
}
impl Surface {
    fn new(parent: isize, rect: Rect) -> Result<Self> {
        unsafe {
            let instance: HINSTANCE =
                windows::Win32::System::LibraryLoader::GetModuleHandleW(None)?.into();
            let class = WNDCLASSW {
                hInstance: instance,
                lpfnWndProc: Some(surface_proc),
                lpszClassName: w!("FilexPreviewSurface"),
                hCursor: LoadCursorW(None, IDC_ARROW)?,
                ..Default::default()
            };
            // Fails harmlessly when an earlier surface registered it.
            RegisterClassW(&class);
            let window = Host(
                CreateWindowExW(
                    WS_EX_LAYERED,
                    w!("FilexPreviewSurface"),
                    None,
                    WS_CHILD | WS_CLIPCHILDREN,
                    rect.x,
                    rect.y,
                    rect.width,
                    rect.height,
                    Some(HWND(parent as *mut _)),
                    None,
                    Some(instance),
                    None,
                )
                // Fails (with a stale last error) in an executable without a
                // Windows 8+ `supportedOS` manifest; see `Surface`.
                .context("creating preview surface")?,
            );
            SetLayeredWindowAttributes(window.0, COLORREF(0), 255, LWA_ALPHA)
                .context("layering preview surface")?;
            Ok(Self {
                handler: None,
                window,
                parent,
                rect,
            })
        }
    }
    fn resize(&mut self, rect: Rect) -> Result<()> {
        if rect == self.rect {
            return Ok(());
        }
        self.rect = rect;
        unsafe {
            MoveWindow(self.window.0, rect.x, rect.y, rect.width, rect.height, true)
                .context("moving preview surface")?;
            if let Some(handler) = &self.handler {
                handler
                    .0
                    .SetRect(&client(&rect))
                    .context("resizing preview handler")?;
            }
        }
        Ok(())
    }
    /// Replaces the handler; `false` when the file has no usable one.
    fn load(&mut self, path: &Path) -> bool {
        self.hide(); // Unload before replacing, on the same STA.
        unsafe {
            let _ = ShowWindow(self.window.0, SW_SHOWNOACTIVATE);
            let _ = InvalidateRect(Some(self.window.0), None, true);
        }
        match load_handler(path, self.window.0, &client(&self.rect)) {
            Ok(handler) => {
                self.handler = Some(handler);
                true
            }
            Err(_) => {
                // Hidden, so it can't take clicks meant for Filex's fallback.
                self.hide();
                false
            }
        }
    }
    /// Hands keyboard focus back to the parent when the handler holds it.
    /// The parent belongs to another process; this works because a child
    /// window shares its parent's input state, and runs here rather than in
    /// Filex so a hung handler stalls this helper, not the UI thread.
    fn return_focus(&self) {
        unsafe {
            let focus = GetFocus();
            if focus == self.window.0 || IsChild(self.window.0, focus).as_bool() {
                let _ = SetFocus(Some(HWND(self.parent as *mut _)));
            }
        }
    }
    fn hide(&mut self) {
        self.handler = None;
        unsafe {
            let _ = ShowWindow(self.window.0, SW_HIDE);
        }
    }
}
fn emit(event: &Event) -> Result<()> {
    protocol::write(&mut std::io::stdout().lock(), event)
}
fn present(
    surface: &mut Option<Surface>,
    path: Vec<u16>,
    parent: isize,
    rect: Rect,
    background: u32,
    request_id: u64,
) -> Result<()> {
    if surface.as_ref().is_some_and(|s| s.parent != parent) {
        *surface = None;
    }
    if surface.is_none() {
        *surface = Some(Surface::new(parent, rect)?);
    }
    let surface = surface.as_mut().context("preview surface missing")?;
    BACKGROUND.with(|b| b.set(background));
    surface.resize(rect)?;
    let path = PathBuf::from(std::ffi::OsString::from_wide(&path));
    // Handlers that open by name (PDF viewers) fail on `\\?\` paths.
    let path = crate::listing::plain_path(&path);
    let kind = if surface.load(&path) {
        "native"
    } else {
        "unavailable"
    };
    emit(&Event::Ready {
        kind: kind.into(),
        request_id,
    })
}
#[derive(Default)]
struct Incoming {
    latest: Option<Request>,
    resize: Option<Rect>,
    stopped: bool,
}
pub fn run(mode: &str) -> Result<()> {
    let first: Request = protocol::read(&mut std::io::stdin().lock())?;
    first.validate()?; // parent has attached the job before this point
    let _com = Com::new()?;
    let incoming = Arc::new((
        Mutex::new(Incoming {
            latest: Some(first),
            ..Default::default()
        }),
        Condvar::new(),
    ));
    let reader = incoming.clone();
    std::thread::Builder::new()
        .name("preview-command-reader".into())
        .spawn(move || {
            loop {
                let request: Result<Request> = protocol::read(&mut std::io::stdin().lock());
                let Ok(mut state) = reader.0.lock() else {
                    return;
                };
                match request {
                    // Resizes coalesce on their own, so a window drag can't
                    // replace a pending presentation, or the reverse.
                    Ok(Request::Resize(rect)) if rect.validate().is_ok() => {
                        state.resize = Some(rect)
                    }
                    Ok(request) if request.validate().is_ok() => state.latest = Some(request),
                    _ => {
                        state.stopped = true;
                        reader.1.notify_one();
                        return;
                    }
                }
                reader.1.notify_one();
            }
        })?;
    let mut surface: Option<Surface> = None;
    let mut heartbeat = Instant::now();
    loop {
        let (request, resize, stopped) = {
            let mut state = incoming
                .0
                .lock()
                .map_err(|_| anyhow::anyhow!("preview command reader failed"))?;
            (state.latest.take(), state.resize.take(), state.stopped)
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
                path,
                parent,
                rect,
                background,
                request_id,
            }) if mode == "--filex-preview-helper" => {
                present(&mut surface, path, parent, rect, background, request_id)?;
            }
            Some(Request::ReturnFocus) if mode == "--filex-preview-helper" => {
                if let Some(surface) = surface.as_ref() {
                    surface.return_focus();
                }
            }
            Some(Request::Hide) if mode == "--filex-preview-helper" => {
                if let Some(surface) = surface.as_mut() {
                    surface.hide();
                }
            }
            Some(_) => anyhow::bail!("wrong preview helper mode"),
            None => {}
        }
        if let (Some(surface), Some(rect)) = (surface.as_mut(), resize) {
            surface.resize(rect)?;
        }
        unsafe {
            let mut message = MSG::default();
            while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                if message.message == WM_KEYDOWN && queue_key(message.wParam.0) {
                    continue;
                }
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        for key in KEYS.with(|keys| std::mem::take(&mut *keys.borrow_mut())) {
            emit(&Event::Key(key))?;
        }
        if mode == "--filex-preview-helper" && heartbeat.elapsed() >= Duration::from_millis(500) {
            emit(&Event::Heartbeat)?;
            heartbeat = Instant::now();
        }
        // Wake immediately for a new file; polling used to add up to 10 ms
        // to every warm thumbnail request. Idle thumbnail workers need only
        // occasional message pumping, while a surface services resize input.
        let state = incoming
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("preview command reader failed"))?;
        if state.latest.is_none() && state.resize.is_none() && !state.stopped {
            let interval = if surface.is_some() { 16 } else { 250 };
            let _ = incoming
                .1
                .wait_timeout(state, Duration::from_millis(interval));
        }
    }
    drop(surface); // unload native content before reporting a normal close
    if mode == "--filex-preview-helper" {
        emit(&Event::Closed)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn viewer_keys_are_escape_space_and_horizontal_arrows_only() {
        assert_eq!(viewer_key(0x1b), Some(Key::Close));
        assert_eq!(viewer_key(0x20), Some(Key::Close));
        assert_eq!(viewer_key(0x25), Some(Key::Previous));
        assert_eq!(viewer_key(0x27), Some(Key::Next));
        // Up/Down and letters stay with the handler (scrolling, find).
        assert_eq!(viewer_key(0x26), None);
        assert_eq!(viewer_key(0x28), None);
        assert_eq!(viewer_key(0x41), None);
    }
    #[test]
    fn handler_rect_is_zero_origin_at_surface_size() {
        let rect = Rect {
            x: 10,
            y: 60,
            width: 300,
            height: 200,
        };
        let client = client(&rect);
        assert_eq!((client.left, client.top), (0, 0));
        assert_eq!((client.right, client.bottom), (300, 200));
    }
}

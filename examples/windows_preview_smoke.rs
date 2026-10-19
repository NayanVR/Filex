//! Live checks of the production IPC, watchdog and native host on Windows.
//! Run in an interactive Windows session. No third-party preview handlers needed.
#[cfg(not(windows))]
fn main() {
    println!("Windows preview smoke requires Windows");
}
#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    windows::run()
}
#[cfg(windows)]
mod windows {
    use ::windows::{
        Win32::{
            Foundation::*, Graphics::Gdi::*, System::LibraryLoader::GetModuleHandleW,
            UI::WindowsAndMessaging::*,
        },
        core::w,
    };
    use anyhow::{Result, ensure};
    use filex::platform_preview::{self, Key, Preview, PreviewEvent, Rect};
    use futures::channel::mpsc::UnboundedReceiver;
    use std::{
        io::Write,
        path::PathBuf,
        time::{Duration, Instant},
    };
    /// Parent fill; anything else outside the surface means a handler drew
    /// where it shouldn't.
    const PARENT_FILL: u32 = 0x00_30_60_90;
    const RECT: Rect = Rect {
        x: 0,
        y: 48,
        width: 640,
        height: 400,
    };
    fn pump() {
        unsafe {
            let mut message = MSG::default();
            while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }
    fn wait(mut condition: impl FnMut() -> bool) -> Result<()> {
        let start = Instant::now();
        while !condition() {
            ensure!(
                start.elapsed() < Duration::from_secs(13),
                "preview condition timed out"
            );
            pump();
            std::thread::sleep(Duration::from_millis(25));
        }
        Ok(())
    }
    fn next_event(events: &mut UnboundedReceiver<PreviewEvent>) -> Result<PreviewEvent> {
        let mut event = None;
        wait(|| {
            event = events.try_recv().ok();
            event.is_some()
        })?;
        Ok(event.unwrap_or(PreviewEvent::Error("unreachable".into())))
    }
    /// The next event must be request `id` finishing; a stale completion
    /// means coalescing or request matching broke.
    fn ready(events: &mut UnboundedReceiver<PreviewEvent>, id: u64, native: bool) -> Result<()> {
        match next_event(events)? {
            PreviewEvent::Ready {
                native: shown,
                request_id,
            } if request_id == id => {
                ensure!(
                    shown == native,
                    "request {id}: native={shown}, want {native}"
                );
                Ok(())
            }
            PreviewEvent::Ready { request_id, .. } => {
                anyhow::bail!("stale request {request_id} reported while waiting for {id}")
            }
            other => anyhow::bail!("unexpected event {other:?}"),
        }
    }
    unsafe extern "system" fn parent_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        unsafe {
            if message == WM_PAINT {
                let mut paint = PAINTSTRUCT::default();
                let dc = BeginPaint(hwnd, &mut paint);
                let brush = CreateSolidBrush(COLORREF(PARENT_FILL));
                FillRect(dc, &paint.rcPaint, brush);
                let _ = DeleteObject(brush.into());
                let _ = EndPaint(hwnd, &paint);
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, message, wparam, lparam)
        }
    }
    /// Stands in for the GPUI preview window: a visible top-level window.
    fn parent() -> Result<HWND> {
        unsafe {
            let instance: HINSTANCE = GetModuleHandleW(None)?.into();
            let class = WNDCLASSW {
                hInstance: instance,
                lpfnWndProc: Some(parent_proc),
                lpszClassName: w!("FilexPreviewSmokeParent"),
                ..Default::default()
            };
            RegisterClassW(&class);
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("FilexPreviewSmokeParent"),
                w!("Filex preview smoke"),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE | WS_CLIPCHILDREN,
                80,
                80,
                760,
                560,
                None,
                None,
                Some(instance),
                None,
            )?;
            pump();
            Ok(hwnd)
        }
    }
    fn surface(parent: HWND) -> Option<HWND> {
        unsafe { FindWindowExW(Some(parent), None, w!("FilexPreviewSurface"), None).ok() }
    }
    /// Screen pixels of `parent`'s client area, as composed by DWM.
    fn capture(parent: HWND, name: &str) -> Result<image::RgbaImage> {
        unsafe {
            let mut rect = RECT::default();
            GetClientRect(parent, &mut rect)?;
            let mut origin = POINT::default();
            let _ = ClientToScreen(parent, &mut origin);
            let screen = GetDC(None);
            let target = CreateCompatibleDC(Some(screen));
            let bitmap = CreateCompatibleBitmap(screen, rect.right, rect.bottom);
            let old = SelectObject(target, bitmap.into());
            let copied = BitBlt(
                target,
                0,
                0,
                rect.right,
                rect.bottom,
                Some(screen),
                origin.x,
                origin.y,
                SRCCOPY,
            );
            SelectObject(target, old);
            let mut pixels = vec![0; (rect.right * rect.bottom * 4) as usize];
            let mut info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: rect.right,
                    biHeight: -rect.bottom,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let rows = GetDIBits(
                target,
                bitmap,
                0,
                rect.bottom as u32,
                Some(pixels.as_mut_ptr().cast()),
                &mut info,
                DIB_RGB_COLORS,
            );
            let _ = DeleteObject(bitmap.into());
            let _ = DeleteDC(target);
            ReleaseDC(None, screen);
            copied?;
            ensure!(rows == rect.bottom, "preview screenshot failed");
            for pixel in pixels.chunks_exact_mut(4) {
                pixel.swap(0, 2);
                pixel[3] = 255;
            }
            let image = image::RgbaImage::from_raw(rect.right as u32, rect.bottom as u32, pixels)
                .ok_or_else(|| anyhow::anyhow!("invalid screenshot dimensions"))?;
            if let Some(output) = std::env::var_os("FILEX_PREVIEW_SMOKE_OUTPUT") {
                std::fs::create_dir_all(&output)?;
                image.save(PathBuf::from(output).join(format!("{name}.png")))?;
            }
            Ok(image)
        }
    }
    fn parent_fill() -> [u8; 4] {
        let [r, g, b, _] = PARENT_FILL.to_le_bytes();
        [r, g, b, 255]
    }
    fn fixture() -> Result<(tempfile::TempDir, PathBuf)> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("unicode café 測試.png");
        image::RgbaImage::from_pixel(256, 128, image::Rgba([40, 100, 220, 255])).save(&path)?;
        Ok((dir, path))
    }
    fn fault_test(path: PathBuf) -> Result<()> {
        let start = Instant::now();
        ensure!(
            platform_preview::thumbnail(&path).is_err(),
            "faulty thumbnail helper accepted"
        );
        ensure!(
            start.elapsed() < Duration::from_secs(13),
            "thumbnail watchdog missed deadline"
        );
        let host = parent()?;
        let (viewer, mut events) = Preview::new()?;
        viewer.present(path, host.0 as isize, RECT, 0);
        ensure!(
            matches!(next_event(&mut events)?, PreviewEvent::Error(_)),
            "faulty preview helper was not reported"
        );
        println!(
            "fault recovery passed: {}",
            std::env::var("FILEX_PREVIEW_SMOKE_FAULT")?
        );
        Ok(())
    }
    fn thumbnails(source: PathBuf) -> Result<()> {
        // Calling from MTA mirrors the GPUI Windows thread pool.
        std::thread::spawn(move || -> Result<()> {
            use ::windows::Win32::System::Com::*;
            unsafe {
                CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
            }
            let result = (|| -> Result<()> {
                let image = platform_preview::thumbnail(&source)?;
                ensure!(image.dimensions() == (128, 64), "native dimensions wrong");
                ensure!(
                    &image.get_pixel(64, 32).0 == &[220, 100, 40, 255],
                    "native pixels wrong"
                );
                ensure!(
                    platform_preview::thumbnail(&source.with_extension("missing")).is_err(),
                    "missing file accepted"
                );
                ensure!(
                    platform_preview::thumbnail(&source)?.dimensions() == (128, 64),
                    "worker did not survive bad file"
                );
                Ok(())
            })();
            unsafe {
                CoUninitialize();
            }
            result
        })
        .join()
        .map_err(|_| anyhow::anyhow!("MTA caller panicked"))?
    }
    pub fn run() -> Result<()> {
        // This executable is also the helper. Like filex.exe, it must carry
        // gpui-ce's Windows 10 manifest, without which layered child windows
        // (the handler surface) can't be created. Referencing GPUI's
        // platform code is what links that manifest resource in.
        if std::env::var_os("FILEX_PREVIEW_SMOKE_LINK_GPUI").is_some() {
            let _ = gpui_platform::application();
        }
        let arg = std::env::args().nth(1).unwrap_or_default();
        // Fault injection exists only in this smoke executable, never in Filex.
        if arg.starts_with("--filex-") {
            if let Ok(fault) = std::env::var("FILEX_PREVIEW_SMOKE_FAULT") {
                match fault.as_str() {
                    "crash" => std::process::exit(7),
                    "bad-frame" => {
                        std::io::stdout().write_all(&u32::MAX.to_le_bytes())?;
                        std::io::stdout().flush()?;
                    }
                    "hang" => {}
                    _ => unreachable!(),
                }
                loop {
                    std::thread::park();
                }
            }
        }
        if platform_preview::dispatch_helper() {
            return Ok(());
        }
        if arg == "--fault-test" {
            return fault_test(fixture()?.1);
        }
        let (dir, path) = fixture()?;
        thumbnails(path)?;
        let unknown = dir.path().join("unhandled.filex-test-format");
        std::fs::write(&unknown, b"No registered handler")?;
        let text = dir.path().join("native.txt");
        std::fs::write(
            &text,
            "Filex native Windows preview smoke\r\nRegistered text handler\r\n",
        )?;
        let host = parent()?;
        let parent_id = host.0 as isize;
        let (viewer, mut events) = Preview::new()?;
        for round in 0..3 {
            // Rapid selection changes coalesce; only the last one reports.
            let mut last = 0;
            for _ in 0..100 {
                last = viewer.present(unknown.clone(), parent_id, RECT, 0x00ffffff);
            }
            ready(&mut events, last, false)?;
            if let Some(surface) = surface(host) {
                ensure!(
                    unsafe { !IsWindowVisible(surface).as_bool() },
                    "surface stayed visible without a handler; it would eat fallback clicks"
                );
            }
            let id = viewer.present(text.clone(), parent_id, RECT, 0x00ffffff);
            ready(&mut events, id, true)?;
            std::thread::sleep(Duration::from_millis(400));
            pump();
            let shot = capture(host, &format!("native-text-{round}"))?;
            // A provider once covered the toolbar despite a successful
            // SetWindow/DoPreview. Above the surface stays the parent's.
            ensure!(
                shot.rows()
                    .take(RECT.y as usize)
                    .flatten()
                    .all(|p| p.0 == parent_fill()),
                "native handler drew outside its surface"
            );
            ensure!(
                shot.rows()
                    .skip(RECT.y as usize)
                    .take(RECT.height as usize)
                    .flat_map(|row| row.take(RECT.width as usize))
                    .any(|p| p.0 != parent_fill()),
                "native text handler surface not visible"
            );
            // Resize moves the surface; the handler follows.
            let resized = Rect {
                width: 500,
                height: 300,
                ..RECT
            };
            viewer.resize(resized);
            let surface_window = surface(host).ok_or_else(|| anyhow::anyhow!("no surface"))?;
            wait(|| unsafe {
                let mut rect = RECT::default();
                GetClientRect(surface_window, &mut rect).is_ok()
                    && (rect.right, rect.bottom) == (resized.width, resized.height)
            })?;
            // Viewer keys reach Filex even while the handler side has them.
            for (key, want) in [(0x27, Key::Next), (0x25, Key::Previous), (0x1b, Key::Close)] {
                unsafe {
                    PostMessageW(Some(surface_window), WM_KEYDOWN, WPARAM(key), LPARAM(0))?;
                }
                ensure!(
                    next_event(&mut events)? == PreviewEvent::Key(want),
                    "key {key:#x} was not forwarded"
                );
            }
            viewer.hide();
            wait(|| unsafe { !IsWindowVisible(surface_window).as_bool() })?;
            viewer.close();
            wait(|| surface(host).is_none())?;
        }
        // Reopen after close, then drop: the helper and its surface go.
        let id = viewer.present(text, parent_id, RECT, 0x00ffffff);
        ready(&mut events, id, true)?;
        drop(viewer);
        wait(|| surface(host).is_none())?;
        ensure!(
            events.try_recv().is_err(),
            "unexpected native preview event"
        );
        for fault in ["crash", "bad-frame", "hang"] {
            let status = std::process::Command::new(std::env::current_exe()?)
                .arg("--fault-test")
                .env("FILEX_PREVIEW_SMOKE_FAULT", fault)
                .status()?;
            ensure!(status.success(), "fault test failed: {fault}");
        }
        unsafe {
            let _ = DestroyWindow(host);
        }
        println!(
            "Production Windows preview smoke passed: MTA thumbnails, BGRA, bad files, embedded native text, unavailable fallback, surface bounds, resize, coalescing, forwarded keys, hide/close/reopen, cleanup, crashes, malformed IPC, timeouts."
        );
        Ok(())
    }
}

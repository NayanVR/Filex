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
        Win32::{Foundation::*, UI::WindowsAndMessaging::*},
        core::w,
    };
    use anyhow::{Result, ensure};
    use filex::platform_preview::{self, Preview};
    use std::{
        io::Write,
        path::PathBuf,
        time::{Duration, Instant},
    };
    fn wait(mut condition: impl FnMut() -> bool) -> Result<()> {
        let start = Instant::now();
        while !condition() {
            ensure!(
                start.elapsed() < Duration::from_secs(13),
                "preview condition timed out"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
        Ok(())
    }
    fn ready(preview: &Preview, kind: &str) -> Result<()> {
        wait(|| preview.ready_kind().as_deref() == Some(kind))
    }
    fn capture(hwnd: HWND, name: &str) -> Result<image::RgbaImage> {
        use ::windows::Win32::Graphics::Gdi::*;
        unsafe {
            let mut rect = RECT::default();
            GetClientRect(hwnd, &mut rect)?;
            let source = GetDC(Some(hwnd));
            let target = CreateCompatibleDC(Some(source));
            let bitmap = CreateCompatibleBitmap(source, rect.right, rect.bottom);
            let old = SelectObject(target, bitmap.into());
            let copied = BitBlt(
                target,
                0,
                0,
                rect.right,
                rect.bottom,
                Some(source),
                0,
                0,
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
                source,
                bitmap,
                0,
                rect.bottom as u32,
                Some(pixels.as_mut_ptr().cast()),
                &mut info,
                DIB_RGB_COLORS,
            );
            let _ = DeleteObject(bitmap.into());
            let _ = DeleteDC(target);
            ReleaseDC(Some(hwnd), source);
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
    fn fixture() -> Result<(tempfile::TempDir, PathBuf)> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("unicode café 測試.png");
        image::RgbaImage::from_pixel(256, 128, image::Rgba([40, 100, 220, 255])).save(&path)?;
        Ok((dir, path))
    }
    pub fn run() -> Result<()> {
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
            let (_dir, path) = fixture()?;
            let start = Instant::now();
            ensure!(
                platform_preview::thumbnail(&path).is_err(),
                "faulty thumbnail helper accepted"
            );
            ensure!(
                start.elapsed() < Duration::from_secs(13),
                "thumbnail watchdog missed deadline"
            );
            let (viewer, mut errors) = Preview::new(0)?;
            viewer.show(vec![path], 0);
            let mut reported = false;
            wait(|| {
                reported |= errors.try_recv().is_ok();
                !viewer.is_visible() && reported
            })?;
            println!(
                "fault recovery passed: {}",
                std::env::var("FILEX_PREVIEW_SMOKE_FAULT")?
            );
            return Ok(());
        }
        let (dir, path) = fixture()?;
        // Calling from MTA mirrors the GPUI Windows thread pool.
        let source = path.clone();
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
        .map_err(|_| anyhow::anyhow!("MTA caller panicked"))??;
        let unknown = dir.path().join("unhandled.filex-test-format");
        std::fs::write(&unknown, b"No registered handler")?;
        let text = dir.path().join("native.txt");
        std::fs::write(
            &text,
            "Filex native Windows preview smoke\r\nRegistered text handler\r\n",
        )?;
        let (viewer, mut errors) = Preview::new(0)?;
        for _ in 0..3 {
            viewer.show(vec![path.clone()], 0);
            ready(&viewer, "image")?;
            let hwnd = unsafe { FindWindowW(w!("FilexIsolatedPreview"), None)? };
            unsafe {
                MoveWindow(hwnd, 80, 80, 700, 500, true)?;
            }
            std::thread::sleep(Duration::from_millis(150));
            let rendered = capture(hwnd, "image-preview")?;
            let center = rendered.get_pixel(rendered.width() / 2, rendered.height() / 2);
            ensure!(
                center.0 == [40, 100, 220, 255],
                "image fallback did not paint expected pixels: {center:?}"
            );
            for _ in 0..100 {
                viewer.show(vec![unknown.clone()], 0);
            }
            ready(&viewer, "unavailable")?;
            viewer.show(vec![text.clone()], 0);
            ready(&viewer, "native")?;
            std::thread::sleep(Duration::from_millis(400));
            let native = capture(hwnd, "native-text-preview")?;
            // A provider once shifted/covered the toolbar despite successful
            // SetWindow/DoPreview. Keep native content below its own HWND.
            ensure!(
                native.rows().take(48).eq(rendered.rows().take(48)),
                "native handler altered or covered the preview toolbar"
            );
            viewer.close();
            wait(|| unsafe { FindWindowW(w!("FilexIsolatedPreview"), None).is_err() })?;
        }
        // Multi-selection keyboard navigation, native close, and another reopen.
        viewer.show(vec![path.clone(), unknown.clone()], 0);
        ready(&viewer, "image")?;
        let hwnd = unsafe { FindWindowW(w!("FilexIsolatedPreview"), None)? };
        unsafe {
            PostMessageW(Some(hwnd), WM_KEYDOWN, WPARAM(0x27), LPARAM(0))?;
        }
        ready(&viewer, "unavailable")?;
        unsafe {
            PostMessageW(Some(hwnd), WM_KEYDOWN, WPARAM(0x1b), LPARAM(0))?;
        }
        wait(|| !viewer.is_visible())?;
        viewer.show(vec![path], 0);
        ready(&viewer, "image")?;
        drop(viewer);
        wait(|| unsafe { FindWindowW(w!("FilexIsolatedPreview"), None).is_err() })?;
        ensure!(
            errors.try_recv().is_err(),
            "unexpected native preview error"
        );
        for fault in ["crash", "bad-frame", "hang"] {
            let status = std::process::Command::new(std::env::current_exe()?)
                .arg("--fault-test")
                .env("FILEX_PREVIEW_SMOKE_FAULT", fault)
                .status()?;
            ensure!(status.success(), "fault test failed: {fault}");
        }
        println!(
            "Production Windows preview smoke passed: MTA thumbnails, BGRA, bad files, native text, image fallback, unavailable fallback, resize, coalescing, keyboard navigation, close/reopen, cleanup, crashes, malformed IPC, timeouts."
        );
        Ok(())
    }
}

//! Opt-in measurement executable. Never linked into the shipped application.
//! Orchestrated by scripts/preview_benchmark.py; each case is a separate process.
#![allow(dead_code)] // Reuse production components without changing their visibility.
#[path = "preview_benchmark/grid.rs"]
mod grid;
#[cfg(windows)]
#[path = "preview_benchmark/windows.rs"]
mod native;
#[path = "../src/thumbnails.rs"]
mod thumbnails;
#[path = "../src/ui/mod.rs"]
mod ui;

use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::{path::Path, sync::Arc, time::Instant};

fn write_json(path: &Path, value: &Value) -> Result<()> {
    std::fs::write(path, serde_json::to_vec_pretty(value)?)?;
    Ok(())
}
fn memory() -> Value {
    #[cfg(windows)]
    {
        native::memory()
    }
    #[cfg(not(windows))]
    {
        json!({"status": "unavailable", "reason": "Windows memory counters only"})
    }
}
fn decode(backend: &str, path: &Path, cache_only: bool) -> Result<Arc<gpui::RenderImage>> {
    if backend == "filex" {
        return thumbnails::decode_thumbnail(path);
    }
    #[cfg(windows)]
    {
        let pixels = native::thumbnail(path, cache_only)?;
        Ok(Arc::new(gpui::RenderImage::new(vec![image::Frame::new(
            pixels,
        )])))
    }
    #[cfg(not(windows))]
    {
        let _ = cache_only;
        anyhow::bail!("Windows Shell is unavailable")
    }
}
fn save_thumbnail(image: &gpui::RenderImage, path: &Path) -> Result<()> {
    let size = image.size(0);
    let mut bytes = image.as_bytes(0).context("no pixels")?.to_vec();
    for pixel in bytes.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    image::save_buffer(
        path,
        &bytes,
        size.width.0 as u32,
        size.height.0 as u32,
        image::ColorType::Rgba8,
    )?;
    Ok(())
}
fn measure_decode(backend: &str, path: &Path, mode: &str, output: &Path) -> Result<()> {
    // COM is initialized in each Shell request, equally in thumbnail and GUI cases.
    #[cfg(windows)]
    if backend == "shell" {
        native::init_thumbnail_thread()?;
    }
    let baseline = memory();
    let mut times = Vec::new();
    let mut errors = Vec::new();
    let mut result = None;
    let mut cache = thumbnails::Cache::default();
    let mut prime = None;
    if mode != "first" {
        match decode(backend, path, false) {
            Ok(image) => {
                prime = Some("ok");
                if backend == "filex" && mode == "cache" {
                    cache.start(path);
                    cache.finish(path, Ok(image));
                }
            }
            Err(_) => prime = Some("failed"),
        }
    }
    for i in 0..20 {
        // Copy BEFORE timing. New path/file identity, NOT a flushed disk cache.
        let copy = path.with_file_name(format!(
            "fresh-{i}-{}",
            path.file_name().unwrap().to_string_lossy()
        ));
        let source = if mode == "first" {
            std::fs::copy(path, &copy)?;
            &copy
        } else {
            path
        };
        let start = Instant::now();
        let decoded = if mode == "cache" && backend == "filex" {
            match cache.get(path) {
                Some(thumbnails::ThumbnailState::Ready(image)) => Ok(image.clone()),
                _ => Err(anyhow::anyhow!("cache miss")),
            }
        } else {
            decode(backend, source, mode == "cache")
        };
        let ms = start.elapsed().as_secs_f64() * 1000.;
        match decoded {
            Ok(image) => {
                times.push(ms);
                result = Some(image);
            }
            Err(error) => errors.push(json!({"ms": ms, "error": format!("{error:#}")})),
        }
        if mode == "first" {
            std::fs::remove_file(copy)?;
        }
    }
    let after = memory(); // Excludes PNG evidence encoding.
    let mut dimensions = Value::Null;
    if let Some(image) = result {
        dimensions = json!([image.size(0).width.0, image.size(0).height.0]);
        save_thumbnail(&image, &output.with_extension("png"))?;
    }
    write_json(
        output,
        &json!({"status": if times.is_empty() {"unavailable"} else if errors.is_empty() {"ok"} else {"partial"},
        "backend": backend, "mode": mode, "samples_ms": times, "errors": errors, "prime": prime,
        "dimensions": dimensions, "memory_before": baseline, "memory_after": after}),
    )
}
fn fixtures(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let rgb = image::RgbImage::from_fn(1600, 1200, |x, y| {
        image::Rgb([
            ((x / 7 + y / 13) % 256) as u8,
            ((x / 11) % 256) as u8,
            ((y / 5) % 256) as u8,
        ])
    });
    for ext in ["jpg", "png", "webp", "bmp", "gif", "tiff"] {
        rgb.save(dir.join(format!("landscape.{ext}")))?;
    }
    image::imageops::rotate90(&rgb).save(dir.join("portrait.jpg"))?;
    image::RgbaImage::from_fn(600, 400, |x, y| {
        image::Rgba([240, 40, 30, ((x + y) % 256) as u8])
    })
    .save(dir.join("alpha.png"))?;
    image::RgbImage::from_fn(6000, 4000, |x, y| {
        image::Rgb([(x % 256) as u8, (y % 256) as u8, 80])
    })
    .save(dir.join("large-24mp.jpg"))?;
    image::GrayImage::new(32769, 1).save(dir.join("dimension-limit.png"))?;
    std::fs::write(dir.join("corrupt.png"), b"not an image")?;
    std::fs::File::create(dir.join("size-limit.png"))?.set_len(64 * 1024 * 1024 + 1)?;
    Ok(())
}
fn run() -> Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    match args.first().map(String::as_str) {
        Some("fixtures") => fixtures(Path::new(args.get(1).context("fixture directory")?)),
        Some("decode") if args.len() == 5 => {
            measure_decode(&args[1], Path::new(&args[2]), &args[3], Path::new(&args[4]))
        }
        Some("scroll") if args.len() == 4 => grid::run(
            args[1].clone(),
            args[2].clone().into(),
            args[3].clone().into(),
        ),
        #[cfg(windows)]
        Some("preview") if args.len() == 3 => {
            native::preview(Path::new(&args[1]), Path::new(&args[2])).or_else(|error| {
                let output = Path::new(&args[2]);
                let mut checkpoint = std::fs::read(output)
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                    .unwrap_or_else(|| json!({}));
                checkpoint["status"] = json!("handler_error");
                checkpoint["error"] = json!(format!("{error:#}"));
                write_json(output, &checkpoint)
            })
        }
        _ => anyhow::bail!(
            "fixtures DIR | decode filex|shell FILE first|repeat|cache OUTPUT | scroll icons|filex|shell DIR OUTPUT | preview FILE OUTPUT"
        ),
    }
}
fn main() -> Result<()> {
    run()
}

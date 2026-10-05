//! Admission overhead and decoder limits versus the previous unbounded path.
//! Uses generated fixtures; no personal files or OS preview caches involved.
#[path = "../src/thumbnails.rs"]
#[allow(dead_code)] // cargo bench also compiles the module's unit-test helpers
mod thumbnails;

use criterion::{Criterion, criterion_group, criterion_main};
use std::{hint::black_box, path::Path, sync::Arc};

fn previous_decode(path: &Path) -> Arc<gpui::RenderImage> {
    let decoded = image::open(path)
        .unwrap()
        .thumbnail(thumbnails::THUMBNAIL_EDGE, thumbnails::THUMBNAIL_EDGE);
    let mut rgba = decoded.to_rgba8();
    for pixel in rgba.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    Arc::new(gpui::RenderImage::new(vec![image::Frame::new(rgba)]))
}

fn bench_thumbnails(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fixture.jpg");
    image::RgbImage::from_fn(1600, 1200, |x, y| {
        image::Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8])
    })
    .save(&path)
    .unwrap();
    let mut group = c.benchmark_group("thumbnails");
    group.sample_size(20);
    group.warm_up_time(std::time::Duration::from_secs(1));
    group.measurement_time(std::time::Duration::from_secs(2));
    group.bench_function("previous_decode", |b| {
        b.iter(|| previous_decode(black_box(&path)))
    });
    group.bench_function("bounded_decode", |b| {
        b.iter(|| thumbnails::decode_thumbnail(black_box(&path)).unwrap())
    });
    let mut cache = thumbnails::Cache::default();
    cache.start(Path::new("a"));
    cache.start(Path::new("b"));
    group.bench_function("busy_admission", |b| {
        b.iter(|| cache.start(black_box(Path::new("visible"))))
    });
    cache.finish(Path::new("a"), thumbnails::decode_thumbnail(&path));
    // Exercise the exact production cache lookup and Ready payload too.
    group.bench_function("ready_lookup", |b| {
        b.iter(|| {
            if let Some(thumbnails::ThumbnailState::Ready(image)) =
                cache.get(black_box(Path::new("a")))
            {
                black_box(image);
            }
        })
    });
    group.finish();
}

criterion_group!(benches, bench_thumbnails);
criterion_main!(benches);

//! Lazy thumbnail decoding for image files in the list.
//!
//! Thumbnails are requested only for rows the virtualized list actually
//! renders (i.e. visible ones), decoded on the background executor, and
//! delivered back as gpui `RenderImage`s. A row scrolled past before its
//! request fires costs nothing. At most two decodes per workspace run at once;
//! there is no backlog of offscreen files. Completion triggers a render, where
//! currently visible rows can claim the next slot. Decodes already in flight
//! finish and warm the bounded cache. Large/corrupt images keep their file icon.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result};
use gpui::RenderImage;

/// Longest edge of a generated thumbnail, in physical pixels. Sized for
/// the largest grid card (block 4); the list view downscales it, which
/// stays crisp, whereas upscaling a tiny thumbnail into a card would not.
pub const THUMBNAIL_EDGE: u32 = 128;

/// At capacity, completed entries are cleared; active jobs keep their slots.
pub const CACHE_CAP: usize = 512;
pub const MAX_IN_FLIGHT: usize = 2;
const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_DECODE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_DIMENSION: u32 = 32_768;

#[derive(Clone)]
pub enum ThumbnailState {
    Loading,
    Ready(Arc<RenderImage>),
    Failed,
}

/// Owns admission as well as storage, so rendering cannot accidentally launch
/// unlimited decodes or evict the bookkeeping for an active request.
#[derive(Default)]
pub struct Cache {
    entries: HashMap<PathBuf, ThumbnailState>,
    in_flight: usize,
}

impl Cache {
    pub fn get(&self, path: &Path) -> Option<&ThumbnailState> {
        self.entries.get(path)
    }

    pub fn start(&mut self, path: &Path) -> bool {
        if self.entries.contains_key(path) || self.in_flight >= MAX_IN_FLIGHT {
            return false;
        }
        if self.entries.len() >= CACHE_CAP {
            // Preserve every active request. Old completions cannot repopulate
            // a cleared cache past its cap or start a duplicate decode.
            self.entries
                .retain(|_, state| matches!(state, ThumbnailState::Loading));
        }
        self.entries
            .insert(path.to_path_buf(), ThumbnailState::Loading);
        self.in_flight += 1;
        true
    }

    pub fn finish(&mut self, path: &Path, decoded: Result<Arc<RenderImage>>) {
        let Some(state @ ThumbnailState::Loading) = self.entries.get_mut(path) else {
            return; // unknown or already completed request
        };
        *state = match decoded {
            Ok(image) => ThumbnailState::Ready(image),
            Err(_) => ThumbnailState::Failed,
        };
        self.in_flight = self.in_flight.saturating_sub(1);
    }
}

/// Decode `path` and downscale to a thumbnail. Blocking — call on the
/// background executor.
pub fn decode_thumbnail(path: &Path) -> Result<Arc<RenderImage>> {
    // Only the isolated, synchronous decoder is recoverable. No UI state,
    // shared mutation, native callbacks, or app-wide panic suppression here.
    decoder_result(|| decode_image(path))
}

/// Production routing. Pure Rust decoding remains separately measurable.
pub fn load_thumbnail(path: &Path) -> Result<Arc<RenderImage>> {
    #[cfg(windows)]
    {
        use filex::platform_preview::{policy, thumbnail};
        let dimensions = if matches!(policy::extension(path).as_str(), "jpg" | "jpeg") {
            image::image_dimensions(path).ok()
        } else {
            None
        };
        if policy::shell_first(path, dimensions) {
            match thumbnail(path) {
                Ok(pixels) => {
                    return Ok(Arc::new(RenderImage::new(vec![image::Frame::new(pixels)])));
                }
                Err(error) if !policy::rust_image(path) => return Err(error),
                Err(_) => {} // A failed native large-image request can use Rust's limits.
            }
        }
    }
    decode_thumbnail(path)
}

fn decoder_result<T>(decode: impl FnOnce() -> Result<T> + std::panic::UnwindSafe) -> Result<T> {
    std::panic::catch_unwind(decode).map_err(|_| anyhow::anyhow!("thumbnail decoder panicked"))?
}

fn decode_image(path: &Path) -> Result<Arc<RenderImage>> {
    let metadata = std::fs::metadata(path)?;
    anyhow::ensure!(metadata.is_file(), "thumbnail source is not a regular file");
    anyhow::ensure!(
        metadata.len() <= MAX_FILE_BYTES,
        "thumbnail source exceeds size limit"
    );
    let mut reader = image::ImageReader::open(path)?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    // ImageReader checks output-buffer size before allocation. Codec-internal
    // allocations are best-effort limited, not an OS-enforced memory sandbox.
    limits.max_alloc = Some(MAX_DECODE_BYTES);
    reader.limits(limits);
    let decoded = reader
        .decode()
        .with_context(|| format!("decoding {}", path.display()))?
        .thumbnail(THUMBNAIL_EDGE, THUMBNAIL_EDGE);
    let mut rgba = decoded.to_rgba8();
    // gpui's RenderImage is BGRA in an RGBA container: swap channels.
    for pixel in rgba.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    let frame = image::Frame::new(rgba);
    Ok(Arc::new(RenderImage::new(vec![frame])))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failed() -> Result<Arc<RenderImage>> {
        Err(anyhow::anyhow!("injected decode failure"))
    }

    #[test]
    fn admission_caps_work_without_queueing_or_duplicating_paths() {
        let mut cache = Cache::default();
        assert!(cache.start(Path::new("a")));
        assert!(!cache.start(Path::new("a")));
        assert!(cache.start(Path::new("b")));
        for i in 0..10_000 {
            assert!(!cache.start(Path::new(&i.to_string())));
        }
        assert_eq!(cache.entries.len(), MAX_IN_FLIGHT);
        cache.finish(Path::new("a"), failed());
        assert!(cache.start(Path::new("now-visible")));
        assert!(!cache.start(Path::new("a")), "failed files do not loop");
    }

    #[test]
    fn eviction_preserves_active_requests_and_completion_stays_bounded() {
        let mut cache = Cache::default();
        let slow = Path::new("slow");
        cache.start(slow);
        for i in 0..CACHE_CAP * 3 {
            let path = PathBuf::from(i.to_string());
            assert!(cache.start(&path));
            cache.finish(&path, failed());
            assert!(cache.entries.len() <= CACHE_CAP);
            assert!(matches!(cache.get(slow), Some(ThumbnailState::Loading)));
            assert_eq!(cache.in_flight, 1);
        }
        cache.finish(slow, failed());
        cache.finish(slow, failed());
        cache.finish(Path::new("unknown"), failed());
        assert_eq!(cache.in_flight, 0);
    }

    #[test]
    fn decoder_failure_does_not_prevent_the_next_job() {
        assert!(decoder_result::<()>(|| panic!("injected codec panic")).is_err());
        assert_eq!(decoder_result(|| Ok(42)).unwrap(), 42);
    }

    #[test]
    fn corrupt_missing_and_oversized_files_fail_without_large_allocations() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.png");
        assert!(decode_thumbnail(&path).is_err());
        std::fs::write(&path, b"invalid PNG").unwrap();
        assert!(decode_thumbnail(&path).is_err());
        std::fs::File::create(&path)
            .unwrap()
            .set_len(MAX_FILE_BYTES + 1)
            .unwrap();
        assert!(
            decode_thumbnail(&path)
                .unwrap_err()
                .to_string()
                .contains("size limit")
        );
        assert!(decode_thumbnail(dir.path()).is_err());
    }

    #[test]
    fn dimension_limit_rejects_a_valid_extremely_wide_image() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wide.png");
        image::GrayImage::new(MAX_DIMENSION + 1, 1)
            .save(&path)
            .unwrap();
        assert!(decode_thumbnail(&path).is_err());
    }

    #[test]
    fn normal_images_still_produce_bgra_thumbnails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("small.png");
        image::RgbaImage::from_pixel(1, 1, image::Rgba([10, 20, 30, 255]))
            .save(&path)
            .unwrap();
        let thumbnail = decode_thumbnail(&path).unwrap();
        let bytes = thumbnail.as_bytes(0).unwrap();
        assert_eq!(bytes.len(), (THUMBNAIL_EDGE * THUMBNAIL_EDGE * 4) as usize);
        assert!(
            bytes
                .as_chunks::<4>()
                .0
                .iter()
                .all(|pixel| *pixel == [30, 20, 10, 255])
        );
    }
}

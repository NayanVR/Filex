//! Lazy thumbnail decoding for image files in the list.
//!
//! Thumbnails are requested only for rows the virtualized list actually
//! renders (i.e. visible ones), decoded on the background executor, and
//! delivered back as gpui `RenderImage`s. A row scrolled past before its
//! request fires costs nothing; a decode already in flight when the user
//! scrolls away completes once and warms the cache (bounded lateness
//! rather than hard cancellation — decodes at thumbnail size are
//! milliseconds). The cache is capacity-capped and cleared wholesale
//! when it overflows.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use gpui::RenderImage;

/// Longest edge of a generated thumbnail, in physical pixels. Sized for
/// the largest grid card (block 4); the list view downscales it, which
/// stays crisp, whereas upscaling a tiny thumbnail into a card would not.
pub const THUMBNAIL_EDGE: u32 = 128;

/// Cache entries beyond this trigger a wholesale clear (visible rows
/// repopulate immediately; simple beats LRU bookkeeping at this size).
pub const CACHE_CAP: usize = 512;

#[derive(Clone)]
pub enum ThumbnailState {
    Loading,
    Ready(Arc<RenderImage>),
    Failed,
}

/// Decode `path` and downscale to a thumbnail. Blocking — call on the
/// background executor.
pub fn decode_thumbnail(path: &Path) -> Result<Arc<RenderImage>> {
    let decoded = image::open(path)
        .with_context(|| format!("decoding {}", path.display()))?
        .thumbnail(THUMBNAIL_EDGE, THUMBNAIL_EDGE);
    let mut rgba = decoded.to_rgba8();
    // gpui's RenderImage is BGRA in an RGBA container: swap channels.
    for pixel in rgba.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    let frame = image::Frame::new(rgba);
    Ok(Arc::new(RenderImage::new(vec![frame])))
}

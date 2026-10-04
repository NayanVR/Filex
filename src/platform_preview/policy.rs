//! Routing is deliberately a heuristic, not a universal performance claim.
//! Our Windows benchmark favored Shell for 24 MP JPEGs and Rust for ~2 MP
//! images. Keep the intermediate threshold explicit and easy to retune.
use std::path::Path;
pub const LARGE_JPEG_PIXELS: u64 = 12_000_000;
pub fn extension(path: &Path) -> String {
    path.extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}
pub fn rust_image(path: &Path) -> bool {
    matches!(
        extension(path).as_str(),
        "jpg" | "jpeg" | "png" | "webp" | "gif" | "bmp" | "tif" | "tiff" | "ico"
    )
}
pub fn shell_candidate(path: &Path) -> bool {
    matches!(
        extension(path).as_str(),
        "jpg"
            | "jpeg"
            | "png"
            | "gif"
            | "bmp"
            | "tif"
            | "tiff"
            | "ico"
            | "heic"
            | "heif"
            | "avif"
            | "raw"
            | "dng"
            | "cr2"
            | "nef"
            | "arw"
            | "mp4"
            | "m4v"
            | "mov"
            | "avi"
            | "mkv"
            | "wmv"
            | "webm"
            | "mp3"
            | "m4a"
            | "flac"
            | "wav"
            | "pdf"
            | "doc"
            | "docx"
            | "xls"
            | "xlsx"
            | "ppt"
            | "pptx"
            | "odt"
            | "ods"
            | "odp"
            | "rtf"
            | "epub"
            | "psd"
            | "ai"
            | "eps"
    )
}
pub fn shell_first(path: &Path, dimensions: Option<(u32, u32)>) -> bool {
    !rust_image(path)
        || (matches!(extension(path).as_str(), "jpg" | "jpeg")
            && dimensions.is_some_and(|(w, h)| u64::from(w) * u64::from(h) >= LARGE_JPEG_PIXELS))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn routes_large_jpegs_without_changing_small_images_or_webp() {
        assert!(!shell_first(Path::new("small.JPG"), Some((1600, 1200))));
        assert!(shell_first(Path::new("large.jpeg"), Some((6000, 4000))));
        assert!(!shell_first(Path::new("large.png"), Some((6000, 4000))));
        assert!(!shell_first(Path::new("bad.jpg"), None));
        assert!(!shell_candidate(Path::new("image.webp")));
        assert!(shell_candidate(Path::new("video.MP4")));
        assert!(!shell_candidate(Path::new("program.exe")));
    }
}

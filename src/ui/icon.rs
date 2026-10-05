//! The fixed-size icon cell at the start of every list row, plus the
//! shared helper for themed UI glyphs.
//!
//! File cells show a decoded thumbnail, a full-color folder image with an
//! optional kind symbol, or document artwork with a kind symbol and extension.
//! All occupy the same width so mixed rows stay column-aligned.

use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use gpui::{
    Animation, AnimationExt as _, AnyElement, ElementId, RenderImage, Rgba, Svg, Transformation,
    div, img, linear, percentage, prelude::*, px, rgb, svg,
};

use filex::listing::FileKind;
use filex::settings::FolderIcon;

use super::theme::Theme;

/// A themed UI glyph (chevrons, gear, search, markers…) from an asset
/// path like `"icons/settings.svg"`. Caller sizes it (`.size(px(..))`)
/// and may add a transformation for animation.
pub fn ui_icon(path: &'static str, color: Rgba) -> Svg {
    svg().path(path).flex_none().text_color(color)
}

/// A continuously rotating glyph (e.g. `loader-circle` as a busy
/// spinner). One full turn per second, linear so it reads as steady
/// motion. `id` must be unique among concurrently animating elements.
/// Lives outside virtualized lists only — never spin inside a
/// `uniform_list` row (polish principle in docs/roadmap.md).
pub fn spinner(path: &'static str, color: Rgba, size: f32, id: impl Into<ElementId>) -> AnyElement {
    ui_icon(path, color)
        .size(px(size))
        .with_animation(
            id,
            Animation::new(Duration::from_secs(1))
                .repeat()
                .with_easing(linear),
            |svg, delta| svg.with_transformation(Transformation::rotate(percentage(delta))),
        )
        .into_any_element()
}

/// Fit the entire thumbnail inside a square cell, preserving its aspect ratio.
/// Apply rounding to the image itself: GPUI passes its corner radii to the
/// image painter at the fitted bounds, while a rounded parent only clips
/// children to a rectangular content mask.
pub fn thumbnail_icon(imagery: Arc<RenderImage>, edge: f32) -> AnyElement {
    img(imagery)
        .size(px(edge))
        .flex_none()
        .object_fit(gpui::ObjectFit::Contain)
        .rounded(px(if edge > 40. { 6. } else { 3. }))
        .into_any_element()
}

/// Document artwork with a centered kind mark and an uppercase extension.
/// Small list cells omit the label, which only becomes readable at grid sizes.
pub fn file_icon(theme: &Theme, kind: FileKind, name: &str, edge: f32) -> AnyElement {
    let base = if let Some(image) = file_image() {
        img(image).absolute().size_full().into_any_element()
    } else {
        ui_icon("icons/file.svg", rgb(0xcfe6fb))
            .absolute()
            .left(px(edge / 7.))
            .w(px(edge * 5. / 7.))
            .h(px(edge))
            .into_any_element()
    };
    let mut file = div().relative().size(px(edge)).flex_none().child(base);
    if edge >= 20. {
        let symbol_size = edge * 0.38;
        file = file.child(
            ui_icon(kind_asset(kind), kind_color(theme, kind))
                .absolute()
                .left(px((edge - symbol_size) / 2.))
                .top(px(edge * 0.50 - symbol_size / 2.))
                .size(px(symbol_size))
                .opacity(0.75),
        );
    }
    if edge >= 40.
        && let Some(extension) = Path::new(name).extension().and_then(|ext| ext.to_str())
        && !extension.is_empty()
    {
        // Bound unusual extensions without splitting Unicode characters or
        // allowing a long label to spill outside the paper silhouette.
        let extension = extension.to_uppercase();
        let label = if extension.chars().count() > 6 {
            format!("{}…", extension.chars().take(5).collect::<String>())
        } else {
            extension
        };
        let font_size = (edge * 0.16).min(edge * 0.88 / label.chars().count() as f32);
        file = file.child(
            div()
                .absolute()
                .left(px(edge * 0.20))
                .bottom(px(edge * 0.08))
                .w(px(edge * 0.60))
                .text_ellipsis()
                .text_center()
                .text_size(px(font_size))
                .line_height(px(edge * 0.20))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(rgb(0x6b8299))
                .child(label),
        );
    }
    file.into_any_element()
}

/// All file cells share one raster and atlas entry, independent of extension.
fn file_image() -> Option<Arc<RenderImage>> {
    static IMAGE: LazyLock<Option<Arc<RenderImage>>> = LazyLock::new(|| {
        match render_icon_svg(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/assets/icons/file.svg"
        ))) {
            Ok(image) => Some(image),
            Err(err) => {
                tracing::warn!("could not render file artwork: {err:#}");
                None
            }
        }
    });
    IMAGE.clone()
}

/// Guess a symbol from known OS folder locations, then common folder names.
/// This never reads folder contents or blocks a virtualized row.
pub fn automatic_folder_icon(path: &Path) -> FolderIcon {
    static KNOWN: LazyLock<Vec<(PathBuf, FolderIcon)>> = LazyLock::new(|| {
        [
            (dirs::picture_dir(), FolderIcon::Pictures),
            (dirs::audio_dir(), FolderIcon::Music),
            (dirs::video_dir(), FolderIcon::Videos),
            (dirs::document_dir(), FolderIcon::Documents),
            (dirs::download_dir(), FolderIcon::Downloads),
            (dirs::desktop_dir(), FolderIcon::Desktop),
        ]
        .into_iter()
        .filter_map(|(path, icon)| path.map(|path| (path, icon)))
        .collect()
    });
    if let Some((_, icon)) = KNOWN.iter().find(|(known, _)| known == path) {
        return *icon;
    }
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return FolderIcon::Plain;
    };
    match name.to_ascii_lowercase().as_str() {
        "pictures" | "photos" | "images" | "screenshots" => FolderIcon::Pictures,
        "music" | "audio" => FolderIcon::Music,
        "videos" | "movies" => FolderIcon::Videos,
        "documents" | "docs" => FolderIcon::Documents,
        "downloads" => FolderIcon::Downloads,
        "code" | "source" | "src" | "projects" => FolderIcon::Code,
        "archives" => FolderIcon::Archives,
        "desktop" => FolderIcon::Desktop,
        _ => FolderIcon::Plain,
    }
}

/// Keep only the active accent in memory. A folder SVG renders once at 256px
/// for each accent change, then every visible folder shares the same image ID
/// and GPUI atlas entry. The center symbol remains a separate SVG mask.
fn folder_image(accent: Rgba) -> Option<Arc<RenderImage>> {
    type Cached = Option<(u32, Arc<RenderImage>)>;
    static CACHE: LazyLock<Mutex<Cached>> = LazyLock::new(|| Mutex::new(None));
    let accent = color_hex(accent);
    let mut cached = CACHE.lock().unwrap_or_else(|err| err.into_inner());
    if let Some((previous, image)) = cached.as_ref()
        && *previous == accent
    {
        return Some(image.clone());
    }
    let svg = recolored_folder_svg(accent);
    match render_icon_svg(&svg) {
        Ok(image) => {
            *cached = Some((accent, image.clone()));
            Some(image)
        }
        Err(err) => {
            tracing::warn!("could not render folder artwork: {err:#}");
            None
        }
    }
}

fn render_icon_svg(svg: &str) -> anyhow::Result<Arc<RenderImage>> {
    let tree = resvg::usvg::Tree::from_data(svg.as_bytes(), &resvg::usvg::Options::default())?;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(256, 256)
        .ok_or_else(|| anyhow::anyhow!("could not allocate icon image"))?;
    // Fit both the square folder and the portrait document into the same
    // square cell, preserving their proportions and transparent margins.
    let size = tree.size();
    let scale = 256. / size.width().max(size.height());
    let transform = resvg::tiny_skia::Transform::from_row(
        scale,
        0.,
        0.,
        scale,
        (256. - size.width() * scale) / 2.,
        (256. - size.height() * scale) / 2.,
    );
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    // tiny-skia gives us premultiplied RGBA. GPUI's RenderImage expects
    // straight-alpha BGRA, as in its own SVG image loading path.
    let mut data = pixmap.take();
    for pixel in data.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
        if pixel[3] > 0 {
            let alpha = pixel[3] as f32 / 255.;
            for channel in &mut pixel[..3] {
                *channel = (*channel as f32 / alpha) as u8;
            }
        }
    }
    let pixels = image::RgbaImage::from_raw(256, 256, data)
        .ok_or_else(|| anyhow::anyhow!("invalid icon image dimensions"))?;
    Ok(Arc::new(RenderImage::new(vec![image::Frame::new(pixels)])))
}

fn color_hex(color: Rgba) -> u32 {
    let byte = |v: f32| (v.clamp(0., 1.) * 255.).round() as u32;
    (byte(color.r) << 16) | (byte(color.g) << 8) | byte(color.b)
}

fn vivid_hex(color: u32, saturation_boost: f32, peak: u8) -> String {
    let channels = [
        ((color >> 16) & 0xff) as f32,
        ((color >> 8) & 0xff) as f32,
        (color & 0xff) as f32,
    ];
    let min = channels.iter().copied().fold(f32::INFINITY, f32::min);
    let max = channels.iter().copied().fold(0., f32::max);
    if max == min {
        return format!("#{peak:02x}{peak:02x}{peak:02x}");
    }
    // Move the darkest channel toward zero, then scale the brightest to
    // `peak`. This keeps the accent hue while restoring the vivid range of
    // the original folder artwork.
    let shift = min * saturation_boost;
    let channel = |value: f32| ((value - shift) * peak as f32 / (max - shift)).round() as u8;
    format!(
        "#{:02x}{:02x}{:02x}",
        channel(channels[0]),
        channel(channels[1]),
        channel(channels[2])
    )
}

fn recolored_folder_svg(accent: u32) -> String {
    include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/assets/branding/folder.svg"
    ))
    // Replace in two passes so a generated stop that happens to equal one
    // of the source blues cannot be recolored again by a later replacement.
    .replace("#28bdfc", "__FRONT_TOP__")
    .replace("#008fe5", "__FRONT_BOTTOM__")
    .replace("#006ddc", "__BACK_TOP__")
    .replace("#002d71", "__BACK_BOTTOM__")
    .replace("__FRONT_TOP__", &vivid_hex(accent, 0.55, 252))
    .replace("__FRONT_BOTTOM__", &vivid_hex(accent, 1.0, 230))
    .replace("__BACK_TOP__", &vivid_hex(accent, 0.85, 210))
    .replace("__BACK_BOTTOM__", &vivid_hex(accent, 1.0, 110))
}

fn folder_symbol(icon: FolderIcon) -> Option<&'static str> {
    match icon {
        FolderIcon::Plain => None,
        FolderIcon::Pictures => Some("icons/image.svg"),
        FolderIcon::Music => Some("icons/music.svg"),
        FolderIcon::Videos => Some("icons/film.svg"),
        FolderIcon::Documents => Some("icons/file-text.svg"),
        FolderIcon::Downloads => Some("icons/download.svg"),
        FolderIcon::Code => Some("icons/file-code.svg"),
        FolderIcon::Archives => Some("icons/archive.svg"),
        FolderIcon::Desktop => Some("icons/house.svg"),
    }
}

/// Full-color folder artwork tinted from the active accent, with a subtle
/// symbol centered on its front face.
pub fn folder_icon(theme: &Theme, icon: FolderIcon, edge: f32) -> AnyElement {
    let base = if let Some(image) = folder_image(theme.accent) {
        img(image).absolute().size_full().into_any_element()
    } else {
        // Keep folders recognizable if SVG rasterization fails.
        ui_icon("icons/folder.svg", theme.accent)
            .absolute()
            .size_full()
            .into_any_element()
    };
    let mut folder = div().relative().size(px(edge)).flex_none().child(base);
    // At 16px the 24px outline glyph collapses into noise; compact rows use
    // the same folder silhouette without a center mark.
    if edge >= 20.
        && let Some(path) = folder_symbol(icon)
    {
        let symbol_size = edge * if edge <= 24. { 0.48 } else { 0.36 };
        folder = folder.child(
            ui_icon(path, theme.on_accent)
                .absolute()
                .left(px((edge - symbol_size) / 2.))
                .top(px(edge * 0.61 - symbol_size / 2.))
                .size(px(symbol_size))
                .opacity(0.45),
        );
    }
    folder.into_any_element()
}

/// The SVG asset path for a file kind.
fn kind_asset(kind: FileKind) -> &'static str {
    match kind {
        FileKind::Directory => "icons/folder.svg",
        FileKind::Image => "icons/image.svg",
        FileKind::Video => "icons/video.svg",
        FileKind::Audio => "icons/music.svg",
        FileKind::Archive => "icons/archive.svg",
        FileKind::Code => "icons/code.svg",
        FileKind::Document | FileKind::Other => "icons/text.svg",
    }
}

/// The tint for a file kind. Folders take the theme accent so they read
/// as the primary item and adapt to light/dark; the rest use a curated
/// set of hues chosen to stay legible on both a white and a dark row.
fn kind_color(theme: &Theme, kind: FileKind) -> Rgba {
    match kind {
        FileKind::Directory => theme.accent,
        FileKind::Image => rgb(0x30a46c),
        FileKind::Video => rgb(0x8b5cf6),
        FileKind::Audio => rgb(0xe0559a),
        FileKind::Archive => rgb(0xd08b1e),
        FileKind::Code => rgb(0x4c8bf0),
        FileKind::Document => rgb(0x6b8299),
        FileKind::Other => rgb(0x6b8299),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_folder_names_get_symbols_without_content_scans() {
        assert_eq!(
            automatic_folder_icon(Path::new("/tmp/Photos")),
            FolderIcon::Pictures
        );
        assert_eq!(
            automatic_folder_icon(Path::new("/tmp/projects")),
            FolderIcon::Code
        );
        assert_eq!(
            automatic_folder_icon(Path::new("/tmp/misc")),
            FolderIcon::Plain
        );
    }

    #[test]
    fn portrait_file_artwork_is_fitted_and_cached() {
        let image = file_image().unwrap();
        assert!(Arc::ptr_eq(&image, &file_image().unwrap()));
        assert_eq!(image.size(0).width.0, 256);
        assert_eq!(image.size(0).height.0, 256);
        let bytes = image.as_bytes(0).unwrap();
        let pixel = |x: usize, y: usize| &bytes[(y * 256 + x) * 4..(y * 256 + x + 1) * 4];
        // The 5:7 paper is centered in the square without clipping its foot.
        assert_eq!(pixel(20, 128)[3], 0);
        assert_eq!(pixel(235, 128)[3], 0);
        assert_eq!(pixel(128, 250)[3], 255);
        let center = pixel(128, 128);
        assert_eq!(center[3], 255);
        assert!(center[0] > center[2], "expected blue paper in BGRA order");
    }

    #[test]
    fn recolored_vector_renders_with_transparency() {
        let blue = render_icon_svg(&recolored_folder_svg(0x3b82f6)).unwrap();
        let orange = render_icon_svg(&recolored_folder_svg(0xf97316)).unwrap();
        assert_eq!(blue.size(0).width.0, 256);
        assert_eq!(&blue.as_bytes(0).unwrap()[0..4], &[0, 0, 0, 0]);
        assert_ne!(blue.as_bytes(0), orange.as_bytes(0));
        // The front of a blue folder must be blue in GPUI's BGRA byte order.
        let center = (160 * 256 + 128) * 4;
        let pixel = &blue.as_bytes(0).unwrap()[center..center + 4];
        assert_eq!(pixel[3], 255);
        assert!(pixel[0] > pixel[2], "expected blue folder, got {pixel:?}");
    }
}

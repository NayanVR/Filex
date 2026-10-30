//! The fixed-size icon cell at the start of every list row, plus the
//! shared helper for themed UI glyphs.
//!
//! File cells show a decoded thumbnail, or icon-system artwork
//! (docs/design-icons.md): paper in the file family's colours, or a folder
//! in the accent's, drawn per size class by [`super::icon_art`] and
//! rasterized once per key. All occupy the same width so mixed rows stay
//! column-aligned.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use gpui::{
    Animation, AnimationExt as _, AnyElement, ElementId, RenderImage, Rgba, Svg, Transformation,
    div, img, linear, percentage, prelude::*, px, svg,
};

use filex::file_types::{FileFamily, extension_label};
use filex::settings::FolderIcon;

use super::icon_art::{self, LABEL_BOX, SizeClass, Symbol};
use super::icon_palette::{file_ramp, folder_ramp};
use super::theme::{Theme, fixed};

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

/// A file's icon: paper in its family's colours with the family symbol.
/// Card sizes (32px and up) print the extension on the label, laid over
/// the artwork as text so one raster serves every extension.
pub fn file_icon(theme: &Theme, name: &str, edge: f32) -> AnyElement {
    let family = FileFamily::of_name(name);
    let class = SizeClass::of(edge);
    let label = extension_label(name).filter(|_| class == SizeClass::Card);
    let key = ArtKey::File {
        family,
        class,
        dark: theme.is_dark(),
        labelled: label.is_some(),
        raster: raster_edge(edge),
    };
    let mut cell = div().relative().size(px(edge)).flex_none();
    cell = match cached_art(key) {
        Some(image) => cell.child(img(image).absolute().size_full()),
        // Keep files recognizable if rasterization fails.
        None => cell.child(
            ui_icon(
                Symbol::of_family(family).unwrap_or(Symbol::Text).path(),
                file_ramp(family, theme.is_dark()).ink,
            )
            .size_full(),
        ),
    };
    if let Some(label) = label {
        let unit = edge / 32.;
        let (x, y, w, h) = LABEL_BOX;
        let size = if label.chars().count() > 3 { 3.5 } else { 3.9 };
        cell = cell.child(
            div()
                .absolute()
                .left(px(x * unit))
                .top(px(y * unit))
                .w(px(w * unit))
                .h(px(h * unit))
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(size * unit))
                .line_height(px(h * unit))
                .font_weight(gpui::FontWeight::BOLD)
                .text_color(fixed::white())
                .child(label),
        );
    }
    cell.into_any_element()
}

/// What a cached icon raster depends on. Folders key on the accent too;
/// only the current accent's folders are kept.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum ArtKey {
    File {
        family: FileFamily,
        class: SizeClass,
        dark: bool,
        labelled: bool,
        raster: u32,
    },
    Folder {
        accent: u32,
        symbol: Option<Symbol>,
        class: SizeClass,
        dark: bool,
        raster: u32,
    },
}

/// Raster edge for an icon shown at `edge` logical px: enough for a 2×
/// display without minifying a large raster into a tiny cell.
fn raster_edge(edge: f32) -> u32 {
    match edge * 2. {
        e if e <= 64. => 64,
        e if e <= 128. => 128,
        _ => 384,
    }
}

/// The raster for `key`, drawn on first use and shared after that (one
/// GPUI image id and atlas entry per key). A failed render is cached as
/// `None` so it's logged once, not every frame.
fn cached_art(key: ArtKey) -> Option<Arc<RenderImage>> {
    type Cache = HashMap<ArtKey, Option<Arc<RenderImage>>>;
    static CACHE: LazyLock<Mutex<Cache>> = LazyLock::new(Default::default);
    let mut cache = CACHE.lock().unwrap_or_else(|err| err.into_inner());
    if let Some(image) = cache.get(&key) {
        return image.clone();
    }
    if let ArtKey::Folder { accent, .. } = key {
        cache.retain(|k, _| !matches!(k, ArtKey::Folder { accent: a, .. } if *a != accent));
    }
    let (svg, edge) = match key {
        ArtKey::File {
            family,
            class,
            dark,
            labelled,
            raster,
        } => (
            icon_art::file_svg(family, &file_ramp(family, dark), class, dark, labelled),
            raster,
        ),
        ArtKey::Folder {
            accent,
            symbol,
            class,
            dark,
            raster,
        } => (
            icon_art::folder_svg(&folder_ramp(gpui::rgb(accent), dark), symbol, class, dark),
            raster,
        ),
    };
    let image = render_icon_svg(&svg, edge)
        .inspect_err(|err| tracing::warn!("could not render icon artwork: {err:#}"))
        .ok();
    cache.insert(key, image.clone());
    image
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

fn render_icon_svg(svg: &str, edge: u32) -> anyhow::Result<Arc<RenderImage>> {
    let tree = resvg::usvg::Tree::from_data(svg.as_bytes(), &resvg::usvg::Options::default())?;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(edge, edge)
        .ok_or_else(|| anyhow::anyhow!("could not allocate icon image"))?;
    // Fit both the square folder and the portrait document into the same
    // square cell, preserving their proportions and transparent margins.
    let size = tree.size();
    let side = edge as f32;
    let scale = side / size.width().max(size.height());
    let transform = resvg::tiny_skia::Transform::from_row(
        scale,
        0.,
        0.,
        scale,
        (side - size.width() * scale) / 2.,
        (side - size.height() * scale) / 2.,
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
    let pixels = image::RgbaImage::from_raw(edge, edge, data)
        .ok_or_else(|| anyhow::anyhow!("invalid icon image dimensions"))?;
    Ok(Arc::new(RenderImage::new(vec![image::Frame::new(pixels)])))
}

/// One layer of the Filex mark, split from `assets/branding/filex.svg` so
/// onboarding can assemble the logo piece by piece. Every layer keeps the
/// full canvas, so stacked at the same size they reproduce the mark exactly.
#[derive(Clone, Copy)]
pub enum LogoLayer {
    /// The rear folder with its tab.
    Back,
    /// The front panel.
    Front,
    /// The "fx" monogram.
    Mark,
}

/// Width over height of the logo canvas (700×601).
pub const LOGO_ASPECT: f32 = 700. / 601.;

/// A logo layer, rasterized once at 512 px (sharp up to ~250 pt on 2× screens).
pub fn logo_layer(layer: LogoLayer) -> Option<Arc<RenderImage>> {
    macro_rules! layer {
        ($name:literal) => {{
            static IMAGE: LazyLock<Option<Arc<RenderImage>>> = LazyLock::new(|| {
                let svg = include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/assets/branding/layers/",
                    $name,
                    ".svg"
                ));
                render_icon_svg(svg, 512)
                    .inspect_err(|err| tracing::warn!("could not render logo layer: {err:#}"))
                    .ok()
            });
            IMAGE.clone()
        }};
    }
    match layer {
        LogoLayer::Back => layer!("back"),
        LogoLayer::Front => layer!("front"),
        LogoLayer::Mark => layer!("mark"),
    }
}

fn color_hex(color: Rgba) -> u32 {
    let byte = |v: f32| (v.clamp(0., 1.) * 255.).round() as u32;
    (byte(color.red) << 16) | (byte(color.green) << 8) | byte(color.blue)
}

/// A folder in the accent's colours, with a special folder's symbol pressed
/// into its front panel (none at 16px, where it would only be noise).
pub fn folder_icon(theme: &Theme, icon: FolderIcon, edge: f32) -> AnyElement {
    let key = ArtKey::Folder {
        accent: color_hex(theme.accent),
        symbol: Symbol::of_folder(icon),
        class: SizeClass::of(edge),
        dark: theme.is_dark(),
        raster: raster_edge(edge),
    };
    let cell = div().relative().size(px(edge)).flex_none();
    match cached_art(key) {
        Some(image) => cell.child(img(image).absolute().size_full()),
        // Keep folders recognizable if rasterization fails.
        None => cell.child(ui_icon("icons/folder.svg", theme.accent).size_full()),
    }
    .into_any_element()
}

/// The full-color Filex app icon, rasterized once. Falls back to the
/// accent folder glyph if rasterization fails.
pub fn app_icon(theme: &Theme, edge: f32) -> AnyElement {
    static IMAGE: LazyLock<Option<Arc<RenderImage>>> = LazyLock::new(|| {
        let svg = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/assets/branding/filex.svg"
        ));
        render_icon_svg(svg, 256)
            .inspect_err(|err| tracing::warn!("could not render app icon: {err:#}"))
            .ok()
    });
    match IMAGE.as_ref() {
        Some(image) => img(image.clone())
            .size(px(edge))
            .flex_none()
            .into_any_element(),
        None => ui_icon("icons/folder.svg", theme.accent)
            .size(px(edge))
            .into_any_element(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_logo_layer_renders() {
        for layer in [LogoLayer::Back, LogoLayer::Front, LogoLayer::Mark] {
            assert!(logo_layer(layer).is_some());
        }
    }

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
    fn app_icon_svg_rasterizes() {
        let svg = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/assets/branding/filex.svg"
        ));
        let image = render_icon_svg(svg, 256).unwrap();
        let bytes = image.as_bytes(0).unwrap();
        // Center of the folder front face is opaque.
        assert_eq!(bytes[(128 * 256 + 128) * 4 + 3], 255);
    }

    #[test]
    fn file_art_rasterizes_and_is_shared() {
        let key = ArtKey::File {
            family: FileFamily::Pdf,
            class: SizeClass::Card,
            dark: false,
            labelled: true,
            raster: 128,
        };
        let image = cached_art(key).unwrap();
        assert!(Arc::ptr_eq(&image, &cached_art(key).unwrap()));
        let bytes = image.as_bytes(0).unwrap();
        // The paper's middle is opaque; the clear space beside it isn't.
        assert_eq!(bytes[(64 * 128 + 64) * 4 + 3], 255);
        assert_eq!(bytes[(64 * 128 + 4) * 4 + 3], 0);
    }

    #[test]
    fn folders_follow_the_accent() {
        let key = |accent| ArtKey::Folder {
            accent,
            symbol: None,
            class: SizeClass::List,
            dark: false,
            raster: 64,
        };
        let blue = cached_art(key(0x3b82f6)).unwrap();
        let orange = cached_art(key(0xf97316)).unwrap();
        let front = (48 * 64 + 32) * 4;
        let (b, o) = (
            &blue.as_bytes(0).unwrap()[front..front + 4],
            &orange.as_bytes(0).unwrap()[front..front + 4],
        );
        assert_eq!(b[3], 255);
        // GPUI's BGRA byte order: blue leads on blue, red on orange.
        assert!(b[0] > b[2], "expected a blue folder, got {b:?}");
        assert!(o[2] > o[0], "expected an orange folder, got {o:?}");
    }

    #[test]
    fn raster_size_tracks_the_display_size() {
        assert_eq!(raster_edge(16.), 64);
        assert_eq!(raster_edge(48.), 128);
        assert_eq!(raster_edge(192.), 384);
    }
}

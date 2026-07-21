//! The fixed-size icon cell at the start of every list row, plus the
//! shared helper for themed UI glyphs.
//!
//! Two file-cell variants: a decoded image thumbnail, or a file-kind
//! SVG mark. Both occupy the same width so mixed rows stay
//! column-aligned. Deciding *which* to show (and scheduling thumbnail
//! decodes) is workspace logic — these only render an already-made
//! choice. All SVGs come from [`super::assets`] and are tinted by the
//! passed color (gpui fills the icon's coverage mask with `text_color`).

use std::sync::Arc;
use std::time::Duration;

use gpui::{
    Animation, AnimationExt as _, AnyElement, ElementId, RenderImage, Rgba, Svg, Transformation,
    img, linear, percentage, prelude::*, px, rgb, svg,
};

use filex::listing::FileKind;

use super::theme::Theme;

/// Width (and height, for thumbnails) of the icon cell. Public so the
/// column header can align its leading spacer with the icon column.
pub const ICON_SIZE: f32 = 20.;

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
            Animation::new(Duration::from_secs(1)).repeat().with_easing(linear),
            |svg, delta| svg.with_transformation(Transformation::rotate(percentage(delta))),
        )
        .into_any_element()
}

/// A decoded thumbnail, rendered as a rounded square.
pub fn thumbnail_icon(imagery: Arc<RenderImage>) -> AnyElement {
    img(imagery)
        .w(px(ICON_SIZE))
        .h(px(ICON_SIZE))
        .rounded_sm()
        .object_fit(gpui::ObjectFit::Cover)
        .into_any_element()
}

/// The file-type mark for `kind`, sized to the icon cell and colored by
/// [`kind_color`].
pub fn file_icon(theme: &Theme, kind: FileKind) -> AnyElement {
    svg()
        .path(kind_asset(kind))
        .w(px(ICON_SIZE))
        .h(px(ICON_SIZE))
        .flex_none()
        .text_color(kind_color(theme, kind))
        .into_any_element()
}

/// The SVG asset path for a file kind.
fn kind_asset(kind: FileKind) -> &'static str {
    match kind {
        FileKind::Directory => "icons/folder.svg",
        FileKind::Image => "icons/image.svg",
        FileKind::Video => "icons/film.svg",
        FileKind::Audio => "icons/music.svg",
        FileKind::Archive => "icons/archive.svg",
        FileKind::Code => "icons/file-code.svg",
        FileKind::Document => "icons/file-text.svg",
        FileKind::Other => "icons/file.svg",
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
        FileKind::Other => theme.text_dim,
    }
}

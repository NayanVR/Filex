//! The fixed-size icon cell at the start of every list row.
//!
//! Two variants: a decoded image thumbnail, or a file-kind glyph. Both
//! occupy the same width so mixed rows stay column-aligned. Deciding
//! *which* to show (and scheduling thumbnail decodes) is workspace
//! logic — these only render an already-made choice.

use std::sync::Arc;

use gpui::{AnyElement, RenderImage, div, img, prelude::*, px, rgb};

use filex::listing::FileKind;

use super::theme::{ACCENT, TEXT_DIM};

/// Width (and height, for thumbnails) of the icon cell.
const ICON_SIZE: f32 = 20.;

/// A decoded thumbnail, rendered as a rounded square.
pub fn thumbnail_icon(imagery: Arc<RenderImage>) -> AnyElement {
    img(imagery)
        .w(px(ICON_SIZE))
        .h(px(ICON_SIZE))
        .rounded_sm()
        .object_fit(gpui::ObjectFit::Cover)
        .into_any_element()
}

/// The glyph for `kind`, accent-colored for directories.
pub fn glyph_icon(kind: FileKind, is_dir: bool) -> AnyElement {
    div()
        .w(px(ICON_SIZE))
        .text_sm()
        .text_color(rgb(if is_dir { ACCENT } else { TEXT_DIM }))
        .child(kind.glyph())
        .into_any_element()
}

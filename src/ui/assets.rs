//! The app's asset source: bundled SVG icons (Lucide, ISC — see
//! `assets/icons/LICENSE`).
//!
//! gpui's `svg()` element loads an icon by path through the
//! [`gpui::AssetSource`] registered on the [`gpui::Application`]. As with
//! the bundled font, the SVGs are embedded into the binary
//! (`include_bytes!`) rather than read from a data directory, so the app
//! stays a single self-contained artifact and an icon can never fail to
//! load because of a missing file at runtime.
//!
//! Icons are single-color: gpui rasterizes the SVG to a coverage mask
//! and fills it with the element's `text_color`, so the stroke geometry
//! is what matters and every glyph is tinted from the active theme.

use std::borrow::Cow;

use anyhow::Result;
use gpui::{AssetSource, SharedString};

/// One embedded icon: its lookup path (`icons/<name>.svg`) and bytes.
macro_rules! icon {
    ($name:literal) => {
        (
            concat!("icons/", $name, ".svg"),
            include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/icons/", $name, ".svg"))
                as &'static [u8],
        )
    };
}

/// Every bundled icon, keyed by the path callers pass to `svg().path()`.
const ICONS: &[(&str, &[u8])] = &[
    // File-type marks.
    icon!("folder"),
    icon!("image"),
    icon!("film"),
    icon!("music"),
    icon!("archive"),
    icon!("file-code"),
    icon!("file-text"),
    icon!("file"),
    // UI glyphs.
    icon!("arrow-up"),
    icon!("settings"),
    icon!("search"),
    icon!("chevron-right"),
    icon!("chevron-up"),
    icon!("chevron-down"),
    icon!("list"),
    icon!("layout-grid"),
    icon!("minus"),
    icon!("plus"),
    icon!("house"),
    icon!("hard-drive"),
    icon!("x"),
    icon!("loader-circle"),
    icon!("triangle-alert"),
    icon!("dot"),
];

/// Serves the embedded [`ICONS`]. Registered via
/// `Application::with_assets(Assets)`.
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(ICONS.iter().find(|(p, _)| *p == path).map(|(_, bytes)| Cow::Borrowed(*bytes)))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(ICONS
            .iter()
            .filter(|(p, _)| p.starts_with(path))
            .map(|(p, _)| SharedString::from(*p))
            .collect())
    }
}

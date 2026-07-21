//! Bundled UI font (Inter, SIL Open Font License — see
//! `assets/fonts/LICENSE.txt`).
//!
//! The four static weights are embedded straight into the binary with
//! `include_bytes!` rather than loaded through an [`gpui::AssetSource`]:
//! a font the whole UI depends on shouldn't hinge on a data directory
//! existing at runtime, and embedding keeps the app a single
//! self-contained artifact on every platform. (SVG icons in a later
//! block will use the asset source, which is the right tool there.)

use gpui::App;

/// The family name the bundled TTFs register under. Components select it
/// by setting `.font_family(UI_FONT_FAMILY)` on the root element; it
/// then cascades to every child, including the search input.
pub const UI_FONT_FAMILY: &str = "Inter";

/// Font bytes baked into the binary at build time. `CARGO_MANIFEST_DIR`
/// anchors the path so the include resolves regardless of the build's
/// working directory.
const INTER_REGULAR: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/fonts/Inter-Regular.ttf"));
const INTER_MEDIUM: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/fonts/Inter-Medium.ttf"));
const INTER_SEMIBOLD: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/fonts/Inter-SemiBold.ttf"));
const INTER_BOLD: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/fonts/Inter-Bold.ttf"));

/// Register the bundled weights with the text system so
/// [`UI_FONT_FAMILY`] resolves. Call once at startup. A failure here is
/// non-fatal — the app falls back to the platform default font — so it's
/// logged, not panicked on.
pub fn register(cx: &App) {
    let fonts = vec![
        INTER_REGULAR.into(),
        INTER_MEDIUM.into(),
        INTER_SEMIBOLD.into(),
        INTER_BOLD.into(),
    ];
    if let Err(err) = cx.text_system().add_fonts(fonts) {
        tracing::warn!("could not register bundled UI font ({err:#}); using system default");
    }
}

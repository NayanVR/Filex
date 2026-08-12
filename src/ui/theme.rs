//! The color theme.
//!
//! A [`Theme`] is a flat set of semantic color slots (background, text,
//! accent, …) that every component styles itself from — no component
//! references a raw hex value. Two built-in palettes ship: [`Theme::dark`]
//! (the original look) and [`Theme::light`] (the Finder-class light
//! skin). The active theme lives in GPUI's global state, so any render
//! method or element can reach it through [`ActiveTheme::theme`]; the
//! workspace swaps the global when the `theme` setting or the OS
//! appearance changes.

use gpui::{App, Global, Rgba, WindowAppearance, rgb, rgba};

use filex::settings::{AccentColor, Density, ThemeMode};

/// The accent presets, in picker order (Default is rendered separately —
/// it keeps the palette's built-in accent). Kept next to [`accent_rgb`]
/// so the swatch grid and the resolver can't disagree.
pub const ACCENT_PRESETS: [AccentColor; 7] = [
    AccentColor::Blue,
    AccentColor::Purple,
    AccentColor::Pink,
    AccentColor::Red,
    AccentColor::Orange,
    AccentColor::Green,
    AccentColor::Teal,
];

/// Comfortable-density list metrics — the pre-density defaults, so the
/// dark look doesn't shift under the metrics migration.
const COMFORTABLE_ROW_HEIGHT: f32 = 28.;
const COMFORTABLE_ICON_SIZE: f32 = 20.;

/// The rgb for an accent preset, or `None` for `Default` (keep the
/// palette's own accent). Mid-saturation hues chosen to stay legible on
/// both a white and a near-black background. `Custom` carries its own hex.
pub fn accent_rgb(accent: AccentColor) -> Option<Rgba> {
    let hex = match accent {
        AccentColor::Default => return None,
        AccentColor::Blue => 0x3b82f6,
        AccentColor::Purple => 0x8b5cf6,
        AccentColor::Pink => 0xec4899,
        AccentColor::Red => 0xef4444,
        AccentColor::Orange => 0xf97316,
        AccentColor::Green => 0x22c55e,
        AccentColor::Teal => 0x14b8a6,
        AccentColor::Custom(hex) => hex,
    };
    Some(rgb(hex))
}

/// Parse a `#RRGGBB` / `RRGGBB` hex string into a `0xRRGGBB` value.
/// Whitespace and a leading `#` are tolerated; anything else is `None`.
pub fn parse_hex(text: &str) -> Option<u32> {
    let hex = text.trim().trim_start_matches('#');
    if hex.len() == 6 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        u32::from_str_radix(hex, 16).ok()
    } else {
        None
    }
}

fn luminance(c: Rgba) -> f32 {
    0.299 * c.r + 0.587 * c.g + 0.114 * c.b
}

fn with_alpha(c: Rgba, a: f32) -> Rgba {
    Rgba { a, ..c }
}

/// Composite `over` onto `base` at fraction `t` (opaque result).
fn blend(base: Rgba, over: Rgba, t: f32) -> Rgba {
    Rgba {
        r: base.r + (over.r - base.r) * t,
        g: base.g + (over.g - base.g) * t,
        b: base.b + (over.b - base.b) * t,
        a: 1.0,
    }
}

/// A complete set of semantic colors. Cheap to copy (a handful of
/// `Rgba` = a few floats each), so components take it by value or borrow
/// freely without worrying about the cost.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Theme {
    /// Window / main content background.
    pub bg: Rgba,
    /// Chrome panels: top bar, sidebar, status bar, menus, dialogs.
    pub panel: Rgba,
    /// Hover background for interactive rows and buttons.
    pub hover: Rgba,
    /// Background of a selected list row (accent-tinted, Finder-style).
    pub selected: Rgba,
    /// Alternating list-row stripe (barely off [`Theme::bg`]).
    pub stripe: Rgba,
    /// Panel and control borders.
    pub border: Rgba,
    /// Primary text.
    pub text: Rgba,
    /// Secondary text (paths, sizes, placeholders, section headers).
    pub text_dim: Rgba,
    /// Accent (directories, ready markers, cursor, focused borders,
    /// primary-button fill).
    pub accent: Rgba,
    /// Text/knob color that sits legibly on top of an [`Theme::accent`]
    /// fill (dark ink on the light-cyan dark theme, white on light).
    pub on_accent: Rgba,
    /// [`Theme::accent`] at low alpha — the text-selection highlight.
    pub accent_selection: Rgba,
    /// Warnings (failed roots, missing permissions, destructive menu
    /// items).
    pub warn: Rgba,
    /// Success / healthy state.
    pub success: Rgba,
    /// List row height (px). Set from the density setting, not the
    /// palette — a metric carried alongside the colors so every component
    /// that already takes `&Theme` gets it without a second parameter.
    pub row_height: f32,
    /// List icon-cell edge (px). Density-driven, like [`Theme::row_height`].
    pub icon_size: f32,
}

impl Global for Theme {}

impl Theme {
    /// The original dark palette. Kept pixel-for-pixel from the pre-theme
    /// constants so the dark look doesn't shift under the migration.
    pub fn dark() -> Self {
        Self {
            bg: rgb(0x1e2227),
            panel: rgb(0x23272e),
            hover: rgb(0x2f343c),
            selected: rgb(0x2a4a63),
            stripe: rgb(0x22262c),
            border: rgb(0x363c45),
            text: rgb(0xd7dae0),
            text_dim: rgb(0x8b929e),
            accent: rgb(0x5ac8fa),
            // The light-cyan accent reads as a "light" fill, so dark ink
            // sits on it (this was the old `BG` the primary button used).
            on_accent: rgb(0x1e2227),
            accent_selection: rgba(0x5ac8fa40),
            warn: rgb(0xe5c07b),
            success: rgb(0x7ec699),
            row_height: COMFORTABLE_ROW_HEIGHT,
            icon_size: COMFORTABLE_ICON_SIZE,
        }
    }

    /// The light palette, derived from the Atlas reference mockup: white
    /// content, soft grey chrome, and a cyan-blue accent deepened enough
    /// to carry white text on a fill.
    pub fn light() -> Self {
        Self {
            bg: rgb(0xffffff),
            panel: rgb(0xf6f7f9),
            hover: rgb(0xeceef1),
            selected: rgb(0xd8ecf9),
            stripe: rgb(0xfafbfc),
            border: rgb(0xe4e6ea),
            text: rgb(0x1c1e22),
            text_dim: rgb(0x82868e),
            accent: rgb(0x0e8fce),
            on_accent: rgb(0xffffff),
            accent_selection: rgba(0x0e8fce3d),
            warn: rgb(0xc0851a),
            success: rgb(0x2ba55a),
            row_height: COMFORTABLE_ROW_HEIGHT,
            icon_size: COMFORTABLE_ICON_SIZE,
        }
    }

    /// A true-black variant of the dark palette for OLED screens: pure
    /// black content so unlit pixels stay off, with the chrome lifted just
    /// enough that panels separate from the void.
    pub fn oled() -> Self {
        Self {
            bg: rgb(0x000000),
            panel: rgb(0x0a0a0c),
            hover: rgb(0x1a1b1e),
            selected: rgb(0x1f3a4d),
            stripe: rgb(0x070708),
            border: rgb(0x242529),
            text: rgb(0xe6e8ec),
            text_dim: rgb(0x8b929e),
            accent: rgb(0x5ac8fa),
            on_accent: rgb(0x000000),
            accent_selection: rgba(0x5ac8fa40),
            warn: rgb(0xe5c07b),
            success: rgb(0x7ec699),
            row_height: COMFORTABLE_ROW_HEIGHT,
            icon_size: COMFORTABLE_ICON_SIZE,
        }
    }

    /// Recolor the accent slots to `accent`, deriving the ink that sits on
    /// it and the selection tints from luminance so any accent stays
    /// legible on this base (light, dark, or OLED).
    fn with_accent(mut self, accent: Rgba) -> Self {
        let light_base = luminance(self.bg) > 0.5;
        self.accent = accent;
        // Dark ink on a bright accent, white on a dark one.
        self.on_accent = if luminance(accent) > 0.55 {
            rgb(0x161616)
        } else {
            rgb(0xffffff)
        };
        self.accent_selection = with_alpha(accent, 0.25);
        // Opaque selected-row fill: the base blended toward the accent, a
        // touch stronger on dark bases so it reads against low contrast.
        let mix = if light_base { 0.16 } else { 0.30 };
        self.selected = blend(self.bg, accent, mix);
        self
    }

    /// Apply the list-density metrics. Comfortable keeps the palette
    /// defaults; Compact packs rows tighter with smaller icons.
    pub fn with_density(mut self, density: Density) -> Self {
        let (row, icon) = match density {
            Density::Comfortable => (COMFORTABLE_ROW_HEIGHT, COMFORTABLE_ICON_SIZE),
            Density::Compact => (22., 16.),
        };
        self.row_height = row;
        self.icon_size = icon;
        self
    }

    /// Resolve a [`ThemeMode`] against the current window appearance, then
    /// apply the chosen [`AccentColor`]. `System` follows the OS (vibrant
    /// variants collapse onto their plain counterparts).
    pub fn resolve(mode: ThemeMode, appearance: WindowAppearance, accent: AccentColor) -> Self {
        let base = match mode {
            ThemeMode::Light => Self::light(),
            ThemeMode::Dark => Self::dark(),
            ThemeMode::Oled => Self::oled(),
            ThemeMode::System => match appearance {
                WindowAppearance::Light | WindowAppearance::VibrantLight => Self::light(),
                WindowAppearance::Dark | WindowAppearance::VibrantDark => Self::dark(),
            },
        };
        match accent_rgb(accent) {
            Some(c) => base.with_accent(c),
            None => base,
        }
    }
}

/// Ergonomic access to the active [`Theme`] from anything that derefs to
/// [`App`] — every `Context<_>`, `&App`, and `&mut App`. Panics only if
/// the global was never installed, which the workspace does before the
/// first paint.
pub trait ActiveTheme {
    fn theme(&self) -> &Theme;
}

impl ActiveTheme for App {
    fn theme(&self) -> &Theme {
        self.global::<Theme>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn light() -> Theme {
        Theme::resolve(ThemeMode::Light, WindowAppearance::Dark, AccentColor::Default)
    }

    #[test]
    fn explicit_modes_ignore_appearance() {
        assert_eq!(
            Theme::resolve(ThemeMode::Light, WindowAppearance::Dark, AccentColor::Default),
            Theme::light()
        );
        assert_eq!(
            Theme::resolve(ThemeMode::Dark, WindowAppearance::Light, AccentColor::Default),
            Theme::dark()
        );
        assert_eq!(
            Theme::resolve(ThemeMode::Oled, WindowAppearance::Light, AccentColor::Default),
            Theme::oled()
        );
    }

    #[test]
    fn system_follows_appearance() {
        assert_eq!(
            Theme::resolve(ThemeMode::System, WindowAppearance::Light, AccentColor::Default),
            Theme::light()
        );
        assert_eq!(
            Theme::resolve(ThemeMode::System, WindowAppearance::Dark, AccentColor::Default),
            Theme::dark()
        );
    }

    #[test]
    fn oled_background_is_pure_black() {
        assert_eq!(Theme::oled().bg, rgb(0x000000));
    }

    #[test]
    fn compact_density_shrinks_rows() {
        let comfy = Theme::dark().with_density(Density::Comfortable);
        let compact = Theme::dark().with_density(Density::Compact);
        assert!(compact.row_height < comfy.row_height);
        assert!(compact.icon_size < comfy.icon_size);
    }

    #[test]
    fn parse_hex_accepts_hash_and_bare() {
        assert_eq!(parse_hex("#ff8800"), Some(0xff8800));
        assert_eq!(parse_hex("  00aaff "), Some(0x00aaff));
        assert_eq!(parse_hex("fff"), None); // wrong length
        assert_eq!(parse_hex("#gggggg"), None); // not hex
    }

    #[test]
    fn custom_accent_uses_its_hex() {
        assert_eq!(accent_rgb(AccentColor::Custom(0x123456)), Some(rgb(0x123456)));
    }

    #[test]
    fn accent_override_recolors_and_stays_legible() {
        let plain = light();
        let purple = Theme::resolve(ThemeMode::Light, WindowAppearance::Dark, AccentColor::Purple);
        // The accent changed and dragged the selection tints with it.
        assert_ne!(purple.accent, plain.accent);
        assert_eq!(purple.accent, accent_rgb(AccentColor::Purple).unwrap());
        assert_ne!(purple.selected, plain.selected);
        // Purple is dark enough to carry white ink.
        assert_eq!(purple.on_accent, rgb(0xffffff));
        // Default keeps the palette's own accent untouched.
        assert_eq!(
            Theme::resolve(ThemeMode::Light, WindowAppearance::Dark, AccentColor::Default).accent,
            plain.accent
        );
    }
}

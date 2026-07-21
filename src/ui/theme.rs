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

use filex::settings::ThemeMode;

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
        }
    }

    /// Resolve a [`ThemeMode`] against the current window appearance.
    /// `System` follows the OS (vibrant variants collapse onto their
    /// plain counterparts).
    pub fn resolve(mode: ThemeMode, appearance: WindowAppearance) -> Self {
        match mode {
            ThemeMode::Light => Self::light(),
            ThemeMode::Dark => Self::dark(),
            ThemeMode::System => match appearance {
                WindowAppearance::Light | WindowAppearance::VibrantLight => Self::light(),
                WindowAppearance::Dark | WindowAppearance::VibrantDark => Self::dark(),
            },
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

    #[test]
    fn explicit_modes_ignore_appearance() {
        assert_eq!(Theme::resolve(ThemeMode::Light, WindowAppearance::Dark), Theme::light());
        assert_eq!(Theme::resolve(ThemeMode::Dark, WindowAppearance::Light), Theme::dark());
    }

    #[test]
    fn system_follows_appearance() {
        assert_eq!(Theme::resolve(ThemeMode::System, WindowAppearance::Light), Theme::light());
        assert_eq!(
            Theme::resolve(ThemeMode::System, WindowAppearance::VibrantLight),
            Theme::light()
        );
        assert_eq!(Theme::resolve(ThemeMode::System, WindowAppearance::Dark), Theme::dark());
        assert_eq!(
            Theme::resolve(ThemeMode::System, WindowAppearance::VibrantDark),
            Theme::dark()
        );
    }
}

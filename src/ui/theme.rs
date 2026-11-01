//! The color theme: a flat set of semantic color slots every component
//! styles itself from, so no component references a raw hex value. Two
//! palettes ship, [`Theme::dark`] and [`Theme::light`]. The active theme
//! lives in GPUI's global state, reachable via [`ActiveTheme::theme`]; the
//! workspace swaps it when the setting or OS appearance changes.

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

/// Shared list metrics. Comfortable gives names and thumbnails room to breathe.
const COMFORTABLE_ROW_HEIGHT: f32 = 36.;
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
    0.299 * c.red + 0.587 * c.green + 0.114 * c.blue
}

fn with_alpha(c: Rgba, a: f32) -> Rgba {
    Rgba { alpha: a, ..c }
}

/// `c` moved fraction `t` of the way toward white: a filled button's
/// hover (see [`super::button`]).
pub fn lift(c: Rgba, t: f32) -> Rgba {
    Rgba {
        alpha: c.alpha,
        ..blend(c, rgb(0xffffff), t)
    }
}

/// `c` moved fraction `t` of the way toward black: a filled button while
/// pressed.
pub fn sink(c: Rgba, t: f32) -> Rgba {
    Rgba {
        alpha: c.alpha,
        ..blend(c, rgb(0x000000), t)
    }
}

/// How far the shell's top-left glow leans toward the accent: enough to
/// read as a tint under the glass, not as a coloured window. Light stays
/// low so `text_dim` sidebar labels at the top left keep 4.5:1.
const LIGHT_GLOW_MIX: f32 = 0.06;
const DARK_GLOW_MIX: f32 = 0.12;

/// Default backdrop blur (px) behind glass surfaces.
const GLASS_BLUR: f32 = 28.;

/// Composite `over` onto `base` at fraction `t` (opaque result).
fn blend(base: Rgba, over: Rgba, t: f32) -> Rgba {
    Rgba::new(
        base.red + (over.red - base.red) * t,
        base.green + (over.green - base.green) * t,
        base.blue + (over.blue - base.blue) * t,
        1.0,
    )
}

/// A complete set of semantic colors. Cheap to copy (a handful of
/// `Rgba` = a few floats each), so components take it by value or borrow
/// freely without worrying about the cost.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Theme {
    /// Window / main content background.
    pub bg: Rgba,
    /// The window shell: the root paints it once and the title/tab bar,
    /// nav bar, sidebar and status bar sit on it unfilled, with the
    /// content canvas ([`Theme::bg`]) raised on top.
    pub panel: Rgba,
    /// Colour of the canvas's soft drop shadow ([`super::soft_shadow`]):
    /// a faint cool tint on light, deeper on dark where it has to read
    /// against a near-black shell.
    pub shadow: Rgba,
    /// Floating surfaces — menus, dialogs, tooltips: one step *above* the
    /// canvas (white on light, lighter graphite on dark), so they read as
    /// raised rather than as holes cut into the content.
    pub elevated: Rgba,
    /// Hover background for interactive rows and buttons. Translucent, so
    /// the same slot reads on both the shell and the canvas.
    pub hover: Rgba,
    /// Fill of text fields (path, search) and segmented-control tracks:
    /// a translucent recess, so fields need no outline at rest.
    pub field: Rgba,
    /// Background of a selected list row (accent-tinted, Finder-style).
    pub selected: Rgba,
    /// Subtle inset surfaces, such as unselected grid cards.
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
    /// Warnings and errors: failed roots, missing permissions, error rings
    /// on fields. Not for destructive actions; those are
    /// [`Theme::danger`].
    pub warn: Rgba,
    /// Success / healthy state.
    pub success: Rgba,
    /// Destructive actions: Danger buttons, destructive menu rows and a
    /// destructive dialog's glyph. Red, so it reads as loss rather than
    /// as a warning.
    pub danger: Rgba,
    /// Label ink on a [`Theme::danger`] fill (white on light, dark ink on
    /// the lighter dark-theme red, as [`Theme::on_accent`] does).
    pub on_danger: Rgba,
    /// Tile behind a destructive dialog's glyph.
    pub danger_tint: Rgba,
    /// Fill of Secondary buttons: a pressable surface on any background.
    pub control: Rgba,
    /// [`Theme::control`] under the pointer.
    pub control_hover: Rgba,
    /// 1px edge of a [`Theme::control`] fill.
    pub control_border: Rgba,
    /// The recessed band behind a dialog's buttons, a step below
    /// [`Theme::elevated`].
    pub dialog_footer: Rgba,
    /// The full-window dim behind a modal dialog. Never blurred.
    pub scrim: Rgba,
    /// Glass: the translucent fill of floating surfaces (menus, dialogs,
    /// tooltips). It tints a blurred copy of what's behind it
    /// ([`Theme::blur`]); with transparency off it is just
    /// [`Theme::elevated`].
    pub glass: Rgba,
    /// Hairline around a glass surface.
    pub glass_edge: Rgba,
    /// Specular top edge of raised and glass surfaces, painted as a 1px
    /// inset highlight ([`super::sheen`]) so they read as lit from above.
    pub sheen: Rgba,
    /// Shadow under floating surfaces: deeper than [`Theme::shadow`], which
    /// only has to lift the canvas off the shell.
    pub float_shadow: Rgba,
    /// The accent-tinted colour the shell gradient fades from at its top
    /// left corner, so the chrome has depth instead of one flat grey.
    /// Equal to [`Theme::panel`] when the shell must stay flat (OLED).
    pub glow: Rgba,
    /// Backdrop blur radius (px) behind glass. 0 when transparency is off:
    /// no blur pass is emitted and [`Theme::glass`] is opaque.
    pub blur: f32,
    /// List row height (px). Set from the density setting, not the
    /// palette — a metric carried alongside the colors so every component
    /// that already takes `&Theme` gets it without a second parameter.
    pub row_height: f32,
    /// List icon-cell edge (px). Density-driven, like [`Theme::row_height`].
    pub icon_size: f32,
}

impl Global for Theme {}

impl Theme {
    /// Neutral graphite: a deep shell with the content canvas lifted one
    /// step above it.
    pub fn dark() -> Self {
        Self {
            bg: rgb(0x1c1d21),
            panel: rgb(0x131416),
            elevated: rgb(0x26272c),
            shadow: rgba(0x00000052),
            hover: rgba(0xffffff12),
            field: rgba(0xffffff0d),
            selected: rgb(0x203a4a),
            stripe: rgb(0x222328),
            border: rgb(0x2a2c31),
            text: rgb(0xececef),
            text_dim: rgb(0x8f939c),
            accent: rgb(0x5ac8fa),
            // The light-cyan accent reads as a "light" fill, so dark ink
            // sits on it (this was the old `BG` the primary button used).
            on_accent: rgb(0x131416),
            accent_selection: rgba(0x5ac8fa40),
            warn: rgb(0xe5c07b),
            success: rgb(0x7ec699),
            danger: rgb(0xf26b63),
            on_danger: rgb(0x1a0d0c),
            danger_tint: rgb(0x3d2321),
            control: rgb(0x34353b),
            control_hover: rgb(0x3c3d44),
            control_border: rgb(0x45474e),
            dialog_footer: rgb(0x202126),
            scrim: rgba(0x00000080),
            glass: rgba(0x2a2b31c7),
            glass_edge: rgba(0xffffff1c),
            sheen: rgba(0xffffff14),
            float_shadow: rgba(0x00000080),
            glow: rgb(0x131416),
            blur: GLASS_BLUR,
            row_height: COMFORTABLE_ROW_HEIGHT,
            icon_size: COMFORTABLE_ICON_SIZE,
        }
        .with_glow(DARK_GLOW_MIX)
    }

    /// The light palette: a white canvas raised on a cool grey shell, and
    /// a cyan-blue accent deepened enough to carry white text on a fill.
    pub fn light() -> Self {
        Self {
            bg: rgb(0xffffff),
            panel: rgb(0xedf0f4),
            elevated: rgb(0xffffff),
            shadow: rgba(0x1018280f),
            hover: rgba(0x1018280f),
            field: rgba(0x1018280b),
            selected: rgb(0xe0effa),
            stripe: rgb(0xf7f8fa),
            border: rgb(0xdfe3e9),
            text: rgb(0x1a2230),
            text_dim: rgb(0x5f6878),
            accent: rgb(0x087eaf),
            on_accent: rgb(0xffffff),
            accent_selection: rgba(0x087eaf3d),
            warn: rgb(0x9a5f08),
            success: rgb(0x2ba55a),
            danger: rgb(0xc4312b),
            on_danger: rgb(0xffffff),
            danger_tint: rgb(0xfde8e7),
            control: rgb(0xffffff),
            control_hover: rgb(0xf4f6f8),
            control_border: rgb(0xd0d5dd),
            dialog_footer: rgb(0xf7f8fa),
            scrim: rgba(0x1018284d),
            glass: rgba(0xfcfdfed6),
            glass_edge: rgba(0x1018281a),
            sheen: rgba(0xffffffd9),
            float_shadow: rgba(0x10182824),
            glow: rgb(0xedf0f4),
            blur: GLASS_BLUR,
            row_height: COMFORTABLE_ROW_HEIGHT,
            icon_size: COMFORTABLE_ICON_SIZE,
        }
        .with_glow(LIGHT_GLOW_MIX)
    }

    /// A true-black variant of the dark palette for OLED screens: pure
    /// black content so unlit pixels stay off, with the chrome lifted just
    /// enough that panels separate from the void.
    pub fn oled() -> Self {
        Self {
            bg: rgb(0x000000),
            panel: rgb(0x000000),
            elevated: rgb(0x151517),
            shadow: rgba(0x00000000),
            hover: rgba(0xffffff14),
            field: rgba(0xffffff0f),
            selected: rgb(0x1f3a4d),
            stripe: rgb(0x070708),
            border: rgb(0x242529),
            text: rgb(0xe6e8ec),
            text_dim: rgb(0x9aa5b3),
            accent: rgb(0x5ac8fa),
            on_accent: rgb(0x000000),
            accent_selection: rgba(0x5ac8fa40),
            warn: rgb(0xe5c07b),
            success: rgb(0x7ec699),
            danger: rgb(0xf26b63),
            on_danger: rgb(0x1a0d0c),
            danger_tint: rgb(0x2e1716),
            control: rgb(0x1f2023),
            control_hover: rgb(0x292a2e),
            control_border: rgb(0x3a3b40),
            dialog_footer: rgb(0x0e0e10),
            scrim: rgba(0x000000a6),
            glass: rgba(0x151517d9),
            glass_edge: rgba(0xffffff1c),
            sheen: rgba(0xffffff12),
            float_shadow: rgba(0x00000000),
            // No glow: unlit pixels stay off.
            glow: rgb(0x000000),
            blur: GLASS_BLUR,
            row_height: COMFORTABLE_ROW_HEIGHT,
            icon_size: COMFORTABLE_ICON_SIZE,
        }
    }

    /// Lean [`Theme::glow`] `mix` of the way from the shell toward the
    /// accent.
    fn with_glow(mut self, mix: f32) -> Self {
        self.glow = blend(self.panel, self.accent, mix);
        self
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
        // Re-tint the shell glow, unless this palette keeps a flat shell.
        if self.glow != self.panel {
            let glow_mix = if light_base {
                LIGHT_GLOW_MIX
            } else {
                DARK_GLOW_MIX
            };
            self = self.with_glow(glow_mix);
        }
        self
    }

    /// Whether this is a dark palette (dark or OLED): icon artwork has a
    /// dimmed variant for it.
    pub fn is_dark(&self) -> bool {
        luminance(self.bg) < 0.5
    }

    /// Apply the transparency setting. Off makes glass the opaque
    /// [`Theme::elevated`] and drops the backdrop blur, for readability or
    /// to spare a weak GPU the blur pass.
    pub fn with_transparency(mut self, enabled: bool) -> Self {
        if !enabled {
            self.glass = self.elevated;
            self.blur = 0.;
        }
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

/// Fixed palettes: identity colours that mean the same thing on every
/// theme, so they live outside [`Theme`]. Kept here so this file stays
/// the only place in the UI that spells a hex value.
pub mod fixed {
    use gpui::{Rgba, rgb, rgba};

    use filex::tags::TagColor;

    /// The Filex mark's light and deep blues (`assets/branding/filex.svg`),
    /// for brand artwork that must not follow the user's accent.
    pub fn brand_sky() -> Rgba {
        rgb(0x28bdfc)
    }

    pub fn brand_deep() -> Rgba {
        rgb(0x006ddc)
    }

    /// Magic mode's colour, paired with [`brand_sky`] in its gradients.
    pub fn magic_violet() -> Rgba {
        rgb(0x8b5cf6)
    }

    /// `color` at `alpha` opacity.
    pub fn with_alpha(mut color: Rgba, alpha: f32) -> Rgba {
        color.alpha = alpha;
        color
    }

    /// The close caption button's hover fill — the system red every
    /// Windows 11 window uses, so it stays fixed across themes.
    pub fn caption_close() -> Rgba {
        rgb(0xc42b1c)
    }

    /// Glyph ink on [`caption_close`].
    pub fn white() -> Rgba {
        rgb(0xffffff)
    }

    /// The faint lit top edge of a filled (accent / danger) button: white
    /// light catching the rim, whatever the fill colour.
    pub fn gloss() -> Rgba {
        rgba(0xffffff33)
    }

    /// The darker bottom lip of a filled button, so it sits on the
    /// surface instead of floating as a flat chip.
    pub fn lip() -> Rgba {
        rgba(0x0000001f)
    }

    /// Solid dot colour for each Finder tag colour. Vivid and
    /// theme-independent, like Finder's.
    pub fn tag(color: TagColor) -> Rgba {
        rgb(match color {
            TagColor::Grey => 0x8e8e93,
            TagColor::Green => 0x5bd15b,
            TagColor::Purple => 0xcb6ce6,
            TagColor::Blue => 0x2f95ff,
            TagColor::Yellow => 0xf5c518,
            TagColor::Red => 0xfb5850,
            TagColor::Orange => 0xf7a53b,
        })
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
        Theme::resolve(
            ThemeMode::Light,
            WindowAppearance::Dark,
            AccentColor::Default,
        )
    }

    #[test]
    fn explicit_modes_ignore_appearance() {
        assert_eq!(
            Theme::resolve(
                ThemeMode::Light,
                WindowAppearance::Dark,
                AccentColor::Default
            ),
            Theme::light()
        );
        assert_eq!(
            Theme::resolve(
                ThemeMode::Dark,
                WindowAppearance::Light,
                AccentColor::Default
            ),
            Theme::dark()
        );
        assert_eq!(
            Theme::resolve(
                ThemeMode::Oled,
                WindowAppearance::Light,
                AccentColor::Default
            ),
            Theme::oled()
        );
    }

    #[test]
    fn system_follows_appearance() {
        assert_eq!(
            Theme::resolve(
                ThemeMode::System,
                WindowAppearance::Light,
                AccentColor::Default
            ),
            Theme::light()
        );
        assert_eq!(
            Theme::resolve(
                ThemeMode::System,
                WindowAppearance::Dark,
                AccentColor::Default
            ),
            Theme::dark()
        );
    }

    #[test]
    fn oled_background_is_pure_black() {
        assert_eq!(Theme::oled().bg, rgb(0x000000));
    }

    /// Floating surfaces must sit above the canvas, not below it: on a
    /// dark base that means lighter, on a light base at least as light.
    #[test]
    fn elevated_surfaces_rise_above_the_canvas() {
        for theme in [Theme::dark(), Theme::oled()] {
            assert!(luminance(theme.elevated) > luminance(theme.bg));
        }
        let light = Theme::light();
        assert!(luminance(light.elevated) >= luminance(light.bg));
        assert!(luminance(light.elevated) > luminance(light.panel));
    }

    #[test]
    fn dark_and_oled_are_dark() {
        assert!(Theme::dark().is_dark() && Theme::oled().is_dark());
        assert!(!Theme::light().is_dark());
    }

    #[test]
    fn compact_density_shrinks_rows() {
        let comfy = Theme::dark().with_density(Density::Comfortable);
        let compact = Theme::dark().with_density(Density::Compact);
        assert!(compact.row_height < comfy.row_height);
        assert!(compact.icon_size < comfy.icon_size);
    }

    #[test]
    fn transparency_off_makes_glass_opaque() {
        for theme in [Theme::light(), Theme::dark(), Theme::oled()] {
            assert!(theme.glass.alpha < 1.0 && theme.blur > 0.);
            let flat = theme.with_transparency(false);
            assert_eq!(flat.glass, theme.elevated);
            assert_eq!(flat.blur, 0.);
            assert_eq!(theme.with_transparency(true), theme);
        }
    }

    /// The shell glow leans toward the accent and follows an accent
    /// override; OLED keeps a flat black shell either way.
    #[test]
    fn shell_glow_follows_the_accent_except_on_oled() {
        let light = Theme::light();
        assert_ne!(light.glow, light.panel);
        let green = Theme::resolve(
            ThemeMode::Light,
            WindowAppearance::Light,
            AccentColor::Green,
        );
        assert_ne!(green.glow, light.glow);
        assert!(green.glow.green > green.glow.red);
        let oled = Theme::resolve(ThemeMode::Oled, WindowAppearance::Dark, AccentColor::Pink);
        assert_eq!(oled.glow, oled.panel);
    }

    #[test]
    fn lift_brightens_and_keeps_alpha() {
        let lifted = lift(rgba(0x00000080), 0.5);
        assert_eq!((lifted.red, lifted.alpha), (0.5, rgba(0x00000080).alpha));
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
        assert_eq!(
            accent_rgb(AccentColor::Custom(0x123456)),
            Some(rgb(0x123456))
        );
    }

    /// Pins the colour arithmetic across GPUI colour-type changes
    /// (gpui-ce moved `Rgba`/`Hsla` onto `palette`).
    #[test]
    fn color_math_is_channelwise() {
        let grey = blend(rgb(0x000000), rgb(0xffffff), 0.5);
        assert_eq!(
            (grey.red, grey.green, grey.blue, grey.alpha),
            (0.5, 0.5, 0.5, 1.0)
        );
        assert_eq!(with_alpha(rgb(0xffffff), 0.25).alpha, 0.25);
        assert!((luminance(rgb(0xffffff)) - 1.0).abs() < 1e-6);
        assert_eq!(luminance(rgb(0x000000)), 0.0);
    }

    #[test]
    fn accent_override_recolors_and_stays_legible() {
        let plain = light();
        let purple = Theme::resolve(
            ThemeMode::Light,
            WindowAppearance::Dark,
            AccentColor::Purple,
        );
        // The accent changed and dragged the selection tints with it.
        assert_ne!(purple.accent, plain.accent);
        assert_eq!(purple.accent, accent_rgb(AccentColor::Purple).unwrap());
        assert_ne!(purple.selected, plain.selected);
        // Purple is dark enough to carry white ink.
        assert_eq!(purple.on_accent, rgb(0xffffff));
        // Default keeps the palette's own accent untouched.
        assert_eq!(
            Theme::resolve(
                ThemeMode::Light,
                WindowAppearance::Dark,
                AccentColor::Default
            )
            .accent,
            plain.accent
        );
    }

    fn assert_contrast(name: &str, fg: Rgba, bg: Rgba, min: f32) {
        let ratio = super::super::icon_palette::contrast(fg, bg);
        assert!(ratio >= min, "{name}: {ratio:.2} < {min}");
    }

    /// Text on the surfaces it lands on reads at 4.5:1 in every palette.
    /// Regression: light `text_dim` was 4.35:1 on the shell, `warn` text
    /// 3.2:1, and white on the amber Danger button 3.2:1.
    #[test]
    fn text_and_fills_meet_contrast_targets() {
        for (label, theme) in [
            ("light", Theme::light()),
            ("dark", Theme::dark()),
            ("oled", Theme::oled()),
        ] {
            let pairs = [
                ("text_dim on bg", theme.text_dim, theme.bg),
                ("text_dim on panel", theme.text_dim, theme.panel),
                ("text_dim on glow", theme.text_dim, theme.glow),
                (
                    "text_dim on dialog_footer",
                    theme.text_dim,
                    theme.dialog_footer,
                ),
                ("warn on bg", theme.warn, theme.bg),
                ("danger on elevated", theme.danger, theme.elevated),
                ("on_danger on danger", theme.on_danger, theme.danger),
                ("text on control", theme.text, theme.control),
                ("text on control_hover", theme.text, theme.control_hover),
                ("on_accent on accent", theme.on_accent, theme.accent),
            ];
            for (name, fg, bg) in pairs {
                assert_contrast(&format!("{label} {name}"), fg, bg, 4.5);
            }
            // A glyph, not text: 3:1.
            assert_contrast(
                &format!("{label} danger on danger_tint"),
                theme.danger,
                theme.danger_tint,
                3.0,
            );
        }
    }

    /// Sidebar labels stay legible on the shell glow whichever accent
    /// preset tints it.
    #[test]
    fn glow_keeps_labels_legible_for_every_preset() {
        for accent in ACCENT_PRESETS {
            for mode in [ThemeMode::Light, ThemeMode::Dark] {
                let theme = Theme::resolve(mode, WindowAppearance::Light, accent);
                assert_contrast(
                    &format!("{accent:?} {mode:?} text_dim on glow"),
                    theme.text_dim,
                    theme.glow,
                    4.5,
                );
            }
        }
    }

    #[test]
    fn sink_darkens_and_keeps_alpha() {
        let sunk = sink(rgba(0xffffff80), 0.5);
        assert_eq!((sunk.red, sunk.alpha), (0.5, rgba(0xffffff80).alpha));
    }
}

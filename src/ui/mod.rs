//! Reusable UI building blocks for the filex app.
//!
//! Each submodule is a small, self-contained component (or family of
//! components) styled from the shared [`theme`] palette. The rule of
//! thumb (see docs/roadmap.md, "reusable components" principle): render
//! code that appears in more than one place, or that would push a
//! workspace render method past a screenful, belongs here — while
//! state and business logic stay in the workspace / lib.

pub mod assets;
pub mod button;
pub mod details;
pub mod fonts;
pub mod grid;
pub mod icon;
pub mod icon_art;
pub mod icon_palette;
pub mod job;
pub mod kbd;
pub mod list_row;
pub mod magic_card;
pub mod menu;
pub mod modal;
pub mod motion;
pub mod onboarding;
pub mod pane;
pub mod scrollbar;
pub mod search_input;
pub mod settings_pane;
pub mod sidebar;
pub mod status_bar;
pub mod tabs;
pub mod theme;
pub mod tooltip;
pub mod top_bar;
pub mod update_banner;
pub mod window_controls;

use gpui::{Background, BoxShadow, Div, Styled, div, linear_color_stop, linear_gradient, px};

/// The window shell's fill: a diagonal wash from the accent-tinted
/// [`theme::Theme::glow`] at the top left into the plain shell, so the
/// chrome reads as a lit material rather than one flat grey. One gradient
/// quad per frame — no blur, nothing per-row.
pub fn shell_backdrop(theme: &theme::Theme) -> Background {
    linear_gradient(
        135.,
        linear_color_stop(theme.glow, 0.),
        linear_color_stop(theme.panel, 0.55),
    )
}

/// The 1px specular top edge of a raised or glass surface: an inset
/// highlight, painted inside the box over its fill.
pub fn sheen(theme: &theme::Theme) -> BoxShadow {
    BoxShadow::new(px(0.), px(1.), gpui::rgb_to_hsla(theme.sheen))
        .blur_radius(px(0.))
        .inset()
}

/// Soft, wide, low-opacity layers for the content canvas: a tight contact
/// shadow plus a broad ambient one that lifts it off the shell without the
/// hard bottom edge of gpui's stacked `shadow_sm`, and the lit top edge.
pub fn soft_shadow(theme: &theme::Theme) -> Vec<BoxShadow> {
    let shadow = gpui::rgb_to_hsla(theme.shadow);
    vec![
        BoxShadow::new(px(0.), px(1.), shadow).blur_radius(px(3.)),
        BoxShadow::new(px(0.), px(8.), shadow)
            .blur_radius(px(24.))
            .spread_radius(px(-4.)),
        sheen(theme),
    ]
}

/// A selected control raised off its track (active tab, active segment,
/// focused field): a small contact shadow and the lit top edge.
pub fn raised(theme: &theme::Theme) -> Vec<BoxShadow> {
    vec![
        BoxShadow::new(px(0.), px(1.), gpui::rgb_to_hsla(theme.shadow)).blur_radius(px(2.)),
        sheen(theme),
    ]
}

/// The glass material for anything that floats over content: a translucent
/// tint over a blurred copy of what's behind it (gpui-ce's in-window
/// `backdrop_blur`, so the window itself stays opaque and keeps ClearType),
/// a hairline edge, a lit top edge and a deep, soft drop shadow. With
/// transparency off ([`theme::Theme::blur`] = 0) it is an opaque card.
///
/// Callers set the shape (radius) themselves; the blur follows it.
pub fn glass<E: Styled>(element: E, theme: &theme::Theme) -> E {
    let shadow = gpui::rgb_to_hsla(theme.float_shadow);
    let element = element
        .border_1()
        .border_color(theme.glass_edge)
        .bg(theme.glass)
        .shadow(vec![
            BoxShadow::new(px(0.), px(12.), shadow)
                .blur_radius(px(32.))
                .spread_radius(px(-8.)),
            BoxShadow::new(px(0.), px(2.), shadow)
                .blur_radius(px(6.))
                .spread_radius(px(-2.)),
            sheen(theme),
        ]);
    if theme.blur > 0. {
        element.backdrop_blur(px(theme.blur))
    } else {
        element
    }
}

/// A transient popover over the browse view — context and dropdown menus:
/// [`glass`] at the floating-card radius. Callers chain their own `.id()`,
/// width, padding and gap.
pub fn card(theme: &theme::Theme) -> Div {
    glass(div().rounded_xl().flex().flex_col(), theme)
}

/// [`card`] made of opaque glass: same edge, lit top and shadow on the
/// solid [`theme::Theme::elevated`] fill, with no blur pass. For dialogs
/// (the scrim behind them leaves nothing worth frosting, and their fields
/// need a steady surface) and for cards that hold their own fills.
pub fn solid_card(theme: &theme::Theme) -> Div {
    card(&theme.with_transparency(false))
}

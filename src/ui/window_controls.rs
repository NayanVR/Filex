//! Minimize / maximize / close caption buttons for windows whose system
//! titlebar is hidden, so the tab strip can be the titlebar.
//!
//! Each button is tagged with a [`WindowControlArea`], so the platform
//! layer answers the OS hit-test for it: on Windows that maps to
//! `HTMINBUTTON` / `HTMAXBUTTON` / `HTCLOSE`, which keeps native behaviour
//! (Snap Layouts on hover over maximize, click handling by the OS). The
//! workspace only renders these where `platform::draws_own_caption()`
//! says the system bar is hidden.

use gpui::{Div, Rgba, Stateful, WindowControlArea, div, prelude::*, px};

use super::icon;
use super::theme::{Theme, fixed};

/// Caption button width — the Windows 11 metric.
pub const BUTTON_WIDTH: f32 = 46.;

/// The three caption buttons, flush against the bar's trailing edge.
/// `maximized` swaps the maximize glyph for restore.
pub fn caption_buttons(theme: &Theme, maximized: bool) -> Div {
    let restore = if maximized {
        "icons/caption-restore.svg"
    } else {
        "icons/caption-maximize.svg"
    };
    div()
        .flex()
        .flex_none()
        .h_full()
        .child(caption_button(
            theme,
            "caption-min",
            "icons/caption-minimize.svg",
            WindowControlArea::Min,
            theme.hover,
            theme.text,
        ))
        .child(caption_button(
            theme,
            "caption-max",
            restore,
            WindowControlArea::Max,
            theme.hover,
            theme.text,
        ))
        .child(caption_button(
            theme,
            "caption-close",
            "icons/caption-close.svg",
            WindowControlArea::Close,
            fixed::caption_close(),
            fixed::white(),
        ))
}

/// One caption button: full bar height, fixed width, a hover fill. The
/// glyph sits in a named group so it can recolour on hover (white on
/// the close button's red).
fn caption_button(
    theme: &Theme,
    id: &'static str,
    glyph: &'static str,
    area: WindowControlArea,
    hover_bg: Rgba,
    hover_ink: Rgba,
) -> Stateful<Div> {
    div()
        .id(id)
        .group(id)
        .flex()
        .items_center()
        .justify_center()
        .w(px(BUTTON_WIDTH))
        .h_full()
        .window_control_area(area)
        .hover(move |s| s.bg(hover_bg))
        .child(
            icon::ui_icon(glyph, theme.text_dim)
                .size(px(10.))
                .group_hover(id, move |s| s.text_color(hover_ink)),
        )
}

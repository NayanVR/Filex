//! Keyboard shortcut hints shared by the search field and controls.
//!
//! Bindings and editable keyboard settings live in `workspace::shortcuts`.

use gpui::{Div, SharedString, div, prelude::*, px};

use super::theme::Theme;

/// One keycap: a single short label ("/", "⌘", "R", "Esc") in a rounded,
/// bordered box styled from the palette so it reads on light and dark.
pub fn keycap(theme: &Theme, label: impl Into<SharedString>) -> Div {
    div()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .h(px(18.))
        .min_w(px(18.))
        .px(px(5.))
        .rounded(px(5.))
        .border_1()
        .border_color(theme.border)
        .bg(theme.bg)
        .text_xs()
        .text_color(theme.text_dim)
        .child(label.into())
}

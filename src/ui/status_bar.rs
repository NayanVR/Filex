//! The bottom status bar: one line of dim text, split left/right.
//!
//! The strings themselves are built by pure functions here so they can
//! be unit-tested without GPUI; the workspace only counts state and
//! passes numbers in.

use gpui::{Div, SharedString, div, prelude::*, px};

use super::theme::Theme;

/// The bar container: left-aligned message, right-aligned index status.
pub fn status_bar(
    theme: &Theme,
    left: impl Into<SharedString>,
    right: impl Into<SharedString>,
) -> Div {
    div()
        .flex()
        .items_center()
        .justify_between()
        .h(px(30.))
        .flex_none()
        .px_4()
        .border_t_1()
        .border_color(theme.border)
        .bg(theme.panel)
        .text_xs()
        .text_color(theme.text_dim)
        .gap_4()
        .child(div().flex_1().min_w_0().truncate().child(left.into()))
        .child(
            div()
                .max_w(gpui::relative(0.6))
                .truncate()
                .child(right.into()),
        )
}

//! A slim, non-blocking "update available" banner shown above the status
//! bar. What it *says* (message + action label) is decided by pure,
//! unit-tested functions in `filex::update` (`banner_content`); this module
//! only renders those strings, matching the `job.rs` pattern where callers
//! append the interactive controls and chain their own `.on_click`.

use gpui::{Div, SharedString, div, prelude::*, px};

use super::theme::Theme;

/// The banner container: a slim, accent-tinted bar with a top border,
/// laid out as message | action | dismiss. Callers add the children.
pub fn update_banner(theme: &Theme) -> Div {
    div()
        .flex()
        .items_center()
        .gap_2()
        .h(px(28.))
        .px_3()
        .border_t_1()
        .border_color(theme.border)
        .bg(theme.panel)
        .text_xs()
}

/// The message label; takes the remaining width so the buttons sit at the
/// right edge.
pub fn message(theme: &Theme, text: impl Into<SharedString>) -> Div {
    div()
        .flex_1()
        .overflow_hidden()
        .text_color(theme.text)
        .child(text.into())
}

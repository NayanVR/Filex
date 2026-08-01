//! A slim, non-blocking "update available" banner shown above the status
//! bar. What it *says* (message + action label) is decided by pure,
//! unit-tested functions in `filex::update` (`banner_content`); this module
//! only renders those strings, matching the `job.rs` pattern where callers
//! append the interactive controls and chain their own `.on_click`.

use gpui::{Div, ElementId, SharedString, Stateful, div, prelude::*, px};

use super::icon;
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
    div().flex_1().overflow_hidden().text_color(theme.text).child(text.into())
}

/// The primary action pill (Copy / Get update / Restart). Accent fill with
/// legible ink; caller chains `.on_click`.
pub fn action_button(
    theme: &Theme,
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
) -> Stateful<Div> {
    let hover = theme.hover;
    div()
        .id(id)
        .flex()
        .items_center()
        .px_2()
        .py_0p5()
        .rounded_sm()
        .cursor_pointer()
        .bg(theme.accent)
        .text_color(theme.on_accent)
        .hover(move |s| s.bg(hover))
        .child(label.into())
}

/// The dismiss ✕. Caller chains `.on_click`. The glyph is tinted
/// explicitly (gpui `svg()` does not inherit text color).
pub fn dismiss_button(theme: &Theme, id: impl Into<ElementId>) -> Stateful<Div> {
    let hover = theme.hover;
    div()
        .id(id)
        .flex()
        .items_center()
        .p_1()
        .rounded_sm()
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
        .child(icon::ui_icon("icons/x.svg", theme.text_dim).size(px(12.)))
}

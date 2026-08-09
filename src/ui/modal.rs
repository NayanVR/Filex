//! Modal dialog building blocks (conflict prompts, future confirms).
//!
//! Rendered as the root element's last child: the dimmed backdrop
//! covers the window, paints above everything, and soaks up clicks so
//! nothing behind it is interactive while the dialog is open.

use gpui::{Div, ElementId, SharedString, Stateful, div, prelude::*, px, rgba};

use super::theme::Theme;

/// Full-window dimmed backdrop, centering its child (the panel).
/// Callers chain `.on_click` for click-outside-to-cancel.
pub fn backdrop(id: impl Into<ElementId>) -> Stateful<Div> {
    div()
        .id(id)
        // Swallow all mouse events so the browse list behind the dialog
        // does not keep hovering and clicking through the dim overlay.
        .occlude()
        .absolute()
        .inset_0()
        .bg(rgba(0x00000099))
        .flex()
        .items_center()
        .justify_center()
}

/// The dialog card.
pub fn panel(theme: &Theme, id: impl Into<ElementId>) -> Stateful<Div> {
    div()
        .id(id)
        .w(px(400.))
        .p_4()
        .rounded_lg()
        .border_1()
        .border_color(theme.border)
        .bg(theme.panel)
        .shadow_lg()
        .flex()
        .flex_col()
        .gap_2()
}

pub fn title(theme: &Theme, text: impl Into<SharedString>) -> Div {
    div().text_sm().text_color(theme.text).child(text.into())
}

pub fn message(theme: &Theme, text: impl Into<SharedString>) -> Div {
    div()
        .text_xs()
        .text_color(theme.text_dim)
        .child(text.into())
}

/// Right-aligned button row.
pub fn buttons() -> Div {
    div().flex().justify_end().gap_2().pt_2()
}

/// A dialog button; `primary` gets the accent fill (the enter-key
/// choice), others stay outlined.
pub fn button(
    theme: &Theme,
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    primary: bool,
) -> Stateful<Div> {
    let base = div()
        .id(id)
        .px_3()
        .py_1()
        .rounded_md()
        .cursor_pointer()
        .text_sm()
        .child(label.into());
    if primary {
        base.bg(theme.accent)
            .text_color(theme.on_accent)
            .hover(|s| s.opacity(0.9))
    } else {
        let hover = theme.hover;
        base.border_1()
            .border_color(theme.border)
            .text_color(theme.text)
            .hover(move |s| s.bg(hover))
    }
}

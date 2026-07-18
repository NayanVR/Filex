//! The top bar: navigation button, current path, and the search box
//! frame (the input entity itself lives in the workspace).

use gpui::{Div, ElementId, SharedString, Stateful, div, prelude::*, px, rgb};

use super::theme::{ACCENT, BG_HOVER, BG_PANEL, BORDER, TEXT, TEXT_DIM};

/// The bar container; children flow left-to-right.
pub fn top_bar() -> Div {
    div()
        .flex()
        .items_center()
        .gap_2()
        .h(px(40.))
        .px_3()
        .border_b_1()
        .border_color(rgb(BORDER))
        .bg(rgb(BG_PANEL))
}

/// A small glyph button ("↑"). Callers chain `.on_click`.
pub fn toolbar_button(id: impl Into<ElementId>, glyph: &'static str) -> Stateful<Div> {
    div()
        .id(id)
        .px_2()
        .py_1()
        .rounded_md()
        .cursor_pointer()
        .hover(|s| s.bg(rgb(BG_HOVER)))
        .text_color(rgb(TEXT_DIM))
        .child(glyph)
}

/// The current-path label filling the bar's middle.
pub fn path_label(text: impl Into<SharedString>) -> Div {
    div().flex_1().text_sm().text_color(rgb(TEXT_DIM)).overflow_hidden().child(text.into())
}

/// The search box frame; the border lights up while a query is active.
/// The caller adds the input entity as the child.
pub fn search_box(active: bool) -> Div {
    div()
        .flex()
        .items_center()
        .w(px(260.))
        .px_2()
        .py_1()
        .rounded_md()
        .border_1()
        .border_color(rgb(if active { ACCENT } else { BORDER }))
        .text_sm()
        .text_color(rgb(TEXT))
}

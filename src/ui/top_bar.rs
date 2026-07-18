//! The top bar: navigation button, current path, and the search box
//! frame (the input entity itself lives in the workspace).

use gpui::{Div, ElementId, SharedString, Stateful, div, prelude::*, px, rgb};

use super::theme::{ACCENT, BG_HOVER, BG_PANEL, BORDER, TEXT, TEXT_DIM};

/// Bar height. The macOS titlebar is transparent and the traffic
/// lights are inset into this bar, so their position (set at window
/// creation) is derived from it.
pub const TOP_BAR_HEIGHT: f32 = 40.;

/// The bar container; children flow left-to-right.
pub fn top_bar() -> Div {
    let bar = div()
        .flex()
        .items_center()
        .gap_2()
        .h(px(TOP_BAR_HEIGHT))
        .px_3()
        .border_b_1()
        .border_color(rgb(BORDER))
        .bg(rgb(BG_PANEL));
    // Clear the inset traffic lights (unified-titlebar look): the
    // three buttons start at x=12 and span ~52px.
    #[cfg(target_os = "macos")]
    let bar = bar.pl(px(76.));
    bar
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

/// The breadcrumb strip filling the bar's middle. Children are
/// [`breadcrumb_segment`]s interleaved with [`breadcrumb_separator`]s.
pub fn breadcrumbs() -> Div {
    div().flex_1().flex().items_center().gap_1().overflow_hidden()
}

/// One clickable path segment. Callers chain `.on_click` to navigate.
pub fn breadcrumb_segment(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
) -> Stateful<Div> {
    div()
        .id(id)
        .px_1()
        .rounded_sm()
        .cursor_pointer()
        .text_sm()
        .text_color(rgb(TEXT_DIM))
        .hover(|s| s.bg(rgb(BG_HOVER)).text_color(rgb(TEXT)))
        .child(label.into())
}

/// The "›" between segments (also used, without siblings, as the
/// non-clickable "…" that stands in for elided middle segments).
pub fn breadcrumb_separator(glyph: &'static str) -> Div {
    div().text_xs().text_color(rgb(TEXT_DIM)).child(glyph)
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

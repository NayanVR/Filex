//! The top bar: navigation button, current path, and the search box
//! frame (the input entity itself lives in the workspace).

use gpui::{Div, ElementId, SharedString, Stateful, div, prelude::*, px};

use super::theme::Theme;

/// Bar height. The macOS titlebar is transparent and the traffic
/// lights are inset into this bar, so their position (set at window
/// creation) is derived from it.
pub const TOP_BAR_HEIGHT: f32 = 40.;

/// The bar container; children flow left-to-right.
pub fn top_bar(theme: &Theme) -> Div {
    let bar = div()
        .flex()
        .items_center()
        .gap_2()
        .h(px(TOP_BAR_HEIGHT))
        .px_3()
        .border_b_1()
        .border_color(theme.border)
        .bg(theme.panel);
    // Clear the inset traffic lights (unified-titlebar look): the
    // three buttons start at x=12 and span ~52px.
    #[cfg(target_os = "macos")]
    let bar = bar.pl(px(76.));
    bar
}

/// A small glyph button ("↑"). Callers chain `.on_click`.
pub fn toolbar_button(theme: &Theme, id: impl Into<ElementId>, glyph: &'static str) -> Stateful<Div> {
    let hover = theme.hover;
    div()
        .id(id)
        .px_2()
        .py_1()
        .rounded_md()
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
        .text_color(theme.text_dim)
        .child(glyph)
}

/// The breadcrumb strip filling the bar's middle. Children are
/// [`breadcrumb_segment`]s interleaved with [`breadcrumb_separator`]s.
pub fn breadcrumbs() -> Div {
    div().flex_1().flex().items_center().gap_1().overflow_hidden()
}

/// One clickable path segment. Callers chain `.on_click` to navigate.
pub fn breadcrumb_segment(
    theme: &Theme,
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
) -> Stateful<Div> {
    let (hover, text) = (theme.hover, theme.text);
    div()
        .id(id)
        .px_1()
        .rounded_sm()
        .cursor_pointer()
        .text_sm()
        .text_color(theme.text_dim)
        .hover(move |s| s.bg(hover).text_color(text))
        .child(label.into())
}

/// The "›" between segments (also used, without siblings, as the
/// non-clickable "…" that stands in for elided middle segments).
pub fn breadcrumb_separator(theme: &Theme, glyph: &'static str) -> Div {
    div().text_xs().text_color(theme.text_dim).child(glyph)
}

/// The search box frame; the border lights up while a query is active.
/// The caller adds the input entity as the child.
pub fn search_box(theme: &Theme, active: bool) -> Div {
    div()
        .flex()
        .items_center()
        .w(px(260.))
        .px_2()
        .py_1()
        .rounded_md()
        .border_1()
        .border_color(if active { theme.accent } else { theme.border })
        .text_sm()
        .text_color(theme.text)
}

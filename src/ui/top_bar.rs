//! The top bar: navigation button, current path, and the search box
//! frame (the input entity itself lives in the workspace).

use gpui::{Div, ElementId, Rgba, SharedString, Stateful, div, prelude::*, px};

use super::icon;
use super::theme::Theme;

/// Bar height. The macOS titlebar is transparent and the traffic
/// lights are inset into this bar, so their position (set at window
/// creation) is derived from it.
pub const TOP_BAR_HEIGHT: f32 = 48.;

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

/// A small icon button (`icon` is an asset path like
/// `"icons/settings.svg"`). `color` tints the glyph explicitly — gpui's
/// `svg()` does not inherit an ancestor's text color, so pass
/// `theme.accent` here (not via a chained `.text_color`) to mark it
/// active. Callers chain `.on_click`.
pub fn toolbar_button(
    theme: &Theme,
    id: impl Into<ElementId>,
    icon: &'static str,
    color: Rgba,
) -> Stateful<Div> {
    let hover = theme.hover;
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .p_1()
        .rounded_md()
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
        .child(icon::ui_icon(icon, color).size(px(18.)))
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

/// The "›" chevron between breadcrumb segments.
pub fn breadcrumb_chevron(theme: &Theme) -> Div {
    div()
        .flex_none()
        .child(icon::ui_icon("icons/chevron-right.svg", theme.text_dim).size(px(14.)))
}

/// The non-clickable "…" that stands in for elided middle segments.
pub fn breadcrumb_ellipsis(theme: &Theme) -> Div {
    div().text_xs().text_color(theme.text_dim).child("…")
}

/// The search box frame: a magnifier, then the input the caller adds.
/// The border lights up while a query is active.
/// The removable-search-chip strip, shown under the top bar while a query
/// has recognized `key:value` filters. A wrapping row of pills.
pub fn filter_chip_strip(theme: &Theme) -> Div {
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_1()
        .px_3()
        .py_1()
        .bg(theme.panel)
        .border_b_1()
        .border_color(theme.border)
}

/// One removable filter pill: an optional color dot (for `tag:`), the
/// label, and a ✕. The whole pill is the click target — callers chain
/// `.on_click` to remove the filter.
pub fn filter_chip(
    theme: &Theme,
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    dot: Option<Rgba>,
) -> Stateful<Div> {
    let selected = theme.selected;
    let mut chip = div()
        .id(id)
        .flex()
        .items_center()
        .gap_1()
        .px_2()
        .py_1()
        .rounded_full()
        .bg(theme.hover)
        .cursor_pointer()
        .text_xs()
        .text_color(theme.text)
        .hover(move |s| s.bg(selected));
    if let Some(dot) = dot {
        chip = chip.child(div().flex_none().size(px(7.)).rounded_full().bg(dot));
    }
    chip.child(label.into()).child(div().text_color(theme.text_dim).child("✕"))
}

/// The magic-mode toggle that lives at the trailing edge of the search
/// box. `active` lights it in the accent colour and gives it a filled
/// background so the mode reads at a glance. The whole pill is the click
/// target; callers chain `.on_click`.
pub fn magic_toggle(theme: &Theme, active: bool) -> Stateful<Div> {
    let hover = theme.hover;
    let (bg, tint) = if active {
        (theme.selected, theme.accent)
    } else {
        (gpui::transparent_black().into(), theme.text_dim)
    };
    div()
        .id("magic-toggle")
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(22.))
        .rounded_md()
        .bg(bg)
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
        .child(icon::ui_icon("icons/sparkles.svg", tint).size(px(15.)))
}

/// The scope dropdown at the search box's left edge: the current scope
/// label and a chevron. `open` tints it accent so it reads as active
/// while its menu is up. Callers chain `.on_mouse_down` to open the menu.
pub fn scope_selector(theme: &Theme, label: &'static str, open: bool) -> Stateful<Div> {
    let hover = theme.hover;
    let text = if open { theme.accent } else { theme.text_dim };
    div()
        .id("scope-selector")
        .flex()
        .flex_none()
        .items_center()
        .gap_1()
        .pl_1()
        .pr_1p5()
        .py_0p5()
        .rounded_md()
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
        .text_xs()
        .text_color(text)
        .child(label)
        .child(icon::ui_icon("icons/chevron-down.svg", text).size(px(12.)))
}

pub fn search_box(theme: &Theme, active: bool) -> Div {
    div()
        .flex()
        .items_center()
        .gap_2()
        // Fills its (centered) column but never gets absurdly wide, and
        // stays usable when the window is narrow.
        .w_full()
        .min_w(px(160.))
        .max_w(px(420.))
        .px_3()
        .py(px(6.))
        .rounded_md()
        .border_1()
        .border_color(if active { theme.accent } else { theme.border })
        .bg(theme.bg)
        .text_sm()
        .text_color(theme.text)
        // Belt-and-suspenders: the input scrolls its own text, but this
        // guarantees a long query can never paint past the box edge.
        .overflow_hidden()
        .child(icon::ui_icon("icons/search.svg", theme.text_dim).size(px(15.)))
}

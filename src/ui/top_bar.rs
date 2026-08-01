//! The navigation bar (second row): the back/forward/refresh controls,
//! the current path breadcrumbs, and the search box frame (the input
//! entity itself lives in the workspace).
//!
//! The traffic lights and the tab strip live one row *above* this now
//! (see [`super::tabs`]), so this bar no longer insets for them.

use gpui::{Div, ElementId, Rgba, SharedString, Stateful, div, prelude::*, px};

use super::icon;
use super::theme::Theme;

/// Bar height.
pub const TOP_BAR_HEIGHT: f32 = 44.;

/// The bar container; children flow left-to-right.
pub fn top_bar(theme: &Theme) -> Div {
    div()
        .flex()
        .items_center()
        .gap_2()
        .h(px(TOP_BAR_HEIGHT))
        .px_3()
        .border_b_1()
        .border_color(theme.border)
        .bg(theme.panel)
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

/// The removable-search-chip strip, shown under the nav bar while a query
/// has recognized `key:value` filters. A wrapping row of pills, led by a
/// small "Filters" label so the strip reads as a distinct affordance
/// rather than a stray row of buttons.
pub fn filter_chip_strip(theme: &Theme) -> Div {
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_1p5()
        .px_3()
        .py(px(5.))
        .bg(theme.panel)
        .border_b_1()
        .border_color(theme.border)
        .child(
            div()
                .flex_none()
                .mr_1()
                .text_xs()
                .text_color(theme.text_dim)
                .child("Filters"),
        )
}

/// One removable filter pill: an optional color dot (for `tag:`), the
/// label, and a trailing ✕ that reads as a close affordance (it lights up
/// on hover). The whole pill is the click target — callers chain
/// `.on_click` to remove the filter.
pub fn filter_chip(
    theme: &Theme,
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    dot: Option<Rgba>,
) -> Stateful<Div> {
    let (accent, hover, text) = (theme.accent, theme.hover, theme.text);
    let mut chip = div()
        .id(id)
        .group("filter-chip")
        .flex()
        .items_center()
        .gap_1p5()
        .h(px(22.))
        .pl_2()
        .pr_1p5()
        .rounded_full()
        // A tinted fill plus a hairline border reads as a discrete token,
        // not a flat highlight; the border warms to the accent on hover so
        // the whole pill signals "click to remove".
        .bg(theme.accent_selection)
        .border_1()
        .border_color(theme.border)
        .cursor_pointer()
        .text_xs()
        .text_color(text)
        .hover(move |s| s.border_color(accent));
    if let Some(dot) = dot {
        chip = chip.child(div().flex_none().size(px(7.)).rounded_full().bg(dot));
    }
    chip.child(label.into()).child(
        // A rounded hit-slug for the ✕ so it reads as its own close
        // control; it fills in on hover of the surrounding chip group.
        div()
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .size(px(15.))
            .rounded_full()
            .group_hover("filter-chip", move |s| s.bg(hover))
            .child(icon::ui_icon("icons/x.svg", theme.text_dim).size(px(11.))),
    )
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

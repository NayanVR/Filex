//! The topmost bar: the tab strip on the left and the window/view icon
//! group on the right.
//!
//! Presentation only: the workspace owns the tab list and supplies each
//! title + active flag, and wires the click / middle-click / close /
//! new-tab handlers. Always shown — a single tab still gets a chip so the
//! icon group has a stable home and the bar reads the same at one tab or
//! many.

use gpui::{Div, ElementId, SharedString, Stateful, div, prelude::*, px};

use super::icon;
use super::theme::Theme;

/// Height of the tab strip.
pub const TAB_BAR_HEIGHT: f32 = 38.;

/// The bar container. Children are the left tab group (tabs +
/// [`new_tab_button`]) and the right icon group, separated by a
/// flex spacer.
///
/// This is now the topmost bar, so on macOS it — not the nav bar below —
/// carries the inset traffic lights (the system titlebar is transparent);
/// it pads left to clear them.
pub fn tab_bar(theme: &Theme) -> Div {
    let bar = div()
        .flex()
        .items_center()
        .gap_1()
        .h(px(TAB_BAR_HEIGHT))
        .px_2()
        .border_b_1()
        .border_color(theme.border)
        .bg(theme.panel);
    // Clear the inset traffic lights: three buttons from x=12 spanning ~52px.
    #[cfg(target_os = "macos")]
    let bar = bar.pl(px(76.));
    bar
}

/// The left group that scrolls/clips its tabs without pushing the icon
/// group off the bar's right edge.
pub fn tab_group() -> Div {
    div().flex().flex_1().min_w_0().items_center().gap(px(2.)).overflow_hidden()
}

/// One tab: the active one gets the window background (reads as "raised
/// into" the content), the rest sit flat on the panel with hover
/// feedback. Callers add the label + close control and chain handlers.
pub fn tab(theme: &Theme, id: impl Into<ElementId>, active: bool) -> Stateful<Div> {
    let hover = theme.hover;
    let base = div()
        .id(id)
        .flex()
        .items_center()
        .gap_1()
        .h(px(26.))
        .max_w(px(180.))
        .px_2()
        .rounded_md()
        .cursor_pointer()
        .overflow_hidden();
    if active {
        base.bg(theme.bg).text_color(theme.text)
    } else {
        base.text_color(theme.text_dim).hover(move |s| s.bg(hover))
    }
}

/// A tab's label (clips at the tab's max width).
pub fn tab_label(name: impl Into<SharedString>) -> Div {
    div().flex_1().text_sm().overflow_hidden().child(name.into())
}

/// The little ✕ on a tab; callers chain `.on_click` (and should stop
/// propagation so closing doesn't also select the tab).
pub fn tab_close(theme: &Theme, id: impl Into<ElementId>) -> Stateful<Div> {
    let hover = theme.hover;
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .flex_none()
        .rounded_sm()
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
        .child(icon::ui_icon("icons/x.svg", theme.text_dim).size(px(12.)))
}

/// The "+" button that opens a new tab.
pub fn new_tab_button(theme: &Theme, id: impl Into<ElementId>) -> Stateful<Div> {
    let hover = theme.hover;
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .flex_none()
        .size(px(24.))
        .rounded_md()
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
        .child(icon::ui_icon("icons/plus.svg", theme.text_dim).size(px(16.)))
}

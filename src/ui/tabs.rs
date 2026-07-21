//! The tab strip under the top bar (block 6).
//!
//! Presentation only: the workspace owns the tab list and supplies each
//! title + active flag, and wires the click / middle-click / close /
//! new-tab handlers. Shown only when more than one tab is open.

use gpui::{Div, ElementId, SharedString, Stateful, div, prelude::*, px};

use super::icon;
use super::theme::Theme;

/// Height of the tab strip.
pub const TAB_BAR_HEIGHT: f32 = 34.;

/// The strip container; children are [`tab`]s then the [`new_tab_button`].
pub fn tab_bar(theme: &Theme) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(2.))
        .h(px(TAB_BAR_HEIGHT))
        .px_2()
        .border_b_1()
        .border_color(theme.border)
        .bg(theme.panel)
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

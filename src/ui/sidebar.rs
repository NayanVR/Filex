//! Sidebar building blocks: the panel container, section headers, and
//! the hoverable row scaffold. Row content (place/state icons via
//! [`super::icon`], labels) is supplied by the caller.
//!
//! Every sidebar entry (places, indexed roots, action rows, banners)
//! starts from [`sidebar_row`] so hover feedback and spacing stay
//! uniform; callers chain their content and `.on_click`, plus style
//! overrides (dim text for action rows, smaller warn text for banners).

use gpui::{Div, ElementId, SharedString, Stateful, div, prelude::*, px};

use super::icon;
use super::theme::Theme;

/// Sidebar width. One place so panels/overlays can align to it later.
pub const SIDEBAR_WIDTH: f32 = 200.;

/// The sidebar panel itself: fixed-width column with panel background
/// and a right border. Children are headers and rows, top-down.
pub fn sidebar_panel(theme: &Theme) -> Div {
    div()
        .flex()
        .flex_col()
        .w(px(SIDEBAR_WIDTH))
        .h_full()
        .py_2()
        .border_r_1()
        .border_color(theme.border)
        .bg(theme.panel)
}

/// A clickable section header with a disclosure chevron (down when
/// expanded, right when collapsed). The whole header toggles; callers
/// render the section's rows only when `!collapsed`.
pub fn collapsible_header(
    theme: &Theme,
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    collapsed: bool,
) -> Stateful<Div> {
    let (hover, text) = (theme.hover, theme.text);
    let chevron = if collapsed { "icons/chevron-right.svg" } else { "icons/chevron-down.svg" };
    div()
        .id(id)
        .flex()
        .items_center()
        .gap_1()
        .mx_2()
        .px_1()
        .pt_3()
        .pb_1()
        .rounded_md()
        .cursor_pointer()
        .text_xs()
        .text_color(theme.text_dim)
        .hover(move |s| s.text_color(text).bg(hover))
        .child(icon::ui_icon(chevron, theme.text_dim).size(px(11.)))
        .child(label.into())
}

/// The common sidebar row: inset, rounded, hover-highlighted.
pub fn sidebar_row(theme: &Theme, id: impl Into<ElementId>) -> Stateful<Div> {
    let hover = theme.hover;
    div()
        .id(id)
        .flex()
        .items_center()
        .gap_2()
        .mx_2()
        .px_2()
        .py_1()
        .rounded_md()
        .cursor_pointer()
        .text_sm()
        // Long labels clip at the sidebar edge instead of painting
        // over the neighboring pane.
        .overflow_hidden()
        .hover(move |s| s.bg(hover))
}

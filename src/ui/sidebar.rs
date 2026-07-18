//! Sidebar building blocks: the panel container, section headers, the
//! hoverable row scaffold, and the root state marker.
//!
//! Every sidebar entry (places, indexed roots, action rows, banners)
//! starts from [`sidebar_row`] so hover feedback and spacing stay
//! uniform; callers chain their content and `.on_click`, plus style
//! overrides (dim text for action rows, smaller warn text for banners).

use gpui::{Div, ElementId, SharedString, Stateful, div, prelude::*, px, rgb};

use super::theme::{BG_HOVER, BG_PANEL, BORDER, TEXT_DIM};

/// Sidebar width. One place so panels/overlays can align to it later.
pub const SIDEBAR_WIDTH: f32 = 200.;

/// The sidebar panel itself: fixed-width column with panel background
/// and a right border. Children are headers and rows, top-down.
pub fn sidebar_panel() -> Div {
    div()
        .flex()
        .flex_col()
        .w(px(SIDEBAR_WIDTH))
        .h_full()
        .py_2()
        .border_r_1()
        .border_color(rgb(BORDER))
        .bg(rgb(BG_PANEL))
}

/// An uppercase section header ("PLACES", "INDEXED"). Sections after
/// the first chain `.pt_3()` to space themselves from the one above.
pub fn section_header(label: impl Into<SharedString>) -> Div {
    div().px_3().pb_1().text_xs().text_color(rgb(TEXT_DIM)).child(label.into())
}

/// The common sidebar row: inset, rounded, hover-highlighted.
pub fn sidebar_row(id: impl Into<ElementId>) -> Stateful<Div> {
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
        .hover(|s| s.bg(rgb(BG_HOVER)))
}

/// The small state marker at the start of a root row (building / ready
/// / failed / service-managed). `color` is a `theme` constant.
pub fn root_marker(glyph: &'static str, color: u32) -> Div {
    div().text_xs().text_color(rgb(color)).child(glyph)
}

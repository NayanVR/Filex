//! Row scaffold shared by the browse and search-result lists, plus the
//! clickable column header above the browse list.

use gpui::{Div, ElementId, SharedString, Stateful, div, prelude::*, px};

use super::theme::Theme;

/// Fixed row height — `uniform_list` requires every row equal-height,
/// so this constant is the single place it's defined.
pub const ROW_HEIGHT: f32 = 28.;

/// Width of the Modified column (rows and header must agree).
pub const MODIFIED_COL_WIDTH: f32 = 64.;
/// Width of the Size column (rows and header must agree).
pub const SIZE_COL_WIDTH: f32 = 72.;

/// A right-aligned fixed-width detail cell (Modified / Size columns).
pub fn detail_cell(theme: &Theme, width: f32, text: impl Into<SharedString>) -> Div {
    div()
        .w(px(width))
        .flex_none()
        .text_xs()
        .text_color(theme.text_dim)
        .text_right()
        .child(text.into())
}

/// The header line above the browse list. Children are
/// [`header_cell`]s; the leading spacer keeps them aligned with the
/// icon column below.
pub fn header_row(theme: &Theme, icon_col_width: f32) -> Div {
    div()
        .flex()
        .items_center()
        .gap_2()
        .px_3()
        .h(px(22.))
        .border_b_1()
        .border_color(theme.border)
        .text_xs()
        .text_color(theme.text_dim)
        .child(div().w(px(icon_col_width)).flex_none())
}

/// One clickable column header. `active` carries the sort direction
/// when this column is the current sort key (`Some(ascending)`).
/// Callers size it (`.flex_1()` or a fixed width) and chain
/// `.on_click`; clicking is expected to select-or-flip the sort.
pub fn header_cell(
    theme: &Theme,
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    active: Option<bool>,
) -> Stateful<Div> {
    let arrow = match active {
        Some(true) => " ▲",
        Some(false) => " ▼",
        None => "",
    };
    let hover = theme.hover;
    let text = theme.text;
    div()
        .id(id)
        .cursor_pointer()
        .rounded_sm()
        .hover(move |s| s.bg(hover))
        .when(active.is_some(), |s| s.text_color(text))
        .child(format!("{}{arrow}", label.into()))
}

/// The common list-row container: fixed height, selection background,
/// hover feedback. Callers chain their cells (icon, name, detail
/// column) and an `.on_click` handler onto the returned element.
pub fn list_row(theme: &Theme, ix: usize, is_selected: bool) -> Stateful<Div> {
    let hover = theme.hover;
    div()
        .id(ix)
        // Fill the pane: uniform_list items otherwise shrink-wrap
        // their content, leaving detail columns unaligned with the
        // header and the selection background ending mid-row.
        .w_full()
        .flex()
        .items_center()
        .gap_2()
        .h(px(ROW_HEIGHT))
        .px_3()
        .cursor_pointer()
        // Subtle alternating stripes (Finder list view); selection and
        // hover paint over them.
        .when(!is_selected && ix % 2 == 1, |s| s.bg(theme.stripe))
        .when(is_selected, |s| s.bg(theme.selected))
        .when(!is_selected, move |s| s.hover(move |s| s.bg(hover)))
}

//! Row scaffold shared by the browse and search-result lists, plus the
//! clickable column header above the browse list.

use std::sync::Arc;

use gpui::{Div, ElementId, FontFeatures, SharedString, Stateful, div, prelude::*, px};

use super::icon;
use super::theme::Theme;

/// Enable tabular (fixed-width) figures on a text element, so a column of
/// sizes and dates aligns digit-for-digit instead of jittering.
pub fn tabular(mut el: Div) -> Div {
    el.text_style().get_or_insert_with(Default::default).font_features =
        Some(FontFeatures(Arc::new(vec![("tnum".into(), 1)])));
    el
}

/// Width of the Modified column (rows and header must agree).
pub const MODIFIED_COL_WIDTH: f32 = 64.;
/// Width of the Size column (rows and header must agree).
pub const SIZE_COL_WIDTH: f32 = 72.;

/// A right-aligned fixed-width detail cell (Modified / Size columns).
pub fn detail_cell(theme: &Theme, width: f32, text: impl Into<SharedString>) -> Div {
    tabular(
        div()
            .w(px(width))
            .flex_none()
            .text_xs()
            .text_color(theme.text_dim)
            .text_right()
            .child(text.into()),
    )
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
/// One clickable column header. `active` carries the sort direction
/// when this column is the current sort key (`Some(ascending)`), shown
/// as a chevron. The cell is a flex row; callers size it (`.flex_1()`
/// or a fixed width) and choose alignment (`.justify_end()` for the
/// right-hand columns), then chain `.on_click`.
pub fn header_cell(
    theme: &Theme,
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    active: Option<bool>,
) -> Stateful<Div> {
    let hover = theme.hover;
    let text = theme.text;
    let chevron = active.map(|ascending| {
        let path = if ascending {
            "icons/chevron-up.svg"
        } else {
            "icons/chevron-down.svg"
        };
        icon::ui_icon(path, text).size(px(12.))
    });
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(px(2.))
        .cursor_pointer()
        .rounded_sm()
        .hover(move |s| s.bg(hover))
        .when(active.is_some(), |s| s.text_color(text))
        .child(label.into())
        .children(chevron)
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
        .h(px(theme.row_height))
        .px_3()
        .cursor_pointer()
        // Rounded selection/hover, Finder-style, so the fill reads as a
        // pill on the row rather than a full-bleed band.
        .rounded_md()
        // Subtle alternating stripes (Finder list view); selection and
        // hover paint over them.
        .when(!is_selected && ix % 2 == 1, |s| s.bg(theme.stripe))
        .when(is_selected, |s| s.bg(theme.selected))
        .when(!is_selected, move |s| s.hover(move |s| s.bg(hover)))
}

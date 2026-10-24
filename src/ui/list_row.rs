//! Row scaffold shared by the browse and search-result lists, plus the
//! clickable column header above the browse list.

use std::sync::Arc;

use gpui::{Div, ElementId, FontFeatures, SharedString, Stateful, div, prelude::*, px};

use super::icon;
use super::theme::{Theme, fixed};

/// Enable tabular (fixed-width) figures on a text element, so a column of
/// sizes and dates aligns digit-for-digit instead of jittering.
pub fn tabular(mut el: Div) -> Div {
    el.text_style().font_features = Some(FontFeatures(Arc::new(vec![("tnum".into(), 1)])));
    el
}

/// Width of the Modified column (rows and header must agree).
pub const MODIFIED_COL_WIDTH: f32 = 112.;
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
        .h(px(32.))
        .flex_none()
        .mb_1()
        .border_b_1()
        .border_color(theme.border)
        .text_xs()
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(theme.text_dim)
        .child(div().w(px(icon_col_width)).flex_none())
}

/// Which edge a column header aligns to — its values' alignment.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum HeaderAlign {
    /// Text columns (Name): label at the left, chevron after it.
    Start,
    /// Number/date columns: label flush right over the right-aligned
    /// values, chevron before it so it never pushes the label inward.
    End,
}

/// One clickable column header. `active` carries the sort direction
/// when this column is the current sort key (`Some(ascending)`), shown
/// as a chevron. Callers size it (`.flex_1()` or a fixed width) and
/// chain `.on_click`; clicking is expected to select-or-flip the sort.
///
/// No hover fill: a stretched column (Name) would light up as a
/// full-width bar. Hover brightens the label instead, and an unsorted
/// column shows a faint chevron so it reads as sortable.
pub fn header_cell(
    theme: &Theme,
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    active: Option<bool>,
    align: HeaderAlign,
) -> Stateful<Div> {
    let id: ElementId = id.into();
    let (text, dim) = (theme.text, theme.text_dim);
    let (glyph, rest) = match active {
        Some(true) => ("icons/chevron-up.svg", text),
        Some(false) => ("icons/chevron-down.svg", text),
        // Hidden until hover; the direction a click would sort.
        None => ("icons/chevron-up.svg", fixed::with_alpha(dim, 0.)),
    };
    let chevron = icon::ui_icon(glyph, rest)
        .size(px(12.))
        .when(active.is_none(), |chevron| {
            chevron.group_hover("header-cell", move |s| s.text_color(dim))
        });
    let cell = div()
        .id(id)
        .group("header-cell")
        .flex()
        .items_center()
        .gap(px(2.))
        .cursor_pointer()
        .when(active.is_some(), |s| s.text_color(text))
        .hover(move |s| s.text_color(text));
    match align {
        HeaderAlign::Start => cell.child(label.into()).child(chevron),
        HeaderAlign::End => cell.justify_end().child(chevron).child(label.into()),
    }
}

/// Search results put the path below the name so both remain readable
/// in narrow panes and long names cannot push the location offscreen.
pub fn search_label(
    theme: &Theme,
    name: impl Into<SharedString>,
    path: impl Into<SharedString>,
) -> Div {
    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_w_0()
        .child(div().w_full().text_sm().truncate().child(name.into()))
        .child(
            div()
                .w_full()
                .text_xs()
                .text_color(theme.text_dim)
                .truncate()
                .child(path.into()),
        )
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
        // Keep unselected rows quiet; selection and hover establish hierarchy.
        .when(is_selected, |s| s.bg(theme.selected))
        .when(!is_selected, move |s| s.hover(move |s| s.bg(hover)))
}

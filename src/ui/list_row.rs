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

/// Width of the Kind column (rows and header must agree).
pub const KIND_COL_WIDTH: f32 = 96.;

/// The Kind cell: a left-aligned word ("Folder", "Image").
pub fn kind_cell(theme: &Theme, kind: impl Into<SharedString>) -> Div {
    div()
        .w(px(KIND_COL_WIDTH))
        .flex_none()
        .text_xs()
        .text_color(theme.text_dim)
        .truncate()
        .child(kind.into())
}

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
        // The rows below leave a scrollbar gutter; so does the header,
        // or its right-aligned labels overhang their values.
        .mr(px(super::scrollbar::GUTTER))
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

/// A search hit on one line: the name (query words lit) and, in a column
/// of its own, the folder it is in. One line like the browse list, so a
/// screen holds as many hits as it holds files; the folder loses its
/// middle, never the part that names it.
pub fn search_label(
    theme: &Theme,
    name: impl Into<SharedString>,
    path: impl Into<SharedString>,
    matches: Vec<std::ops::Range<usize>>,
    selected: bool,
) -> Div {
    // On the accent-tinted selected row the accent match would sink in:
    // bright text there, the weight still marks it.
    let hit = gpui::HighlightStyle {
        color: Some(gpui::rgb_to_hsla(if selected {
            theme.text
        } else {
            theme.accent
        })),
        font_weight: Some(gpui::FontWeight::SEMIBOLD),
        ..Default::default()
    };
    let name = gpui::StyledText::new(name.into())
        .with_highlights(matches.into_iter().map(|range| (range, hit)));
    div()
        .flex()
        .flex_1()
        .min_w_0()
        .items_center()
        .gap_4()
        // Name takes a third: hits often share a name, and the folder is
        // what tells them apart, so the folder gets the room.
        .child(
            div()
                .w(gpui::relative(NAME_SHARE))
                .flex_none()
                .min_w_0()
                .text_sm()
                .truncate()
                .child(name),
        )
        .child(
            div().flex_1().min_w_0().child(
                super::middle_truncated(path.into())
                    .w_full()
                    .text_xs()
                    .text_color(theme.text_dim),
            ),
        )
}

/// The Name column's share of a search row's text area.
const NAME_SHARE: f32 = 0.34;

/// The labels over [`search_label`]'s two columns.
pub fn search_header(theme: &Theme, icon_col_width: f32) -> Div {
    header_row(theme, icon_col_width)
        // The same box as `search_label`, so the share resolves against
        // the same width and the labels sit over their columns.
        .child(
            div()
                .flex()
                .flex_1()
                .min_w_0()
                .gap_4()
                .child(
                    div()
                        .w(gpui::relative(NAME_SHARE))
                        .flex_none()
                        .min_w_0()
                        .child("Name"),
                )
                .child(div().flex_1().min_w_0().child("Folder")),
        )
        .child(
            div()
                .w(px(MODIFIED_COL_WIDTH))
                .flex_none()
                .text_right()
                .child("Modified"),
        )
        .child(
            div()
                .w(px(SIZE_COL_WIDTH))
                .flex_none()
                .text_right()
                .child("Size"),
        )
}

/// Where the query's words occur in `name` (first occurrence of each,
/// ASCII case-insensitive), sorted and merged, for highlighting a hit.
/// `words` must already be lowercase.
// ponytail: ASCII folding only, so "É" typed as "é" matches in the index
// but isn't lit here; and a fuzzy hit lights nothing. Have the daemon
// return match offsets if that matters.
pub fn match_ranges(name: &str, words: &[String]) -> Vec<std::ops::Range<usize>> {
    let folded = name.to_ascii_lowercase();
    let mut found: Vec<_> = words
        .iter()
        .filter(|word| !word.is_empty())
        .filter_map(|word| folded.find(word.as_str()).map(|at| at..at + word.len()))
        .collect();
    found.sort_by_key(|range| range.start);
    let mut merged: Vec<std::ops::Range<usize>> = Vec::new();
    for range in found {
        match merged.last_mut() {
            Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
            _ => merged.push(range),
        }
    }
    merged
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

#[cfg(test)]
mod tests {
    use super::match_ranges;

    #[test]
    fn query_words_are_found_in_a_name_whatever_their_case() {
        let words = |q: &str| q.split(' ').map(str::to_owned).collect::<Vec<_>>();
        assert_eq!(match_ranges("Cargo.lock", &words("cargo")), vec![0..5]);
        assert_eq!(
            match_ranges("my-report-final.pdf", &words("final report")),
            vec![3..9, 10..15]
        );
        // Overlapping words merge into one run; a miss lights nothing.
        assert_eq!(match_ranges("abcdef", &words("abc bcd")), vec![0..4]);
        assert!(match_ranges("notes.txt", &words("cargo")).is_empty());
        // Offsets stay on character boundaries around non-ASCII text.
        assert_eq!(match_ranges("日本Cargo", &words("cargo")), vec![6..11]);
    }
}

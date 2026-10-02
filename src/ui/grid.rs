//! Grid / card browse-layout building blocks (block 4).
//!
//! The grid is virtualized the same way the list is: a `uniform_list`
//! whose *rows* are full-width strips of N cards, N chosen from the pane
//! width. So even a folder of 100k files only ever builds the cards for
//! the visible rows — never one element per file. The column math is a
//! pure function so it can be unit-tested without GPUI.

use gpui::{App, Div, ElementId, LineFragment, Stateful, div, font, prelude::*, px};
use unicode_segmentation::UnicodeSegmentation as _;

use super::theme::Theme;

/// Icon/thumbnail edge (px) for each zoom step; `grid_zoom` in settings
/// indexes this. [`card_size`] clamps out-of-range indices.
pub const CARD_SIZES: [f32; 4] = [104., 128., 156., 192.];

/// Padding inside a card, on every side.
const CARD_PAD: f32 = 8.;
/// Height reserved under the icon for the (up to 2-line) name plus the
/// detail line — see [`card_name`]. Fixed so every card is the same
/// height and a long name can't push into the row below.
const LABEL_HEIGHT: f32 = 74.;
/// Explicit metrics keep filename measurement and the two rendered lines in sync.
const NAME_FONT_SIZE: f32 = 12.;
const NAME_LINE_HEIGHT: f32 = 19.;
const NAME_HEIGHT: f32 = NAME_LINE_HEIGHT * 2.;
/// Gap between cards (and the row's own inset).
pub const CARD_GAP: f32 = 8.;

/// The icon edge for a (possibly out-of-range) zoom step.
pub fn card_size(zoom: u8) -> f32 {
    CARD_SIZES[(zoom as usize).min(CARD_SIZES.len() - 1)]
}

/// The highest valid zoom index.
pub fn max_zoom() -> u8 {
    (CARD_SIZES.len() - 1) as u8
}

/// Full width one card occupies (icon edge + its padding), excluding the
/// inter-card gap.
pub fn cell_width(size: f32) -> f32 {
    size + CARD_PAD * 2.
}

/// Full height one grid row occupies.
pub fn row_height(size: f32) -> f32 {
    size + CARD_PAD * 2. + LABEL_HEIGHT
}

/// How many cards of `cell` width (separated by [`CARD_GAP`]) fit across
/// `content_width`. Always at least one, so a too-narrow pane still
/// shows a (clipped) single column rather than nothing.
pub fn columns_for(content_width: f32, cell: f32) -> usize {
    if content_width <= 0. || cell <= 0. {
        return 1;
    }
    // N cards + (N-1) gaps ≤ width  ⇒  N ≤ (width + gap) / (cell + gap).
    (((content_width + CARD_GAP) / (cell + CARD_GAP)).floor() as usize).max(1)
}

/// Distribute spare pane width evenly, including on the final partial row.
/// This keeps cards aligned while avoiding a dead strip beside the grid.
pub fn column_width(content_width: f32, columns: usize) -> f32 {
    let columns = columns.max(1);
    ((content_width - CARD_GAP * columns.saturating_sub(1) as f32) / columns as f32)
        .floor()
        .max(1.)
}

/// A grid row: a fixed-height, full-width flex strip the caller fills
/// with [`card`]s.
pub fn grid_row(size: f32) -> Div {
    div()
        .flex()
        .items_start()
        .gap(px(CARD_GAP))
        .px(px(CARD_GAP))
        .w_full()
        .h(px(row_height(size)))
}

/// One card scaffold: a centered icon area over the name + detail lines.
/// Callers add the icon element and the two text children, then chain
/// click handlers.
pub fn card(
    theme: &Theme,
    id: impl Into<ElementId>,
    size: f32,
    width: f32,
    is_selected: bool,
) -> Stateful<Div> {
    let hover = theme.hover;
    div()
        .id(id)
        .flex()
        .flex_col()
        .items_center()
        .gap_1()
        .w(px(width))
        // Fixed height with a trailing row gap + clip: a long name can never
        // grow the card and spill into the cards below it.
        .h(px(row_height(size) - CARD_GAP))
        .p(px(CARD_PAD))
        .bg(theme.stripe)
        .rounded_lg()
        .cursor_pointer()
        .overflow_hidden()
        .when(is_selected, |s| s.bg(theme.selected))
        .when(!is_selected, move |s| s.hover(move |s| s.bg(hover)))
}

/// The square that holds a card's icon or thumbnail, centered.
pub fn card_icon_area(size: f32) -> Div {
    div()
        .flex()
        .items_center()
        .justify_center()
        .w(px(size))
        .h(px(size))
        .flex_none()
}

/// Two independently bounded lines. GPUI 0.2's combined line-clamp/ellipsis
/// truncates against twice the width before wrapping; a short first word can
/// leave an over-wide final line whose beginning gets clipped when centered.
pub fn card_name(theme: &Theme, width: f32, name: &str, cx: &App) -> Div {
    let width = px((width - CARD_PAD * 2.).max(1.));
    // Filenames may contain line breaks; treat those as spaces in the label.
    let name = name.replace(['\n', '\r', '\t'], " ");
    let mut wrapper = cx
        .text_system()
        .line_wrapper(font(super::fonts::UI_FONT_FAMILY), px(NAME_FONT_SIZE));
    let boundary = wrapper
        .wrap_line(&[LineFragment::text(&name)], width)
        .next()
        .map_or(name.len(), |boundary| boundary.ix);
    let (first, second) = split_filename(&name, boundary);
    div()
        .flex()
        .flex_col()
        .w(width)
        .flex_none()
        .h(px(NAME_HEIGHT))
        .text_center()
        .font_family(super::fonts::UI_FONT_FAMILY)
        .text_size(px(NAME_FONT_SIZE))
        .line_height(px(NAME_LINE_HEIGHT))
        .text_color(theme.text)
        .children([first, second].map(|line| {
            div()
                .w(width)
                .h(px(NAME_LINE_HEIGHT))
                .flex_none()
                .truncate()
                .child(line.to_owned())
        }))
}

/// Keep the remainder intact for end truncation, without dividing a grapheme
/// if the line wrapper chose a boundary inside a combining or emoji sequence.
fn split_filename(name: &str, boundary: usize) -> (&str, &str) {
    let boundary = name
        .grapheme_indices(true)
        .map(|(index, _)| index)
        .chain(std::iter::once(name.len()))
        .take_while(|index| *index <= boundary)
        .last()
        .unwrap_or(0);
    let (first, second) = name.split_at(boundary);
    (first.trim_end(), second.trim_start())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filename_second_line_keeps_its_leading_characters() {
        for name in [
            "Background Verification Form.doc",
            "Background Verification Form.pages",
        ] {
            let (first, second) = split_filename(name, "Background ".len());
            assert_eq!(first, "Background");
            assert_eq!(second, name.strip_prefix("Background ").unwrap());
            assert!(second.starts_with("Verification"));
        }
    }

    #[test]
    fn filename_wrap_does_not_split_unicode_graphemes() {
        assert_eq!(
            split_filename("Cafe\u{301}.txt", 4),
            ("Caf", "e\u{301}.txt")
        );
        assert_eq!(split_filename("a👩‍💻.png", 5), ("a", "👩‍💻.png"));
        assert_eq!(
            split_filename("报告.pdf", "报告.pdf".len()),
            ("报告.pdf", "")
        );
        assert_eq!(split_filename("", 0), ("", ""));
    }

    #[test]
    fn columns_never_below_one() {
        assert_eq!(columns_for(0., 100.), 1);
        assert_eq!(columns_for(50., 100.), 1); // narrower than one card
        assert_eq!(columns_for(-5., 100.), 1);
    }

    #[test]
    fn columns_account_for_the_inter_card_gap() {
        // cell=100, gap=8. Two cards need 100+8+100 = 208.
        assert_eq!(columns_for(207., 100.), 1);
        assert_eq!(columns_for(208., 100.), 2);
        // Three need 100*3 + 8*2 = 316.
        assert_eq!(columns_for(315., 100.), 2);
        assert_eq!(columns_for(316., 100.), 3);
    }

    #[test]
    fn fluid_columns_fit_the_pane_without_a_trailing_empty_strip() {
        for pane_width in [344., 348., 624., 904., 1264.] {
            for zoom in 0..=max_zoom() {
                let minimum = cell_width(card_size(zoom));
                let columns = columns_for(pane_width, minimum);
                let width = column_width(pane_width, columns);
                let occupied = width * columns as f32 + CARD_GAP * (columns - 1) as f32;
                assert!(width >= minimum);
                assert!(occupied <= pane_width);
                assert!(pane_width - occupied < columns as f32);
            }
        }
    }

    #[test]
    fn card_size_clamps_out_of_range_zoom() {
        assert_eq!(card_size(0), CARD_SIZES[0]);
        assert_eq!(card_size(max_zoom()), CARD_SIZES[CARD_SIZES.len() - 1]);
        assert_eq!(card_size(250), CARD_SIZES[CARD_SIZES.len() - 1]);
    }
}

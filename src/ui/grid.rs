//! Grid / card browse-layout building blocks (block 4).
//!
//! The grid is virtualized the same way the list is: a `uniform_list`
//! whose *rows* are full-width strips of N cards, N chosen from the pane
//! width. So even a folder of 100k files only ever builds the cards for
//! the visible rows — never one element per file. The column math is a
//! pure function so it can be unit-tested without GPUI.

use gpui::{Div, ElementId, Stateful, div, prelude::*, px};

use super::theme::Theme;

/// Icon/thumbnail edge (px) for each zoom step; `grid_zoom` in settings
/// indexes this. [`card_size`] clamps out-of-range indices.
pub const CARD_SIZES: [f32; 4] = [88., 112., 140., 176.];

/// Padding inside a card, on every side.
const CARD_PAD: f32 = 8.;
/// Height reserved under the icon for the (up to 2-line) name plus the
/// detail line — see [`card_name`]. Fixed so every card is the same
/// height and a long name can't push into the row below.
const LABEL_HEIGHT: f32 = 66.;
/// Fixed height of the name block: exactly two `text_xs` line-boxes. gpui
/// defaults line height to φ·font-size (≈1.618 × 12px ≈ 19px), so two
/// lines need ~38px — a shorter box clips descenders (y, g, p) on the
/// second line.
const NAME_HEIGHT: f32 = 38.;
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
    is_selected: bool,
) -> Stateful<Div> {
    let hover = theme.hover;
    div()
        .id(id)
        .flex()
        .flex_col()
        .items_center()
        .gap_1()
        .w(px(cell_width(size)))
        // Fixed height (matching the row) + clip: a long name can never
        // grow the card and spill into the cards below it.
        .h(px(row_height(size)))
        .p(px(CARD_PAD))
        .rounded_lg()
        .cursor_pointer()
        .overflow_hidden()
        .when(is_selected, |s| s.bg(theme.selected))
        .when(!is_selected, move |s| s.hover(move |s| s.bg(hover)))
}

/// The square that holds a card's icon or thumbnail, centered.
pub fn card_icon_area(size: f32) -> Div {
    div().flex().items_center().justify_center().w(px(size)).h(px(size)).flex_none()
}

/// A card's name block: centered, wrapping to at most two lines with a
/// trailing ellipsis. gpui only truncates against a *definite* width, so
/// the explicit `w(size)` here (not a flex-derived width) is what makes
/// the ellipsis reliable; the line wrapper force-breaks over-long words,
/// so no single line can spill past the card edge. Fixed height keeps the
/// detail line aligned across cards. Pair it with a tooltip for the full
/// name.
pub fn card_name(theme: &Theme, size: f32) -> Div {
    div()
        .w(px(size))
        .flex_none()
        .h(px(NAME_HEIGHT))
        .text_center()
        .text_xs()
        .text_color(theme.text)
        .line_clamp(2)
        .text_ellipsis()
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn card_size_clamps_out_of_range_zoom() {
        assert_eq!(card_size(0), CARD_SIZES[0]);
        assert_eq!(card_size(max_zoom()), CARD_SIZES[CARD_SIZES.len() - 1]);
        assert_eq!(card_size(250), CARD_SIZES[CARD_SIZES.len() - 1]);
    }
}

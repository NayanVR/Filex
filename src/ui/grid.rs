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
/// Height reserved under the icon for the (up to 2-line) name — see
/// [`card_name`]. Fixed so every card is the same
/// height and a long name can't push into the row below.
const LABEL_HEIGHT: f32 = 46.;
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

/// The icon area's height for an icon edge of `size`. Shorter than wide:
/// icons are drawn at 80% and seated at its foot, so a full square left a
/// band of empty space above every row.
pub fn icon_area_height(size: f32) -> f32 {
    (size * ICON_AREA_RATIO).round()
}

/// [`icon_area_height`] as a fraction of the icon edge.
pub const ICON_AREA_RATIO: f32 = 0.84;

/// Full height one grid row occupies.
pub fn row_height(size: f32) -> f32 {
    icon_area_height(size) + CARD_PAD * 2. + LABEL_HEIGHT
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

/// One card scaffold: a centered icon area over the name. Callers add the
/// icon element and the label, then chain
/// click handlers.
pub fn card(id: impl Into<ElementId>, size: f32, width: f32) -> Stateful<Div> {
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
        // No resting fill: a grid of filled tiles reads as a wall of
        // boxes. Hover and selection supply the shape.
        .rounded_xl()
        .cursor_pointer()
        .overflow_hidden()
        // Selection marks the icon and the name (see `card_icon_area`,
        // `card_name`), not the whole cell: a filled tile per selected
        // item read as one heavy block.
        // Hover lights the icon tile, the same shape selection uses.
        .group("grid-card")
}

/// The square that holds a card's icon or thumbnail, centered across
/// and seated at the bottom, so a glyph smaller than the square (folders,
/// documents) hugs its label instead of floating above a gap.
pub fn card_icon_area(theme: &Theme, size: f32, selected: bool) -> Div {
    div()
        .flex()
        .items_end()
        .justify_center()
        .w(px(size))
        .h(px(icon_area_height(size)))
        .flex_none()
        .rounded_lg()
        .when(selected, |area| area.bg(theme.selected))
        .when(!selected, |area| {
            let hover = theme.hover;
            area.group_hover("grid-card", move |s| s.bg(hover))
        })
}

/// Two independently bounded lines. GPUI 0.2's combined line-clamp/ellipsis
/// truncates against twice the width before wrapping; a short first word can
/// leave an over-wide final line whose beginning gets clipped when centered.
///
/// Returns the label and whether the name had to be cut (it needs more
/// than two lines), so callers only add a full-name tooltip when it adds
/// something.
/// `selected` is `Some(focused)` for a selected item: an accent pill
/// while the list has focus, grey otherwise.
pub fn card_name(
    theme: &Theme,
    width: f32,
    name: &str,
    selected: Option<bool>,
    cx: &App,
) -> (Div, bool) {
    let width = px((width - CARD_PAD * 2.).max(1.));
    // Filenames may contain line breaks; treat those as spaces in the label.
    let name = name.replace(['\n', '\r', '\t'], " ");
    let mut wrapper = cx
        .text_system()
        .line_wrapper(font(super::fonts::UI_FONT_FAMILY), px(NAME_FONT_SIZE));
    let fragments = [LineFragment::text(&name)];
    let mut boundaries = wrapper.wrap_line(&fragments, width);
    let boundary = boundaries.next().map_or(name.len(), |boundary| boundary.ix);
    drop(boundaries);
    let (first, second) = split_filename(&name, friendly_break(&name, boundary));
    // A second line that still doesn't fit loses its middle, not its end:
    // the extension is the part that tells two long names apart.
    let fits = wrapper
        .wrap_line(&[LineFragment::text(second)], width)
        .next();
    let truncated = fits.is_some();
    let label = div()
        .flex()
        .flex_col()
        .w(width)
        .flex_none()
        // Shrinks to one line for a short name, so its date sits right
        // under it; a fixed two-line box left a floating gap instead.
        .max_h(px(NAME_HEIGHT))
        .items_center()
        .text_center()
        .font_family(super::fonts::UI_FONT_FAMILY)
        .text_size(px(NAME_FONT_SIZE))
        .line_height(px(NAME_LINE_HEIGHT))
        .text_color(if selected == Some(true) {
            theme.on_accent
        } else {
            theme.text
        });
    // Each line hugs its text (max width, not width), so a selected name
    // gets Finder's accent pill around the words, not a full-cell band.
    let pill = |line: Div| {
        line.max_w(width)
            .h(px(NAME_LINE_HEIGHT))
            .flex_none()
            .px_1()
            .rounded_sm()
            .when_some(selected, |line, focused| {
                line.bg(if focused { theme.accent } else { theme.control })
            })
    };
    let label = label
        .child(pill(div().truncate().child(first.to_owned())))
        .children(
            (!second.is_empty())
                .then(|| pill(super::middle_truncated(second.to_owned()).text_center())),
        );
    (label, truncated)
}

/// When the wrapper had to cut inside a word, back up to a separator, so
/// a name breaks as "LICENSE-" / "APACHE" or "rust-" / "toolchain.toml"
/// rather than one letter onto the second line.
fn friendly_break(name: &str, boundary: usize) -> usize {
    if boundary >= name.len()
        || name[..boundary].ends_with(' ')
        || name[boundary..].starts_with(' ')
    {
        return boundary;
    }
    name[..boundary]
        .char_indices()
        .rev()
        // A word break well into the line (not so early that the rest
        // can't fit either) beats a mid-word cut.
        .find_map(|(i, c)| {
            ((i + 1) * 4 >= boundary && matches!(c, '-' | '_' | ' ')).then_some(i + 1)
        })
        // A dot only as a last resort: "rust-toolchain" / ".toml" reads
        // as a stray extension.
        .or_else(|| {
            name[..boundary]
                .char_indices()
                .rev()
                .take_while(|(i, _)| *i * 2 >= boundary)
                .find_map(|(i, c)| (c == '.').then_some(i))
        })
        .unwrap_or(boundary)
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

    /// Regression: "LICENSE-APACHE" wrapped as "LICENSE-APACH" / "E".
    #[test]
    fn a_forced_break_backs_up_to_a_separator() {
        fn cut(name: &str, at: usize) -> (&str, &str) {
            split_filename(name, friendly_break(name, at))
        }
        assert_eq!(cut("LICENSE-APACHE", 13), ("LICENSE-", "APACHE"));
        assert_eq!(cut("rust-toolchain.toml", 17), ("rust-", "toolchain.toml"));
        assert_eq!(
            cut("verylongfilename.toml", 18),
            ("verylongfilename", ".toml")
        );
        // No separator in the back half: the wrapper's cut stands.
        assert_eq!(cut("a-verylongsinglewordname", 20).0.len(), 20);
        // A break at a space is already a good one.
        assert_eq!(cut("Background Verification.doc", 11).0, "Background");
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

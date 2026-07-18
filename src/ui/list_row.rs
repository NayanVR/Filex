//! Row scaffold shared by the browse and search-result lists.

use gpui::{Div, Stateful, div, prelude::*, px, rgb};

use super::theme::{BG_HOVER, BG_SELECTED};

/// Fixed row height — `uniform_list` requires every row equal-height,
/// so this constant is the single place it's defined.
pub const ROW_HEIGHT: f32 = 28.;

/// The common list-row container: fixed height, selection background,
/// hover feedback. Callers chain their cells (icon, name, detail
/// column) and an `.on_click` handler onto the returned element.
pub fn list_row(ix: usize, is_selected: bool) -> Stateful<Div> {
    div()
        .id(ix)
        .flex()
        .items_center()
        .gap_2()
        .h(px(ROW_HEIGHT))
        .px_3()
        .cursor_pointer()
        .when(is_selected, |s| s.bg(rgb(BG_SELECTED)))
        .when(!is_selected, |s| s.hover(|s| s.bg(rgb(BG_HOVER))))
}

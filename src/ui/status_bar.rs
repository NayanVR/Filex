//! The bottom status bar: one line of dim text, split left/right.
//!
//! The strings themselves are built by pure functions here so they can
//! be unit-tested without GPUI; the workspace only counts state and
//! passes numbers in.

use gpui::{Div, Rgba, SharedString, div, prelude::*, px};

use super::theme::Theme;

/// The bar container: left-aligned message, right-aligned index status
/// led by a small state dot (`dot`: ready / working / attention).
pub fn status_bar(
    theme: &Theme,
    left: impl Into<SharedString>,
    right: impl Into<SharedString>,
    dot: Rgba,
) -> Div {
    div()
        .flex()
        .items_center()
        .justify_between()
        .h(px(28.))
        .flex_none()
        .px_4()
        // On the shell, below the canvas: no divider or fill of its own.
        .text_xs()
        .text_color(theme.text_dim)
        .gap_4()
        .child(div().flex_1().min_w_0().truncate().child(left.into()))
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .max_w(gpui::relative(0.6))
                .child(div().flex_none().size(px(6.)).rounded_full().bg(dot))
                .child(div().min_w_0().truncate().child(right.into())),
        )
}

/// "1 item" / "N items" — the count line for a folder.
pub fn item_count(n: usize) -> String {
    if n == 1 {
        "1 item".to_owned()
    } else {
        format!("{n} items")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn item_count_is_singular_only_for_one() {
        assert_eq!(item_count(0), "0 items");
        assert_eq!(item_count(1), "1 item");
        assert_eq!(item_count(2), "2 items");
    }
}

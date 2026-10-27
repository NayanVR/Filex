//! The bottom status bar: one line of dim text, split left/right.
//!
//! The strings themselves are built by pure functions here so they can
//! be unit-tested without GPUI; the workspace only counts state and
//! passes numbers in.

use gpui::{Div, Rgba, SharedString, div, prelude::*, px};

use super::tooltip;

use super::theme::Theme;

/// The bar container: left-aligned message, right-aligned index status
/// led by a small state dot (`dot`: ready / working / attention), with
/// an optional hover explanation (`right_tip`) for the index status.
pub fn status_bar(
    theme: &Theme,
    left: impl Into<SharedString>,
    right: impl Into<SharedString>,
    dot: Rgba,
    right_tip: Option<SharedString>,
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
                .id("index-status")
                .flex()
                .items_center()
                .gap_2()
                .max_w(gpui::relative(0.6))
                .when_some(right_tip, |status, tip| {
                    status.tooltip(tooltip::text_tooltip(tip, *theme))
                })
                .child(div().flex_none().size(px(6.)).rounded_full().bg(dot))
                .child(div().min_w_0().truncate().child(right.into())),
        )
}

/// The index side of the status bar once the index is ready: "N files
/// indexed", "· N skipped" when some entries couldn't be read, and a note
/// when the last search had more matches than it showed.
pub fn index_summary(files: u64, skipped: u64, more_matches: bool) -> String {
    let mut line = format!("{files} files indexed");
    if skipped > 0 {
        line.push_str(&format!(" · {skipped} skipped"));
    }
    if more_matches {
        line.push_str(" · more matches available");
    }
    line
}

/// Hover text explaining skipped entries, or `None` when there are none.
pub fn skipped_explanation(skipped: u64) -> Option<String> {
    (skipped > 0).then(|| {
        let what = if skipped == 1 {
            "1 folder or file"
        } else {
            "folders or files"
        };
        let count = if skipped == 1 {
            String::new()
        } else {
            format!("{skipped} ")
        };
        format!(
            "{count}{what} couldn't be read (no permission), so their contents \
             aren't searchable. Everything else is indexed, and browsing isn't \
             affected."
        )
    })
}

/// "1 item" / "N items" — the count line for a folder.
pub fn item_count(n: usize) -> String {
    counted(n, "item")
}

/// "1 result" / "N results" — the count line for a search.
pub fn result_count(n: usize) -> String {
    counted(n, "result")
}

fn counted(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
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

    #[test]
    fn index_summary_mentions_skips_only_when_there_are_some() {
        assert_eq!(index_summary(1200, 0, false), "1200 files indexed");
        assert_eq!(
            index_summary(1200, 71, false),
            "1200 files indexed · 71 skipped"
        );
        assert_eq!(
            index_summary(5, 0, true),
            "5 files indexed · more matches available"
        );
        assert_eq!(skipped_explanation(0), None);
        assert!(
            skipped_explanation(71)
                .unwrap()
                .starts_with("71 folders or files")
        );
        assert!(
            skipped_explanation(1)
                .unwrap()
                .starts_with("1 folder or file couldn't")
        );
    }

    /// Regression: the search heading read "1 results".
    #[test]
    fn result_count_is_singular_only_for_one() {
        assert_eq!(result_count(1), "1 result");
        assert_eq!(result_count(100), "100 results");
    }
}

//! The bottom status bar: one line of dim text, split left/right.
//!
//! The strings themselves are built by pure functions here so they can
//! be unit-tested without GPUI; the workspace only counts state and
//! passes numbers in.

use gpui::{Div, SharedString, div, prelude::*, px, rgb};

use super::theme::{BG_PANEL, BORDER, TEXT_DIM};

/// The bar container: left-aligned message, right-aligned index status.
pub fn status_bar(left: impl Into<SharedString>, right: impl Into<SharedString>) -> Div {
    div()
        .flex()
        .items_center()
        .justify_between()
        .h(px(26.))
        .px_3()
        .border_t_1()
        .border_color(rgb(BORDER))
        .bg(rgb(BG_PANEL))
        .text_xs()
        .text_color(rgb(TEXT_DIM))
        .child(left.into())
        .child(right.into())
}

/// Right-hand status line while indexing locally: file/root counts,
/// plus failure and degraded-watch suffixes when applicable.
pub fn local_index_status(
    ready: usize,
    total: usize,
    failed: usize,
    files: usize,
    degraded: bool,
) -> String {
    let mut text = if ready == total {
        format!("{files} files · {total} root{}", if total == 1 { "" } else { "s" })
    } else {
        format!("indexing {ready}/{total} roots · {files} files")
    };
    if failed > 0 {
        text.push_str(&format!(" · {failed} failed"));
    }
    if degraded {
        text.push_str(" · partial watch coverage");
    }
    text
}

/// Right-hand status line when the elevated index service (Windows)
/// is answering searches.
#[cfg(target_os = "windows")]
pub fn service_index_status(files: u64, roots: usize) -> String {
    format!("service · {files} files · {roots} root{}", if roots == 1 { "" } else { "s" })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_ready_pluralizes_roots() {
        assert_eq!(local_index_status(1, 1, 0, 42, false), "42 files · 1 root");
        assert_eq!(local_index_status(2, 2, 0, 42, false), "42 files · 2 roots");
    }

    #[test]
    fn still_indexing_shows_progress() {
        assert_eq!(local_index_status(1, 3, 0, 10, false), "indexing 1/3 roots · 10 files");
    }

    #[test]
    fn failure_and_degraded_suffixes_stack() {
        assert_eq!(
            local_index_status(2, 3, 1, 10, true),
            "indexing 2/3 roots · 10 files · 1 failed · partial watch coverage"
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn service_line_pluralizes_roots() {
        assert_eq!(service_index_status(7, 1), "service · 7 files · 1 root");
        assert_eq!(service_index_status(7, 2), "service · 7 files · 2 roots");
    }
}

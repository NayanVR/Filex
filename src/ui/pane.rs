//! Main-pane states that replace a list (placeholders, empty results).

use gpui::{AnyElement, SharedString, div, prelude::*};

use super::theme::Theme;

/// A dim one-line message filling the pane ("still indexing…",
/// "no matches for …").
pub fn empty_state(theme: &Theme, text: impl Into<SharedString>) -> AnyElement {
    div()
        .flex_1()
        .p_4()
        .text_sm()
        .text_color(theme.text_dim)
        .child(text.into())
        .into_any_element()
}

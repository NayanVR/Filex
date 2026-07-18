//! Main-pane states that replace a list (placeholders, empty results).

use gpui::{AnyElement, SharedString, div, prelude::*, rgb};

use super::theme::TEXT_DIM;

/// A dim one-line message filling the pane ("still indexing…",
/// "no matches for …").
pub fn empty_state(text: impl Into<SharedString>) -> AnyElement {
    div().flex_1().p_4().text_sm().text_color(rgb(TEXT_DIM)).child(text.into()).into_any_element()
}

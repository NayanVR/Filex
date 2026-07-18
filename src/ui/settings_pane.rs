//! Settings form controls: the pane container, toggle rows, and the
//! footnote line.
//!
//! Hand-rolled after checking the references (gpui ships no form
//! widgets; gpui-component's Switch is the pattern followed here, but
//! taking it as a dependency for one control isn't worth it). Rows are
//! presentation-only: the caller supplies current values and chains
//! `.on_click` to mutate the settings store.

use gpui::{AnyElement, Div, ElementId, SharedString, Stateful, div, prelude::*, px, rgb};

use super::theme::{ACCENT, BG_HOVER, BG_PANEL, BORDER, TEXT, TEXT_DIM};

/// The pane container filling the main content area.
pub fn settings_pane() -> Div {
    div().flex().flex_col().flex_1().min_h_0().p_4().gap_1()
}

/// The pane's heading.
pub fn pane_title(text: impl Into<SharedString>) -> Div {
    div().pb_2().text_sm().text_color(rgb(TEXT)).child(text.into())
}

/// One toggleable setting: label + explanation on the left, a switch
/// showing `on` on the right. The whole row is the click target.
pub fn toggle_row(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    description: impl Into<SharedString>,
    on: bool,
) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_between()
        .gap_4()
        .px_3()
        .py_2()
        .rounded_md()
        .cursor_pointer()
        .hover(|s| s.bg(rgb(BG_HOVER)))
        .child(
            div()
                .flex()
                .flex_col()
                .child(div().text_sm().text_color(rgb(TEXT)).child(label.into()))
                .child(div().text_xs().text_color(rgb(TEXT_DIM)).child(description.into())),
        )
        .child(switch(on))
}

/// The switch pill: accent track with the knob at the end when on,
/// dim track with the knob at the start when off.
fn switch(on: bool) -> AnyElement {
    div()
        .flex_none()
        .w(px(30.))
        .h(px(18.))
        .rounded_full()
        .p(px(2.))
        .bg(rgb(if on { ACCENT } else { BORDER }))
        .flex()
        .items_center()
        .when(on, |s| s.justify_end())
        .child(div().size(px(14.)).rounded_full().bg(rgb(BG_PANEL)))
        .into_any_element()
}

/// Dim footnote at the bottom of the pane (e.g. where settings live
/// on disk).
pub fn footnote(text: impl Into<SharedString>) -> Div {
    div().pt_3().text_xs().text_color(rgb(TEXT_DIM)).child(text.into())
}

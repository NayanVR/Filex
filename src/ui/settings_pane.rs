//! Settings form controls: the pane container, toggle rows, and the
//! footnote line.
//!
//! Hand-rolled after checking the references (gpui ships no form
//! widgets; gpui-component's Switch is the pattern followed here, but
//! taking it as a dependency for one control isn't worth it). Rows are
//! presentation-only: the caller supplies current values and chains
//! `.on_click` to mutate the settings store.

use gpui::{AnyElement, Div, ElementId, SharedString, Stateful, div, prelude::*, px};

use super::theme::Theme;

/// The settings card: a centred modal panel floating over the browse
/// view. Fixed width, sized to its content. Callers chain `.on_click`
/// (to stop the backdrop's click-through) and the rows.
pub fn settings_card(theme: &Theme, id: impl Into<ElementId>) -> Stateful<Div> {
    div()
        .id(id)
        .w(px(480.))
        .p_4()
        .rounded_lg()
        .border_1()
        .border_color(theme.border)
        .bg(theme.panel)
        .shadow_lg()
        .flex()
        .flex_col()
        .gap_1()
}

/// The card's header: a heading on the left and a close ✕ on the right.
/// The caller supplies the close button as the trailing control.
pub fn card_header(theme: &Theme, text: impl Into<SharedString>, close: AnyElement) -> Div {
    div()
        .flex()
        .items_center()
        .justify_between()
        .pb_2()
        .child(div().text_sm().text_color(theme.text).child(text.into()))
        .child(close)
}

/// The label + explanation stack shared by every settings row.
fn row_label(
    theme: &Theme,
    label: impl Into<SharedString>,
    description: impl Into<SharedString>,
) -> Div {
    div()
        // Take the row's free space and allow shrinking below content width
        // (`min_w(0)`) so a long description wraps here instead of
        // overflowing the row and the modal.
        .flex_1()
        .min_w(px(0.))
        .flex()
        .flex_col()
        .child(div().text_sm().text_color(theme.text).child(label.into()))
        .child(
            div()
                .text_xs()
                .text_color(theme.text_dim)
                .child(description.into()),
        )
}

/// The shell of a settings row: label on the left, a control on the
/// right. [`toggle_row`] and [`choice_row`] both build on it.
fn row_shell() -> Div {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap_4()
        .px_3()
        .py_2()
        .rounded_md()
}

/// One toggleable setting: label + explanation on the left, a switch
/// showing `on` on the right. The whole row is the click target.
pub fn toggle_row(
    theme: &Theme,
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    description: impl Into<SharedString>,
    on: bool,
) -> Stateful<Div> {
    let hover = theme.hover;
    row_shell()
        .id(id)
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
        .child(row_label(theme, label, description))
        .child(switch(theme, on))
}

/// A setting picked from a small fixed set of choices: label on the
/// left, a [`segmented`] control on the right. The caller builds the
/// segments (each its own click target) and passes them in.
pub fn choice_row(
    theme: &Theme,
    label: impl Into<SharedString>,
    description: impl Into<SharedString>,
    control: AnyElement,
) -> Div {
    row_shell()
        .child(row_label(theme, label, description))
        .child(control)
}

/// The container for a segmented control (a pill split into [`segment`]s).
pub fn segmented(theme: &Theme) -> Div {
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(2.))
        .p(px(2.))
        .rounded_md()
        .bg(theme.hover)
}

/// One choice in a [`segmented`] control; `selected` fills it with the
/// accent. Callers chain `.on_click`.
pub fn segment(
    theme: &Theme,
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    selected: bool,
) -> Stateful<Div> {
    let base = div()
        .id(id)
        .px_2()
        .py(px(2.))
        .rounded_sm()
        .cursor_pointer()
        .text_xs()
        .child(label.into());
    if selected {
        base.bg(theme.accent).text_color(theme.on_accent)
    } else {
        base.text_color(theme.text_dim)
    }
}

/// The switch pill: accent track with the knob at the end when on,
/// dim track with the knob at the start when off.
fn switch(theme: &Theme, on: bool) -> AnyElement {
    div()
        .flex_none()
        .w(px(30.))
        .h(px(18.))
        .rounded_full()
        .p(px(2.))
        .bg(if on { theme.accent } else { theme.border })
        .flex()
        .items_center()
        .when(on, |s| s.justify_end())
        .child(div().size(px(14.)).rounded_full().bg(theme.panel))
        .into_any_element()
}

/// Dim footnote at the bottom of the pane (e.g. where settings live
/// on disk).
pub fn footnote(theme: &Theme, text: impl Into<SharedString>) -> Div {
    div()
        .pt_3()
        .text_xs()
        .text_color(theme.text_dim)
        .child(text.into())
}

//! Settings form controls: the categorized pane, choice controls, and toggle rows.
//!
//! Hand-rolled after checking the references (gpui ships no form
//! widgets; gpui-component's Switch is the pattern followed here, but
//! taking it as a dependency for one control isn't worth it). Rows are
//! presentation-only: the caller supplies current values and chains
//! `.on_click` to mutate the settings store.

use gpui::{
    AnyElement, Div, ElementId, Rgba, SharedString, Stateful, div, prelude::*, px,
    transparent_black,
};

use super::theme::Theme;

/// The settings card: a centred modal panel floating over the browse
/// view. Fixed width with a scrollable page. Callers chain `.on_click`
/// (to stop the backdrop's click-through) and the rows.
pub fn settings_card(theme: &Theme, id: impl Into<ElementId>) -> Stateful<Div> {
    super::card(theme)
        .id(id)
        .w(px(780.))
        .bg(theme.bg)
        .overflow_hidden()
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
        .flex_none()
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

/// A setting picked from a small fixed set of choices: explanation above
/// the controls, so swatches never squeeze the label into a narrow column.
/// The caller supplies the segments, each with its own click target.
pub fn choice_row(
    theme: &Theme,
    label: impl Into<SharedString>,
    description: impl Into<SharedString>,
    control: AnyElement,
) -> Div {
    row_shell()
        .flex_col()
        .items_start()
        .gap_2()
        .child(row_label(theme, label, description).flex_none().w_full())
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

/// The container for a row of accent [`swatch`]es. Wraps
/// so the swatches + hex field never overflow the settings row.
pub fn swatch_row() -> Div {
    div().flex().flex_wrap().items_center().gap(px(6.))
}

/// One accent-color swatch: a filled dot, ringed when selected. Callers
/// chain `.on_click`.
pub fn swatch(
    theme: &Theme,
    id: impl Into<ElementId>,
    color: Rgba,
    selected: bool,
) -> Stateful<Div> {
    let ring = if selected {
        theme.text
    } else {
        transparent_black().into()
    };
    div()
        .id(id)
        .size(px(22.))
        .rounded_full()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .border_2()
        .border_color(ring)
        .child(div().size(px(14.)).rounded_full().bg(color))
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

/// Stable outer header for the settings window.
pub fn shell_header(theme: &Theme, title: impl Into<SharedString>, close: impl IntoElement) -> Div {
    div()
        .flex()
        .flex_none()
        .items_center()
        .justify_between()
        .h(px(64.))
        .px_5()
        .border_b_1()
        .border_color(theme.border)
        .child(
            div()
                .text_lg()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .child(title.into()),
        )
        .child(close)
}

pub fn navigation(theme: &Theme) -> Div {
    div()
        .flex()
        .flex_col()
        .flex_none()
        .w(px(176.))
        .p_3()
        .gap_1()
        .bg(theme.panel)
        .border_r_1()
        .border_color(theme.border)
}

pub fn navigation_item(
    theme: &Theme,
    label: &'static str,
    icon: &'static str,
    active: bool,
) -> Stateful<Div> {
    let color = if active { theme.accent } else { theme.text_dim };
    div()
        .id(label)
        .flex()
        .flex_none()
        .items_center()
        .h(px(38.))
        .px_3()
        .gap_2()
        .rounded_lg()
        .text_sm()
        .text_color(color)
        .cursor_pointer()
        .when(active, |s| {
            s.bg(theme.selected).font_weight(gpui::FontWeight::MEDIUM)
        })
        .when(!active, |s| s.hover(|s| s.bg(theme.hover)))
        .child(super::icon::ui_icon(icon, color).size(px(16.)))
        .child(label)
}

pub fn page(id: &'static str) -> Stateful<Div> {
    div()
        .id(SharedString::from(format!("settings-page-{id}")))
        .flex()
        .flex_col()
        .flex_1()
        .min_w_0()
        .min_h_0()
        .overflow_y_scroll()
        .p_5()
        .gap_5()
}

pub fn page_heading(theme: &Theme, title: &'static str, description: &'static str) -> Div {
    div()
        .flex()
        .flex_col()
        .flex_none()
        .gap_1()
        .child(
            div()
                .text_xl()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .child(title),
        )
        .child(
            div()
                .text_sm()
                .text_color(theme.text_dim)
                .child(description),
        )
}

pub fn group(theme: &Theme) -> Div {
    div()
        .flex()
        .flex_col()
        .flex_none()
        .p_1()
        .gap_1()
        .rounded_xl()
        .border_1()
        .border_color(theme.border)
        .bg(theme.bg)
}

pub fn note(theme: &Theme, text: impl Into<SharedString>) -> Div {
    div()
        .min_w_0()
        .text_xs()
        .text_color(theme.text_dim)
        .child(text.into())
}

pub fn shortcut_row(theme: &Theme, label: &'static str, aliases: String) -> Div {
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap_2()
        .px_2()
        .py_2()
        .min_h(px(48.))
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .child(div().text_sm().child(label))
                .when(!aliases.is_empty(), |s| {
                    s.child(note(theme, format!("Also {aliases}")))
                }),
        )
}

pub fn shortcut_button(
    theme: &Theme,
    id: &'static str,
    label: String,
    recording: bool,
) -> Stateful<Div> {
    div()
        .id(SharedString::from(format!("shortcut-{id}")))
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .min_w(px(86.))
        .h(px(30.))
        .px_2()
        .rounded_md()
        .border_1()
        .border_color(if recording {
            theme.accent
        } else {
            theme.border
        })
        .bg(if recording {
            theme.selected
        } else {
            theme.panel
        })
        .text_xs()
        .text_color(if recording { theme.accent } else { theme.text })
        .cursor_pointer()
        .hover(|s| s.border_color(theme.accent))
        .child(label)
}

//! The Magic card — the review step between a parsed natural-language
//! command and any file actually moving (`docs/design-magic-mode.md` §3).
//!
//! Presentation only: every function here takes what to draw and returns
//! an element. Parsing lives in [`filex::magic`], plan execution in
//! [`filex::ops`], and the state (which rows are checked) in the
//! workspace — same split as the rest of `ui`.
//!
//! The card is deliberately *not* a modal. A modal would demand an
//! answer to a question the user did not ask: they typed into a search
//! box, and a dialog seizing focus for a plan they may not have wanted
//! is exactly the "magic that acts on its own" failure this design
//! exists to avoid. It renders inline above the results, ignorable by
//! typing another character.

use gpui::{AnyElement, Div, ElementId, SharedString, Stateful, div, prelude::*, px};

use super::theme::Theme;

/// The card shell: an accent-edged panel above the result list.
pub fn card(theme: &Theme) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .mx_3()
        .mt_2()
        .p_3()
        .rounded_lg()
        .border_1()
        .border_color(theme.accent)
        .bg(theme.panel)
}

/// Heading row: what the command was understood to mean, in plain words.
pub fn heading(theme: &Theme, text: impl Into<SharedString>) -> Div {
    div().flex().items_center().gap_2().child(
        div().text_sm().text_color(theme.text).child(text.into()),
    )
}

/// The sub-line under the heading — match counts, skips, warnings.
pub fn subtitle(theme: &Theme, text: impl Into<SharedString>) -> Div {
    div().text_xs().text_color(theme.text_dim).child(text.into())
}

/// A refusal: the command parsed but produced no runnable plan. Says
/// which reason, so "nothing happened" is never unexplained.
pub fn refusal(theme: &Theme, text: impl Into<SharedString>) -> Div {
    div().text_xs().text_color(theme.warn).child(text.into())
}

/// Scroll container for the op rows. Capped in height so a 300-op plan
/// cannot push the results list off screen; the rows scroll inside it.
pub fn op_list(id: impl Into<ElementId>) -> Stateful<Div> {
    div().id(id).flex().flex_col().max_h(px(180.)).overflow_y_scroll().mt_1()
}

/// One operation in the plan: a checkbox, the source name, and where it
/// is going. Unchecked rows dim but stay legible — the user is
/// reviewing them, not dismissing them.
pub fn op_row(
    theme: &Theme,
    id: impl Into<ElementId>,
    checked: bool,
    source: impl Into<SharedString>,
    arrow: Option<SharedString>,
) -> Stateful<Div> {
    let hover = theme.hover;
    div()
        .id(id)
        .flex()
        .items_center()
        .gap_2()
        .px_1()
        .py_0p5()
        .rounded_sm()
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
        .when(!checked, |row| row.opacity(0.45))
        .child(checkbox(theme, checked))
        .child(
            div()
                .flex_1()
                .min_w_0()
                // w_full inside flex_1 is what actually gives gpui a
                // definite width to paint the … ellipsis against.
                .child(div().w_full().text_xs().text_color(theme.text).truncate().child(source.into())),
        )
        .children(arrow.map(|target| {
            div()
                .flex_1()
                .min_w_0()
                .child(div().w_full().text_xs().text_color(theme.text_dim).truncate().child(target))
        }))
}

/// A small square check indicator. Hand-drawn rather than a glyph so it
/// carries the accent colour in both themes without a font dependency.
fn checkbox(theme: &Theme, checked: bool) -> Div {
    let base = div()
        .flex_none()
        .size(px(13.))
        .rounded_sm()
        .border_1()
        .flex()
        .items_center()
        .justify_center();
    if checked {
        base.border_color(theme.accent).bg(theme.accent).child(
            div().text_color(theme.on_accent).text_xs().child("✓"),
        )
    } else {
        base.border_color(theme.border)
    }
}

/// Right-aligned action row (Cancel / the confirm button).
pub fn actions() -> Div {
    div().flex().items_center().justify_end().gap_2().pt_2()
}

/// The confirm button. `enabled` is false when nothing is checked —
/// clicking it then would be a no-op, so it reads as unavailable rather
/// than silently doing nothing.
pub fn confirm_button(
    theme: &Theme,
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    enabled: bool,
    destructive: bool,
) -> Stateful<Div> {
    let fill = if destructive { theme.warn } else { theme.accent };
    let base = div().id(id).px_3().py_1().rounded_md().text_sm().child(label.into());
    if enabled {
        base.cursor_pointer().bg(fill).text_color(theme.on_accent).hover(|s| s.opacity(0.9))
    } else {
        base.border_1().border_color(theme.border).text_color(theme.text_dim).opacity(0.6)
    }
}

/// Secondary (outlined) action, e.g. Cancel.
pub fn secondary_button(
    theme: &Theme,
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
) -> Stateful<Div> {
    let hover = theme.hover;
    div()
        .id(id)
        .px_3()
        .py_1()
        .rounded_md()
        .cursor_pointer()
        .text_sm()
        .border_1()
        .border_color(theme.border)
        .text_color(theme.text)
        .hover(move |s| s.bg(hover))
        .child(label.into())
}

/// Wrap the finished card so callers can drop it straight into a
/// `.children(...)` slot.
pub fn finish(card: Div) -> AnyElement {
    card.into_any_element()
}

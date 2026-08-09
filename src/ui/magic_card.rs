//! The Magic view — the review step between a parsed natural-language
//! command and any file actually moving (`docs/design-magic-mode.md`).
//!
//! Presentation only: every function here takes what to draw and returns
//! an element. Parsing lives in [`filex::magic`], plan execution in
//! [`filex::ops`], and the state (which rows are checked) in the
//! workspace — same split as the rest of `ui`.
//!
//! Deliberately *not* a modal. A modal would demand an answer to a
//! question the user did not ask. In v2 the plan fills the content area
//! (the [`pane`] helpers) in place of the search results, entered by an
//! explicit toggle or an auto-switch on a clear command — never seizing
//! focus, always dismissable by clearing the query or toggling back.

use gpui::{AnyElement, Div, ElementId, SharedString, Stateful, div, prelude::*, px};

use super::theme::Theme;

/// Heading row: what the command was understood to mean, in plain words.
pub fn heading(theme: &Theme, text: impl Into<SharedString>) -> Div {
    div()
        .flex()
        .items_center()
        .gap_2()
        .child(div().text_sm().text_color(theme.text).child(text.into()))
}

/// The sub-line under the heading — match counts, skips, warnings.
pub fn subtitle(theme: &Theme, text: impl Into<SharedString>) -> Div {
    div()
        .text_xs()
        .text_color(theme.text_dim)
        .child(text.into())
}

/// A refusal: the command parsed but produced no runnable plan. Says
/// which reason, so "nothing happened" is never unexplained.
pub fn refusal(theme: &Theme, text: impl Into<SharedString>) -> Div {
    div().text_xs().text_color(theme.warn).child(text.into())
}

/// One operation in the plan: a checkbox, the source name, and where it
/// is going. Unchecked rows dim but stay legible — the user is
/// reviewing them, not dismissing them.
/// One operation in the plan, on a single fixed-height line (so the list
/// can virtualize): the checkbox, the source (name + the folder it lives
/// in), and where it is going. Both sides truncate with a … and the whole
/// row carries a tooltip with the full source → destination, since the
/// distinguishing part of a path is often the tail the truncation hides.
///
/// `dest` is `None` for deletes (there is no destination); the row then
/// just names the file and its folder.
pub fn op_row(
    theme: &Theme,
    id: impl Into<ElementId>,
    checked: bool,
    name: impl Into<SharedString>,
    location: impl Into<SharedString>,
    dest: Option<SharedString>,
) -> Stateful<Div> {
    let hover = theme.hover;
    // The source cell: the name stays whole (flex_none), its folder path
    // dims and truncates beside it.
    let source = div()
        .flex_1()
        .min_w_0()
        .flex()
        .items_baseline()
        .gap_2()
        .child(
            div()
                .flex_none()
                .text_xs()
                .text_color(theme.text)
                .whitespace_nowrap()
                .child(name.into()),
        )
        .child(
            div().flex_1().min_w_0().child(
                div()
                    .w_full()
                    .text_xs()
                    .text_color(theme.text_dim)
                    .truncate()
                    .child(location.into()),
            ),
        );
    div()
        .id(id)
        .flex()
        .items_center()
        .gap_2()
        .px_2()
        .h(px(28.))
        .rounded_sm()
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
        .when(!checked, |row| row.opacity(0.45))
        .child(checkbox(theme, checked))
        .child(source)
        .children(dest.map(|target| {
            div().flex_1().min_w_0().child(
                div()
                    .w_full()
                    .text_xs()
                    .text_color(theme.text_dim)
                    .truncate()
                    .child(target),
            )
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
        base.border_color(theme.accent)
            .bg(theme.accent)
            .child(div().text_color(theme.on_accent).text_xs().child("✓"))
    } else {
        base.border_color(theme.border)
    }
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
    let fill = if destructive {
        theme.warn
    } else {
        theme.accent
    };
    let base = div()
        .id(id)
        .px_3()
        .py_1()
        .rounded_md()
        .text_sm()
        .child(label.into());
    if enabled {
        base.cursor_pointer()
            .bg(fill)
            .text_color(theme.on_accent)
            .hover(|s| s.opacity(0.9))
    } else {
        base.border_1()
            .border_color(theme.border)
            .text_color(theme.text_dim)
            .opacity(0.6)
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

// ---------------------------------------------------------------------
// Full-pane layout (docs/design-magic-mode.md v2)
//
// The plan is the whole content area, not a card floating above unrelated
// search results. The atoms above (heading, subtitle, op_row, buttons)
// are shared; these add the pane shell, a scroll region that *fills*, and
// the header/action bands.
// ---------------------------------------------------------------------

/// The pane shell: a full-height column that replaces the results list.
pub fn pane() -> Div {
    div().flex().flex_1().min_h_0().flex_col().size_full()
}

/// The fixed header block at the top of the pane — heading, echo line,
/// and any notes. Does not scroll; the plan rows below it do.
pub fn pane_header(theme: &Theme) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .px_4()
        .py_3()
        .border_b_1()
        .border_color(theme.border)
}

/// The action bar pinned to the bottom of the pane (Cancel / confirm).
pub fn pane_actions(theme: &Theme) -> Div {
    div()
        .flex()
        .items_center()
        .justify_end()
        .gap_2()
        .px_4()
        .py_3()
        .border_t_1()
        .border_color(theme.border)
}

/// A centered empty/hint state for the pane — shown when magic mode is on
/// but no command has been typed yet, so the space explains what to type
/// rather than sitting blank.
pub fn pane_hint(theme: &Theme, title: impl Into<SharedString>, examples: &[&str]) -> AnyElement {
    let mut col = div()
        .flex()
        .flex_1()
        .min_h_0()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_2()
        .child(div().text_sm().text_color(theme.text).child(title.into()));
    for example in examples {
        col = col.child(
            div()
                .px_2()
                .py_0p5()
                .rounded_md()
                .bg(theme.hover)
                .text_xs()
                .text_color(theme.text_dim)
                .child(SharedString::from(example.to_string())),
        );
    }
    col.into_any_element()
}

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

/// Width of the "→" gutter between the source and destination columns.
/// Shared by [`op_row`] and [`plan_header`] so the header labels sit over
/// the cells they name.
const ARROW_COL_WIDTH: f32 = 12.;
/// Width of the checkbox column, likewise shared with the header.
const CHECK_COL_WIDTH: f32 = 13.;

/// One operation in the plan, on a single density-aware line (so the list
/// can virtualize): a checkbox, then equal-width columns for the source
/// name, the folder it lives in, and — for everything but a delete — the
/// destination, separated by a "→".
///
/// Every column is `flex_1 min_w_0`, so a cell starts at the same x on
/// every row. A plan is reviewed by scanning *down* a column ("are these
/// all really screenshots?"), which only works if the columns line up;
/// the previous layout let a long filename push its folder rightward, so
/// no two rows agreed on where anything began.
///
/// `dest` is `None` for deletes (there is no destination); the row then
/// just names the file and its folder.
pub fn op_row(
    theme: &Theme,
    ix: usize,
    checked: bool,
    name: impl Into<SharedString>,
    location: impl Into<SharedString>,
    dest: Option<SharedString>,
) -> Stateful<Div> {
    let hover = theme.hover;
    let column = |text: SharedString, color| {
        div().flex_1().min_w_0().child(
            div()
                .w_full()
                .text_xs()
                .text_color(color)
                .truncate()
                .child(text),
        )
    };
    // Unchecked rows dim their content but never the checkbox — dimming
    // the one control that re-arms the row made it read as disabled.
    let content = div()
        .flex_1()
        .min_w_0()
        .flex()
        .items_center()
        .gap_2()
        .when(!checked, |c| c.opacity(0.45))
        .child(column(name.into(), theme.text))
        .child(column(location.into(), theme.text_dim))
        .children(dest.into_iter().flat_map(|target| {
            [
                div()
                    .w(px(ARROW_COL_WIDTH))
                    .flex_none()
                    .text_xs()
                    .text_color(theme.text_dim)
                    .child("→"),
                column(target, theme.text_dim),
            ]
        }));
    div()
        .id(("magic-op", ix))
        // Fill the pane: uniform_list items shrink-wrap their content
        // otherwise, which leaves the columns ragged row to row and the
        // hover fill ending mid-row — the same trap `list_row` documents.
        .w_full()
        .flex()
        .items_center()
        .gap_2()
        .px_3()
        // Density-aware like every other row in the app. The previous
        // fixed 28px ignored the density setting from block 1, so the
        // plan was the one list that did not respond to it.
        .h(px(theme.row_height))
        .rounded_md()
        .cursor_pointer()
        // Same alternating stripe as the browse list, which is what makes
        // a long plan scannable.
        .when(ix % 2 == 1, |s| s.bg(theme.stripe))
        .hover(move |s| s.bg(hover))
        .child(checkbox(theme, checked))
        .child(content)
}

/// The column header above the plan rows — the browse list has one and
/// the plan did not, leaving its third column unexplained. Mirrors
/// [`op_row`]'s inset, gaps and column widths exactly, so the labels sit
/// over their cells.
///
/// `dest_label` is `None` for deletes, matching `op_row`'s missing
/// destination column.
pub fn plan_header(theme: &Theme, dest_label: Option<&'static str>) -> Div {
    let cell = |label: &'static str| {
        div()
            .flex_1()
            .min_w_0()
            .child(div().w_full().truncate().child(label))
    };
    div()
        .flex()
        .items_center()
        .gap_2()
        .px_3()
        .h(px(22.))
        .border_b_1()
        .border_color(theme.border)
        .text_xs()
        .text_color(theme.text_dim)
        // Spacer over the checkbox column so the labels align with the
        // cells below rather than sitting one control to the left.
        .child(div().w(px(CHECK_COL_WIDTH)).flex_none())
        .child(cell("Name"))
        .child(cell("In folder"))
        .children(
            dest_label
                .into_iter()
                .flat_map(|label| [div().w(px(ARROW_COL_WIDTH)).flex_none(), cell(label)]),
        )
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
    // `flex_1` already claims the width from the row parent; the old
    // `size_full` set an explicit w/h on top of that, which fights the
    // flex sizing instead of cooperating with it.
    div().flex().flex_1().min_h_0().flex_col()
}

/// The scroll region holding the plan rows. Carries the same small
/// horizontal inset the browse and search panes use, so `op_row`'s
/// rounded hover fill floats off the pane edge instead of running into
/// it.
pub fn pane_list() -> Div {
    div().flex().flex_col().flex_1().min_h_0().px_1p5()
}

/// The fixed header block at the top of the pane — heading, echo line,
/// and any notes. Does not scroll; the plan rows below it do.
pub fn pane_header(theme: &Theme) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        // px_3 (not px_4): the rows below sit at px_1p5 + px_3, and the
        // header text has to start on the same vertical line as the
        // column it introduces.
        .px_3()
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
        .px_3()
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

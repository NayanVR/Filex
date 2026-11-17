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

use gpui::{AnyElement, Div, SharedString, Stateful, div, prelude::*, px};

use super::theme::{Theme, fixed};

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
const CHECK_COL_WIDTH: f32 = 16.;
/// Widths of the trailing Modified and Size columns.
const DATE_COL_WIDTH: f32 = 96.;
const SIZE_COL_WIDTH: f32 = 64.;

/// The Modified and Size cells (or their labels, in the header): fixed
/// width, right-aligned, so a column of dates reads down cleanly.
fn facts_cells(theme: &Theme, date: SharedString, size: SharedString) -> [Div; 2] {
    let cell = |width: f32, text: SharedString| {
        super::list_row::tabular(
            div()
                .w(px(width))
                .flex_none()
                .text_xs()
                .text_right()
                .text_color(theme.text_dim)
                .truncate()
                .child(text),
        )
    };
    [cell(DATE_COL_WIDTH, date), cell(SIZE_COL_WIDTH, size)]
}

/// Width of the file-icon column.
const ICON_COL_WIDTH: f32 = 24.;
/// How far a file row sits in from its folder header: its checkbox
/// lands under the folder's icon, a step in from the folder's own.
const GROUP_INDENT: f32 = 46.;

/// A cell that takes an equal share of the row; an over-long name loses
/// its middle rather than its end, so the extension stays in view.
fn name_column(name: &str, color: gpui::Rgba) -> Div {
    div().flex_1().min_w_0().child(
        super::middle_truncated(name.to_owned())
            .w_full()
            .text_sm()
            .text_color(color),
    )
}

/// A secondary path cell (an answer's folder): smaller and dim, as the
/// folder column is in search results.
fn path_column(path: &str, color: gpui::Rgba) -> Div {
    div().flex_1().min_w_0().child(
        super::middle_truncated(path.to_owned())
            .w_full()
            .text_xs()
            .text_color(color),
    )
}

/// The shell every review-list line shares: full width (uniform_list
/// items shrink-wrap otherwise, leaving ragged columns and a hover fill
/// that ends mid-row), density-aware height, hover fill.
fn line(theme: &Theme, id: impl Into<gpui::ElementId>) -> Stateful<Div> {
    let hover = theme.hover;
    div()
        .id(id)
        .w_full()
        .flex()
        .items_center()
        .gap_2()
        .px_3()
        // The same rows as the file list and search results, density
        // included, so every list of files reads as one app.
        .h(px(theme.row_height))
        .rounded_md()
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
}

/// One operation in the plan, on a single density-aware line (so the list
/// can virtualize): a checkbox, the file's icon and name, and — for
/// everything but a delete — where it ends up, after a "→".
///
/// The name and destination are equal `flex_1 min_w_0` columns, so a cell
/// starts at the same x on every row. A plan is reviewed by scanning
/// *down* a column ("are these all really screenshots?"), which only
/// works if the columns line up.
///
/// The folder a file comes from is not a column: it is the group header
/// above the row (or the pane's subtitle when the plan has one folder).
/// `indent` sets the row in under such a header.
// One call per visible row; a params struct would only rename the list.
#[allow(clippy::too_many_arguments)]
pub fn op_row(
    theme: &Theme,
    ix: usize,
    checked: bool,
    icon: AnyElement,
    name: impl Into<SharedString>,
    dest: Option<(SharedString, gpui::Rgba)>,
    indent: bool,
    facts: Option<(SharedString, SharedString)>,
) -> Stateful<Div> {
    // Unchecked rows dim their content but never the checkbox — dimming
    // the one control that re-arms the row made it read as disabled.
    let content = div()
        .flex_1()
        .min_w_0()
        .flex()
        .items_center()
        .gap_2()
        .when(!checked, |c| c.opacity(0.45))
        .child(
            div()
                .w(px(ICON_COL_WIDTH))
                .flex_none()
                .flex()
                .justify_center()
                .child(icon),
        )
        .child(name_column(&name.into(), theme.text))
        // An empty destination still takes its cell (no arrow), so the
        // name column is the same width on every row.
        .children(dest.into_iter().flat_map(|(target, dest_color)| {
            let arrow = if target.is_empty() { "" } else { "→" };
            [
                div()
                    .w(px(ARROW_COL_WIDTH))
                    .flex_none()
                    .text_xs()
                    .text_color(theme.text_dim)
                    .child(arrow),
                name_column(&target, dest_color),
            ]
        }))
        .children(
            facts
                .into_iter()
                .flat_map(|(date, size)| facts_cells(theme, date, size)),
        );
    line(theme, ("magic-op", ix))
        .when(indent, |row| row.pl(px(12. + GROUP_INDENT)))
        .child(checkbox(theme, Some(checked)))
        .child(content)
}

/// A source-folder header in the review list: fold arrow, a checkbox for
/// the whole folder (`None` = some of its files), the folder, and how
/// many of its files are in. Clicking the row folds it; the caller wires
/// the checkbox, which is handed back separately for that reason.
pub fn group_row(
    theme: &Theme,
    ix: usize,
    collapsed: bool,
    parent: impl Into<SharedString>,
    leaf: impl Into<SharedString>,
    count: impl Into<SharedString>,
    check: Stateful<Div>,
) -> Stateful<Div> {
    line(theme, ("magic-group", ix))
        .child(
            super::icon::ui_icon(
                if collapsed {
                    "icons/chevron-right.svg"
                } else {
                    "icons/chevron-down.svg"
                },
                theme.text_dim,
            )
            .size(px(14.))
            .flex_none(),
        )
        .child(check)
        .child(
            super::icon::ui_icon("icons/folder.svg", theme.text_dim)
                .size(px(14.))
                .flex_none(),
        )
        // The way there is dim and gives way first; the folder itself is
        // what the eye is looking for.
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .items_center()
                .text_xs()
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_color(theme.text_dim)
                        .child(parent.into()),
                )
                .child(
                    div()
                        .flex_none()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text)
                        .child(leaf.into()),
                ),
        )
        .child(
            div()
                .flex_none()
                .text_xs()
                .text_color(theme.text_dim)
                .child(count.into()),
        )
}

/// A group header's checkbox as its own clickable element, so checking a
/// folder doesn't also fold it.
pub fn group_check(theme: &Theme, ix: usize, state: Option<bool>) -> Stateful<Div> {
    div()
        .id(("magic-group-check", ix))
        .flex_none()
        .child(checkbox(theme, state))
}

/// The column header above the plan rows. Mirrors [`op_row`]'s inset,
/// gaps and column widths exactly, so the labels sit over their cells.
///
/// `master` is the select-all checkbox, sitting over the rows' own.
/// `dest_label` is `None` when rows have no destination column; `indent`
/// says the rows sit in under folder headers.
pub fn plan_header(
    theme: &Theme,
    master: Stateful<Div>,
    dest_label: Option<&'static str>,
    indent: bool,
    facts: bool,
) -> Div {
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
        .h(px(28.))
        .mr(px(super::scrollbar::GUTTER))
        .border_b_1()
        .border_color(theme.border)
        .text_xs()
        .text_color(theme.text_dim)
        // Over the folder rows' checkboxes when there are folders: past
        // the fold arrow.
        .when(indent, |row| row.child(div().w(px(14.)).flex_none()))
        .child(master)
        // The rest of the way in to the file rows' name column.
        .when(indent, |row| {
            row.child(div().w(px(GROUP_INDENT - 30.)).flex_none())
        })
        // A spacer over the icon column so the labels align with the
        // cells below.
        .child(div().w(px(ICON_COL_WIDTH)).flex_none())
        .child(cell("Name"))
        .children(
            dest_label
                .into_iter()
                .flat_map(|label| [div().w(px(ARROW_COL_WIDTH)).flex_none(), cell(label)]),
        )
        .when(facts, |row| {
            row.children(facts_cells(theme, "Modified".into(), "Size".into()))
        })
}

/// A note under the pane heading with a leading icon — what the plan
/// will create, where an archive goes, which tag is applied.
pub fn note(theme: &Theme, icon: &'static str, text: impl Into<SharedString>) -> Div {
    div()
        .flex()
        .items_center()
        .gap_1p5()
        .child(
            super::icon::ui_icon(icon, theme.text_dim)
                .size(px(12.))
                .flex_none(),
        )
        .child(subtitle(theme, text))
}

/// One choice of a small segmented control (the conflict policy).
pub fn segment(
    theme: &Theme,
    id: impl Into<gpui::ElementId>,
    label: &'static str,
    active: bool,
) -> Stateful<Div> {
    let hover = theme.hover;
    div()
        .id(id)
        .px_2()
        .py_0p5()
        .rounded_md()
        .text_xs()
        .cursor_pointer()
        .when(active, |s| s.bg(theme.selected).text_color(theme.text))
        .when(!active, |s| {
            s.text_color(theme.text_dim).hover(move |s| s.bg(hover))
        })
        .child(label)
}

/// One file behind an answer: name, folder, size. Clicking reveals it.
pub fn answer_row(
    theme: &Theme,
    ix: usize,
    highlighted: bool,
    name: impl Into<SharedString>,
    folder: impl Into<SharedString>,
    size: impl Into<SharedString>,
    // This row's size over the largest row's, for the bar beside it.
    share: f32,
) -> Stateful<Div> {
    let name = name.into();
    line(theme, ("magic-answer-row", ix))
        .when(highlighted, |row| row.bg(theme.selected))
        .child(
            div()
                .w(px(ICON_COL_WIDTH))
                .flex_none()
                .flex()
                .justify_center()
                .child(super::icon::file_icon(theme, &name, 16.)),
        )
        .child(name_column(&name, theme.text))
        // A wider gutter only between the two text columns, so the name
        // starts where it does in search results.
        .child(path_column(&folder.into(), theme.text_dim).ml_2().mr_4())
        // A bar scaled to the largest file, so "8 GB … 684 MB" reads at a
        // glance instead of digit by digit.
        .child(
            div()
                .w(px(BAR_WIDTH))
                .h(px(4.))
                .flex_none()
                .rounded_full()
                .bg(theme.field)
                .child(
                    div()
                        .h_full()
                        .rounded_full()
                        .bg(fixed::with_alpha(theme.accent, 0.7))
                        .w(px((BAR_WIDTH * share.clamp(0., 1.)).max(2.))),
                ),
        )
        .child(super::list_row::tabular(
            div()
                .w(px(72.))
                .flex_none()
                .text_xs()
                .text_right()
                .text_color(theme.text_dim)
                .child(size.into()),
        ))
}

/// Width of an answer row's size bar.
const BAR_WIDTH: f32 = 56.;

/// Column labels over [`answer_row`]s, on the same grid.
pub fn answer_header(theme: &Theme) -> Div {
    let cell = |label: &'static str| div().flex_1().min_w_0().child(label);
    div()
        .flex()
        .items_center()
        .px_3()
        .h(px(28.))
        .mr(px(super::scrollbar::GUTTER))
        .border_b_1()
        .border_color(theme.border)
        .text_xs()
        .text_color(theme.text_dim)
        .gap_2()
        .child(div().w(px(ICON_COL_WIDTH)).flex_none())
        .child(cell("Name"))
        .child(cell("Folder").ml_2().mr_4())
        .child(div().w(px(BAR_WIDTH)).flex_none())
        .child(div().w(px(72.)).flex_none().text_right().child("Size"))
}

/// A quiet label between kinds of suggestion.
pub fn section_row(theme: &Theme, label: impl Into<SharedString>) -> Div {
    div()
        .w_full()
        .flex()
        .items_end()
        .px_3()
        .pb_1p5()
        .h(px(theme.row_height.max(30.)))
        .text_xs()
        .text_color(theme.text_dim)
        .child(label.into())
}

/// One thing the query could become, or the one thing it is. `enter`
/// shows the key on the row Enter would choose.
pub fn suggestion_row(
    theme: &Theme,
    ix: usize,
    highlighted: bool,
    icon: &'static str,
    label: impl Into<SharedString>,
    hint: &'static str,
    enter: bool,
) -> Stateful<Div> {
    line(theme, ("magic-suggestion", ix))
        .when(highlighted, |row| row.bg(theme.selected))
        .child(
            super::icon::ui_icon(
                icon,
                if highlighted {
                    theme.accent
                } else {
                    theme.text_dim
                },
            )
            .size(px(14.))
            .flex_none(),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_sm()
                .text_color(theme.text)
                .truncate()
                .child(label.into()),
        )
        .child(
            div()
                .flex_none()
                .text_xs()
                .text_color(theme.text_dim)
                .child(hint),
        )
        // The ↩ slot is kept on every row, so the hints don't shift as the
        // highlight moves.
        .child(
            div()
                .w(px(22.))
                .flex_none()
                .flex()
                .justify_end()
                .children(enter.then(|| super::kbd::keycap(theme, "↩"))),
        )
}

/// A small square check indicator. Hand-drawn rather than a glyph so it
/// carries the accent colour in both themes without a font dependency.
/// `None` is the mixed state of a group whose files are partly checked.
fn checkbox(theme: &Theme, state: Option<bool>) -> Div {
    let base = div()
        .flex_none()
        .size(px(CHECK_COL_WIDTH))
        .rounded(px(4.))
        .border_1()
        .flex()
        .items_center()
        .justify_center();
    // Checked is an accent box at part strength with a bright tick:
    // plainly apart from an empty one, without a column of solid fills
    // outweighing the file names or the one solid button.
    let on = |base: Div| {
        base.border_color(theme.accent)
            .bg(fixed::with_alpha(theme.accent, 0.45))
    };
    match state {
        Some(false) => base.border_color(theme.control_border).bg(theme.field),
        Some(true) => on(base).child(
            super::icon::ui_icon("icons/check.svg", theme.text)
                .size(px(11.))
                .flex_none(),
        ),
        None => on(base).child(div().w(px(8.)).h(px(1.5)).bg(theme.text)),
    }
}

/// A one-line reminder of the keys, pinned under a list that has no
/// buttons of its own.
pub fn hint_bar(theme: &Theme, text: &'static str) -> Div {
    div()
        .flex_none()
        .px_5()
        .py_2()
        .border_t_1()
        .border_color(theme.border)
        .text_xs()
        .text_color(theme.text_dim)
        .child(text)
}

/// The strip of notes between the pane's title and its rows: what the
/// plan creates, what it leaves out, how the sentence was read. One
/// wrapping line, so a plan with nothing to say spends no height on it.
pub fn notes(theme: &Theme) -> Div {
    // One line, wide gaps between notes, each note's action right after
    // its own words: a line per note cost the plan two rows of files.
    div()
        .flex_none()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_x_8()
        .gap_y_1()
        .px_5()
        .pb_3()
        .border_b_1()
        .border_color(theme.border)
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
    div()
        .flex()
        .flex_1()
        .min_w_0()
        .min_h_0()
        .flex_col()
        .overflow_hidden()
}

/// The scroll region holding the plan rows. Carries the same small
/// horizontal inset the browse and search panes use, so `op_row`'s
/// rounded hover fill floats off the pane edge instead of running into
/// it.
pub fn pane_list() -> Div {
    div().relative().flex().flex_col().flex_1().min_h_0().px_3()
}

/// The action bar pinned to the bottom of the pane (Cancel / confirm).
pub fn pane_actions(theme: &Theme) -> Div {
    div()
        .flex_none()
        .flex()
        .items_center()
        .justify_end()
        .gap_2()
        .px_3()
        .py_3()
        .border_t_1()
        .border_color(theme.border)
}

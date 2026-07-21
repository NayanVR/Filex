//! Rows for in-flight background jobs (long copies/moves): label,
//! progress bar, cancel control. Shown in a slim bar above the status
//! bar only while jobs run.

use gpui::{Div, ElementId, SharedString, Stateful, div, prelude::*, px, relative};

use super::icon;
use super::theme::Theme;

/// Container stacking one [`job_row`] per active job.
pub fn jobs_bar(theme: &Theme) -> Div {
    div().flex().flex_col().border_t_1().border_color(theme.border).bg(theme.panel)
}

/// One job: label on the left, progress bar filling the middle.
/// `fraction` is completed 0..=1, or `None` while the total is still
/// being sized (bar stays empty). Callers append the cancel button.
pub fn job_row(
    theme: &Theme,
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    fraction: Option<f32>,
) -> Stateful<Div> {
    let percent: SharedString = match fraction {
        Some(f) => format!("{:.0}%", f * 100.).into(),
        None => "…".into(),
    };
    div()
        .id(id)
        .flex()
        .items_center()
        .gap_2()
        .px_3()
        .h(px(24.))
        .text_xs()
        .text_color(theme.text_dim)
        .child(div().overflow_hidden().child(label.into()))
        .child(
            div().flex_1().h(px(4.)).rounded_full().bg(theme.border).child(
                div()
                    .h_full()
                    .rounded_full()
                    .bg(theme.accent)
                    .w(relative(fraction.unwrap_or(0.))),
            ),
        )
        .child(div().w(px(36.)).text_right().child(percent))
}

/// The ✕ that cancels a job; callers chain `.on_click`. The glyph is
/// tinted explicitly (gpui's `svg()` does not inherit text color); hover
/// just shifts the background.
pub fn cancel_button(theme: &Theme, id: impl Into<ElementId>) -> Stateful<Div> {
    let hover = theme.hover;
    div()
        .id(id)
        .flex()
        .items_center()
        .p_1()
        .rounded_sm()
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
        .child(icon::ui_icon("icons/x.svg", theme.text_dim).size(px(14.)))
}

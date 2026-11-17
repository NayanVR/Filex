//! Shared empty and unavailable states for the main content pane.

use super::{icon, theme::Theme};
use gpui::{AnyElement, Div, SharedString, div, prelude::*, px};

/// A neutral status message for search readiness and transient notices.
pub fn empty_state(theme: &Theme, text: impl Into<SharedString>) -> AnyElement {
    message_state(theme, "icons/search.svg", "Search", text)
}

/// A bounded empty/error state. A clear heading carries the state while
/// the supporting copy can wrap without extending past the file pane.
pub fn message_state(
    theme: &Theme,
    symbol: &'static str,
    title: impl Into<SharedString>,
    description: impl Into<SharedString>,
) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_w_0()
        .items_center()
        .justify_center()
        .gap_3()
        .p_6()
        .child(
            div()
                .flex()
                .items_center()
                .justify_center()
                .flex_none()
                .size(px(64.))
                .rounded_2xl()
                .bg(theme.panel)
                .child(icon::ui_icon(symbol, theme.text_dim).size(px(28.))),
        )
        .child(
            div()
                .w_full()
                .max_w(px(360.))
                .text_center()
                .text_base()
                .font_weight(gpui::FontWeight::MEDIUM)
                .child(title.into()),
        )
        .child(
            div()
                .w_full()
                .max_w(px(360.))
                .text_center()
                .text_sm()
                .text_color(theme.text_dim)
                .child(description.into()),
        )
        .into_any_element()
}

/// The shared content heading, with a compact summary and trailing controls.
pub fn content_header(
    theme: &Theme,
    title: impl Into<SharedString>,
    subtitle: impl Into<SharedString>,
    controls: impl IntoElement,
) -> Div {
    let subtitle = subtitle.into();
    div()
        .flex()
        .flex_none()
        // Top-aligned at a fixed inset, title and controls alike: centred,
        // the title moved whenever the subtitle or the controls beside it
        // changed height between panes.
        .items_start()
        .justify_between()
        .gap_3()
        .px_5()
        .pt(px(14.))
        // A title alone needs less room than a title and a summary.
        .h(px(if subtitle.is_empty() { 60. } else { 76. }))
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(
                    div()
                        .w_full()
                        .truncate()
                        .text_xl()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child(title.into()),
                )
                .children((!subtitle.is_empty()).then(|| {
                    div()
                        .w_full()
                        .truncate()
                        .text_xs()
                        .text_color(theme.text_dim)
                        .child(subtitle)
                })),
        )
        .child(
            div()
                .flex_none()
                .h(px(32.))
                .flex()
                .items_center()
                .child(controls),
        )
}

/// A short fade over a scrolling list's foot, so a row or section header
/// cut by the pane's edge (or a footer) dissolves instead of being sliced.
/// The parent must be `relative`; lists keep a matching bottom padding so
/// their last row clears it at the end of the scroll. Takes no input.
pub fn bottom_fade(theme: &Theme) -> Div {
    div()
        .absolute()
        .left_0()
        .right_0()
        .bottom_0()
        .h(px(24.))
        .bg(gpui::linear_gradient(
            0.,
            gpui::linear_color_stop(theme.bg, 0.),
            gpui::linear_color_stop(super::theme::fixed::with_alpha(theme.bg, 0.), 1.),
        ))
}

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
    div()
        .flex()
        .flex_none()
        .items_center()
        .justify_between()
        .gap_3()
        .px_5()
        .h(px(76.))
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
                .child(
                    div()
                        .w_full()
                        .truncate()
                        .text_xs()
                        .text_color(theme.text_dim)
                        .child(subtitle.into()),
                ),
        )
        .child(controls)
}

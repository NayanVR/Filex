//! The right-hand details / preview panel (block 5).
//!
//! Presentation only: a fixed-width column with a large preview area
//! over the selected item's name and a stack of "label — value"
//! metadata rows. The workspace supplies the values (and fetches the
//! lazy ones — created time, image dimensions — off-thread).

use gpui::{Div, SharedString, div, prelude::*, px};

use super::theme::Theme;

/// Narrowest the panel may be persisted/resized to.
pub const MIN_WIDTH: f32 = 220.;
/// Widest the panel may be persisted/resized to.
pub const MAX_WIDTH: f32 = 480.;

/// Clamp a persisted/requested width into the allowed range.
pub fn clamp_width(width: f32) -> f32 {
    width.clamp(MIN_WIDTH, MAX_WIDTH)
}

/// The panel container: fixed-width column, left border, panel bg.
pub fn panel(theme: &Theme, width: f32) -> Div {
    div()
        .flex()
        .flex_col()
        .flex_none()
        .w(px(clamp_width(width)))
        .h_full()
        .p_4()
        .gap_3()
        .border_l_1()
        .border_color(theme.border)
        .bg(theme.panel)
        .overflow_hidden()
}

/// The large preview area holding an image or a big file icon, centered.
pub fn preview_box(theme: &Theme) -> Div {
    div()
        .flex()
        .items_center()
        .justify_center()
        .w_full()
        .h(px(180.))
        .flex_none()
        .rounded_lg()
        .bg(theme.hover)
        .overflow_hidden()
}

/// The item name heading (wraps onto a second line if needed).
pub fn title(theme: &Theme, name: impl Into<SharedString>) -> Div {
    div().text_sm().text_color(theme.text).child(name.into())
}

/// A "label — value" metadata row: dim label left, value right.
pub fn meta_row(
    theme: &Theme,
    label: impl Into<SharedString>,
    value: impl Into<SharedString>,
) -> Div {
    div()
        .flex()
        .justify_between()
        .items_start()
        .gap_4()
        .text_xs()
        .child(div().flex_none().text_color(theme.text_dim).child(label.into()))
        .child(
            div()
                .flex_1()
                .text_right()
                .text_color(theme.text)
                .overflow_hidden()
                .child(value.into()),
        )
}

/// A thin section divider.
pub fn divider(theme: &Theme) -> Div {
    div().flex_none().h(px(1.)).bg(theme.border)
}

/// Centered dim message when nothing is selected.
pub fn empty(theme: &Theme, text: impl Into<SharedString>) -> Div {
    div()
        .flex()
        .flex_1()
        .items_center()
        .justify_center()
        .text_xs()
        .text_color(theme.text_dim)
        .child(text.into())
}

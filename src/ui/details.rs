//! The right-hand details / preview panel (block 5).
//!
//! Presentation only: a fixed-width column with a large preview area
//! over the selected item's name and a stack of "label — value"
//! metadata rows. The workspace supplies the values (and fetches the
//! lazy ones — created time, image dimensions — off-thread).

use gpui::{Div, ElementId, Rgba, SharedString, Stateful, div, prelude::*, px, rgb, rgba};

use filex::tags::TagColor;

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

/// A dim section label (e.g. the "Tags" heading above the chips).
pub fn section_label(theme: &Theme, text: impl Into<SharedString>) -> Div {
    div().flex_none().text_xs().text_color(theme.text_dim).child(text.into())
}

/// Solid RGB dot color for each Finder tag color. Kept vivid and
/// theme-independent (Finder's dots read the same on either background);
/// the uncolored case uses the theme's dim text instead.
fn tag_hex(color: TagColor) -> u32 {
    match color {
        TagColor::Grey => 0x8e8e93,
        TagColor::Green => 0x5bd15b,
        TagColor::Purple => 0xcb6ce6,
        TagColor::Blue => 0x2f95ff,
        TagColor::Yellow => 0xf5c518,
        TagColor::Red => 0xfb5850,
        TagColor::Orange => 0xf7a53b,
    }
}

/// The solid dot color for a tag (dim grey when it has no color).
pub fn tag_dot_color(theme: &Theme, color: Option<TagColor>) -> Rgba {
    color.map(|c| rgb(tag_hex(c))).unwrap_or(theme.text_dim)
}

/// A small filled circle — the color dot inside chips and swatches.
fn dot(color: Rgba, diameter: f32) -> Div {
    div().flex_none().size(px(diameter)).rounded_full().bg(color)
}

/// A tag chip: a rounded pill with a color dot and the tag name, tinted
/// by the tag's color. The whole chip is one click target (opens the
/// editor for that tag); callers chain `.on_click`.
pub fn tag_chip(
    theme: &Theme,
    id: impl Into<ElementId>,
    name: impl Into<SharedString>,
    color: Option<TagColor>,
) -> Stateful<Div> {
    // A faint wash of the tag color behind the pill (theme hover when
    // uncolored), lifting a touch on hover.
    let (bg, hover_bg) = match color {
        Some(c) => (rgba(tag_hex(c) << 8 | 0x1f), rgba(tag_hex(c) << 8 | 0x33)),
        None => (theme.hover, theme.selected),
    };
    div()
        .id(id)
        .flex()
        .items_center()
        .gap_1()
        .px_2()
        .py_1()
        .rounded_full()
        .bg(bg)
        .cursor_pointer()
        .hover(move |s| s.bg(hover_bg))
        .child(dot(tag_dot_color(theme, color), 7.))
        .child(div().text_xs().text_color(theme.text).child(name.into()))
}

/// The "add a tag" affordance: a bordered pill with a plus. Callers chain
/// `.on_click` to open the editor for a new tag.
pub fn add_tag_chip(theme: &Theme, id: impl Into<ElementId>) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .px_2()
        .py_1()
        .rounded_full()
        .border_1()
        .border_color(theme.border)
        .cursor_pointer()
        .text_xs()
        .text_color(theme.text_dim)
        .hover(|s| s.bg(theme.hover))
        .child("+ Tag")
}

/// A color swatch in the picker: a filled dot for a color, or a hollow
/// ring for "no color". `selected` rings it in the accent color. Callers
/// chain `.on_click`.
pub fn tag_swatch(
    theme: &Theme,
    id: impl Into<ElementId>,
    color: Option<TagColor>,
    selected: bool,
) -> Stateful<Div> {
    let ring = if selected { theme.accent } else { theme.border };
    let base = div()
        .id(id)
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .size(px(20.))
        .rounded_full()
        .border_1()
        .border_color(ring)
        .cursor_pointer();
    match color {
        Some(c) => base.child(dot(rgb(tag_hex(c)), 12.)),
        // "No color": an empty ring, dim-filled so it reads as a target.
        None => base.bg(theme.hover),
    }
}

/// A small text button for the editor footer (Save / Remove / Cancel).
/// `danger` paints it in the warn color; otherwise it uses the accent.
pub fn tag_button(
    theme: &Theme,
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    danger: bool,
) -> Stateful<Div> {
    div()
        .id(id)
        .px_2()
        .py_1()
        .rounded_md()
        .cursor_pointer()
        .text_xs()
        .text_color(if danger { theme.warn } else { theme.accent })
        .hover(|s| s.bg(theme.hover))
        .child(label.into())
}

/// The inline tag editor container (input + swatches + buttons), a
/// bordered card that appears under the chips while editing.
pub fn tag_editor_box(theme: &Theme) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_2()
        .mt_1()
        .p_2()
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .bg(theme.bg)
}

/// A row that wraps its children (the chip strip and the swatch strip).
pub fn wrap_row() -> Div {
    div().flex().flex_wrap().items_center().gap_1()
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

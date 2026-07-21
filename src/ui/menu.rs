//! Hand-rolled anchored context menu.
//!
//! gpui-component was evaluated first (per the roadmap rule): it
//! tracks gpui's git head while filex pins crates.io gpui, and pulling
//! its whole widget tree for one menu isn't a good trade. This is a
//! transparent full-window overlay (any click closes) with an anchored
//! card on top, faded/slid in with an eased animation per the polish
//! principle — the animation lives on the popover, never on list rows.

use std::time::Duration;

use gpui::{
    Animation, AnimationExt as _, AnyElement, Div, ElementId, Pixels, Point, SharedString,
    Stateful, div, ease_out_quint, prelude::*, px,
};

use super::theme::Theme;

/// Menu card width; callers use it to clamp the anchor so the menu
/// never opens past the window edge.
pub const MENU_WIDTH: f32 = 200.;

/// Transparent overlay covering the window; a click anywhere on it
/// (i.e. outside the menu) should close — callers chain `.on_click`.
pub fn overlay(id: impl Into<ElementId>) -> Stateful<Div> {
    div().id(id).absolute().inset_0()
}

/// The anchored menu card. `position` is in window coordinates,
/// already clamped by the caller; `items` come from [`item`] /
/// [`separator`].
pub fn panel(theme: &Theme, position: Point<Pixels>, items: Vec<AnyElement>) -> impl IntoElement {
    div()
        .absolute()
        .left(position.x)
        .top(position.y)
        .w(px(MENU_WIDTH))
        .py_1()
        .rounded_lg()
        .border_1()
        .border_color(theme.border)
        .bg(theme.panel)
        .shadow_lg()
        .flex()
        .flex_col()
        .children(items)
        .with_animation(
            "context-menu-open",
            Animation::new(Duration::from_millis(120)).with_easing(ease_out_quint()),
            |panel, delta| panel.opacity(delta),
        )
}

/// One menu entry; callers chain `.on_click`. `danger` marks
/// destructive entries (trash, remove) with the warn color.
pub fn item(
    theme: &Theme,
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    danger: bool,
) -> Stateful<Div> {
    let selected = theme.selected;
    div()
        .id(id)
        .mx_1()
        .px_2()
        .py_1()
        .rounded_md()
        .cursor_pointer()
        .text_sm()
        .text_color(if danger { theme.warn } else { theme.text })
        .hover(move |s| s.bg(selected))
        .child(label.into())
}

/// Thin line between item groups.
pub fn separator(theme: &Theme) -> Div {
    div().my_1().h(px(1.)).bg(theme.border)
}

/// Dim, non-interactive heading (e.g. the file name the menu is for).
pub fn heading(theme: &Theme, label: impl Into<SharedString>) -> Div {
    div()
        .mx_1()
        .px_2()
        .pb_1()
        .text_xs()
        .text_color(theme.text_dim)
        .overflow_hidden()
        .child(label.into())
}

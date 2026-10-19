//! Hand-rolled anchored context menu.
//!
//! gpui-component was evaluated first (per the roadmap rule): it
//! tracks gpui's git head while filex pins crates.io gpui, and pulling
//! its whole widget tree for one menu isn't a good trade. This is a
//! transparent full-window overlay (any click closes) with an anchored
//! card on top, faded/slid in with an eased animation per the polish
//! principle — the animation lives on the popover, never on list rows.
//!
//! What a menu contains comes from `filex::context_menu` (pure, tested);
//! this module only draws its rows. Every row kind has a fixed height, so
//! [`row_top`] / [`panel_height`] can place a submenu beside its parent row
//! and keep a menu on screen without measuring the laid-out element.

use std::time::Duration;

use filex::context_menu::{Item, Row};
use gpui::{
    Animation, AnimationExt as _, AnyElement, Div, ElementId, Pixels, Point, SharedString,
    Stateful, div, ease_out_quint, prelude::*, px,
};

use super::icon::ui_icon;
use super::theme::Theme;

/// Menu card width; callers use it to clamp the anchor so the menu
/// never opens past the window edge. Wide enough for the five-button
/// icon bar and a right-aligned shortcut hint.
pub const MENU_WIDTH: f32 = 248.;

const ITEM_HEIGHT: f32 = 28.;
const SEPARATOR_HEIGHT: f32 = 9.;
const HEADING_HEIGHT: f32 = 22.;
const ICON_BAR_HEIGHT: f32 = 40.;
/// Card border (1px) plus `py_1` padding, above the first row.
const PANEL_INSET: f32 = 5.;

fn row_height(row: &Row) -> f32 {
    match row {
        Row::Item(_) => ITEM_HEIGHT,
        Row::Separator => SEPARATOR_HEIGHT,
        Row::Heading(_) => HEADING_HEIGHT,
        Row::Icons(_) => ICON_BAR_HEIGHT,
    }
}

/// Offset of row `ix` from the top of its panel.
pub fn row_top(rows: &[Row], ix: usize) -> f32 {
    PANEL_INSET + rows.iter().take(ix).map(row_height).sum::<f32>()
}

/// Full height of a panel holding `rows`.
pub fn panel_height(rows: &[Row]) -> f32 {
    row_top(rows, rows.len()) + PANEL_INSET
}

/// Transparent overlay covering the window; a click anywhere on it
/// (i.e. outside the menu) should close — callers chain `.on_click`.
///
/// `.occlude()` is load-bearing, not cosmetic: without it the overlay is
/// transparent to hover and clicks, so the rows *behind* the menu still
/// highlight on hover and — worse — still fire their own click when you
/// pick a menu item. Occluding makes the overlay swallow every mouse
/// event in its bounds; the menu panel sits on top as a child, so its
/// items keep working while the background is inert.
pub fn overlay(id: impl Into<ElementId>) -> Stateful<Div> {
    div().id(id).absolute().inset_0().occlude()
}

/// The anchored menu card. `position` is in window coordinates,
/// already clamped by the caller; `items` come from [`item`] /
/// [`separator`].
pub fn panel(theme: &Theme, position: Point<Pixels>, items: Vec<AnyElement>) -> impl IntoElement {
    fade_in(
        "context-menu-open",
        anchored_card(theme, position).children(items),
    )
}

/// A [`panel`] that scrolls once its items pass `max_height`. `id` also
/// keys its fade-in, so it must differ from other panels shown with it.
pub fn scroll_panel(
    theme: &Theme,
    id: &'static str,
    position: Point<Pixels>,
    max_height: Pixels,
    items: Vec<AnyElement>,
) -> impl IntoElement {
    fade_in(
        id,
        anchored_card(theme, position)
            .id(id)
            .max_h(max_height)
            .overflow_y_scroll()
            .children(items),
    )
}

fn anchored_card(theme: &Theme, position: Point<Pixels>) -> Div {
    super::card(theme)
        .absolute()
        .left(position.x)
        .top(position.y)
        .w(px(MENU_WIDTH))
        .py_1()
}

/// `id` must differ between panels shown together, or they share one
/// animation's state.
fn fade_in<E: IntoElement + Styled + 'static>(id: &'static str, panel: E) -> impl IntoElement {
    panel.with_animation(
        id,
        Animation::new(Duration::from_millis(120)).with_easing(ease_out_quint()),
        |panel, delta| panel.opacity(delta),
    )
}

/// One plain text entry (the search scope dropdown); callers chain
/// `.on_click`. `danger` marks destructive entries with the warn color.
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

/// A context-menu row drawn from the model: leading icon (or check mark),
/// label, right-aligned shortcut hint, and a chevron when it opens a
/// submenu. `highlighted` is the pointer/keyboard position — the view
/// tracks it, so there is no CSS hover here (pointer and arrow keys must
/// agree on one highlighted row). Callers chain `.on_click` / `.on_hover`.
pub fn model_row(
    theme: &Theme,
    id: impl Into<ElementId>,
    item: &Item,
    shortcut: Option<SharedString>,
    highlighted: bool,
) -> Stateful<Div> {
    let color = if item.danger { theme.warn } else { theme.text };
    let lead = if item.checked {
        Some(ui_icon("icons/check.svg", theme.accent))
    } else {
        item.icon.map(|icon| ui_icon(icon, color))
    };
    div()
        .id(id)
        .mx_1()
        .px_2()
        .h(px(ITEM_HEIGHT))
        .flex()
        .items_center()
        .gap_2()
        .rounded_md()
        .cursor_pointer()
        .text_sm()
        .text_color(color)
        .when(highlighted, |row| row.bg(theme.selected))
        .child(
            div()
                .w(px(16.))
                .flex_none()
                .flex()
                .justify_center()
                .children(lead.map(|icon| icon.size(px(14.)))),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .child(SharedString::from(item.label.to_string())),
        )
        .children(shortcut.map(|keys| {
            div()
                .flex_none()
                .text_xs()
                .text_color(theme.text_dim)
                .child(keys)
        }))
        .children(
            item.submenu
                .map(|_| ui_icon("icons/chevron-right.svg", theme.text_dim).size(px(14.))),
        )
}

/// Windows 11's strip of icon-only verbs (cut, copy, rename, share,
/// delete) across the top of the menu.
pub fn icon_bar(buttons: Vec<AnyElement>) -> Div {
    div()
        .h(px(ICON_BAR_HEIGHT))
        .mx_1()
        .flex()
        .items_center()
        .justify_between()
        .px_1()
        .children(buttons)
}

/// One icon-bar button; its label is the tooltip.
pub fn icon_button(
    theme: &Theme,
    id: impl Into<ElementId>,
    item: &Item,
    highlighted: bool,
) -> Stateful<Div> {
    let color = if item.danger { theme.warn } else { theme.text };
    div()
        .id(id)
        .size(px(32.))
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .cursor_pointer()
        .when(highlighted, |button| button.bg(theme.selected))
        .tooltip(super::tooltip::text_tooltip(item.label.to_string(), *theme))
        .children(item.icon.map(|icon| ui_icon(icon, color).size(px(16.))))
}

/// Thin line between item groups.
pub fn separator(theme: &Theme) -> Div {
    div()
        .h(px(SEPARATOR_HEIGHT))
        .flex()
        .items_center()
        .child(div().w_full().h(px(1.)).bg(theme.border))
}

/// Dim, non-interactive heading (e.g. the file name the menu is for).
pub fn heading(theme: &Theme, label: impl Into<SharedString>) -> Div {
    div()
        .h(px(HEADING_HEIGHT))
        .mx_1()
        .px_2()
        .flex()
        .items_center()
        .text_xs()
        .text_color(theme.text_dim)
        .child(div().truncate().child(label.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use filex::context_menu::{Capabilities, EntryFacts, entry_menu};

    #[test]
    fn row_tops_accumulate_fixed_heights() {
        let rows = entry_menu(
            &EntryFacts {
                heading: "a.txt".into(),
                count: 1,
                is_dir: false,
                from_search: false,
                favorite: false,
            },
            Capabilities {
                share: true,
                terminal: true,
            },
        );
        assert_eq!(row_top(&rows, 0), PANEL_INSET);
        // Heading, then the icon bar, then "Open".
        assert_eq!(
            row_top(&rows, 2),
            PANEL_INSET + HEADING_HEIGHT + ICON_BAR_HEIGHT
        );
        assert_eq!(
            panel_height(&rows),
            row_top(&rows, rows.len() - 1) + ITEM_HEIGHT + PANEL_INSET
        );
    }
}

//! Sidebar building blocks: the panel container, section headers, and
//! the hoverable row scaffold. Row content (place/state icons via
//! [`super::icon`], labels) is supplied by the caller.
//!
//! Every sidebar entry (places, tags, action rows, banners)
//! starts from [`sidebar_row`] so hover feedback and spacing stay
//! uniform; callers chain their content and `.on_click`, plus style
//! overrides (dim text for action rows, smaller warn text for banners).

use gpui::{
    Animation, AnimationExt as _, Div, ElementId, SharedString, Stateful, Transformation, div,
    ease_out_quint, prelude::*, px, radians, relative,
};

use super::icon;
use super::theme::{Theme, fixed};

/// Sidebar width. One place so panels/overlays can align to it later.
pub const SIDEBAR_WIDTH: f32 = 216.;

/// Fixed height of a standard sidebar row. Every row is this tall so the
/// panel scrolls cleanly instead of compressing rows into each other.
pub const ROW_HEIGHT: f32 = 34.;

/// Definite width for a row's truncating label: the panel width minus the
/// row's horizontal margins (`mx_2`) and padding (`px_2`), a leading icon
/// ([`ITEM_ICON`]), and the gap after it. gpui only paints the … ellipsis against
/// a *definite* width, and — unlike `uniform_list` rows — a nested sidebar
/// row never gets one from a `w_full`-in-flex child, so it's pinned here.
const LABEL_WIDTH: f32 = SIDEBAR_WIDTH - 16. - 16. - ITEM_ICON - 8.;

/// The sidebar panel itself: a fixed-width column on the shell surface.
/// No divider — the raised content canvas beside it provides the edge.
/// Children are headers and rows, top-down.
pub fn sidebar_panel() -> Div {
    div()
        .flex()
        .flex_col()
        .w(px(SIDEBAR_WIDTH))
        .flex_none()
        .h_full()
        .pb_2()
}

/// A clickable section header: a quiet label, with the disclosure
/// chevron at the trailing edge. Expanded sections show the chevron only
/// on hover, so the column reads as calm labels rather than a tree view;
/// a collapsed one keeps it visible so it's clear there's more inside.
/// The whole header toggles; callers render the section's rows only when
/// `!collapsed`.
pub fn collapsible_header(
    theme: &Theme,
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    collapsed: bool,
) -> Stateful<Div> {
    let (dim, text) = (theme.text_dim, theme.text);
    // One right-pointing chevron that turns to point down when the
    // section opens; the id carries the state so each toggle replays it.
    let (from, to) = if collapsed { (90., 0.) } else { (0., 90.) };
    let rest = if collapsed {
        dim
    } else {
        fixed::with_alpha(dim, 0.)
    };
    div()
        .id(id)
        .group("sidebar-header")
        .flex()
        .flex_shrink_0()
        .items_center()
        .justify_between()
        .mx_2()
        .px_2()
        .mt_4()
        .mb_0p5()
        .h(px(24.))
        .rounded_md()
        .cursor_pointer()
        .text_xs()
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(dim)
        .hover(move |s| s.text_color(text))
        .child(label.into())
        .child(
            icon::ui_icon("icons/chevron-right.svg", rest)
                .size(px(12.))
                .group_hover("sidebar-header", move |s| s.text_color(dim))
                .with_animation(
                    ("chevron", collapsed as usize),
                    Animation::new(super::motion::FADE).with_easing(ease_out_quint()),
                    move |chevron, t| {
                        let degrees = from + (to - from) * t;
                        chevron.with_transformation(Transformation::rotate(radians(
                            degrees.to_radians(),
                        )))
                    },
                ),
        )
}

/// A stacked drive entry (taller than a row): name line, capacity bar,
/// and a free/total line. Callers add the children and chain `.on_click`.
pub fn drive_row(theme: &Theme, id: impl Into<ElementId>) -> Stateful<Div> {
    let hover = theme.hover;
    div()
        .id(id)
        .flex()
        .flex_shrink_0()
        .flex_col()
        .gap_1()
        .mx_2()
        .px_2()
        .py_2()
        .rounded_lg()
        .cursor_pointer()
        .overflow_hidden()
        .hover(move |s| s.bg(hover))
}

/// A capacity bar showing `fraction` used (0..=1), turning warn-colored
/// when the volume is nearly full.
pub fn capacity_bar(theme: &Theme, fraction: f32) -> Div {
    let f = fraction.clamp(0., 1.);
    let fill = if f > 0.9 { theme.warn } else { theme.accent };
    div()
        .w_full()
        .h(px(4.))
        .rounded_full()
        .bg(theme.border)
        .child(div().h_full().rounded_full().bg(fill).w(relative(f)))
}

/// Glyph edge for sidebar item icons — one size for every section.
const ITEM_ICON: f32 = 16.;

/// A navigation entry: icon + truncating label on a [`sidebar_row`].
/// `active` marks the current location the Fluent way — a neutral fill
/// (an accent tint turns muddy over Mica), a short accent bar at the
/// leading edge, and an accent glyph — while the label keeps the regular
/// text colour for legibility.
/// Callers chain a tooltip, `.on_click` and menus.
pub fn sidebar_item(
    theme: &Theme,
    id: impl Into<ElementId>,
    glyph: &'static str,
    label: impl Into<SharedString>,
    active: bool,
) -> Stateful<Div> {
    let tint = if active { theme.accent } else { theme.text_dim };
    sidebar_row(theme, id)
        .relative()
        .when(active, |row| {
            row.bg(theme.hover)
                .shadow(vec![super::sheen(theme)])
                .font_weight(gpui::FontWeight::MEDIUM)
                .child(
                    div()
                        .absolute()
                        .left_0()
                        .top(px((ROW_HEIGHT - 16.) / 2.))
                        .w(px(3.))
                        .h(px(16.))
                        .rounded_full()
                        .bg(theme.accent),
                )
        })
        .child(icon::ui_icon(glyph, tint).size(px(ITEM_ICON)))
        .child(sidebar_label(label))
}

/// A single-line, ellipsis-truncated label for a [`sidebar_row`]. Uses an
/// explicit [`LABEL_WIDTH`] (not `flex_1`) because gpui only paints the …
/// against a definite width, and a nested sidebar row doesn't hand one
/// down through flex the way a `uniform_list` row does. Pair it with a
/// tooltip on the row for the full text.
pub fn sidebar_label(label: impl Into<SharedString>) -> Div {
    div()
        .flex_none()
        .w(px(LABEL_WIDTH))
        .truncate()
        .child(label.into())
}

/// The common sidebar row: inset, rounded, hover-highlighted.
///
/// Fixed height + `flex_shrink_0`: without these, a full sidebar's rows
/// would be squeezed thinner than their text (overlapping) to fit the
/// column instead of letting the panel scroll. Now every row keeps
/// [`ROW_HEIGHT`] and the overflow scrolls.
pub fn sidebar_row(theme: &Theme, id: impl Into<ElementId>) -> Stateful<Div> {
    let hover = theme.hover;
    div()
        .id(id)
        .flex()
        .flex_shrink_0()
        .items_center()
        .gap_2()
        .mx_2()
        .px_2()
        .h(px(ROW_HEIGHT))
        .rounded_lg()
        .cursor_pointer()
        .text_sm()
        // Long labels clip at the sidebar edge instead of painting
        // over the neighboring pane.
        .overflow_hidden()
        .hover(move |s| s.bg(hover))
}

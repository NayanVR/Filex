//! The topmost bar: the tab strip on the left and the global utility icon
//! group on the right.
//!
//! Presentation only: the workspace owns the tab list and supplies each
//! title + active flag, and wires the click / middle-click / close /
//! new-tab handlers. Always shown — a single tab still gets a chip so the
//! icon group has a stable home and the bar reads the same at one tab or
//! many.

use gpui::{Div, ElementId, SharedString, Stateful, div, prelude::*, px};

use super::theme::Theme;

/// Height of the tab strip (also the titlebar on Windows).
pub const TAB_BAR_HEIGHT: f32 = 42.;

/// The bar container. Children are the left tab group (tabs +
/// the new-tab button), a draggable spacer, and the right icon group.
///
/// This is the topmost bar, so on macOS it carries the inset traffic
/// lights (the system titlebar is transparent) and pads left to clear
/// them; on Windows it is the titlebar and the caption buttons sit flush
/// at its right edge. It shares the shell surface with the nav bar below
/// — no divider, no fill of its own (the root paints the shell) — so the
/// chrome reads as one piece.
pub fn tab_bar() -> Div {
    let bar = div()
        .flex()
        .items_center()
        .gap_1()
        .h(px(TAB_BAR_HEIGHT))
        .flex_none()
        .pl_2();
    // Clear the inset traffic lights: three buttons from x=12 spanning ~52px.
    #[cfg(target_os = "macos")]
    let bar = bar.pl(px(76.));
    bar
}

/// The left group: tabs plus the new-tab button. Shrinks (clipping its
/// tabs) before it pushes the icon group off the bar, but doesn't grow —
/// the space after it is the window's drag handle ([`drag_region`]).
pub fn tab_group() -> Div {
    div()
        .flex()
        // Shrinks by default (flex-shrink: 1); `min_w_0` lets it go below
        // its content width so tabs clip rather than overflow.
        .min_w_0()
        .items_center()
        .gap_1()
        .overflow_hidden()
}

/// The empty run of the bar after the tabs. Tagged as the window's drag
/// area, so where the system titlebar is hidden it moves the window and
/// double-click maximizes (the OS handles both via the caption hit-test).
pub fn drag_region() -> Div {
    div()
        .flex_1()
        .h_full()
        .window_control_area(gpui::WindowControlArea::Drag)
}

/// One tab: the active one is raised — canvas fill, a soft shadow and a lit
/// top edge, so
/// it reads as the page the content below belongs to; the rest sit flat
/// on the shell with hover feedback. Callers add the label + close
/// control and chain handlers.
pub fn tab(theme: &Theme, id: impl Into<ElementId>, active: bool) -> Stateful<Div> {
    let hover = theme.hover;
    let base = div()
        .id(id)
        .flex()
        .items_center()
        .gap_2()
        .h(px(30.))
        .w(px(180.))
        .min_w(px(64.))
        .max_w(px(200.))
        .pl_3()
        .pr_1()
        .rounded_lg()
        .cursor_pointer()
        .overflow_hidden();
    if active {
        base.bg(theme.bg)
            .shadow(super::raised(theme))
            .text_color(theme.text)
            .font_weight(gpui::FontWeight::MEDIUM)
    } else {
        base.text_color(theme.text_dim).hover(move |s| s.bg(hover))
    }
}

/// A tab's label (clips at the tab's max width).
pub fn tab_label(name: impl Into<SharedString>) -> Div {
    div()
        .flex_1()
        .min_w_0()
        .text_xs()
        .truncate()
        .overflow_hidden()
        .child(name.into())
}

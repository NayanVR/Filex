//! Modal dialog building blocks (conflict prompts, future confirms).
//!
//! Rendered as the root element's last child: the dimmed backdrop
//! covers the window, paints above everything, and soaks up clicks so
//! nothing behind it is interactive while the dialog is open.

use gpui::{Div, ElementId, SharedString, Stateful, div, prelude::*, px};

use super::button::{Size, Variant};
use super::theme::{Theme, fixed};

/// Full-window dimmed backdrop, centering its child (the panel).
/// Callers chain `.on_click` for click-outside-to-cancel.
pub fn backdrop(id: impl Into<ElementId>) -> Stateful<Div> {
    div()
        .id(id)
        // Swallow all mouse events so the browse list behind the dialog
        // does not keep hovering and clicking through the dim overlay.
        .occlude()
        .absolute()
        .inset_0()
        .bg(fixed::scrim())
        .flex()
        .items_center()
        .justify_center()
}

/// The dialog card.
pub fn panel(theme: &Theme, id: impl Into<ElementId>) -> Stateful<Div> {
    super::card(theme).id(id).w(px(440.)).p_5().gap_3()
}

pub fn title(theme: &Theme, text: impl Into<SharedString>) -> Div {
    div()
        .text_lg()
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(theme.text)
        .child(text.into())
}

pub fn message(theme: &Theme, text: impl Into<SharedString>) -> Div {
    div()
        .w_full()
        .text_sm()
        .text_color(theme.text_dim)
        .child(text.into())
}

/// Right-aligned button row.
pub fn buttons() -> Div {
    div().flex().justify_end().gap_2().pt_2()
}

/// A dialog button; `primary` gets the accent fill (the enter-key
/// choice), others stay outlined. A min width keeps short labels
/// ("OK") from shrinking into a pill.
pub fn button(
    theme: &Theme,
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    primary: bool,
) -> Stateful<Div> {
    let variant = if primary {
        Variant::Primary
    } else {
        Variant::Secondary
    };
    super::button::button(theme, id, label, variant, Size::Regular).min_w(px(84.))
}

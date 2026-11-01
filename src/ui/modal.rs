//! Modal dialog building blocks (conflict prompts, confirms, small forms).
//!
//! Rendered as the root element's last child: the dimmed backdrop
//! covers the window, paints above everything, and soaks up clicks so
//! nothing behind it is interactive while the dialog is open.
//!
//! A dialog is a [`panel`] holding an optional [`intro`] (a [`glyph`]
//! tile beside the [`title`] and [`message`]), any fields, and
//! [`buttons`] last: a footer band that reaches the card's edges.

use gpui::{
    Animation, AnimationExt as _, AnyElement, BoxShadow, Div, ElementId, Rgba, SharedString,
    Stateful, div, ease_out_quint, prelude::*, px, relative,
};

use super::button::{Size, Variant};
use super::theme::Theme;

/// The panel's padding (`p_6`). The footer band cancels it with negative
/// margins so it can span the card.
const PANEL_PADDING: f32 = 24.;

/// Full-window dimmed backdrop, centering its child (the panel).
/// Callers chain `.on_click` for click-outside-to-cancel.
pub fn backdrop(theme: &Theme, id: impl Into<ElementId>) -> Stateful<Div> {
    div()
        .id(id)
        // Swallow all mouse events so the browse list behind the dialog
        // does not keep hovering and clicking through the dim overlay.
        .occlude()
        .absolute()
        .inset_0()
        .bg(theme.scrim)
        .flex()
        .items_center()
        .justify_center()
}

/// Present a finished [`backdrop`] (with its dialog inside): the scrim
/// and dialog fade in together while the dialog settles up into place.
/// Purely decorative — the dialog is interactive from the first frame.
/// `id` keys the animation, so it replays each time the dialog opens.
pub fn present(id: &'static str, backdrop: Stateful<Div>) -> AnyElement {
    backdrop
        .with_animation(
            id,
            Animation::new(super::motion::FADE).with_easing(ease_out_quint()),
            |backdrop, t| {
                // The dialog is centred, so padding the top by 2× lifts
                // it by 1×.
                backdrop
                    .opacity(t)
                    .pt(px(2. * super::motion::DIALOG_RISE * (1. - t)))
            },
        )
        .into_any_element()
}

/// The dialog card: solid glass with a deeper lift than a popover, since
/// it sits over the whole window.
pub fn panel(theme: &Theme, id: impl Into<ElementId>) -> Stateful<Div> {
    super::solid_card(theme)
        .id(id)
        .w(px(420.))
        .p(px(PANEL_PADDING))
        .gap_3()
        .shadow(dialog_shadow(theme))
}

fn dialog_shadow(theme: &Theme) -> Vec<BoxShadow> {
    let shadow = gpui::rgb_to_hsla(theme.float_shadow);
    vec![
        BoxShadow::new(px(0.), px(24.), shadow)
            .blur_radius(px(48.))
            .spread_radius(px(-12.)),
        BoxShadow::new(px(0.), px(4.), shadow)
            .blur_radius(px(12.))
            .spread_radius(px(-4.)),
        super::sheen(theme),
    ]
}

pub fn title(theme: &Theme, text: impl Into<SharedString>) -> Div {
    div()
        .text_base()
        .line_height(relative(1.35))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(theme.text)
        .child(text.into())
}

pub fn message(theme: &Theme, text: impl Into<SharedString>) -> Div {
    div()
        .w_full()
        .text_sm()
        .line_height(relative(1.5))
        .text_color(theme.text_dim)
        .child(text.into())
}

/// What a dialog's [`glyph`] says about the decision.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Tone {
    /// Something is lost or overwritten: danger red on its tint.
    Danger,
    /// An ordinary choice: accent on the selection tint.
    Neutral,
}

/// A 40px tile showing what kind of decision the dialog asks for. Leave
/// it out of plain forms.
pub fn glyph(theme: &Theme, icon: &'static str, tone: Tone) -> Div {
    let (fill, ink): (Rgba, Rgba) = match tone {
        Tone::Danger => (theme.danger_tint, theme.danger),
        Tone::Neutral => (theme.selected, theme.accent),
    };
    div()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(40.))
        .rounded(px(10.))
        .bg(fill)
        .child(super::icon::ui_icon(icon, ink).size(px(20.)))
}

/// The top of a dialog: a [`glyph`] tile beside the title and message.
pub fn intro(glyph: Div, title: Div, message: Div) -> Div {
    div().flex().gap_4().child(glyph).child(
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .gap(px(6.))
            .child(title)
            .child(message),
    )
}

/// The footer band: right-aligned buttons on a recessed strip that spans
/// the card below a hairline. Always the panel's last child.
pub fn buttons(theme: &Theme) -> Div {
    div()
        .flex()
        .justify_end()
        .gap_2()
        .mt_2()
        .mx(px(-PANEL_PADDING))
        .mb(px(-PANEL_PADDING))
        .px(px(PANEL_PADDING))
        .py(px(14.))
        .bg(theme.dialog_footer)
        .border_t_1()
        .border_color(theme.glass_edge)
        .rounded_b_xl()
}

/// A dialog button. Secondary first, the default (Primary, or Danger when
/// it loses data) last. A min width keeps short labels ("No", "Delete")
/// balanced.
pub fn button(
    theme: &Theme,
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    variant: Variant,
) -> Stateful<Div> {
    super::button::button(theme, id, label, variant, Size::Regular).min_w(px(96.))
}

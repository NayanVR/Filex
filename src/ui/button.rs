//! Buttons: the one place their height, radius, hover and colour rules
//! live, so every dialog, card and banner action looks the same. Text
//! buttons are a [`Variant`] × [`Size`]; icon buttons are square hit boxes
//! in an [`IconSize`]. Callers chain `.on_click` (and any layout tweaks).

use gpui::{BoxShadow, Div, ElementId, Rgba, SharedString, Stateful, div, prelude::*, px};

use super::icon;
use super::theme::{Theme, fixed, lift, sink};

/// What the button means, which decides its colour.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Variant {
    /// The main action (the Enter-key choice): accent fill.
    Primary,
    /// Everything else (Cancel, Refresh…): a filled control surface.
    Secondary,
    /// Irreversible actions (Delete, Replace, Remove): danger-red fill.
    Danger,
    /// Low-stakes actions beside others ("Learn more", "Undo"): accent
    /// text, no fill until hovered.
    Subtle,
}

/// How far a filled button lightens on hover and darkens while pressed.
const FILL_SHIFT: f32 = 0.12;

/// Text-button scale. Regular sits in dialogs and panes; Small fits slim
/// bars (update banner) and inline editors.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Size {
    Regular,
    Small,
}

/// Icon-button scale as (hit box, glyph) px. Large is the toolbar, Medium
/// tab-strip controls, Small inline ✕ dismissals.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum IconSize {
    Small,
    Medium,
    Large,
}

impl IconSize {
    pub fn metrics(self) -> (f32, f32) {
        match self {
            IconSize::Small => (20., 12.),
            IconSize::Medium => (24., 16.),
            IconSize::Large => (30., 18.),
        }
    }
}

/// The shared shell of every text button: centered label at the size's
/// height, padding, radius and type size. No colour yet.
fn shell(id: impl Into<ElementId>, label: impl Into<SharedString>, size: Size) -> Stateful<Div> {
    let base = div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .font_weight(gpui::FontWeight::MEDIUM);
    let base = match size {
        Size::Regular => base.h(px(32.)).px_3().rounded_lg().text_sm(),
        Size::Small => base.h(px(22.)).px_2().rounded_md().text_xs(),
    };
    base.child(label.into())
}

/// A text button. Filled variants lighten on hover and darken while
/// pressed, so the fill (and its meaning) never disappears.
pub fn button(
    theme: &Theme,
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    variant: Variant,
    size: Size,
) -> Stateful<Div> {
    let base = shell(id, label, size).cursor_pointer();
    match variant {
        Variant::Primary => filled(theme, base, theme.accent, theme.on_accent),
        Variant::Danger => filled(theme, base, theme.danger, theme.on_danger),
        Variant::Secondary => {
            let (hover, pressed) = (theme.control_hover, theme.field);
            base.bg(theme.control)
                .border_1()
                .border_color(theme.control_border)
                .shadow(control_shadow(theme))
                .text_color(theme.text)
                .hover(move |s| s.bg(hover))
                .active(move |s| s.bg(pressed))
        }
        Variant::Subtle => {
            let hover = theme.hover;
            base.text_color(theme.accent).hover(move |s| s.bg(hover))
        }
    }
}

/// A flat fill with a faint lit top edge, a darker bottom lip and a small
/// contact shadow: a pressable surface, not a flat chip and not a glossy
/// gradient.
fn filled(theme: &Theme, base: Stateful<Div>, fill: Rgba, ink: Rgba) -> Stateful<Div> {
    base.bg(fill)
        .shadow(fill_shadow(theme))
        .text_color(ink)
        .hover(move |s| s.bg(lift(fill, FILL_SHIFT)))
        .active(move |s| s.bg(sink(fill, FILL_SHIFT)))
}

fn fill_shadow(theme: &Theme) -> Vec<BoxShadow> {
    vec![
        BoxShadow::new(px(0.), px(1.), gpui::rgb_to_hsla(theme.float_shadow)).blur_radius(px(2.)),
        BoxShadow::new(px(0.), px(1.), gpui::rgb_to_hsla(fixed::gloss())).inset(),
        BoxShadow::new(px(0.), px(-1.), gpui::rgb_to_hsla(fixed::lip())).inset(),
    ]
}

fn control_shadow(theme: &Theme) -> Vec<BoxShadow> {
    vec![
        BoxShadow::new(px(0.), px(1.), gpui::rgb_to_hsla(theme.shadow)).blur_radius(px(2.)),
        super::sheen(theme),
    ]
}

/// A button that can't be pressed right now: a recessed `field` fill with
/// dim text, no hover and no pointer cursor. Full opacity, so it reads as
/// unavailable rather than faded. Callers still own whether to attach a
/// handler.
pub fn disabled_button(
    theme: &Theme,
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    size: Size,
) -> Stateful<Div> {
    shell(id, label, size)
        .bg(theme.field)
        .text_color(theme.text_dim)
}

/// A square icon-only button. The glyph is tinted explicitly (gpui's
/// `svg()` paints nothing without its own colour); hover shifts the
/// background.
pub fn icon_button(
    theme: &Theme,
    id: impl Into<ElementId>,
    icon: &'static str,
    color: Rgba,
    size: IconSize,
) -> Stateful<Div> {
    let (edge, glyph) = size.metrics();
    let hover = theme.hover;
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(edge))
        .rounded_md()
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
        .child(icon::ui_icon(icon, color).size(px(glyph)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The glyph must leave hover padding inside the hit box at every size,
    /// and the scale must actually grow.
    #[test]
    fn icon_sizes_grow_and_pad_their_glyph() {
        let sizes = [IconSize::Small, IconSize::Medium, IconSize::Large].map(IconSize::metrics);
        for (edge, glyph) in sizes {
            assert!(edge - glyph >= 6., "{edge} box too tight for {glyph} glyph");
        }
        assert!(sizes.windows(2).all(|w| w[0].0 < w[1].0));
    }
}

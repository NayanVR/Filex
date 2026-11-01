//! A minimal text tooltip. gpui ships the `.tooltip()` hook but no
//! ready-made tooltip view, so this is the smallest thing that renders
//! one: a glass card holding a single (wrapping) line of text, handed
//! to `.tooltip(..)` as an `AnyView`.

use gpui::{
    Animation, AnimationExt as _, App, AppContext as _, Context, IntoElement, Render, SharedString,
    Window, div, ease_out_quint, prelude::*, px,
};

use super::theme::Theme;

pub struct TextTooltip {
    text: SharedString,
    /// The key binding for the action, shown dimmed after the label.
    /// Empty when the action has none (or it was unassigned).
    keys: SharedString,
    theme: Theme,
}

impl TextTooltip {
    pub fn new(text: impl Into<SharedString>, theme: Theme) -> Self {
        Self {
            text: text.into(),
            keys: SharedString::default(),
            theme,
        }
    }
}

impl Render for TextTooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        super::glass(div().rounded_md(), &self.theme)
            .px_2()
            .py_1()
            .max_w(px(680.))
            .text_xs()
            .text_color(self.theme.text)
            .flex()
            .items_center()
            .gap_2()
            .child(self.text.clone())
            .when(!self.keys.is_empty(), |tip| {
                tip.child(
                    div()
                        .text_color(self.theme.text_dim)
                        .child(self.keys.clone()),
                )
            })
            // Each tooltip is a fresh view, so this fades in every time it
            // appears (after gpui's hover delay — the fade adds no wait).
            .with_animation(
                "tooltip-in",
                Animation::new(super::motion::FADE).with_easing(ease_out_quint()),
                |tip, t| tip.opacity(t),
            )
    }
}

/// Build a `.tooltip(..)` callback that shows `text` in a [`TextTooltip`].
/// Empty text yields `None` so callers can skip the hook entirely.
pub fn text_tooltip(
    text: impl Into<SharedString>,
    theme: Theme,
) -> impl Fn(&mut Window, &mut App) -> gpui::AnyView + 'static {
    let text = text.into();
    move |_window, cx| cx.new(|_| TextTooltip::new(text.clone(), theme)).into()
}

/// A tooltip naming an action and its key binding: "Toggle details
/// Ctrl+I". `keys` comes from the live shortcut table (so a remapped key
/// shows its new binding); empty `keys` renders as a plain tooltip.
pub fn shortcut_tooltip(
    text: impl Into<SharedString>,
    keys: impl Into<SharedString>,
    theme: Theme,
) -> impl Fn(&mut Window, &mut App) -> gpui::AnyView + 'static {
    let (text, keys) = (text.into(), keys.into());
    move |_window, cx| {
        cx.new(|_| TextTooltip {
            text: text.clone(),
            keys: keys.clone(),
            theme,
        })
        .into()
    }
}

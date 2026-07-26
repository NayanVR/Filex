//! A minimal text tooltip. gpui ships the `.tooltip()` hook but no
//! ready-made tooltip view, so this is the smallest thing that renders
//! one: a bordered card holding a single (wrapping) line of text, handed
//! to `.tooltip(..)` as an `AnyView`.

use gpui::{
    App, AppContext as _, Context, IntoElement, ParentElement as _, Render, SharedString,
    Styled as _, Window, div, px,
};

use super::theme::Theme;

pub struct TextTooltip {
    text: SharedString,
    theme: Theme,
}

impl TextTooltip {
    pub fn new(text: impl Into<SharedString>, theme: Theme) -> Self {
        Self { text: text.into(), theme }
    }
}

impl Render for TextTooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .rounded_md()
            .border_1()
            .border_color(self.theme.border)
            .bg(self.theme.panel)
            .shadow_lg()
            .max_w(px(680.))
            .text_xs()
            .text_color(self.theme.text)
            .child(self.text.clone())
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

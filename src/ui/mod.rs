//! Reusable UI building blocks for the filex app.
//!
//! Each submodule is a small, self-contained component (or family of
//! components) styled from the shared [`theme`] palette. The rule of
//! thumb (see docs/roadmap.md, "reusable components" principle): render
//! code that appears in more than one place, or that would push a
//! workspace render method past a screenful, belongs here — while
//! state and business logic stay in the workspace / lib.

pub mod assets;
pub mod button;
pub mod details;
pub mod fonts;
pub mod grid;
pub mod icon;
pub mod job;
pub mod kbd;
pub mod list_row;
pub mod magic_card;
pub mod menu;
pub mod modal;
pub mod motion;
pub mod onboarding;
pub mod pane;
pub mod scrollbar;
pub mod search_input;
pub mod settings_pane;
pub mod sidebar;
pub mod status_bar;
pub mod tabs;
pub mod theme;
pub mod tooltip;
pub mod top_bar;
pub mod update_banner;
pub mod window_controls;

use gpui::{BoxShadow, Div, div, prelude::*, px};

/// One soft, wide, low-opacity layer for the content canvas: enough to lift
/// it off the shell without the hard bottom edge of gpui's stacked
/// `shadow_sm`.
pub fn soft_shadow(theme: &theme::Theme) -> Vec<BoxShadow> {
    vec![BoxShadow::new(px(0.), px(1.), gpui::rgb_to_hsla(theme.shadow)).blur_radius(px(6.))]
}

/// The shared look of every surface that floats above the browse view —
/// context menu, modal dialog, settings and shortcuts cards. Callers chain
/// their own `.id()`, width, padding and gap.
pub fn card(theme: &theme::Theme) -> Div {
    div()
        .rounded_xl()
        .border_1()
        .border_color(theme.border)
        .bg(theme.elevated)
        .shadow_lg()
        .flex()
        .flex_col()
}

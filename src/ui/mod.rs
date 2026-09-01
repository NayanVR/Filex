//! Reusable UI building blocks for the filex app.
//!
//! Each submodule is a small, self-contained component (or family of
//! components) styled from the shared [`theme`] palette. The rule of
//! thumb (see docs/roadmap.md, "reusable components" principle): render
//! code that appears in more than one place, or that would push a
//! workspace render method past a screenful, belongs here — while
//! state and business logic stay in the workspace / lib.

pub mod assets;
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

use gpui::{Div, div, prelude::*};

/// The shared look of every surface that floats above the browse view —
/// context menu, modal dialog, settings and shortcuts cards. Callers chain
/// their own `.id()`, width, padding and gap.
pub fn card(theme: &theme::Theme) -> Div {
    div()
        .rounded_lg()
        .border_1()
        .border_color(theme.border)
        .bg(theme.panel)
        .shadow_lg()
        .flex()
        .flex_col()
}

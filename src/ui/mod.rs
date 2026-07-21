//! Reusable UI building blocks for the filex app.
//!
//! Each submodule is a small, self-contained component (or family of
//! components) styled from the shared [`theme`] palette. The rule of
//! thumb (see docs/roadmap.md, "reusable components" principle): render
//! code that appears in more than one place, or that would push a
//! workspace render method past a screenful, belongs here — while
//! state and business logic stay in the workspace / lib.

pub mod assets;
pub mod fonts;
pub mod icon;
pub mod job;
pub mod list_row;
pub mod menu;
pub mod modal;
pub mod pane;
pub mod search_input;
pub mod settings_pane;
pub mod sidebar;
pub mod status_bar;
pub mod theme;
pub mod top_bar;

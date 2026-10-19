//! Keyboard dispatch and context-menu opening.

use super::*;

impl Workspace {
    /// List-navigation keys. Text editing lives in the SearchInput (which
    /// is focused); unhandled keys bubble up here.
    pub(super) fn handle_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keystroke = &event.keystroke;
        if keystroke.modifiers.platform || keystroke.modifiers.control {
            return; // shortcuts are handled by actions
        }
        if self.conflict.is_some() {
            // Enter = the primary (keep both); escape arrives as the
            // input's ClearInput action, handled in render().
            if keystroke.key.as_str() == "enter" {
                self.resolve_conflict(true, cx);
            }
            return;
        }
        if self.renaming.is_some() {
            // Escape is consumed by the input itself (Dismissed event);
            // enter lands here and commits.
            if keystroke.key.as_str() == "enter" {
                self.commit_rename(window, cx);
            }
            return;
        }
        if self.feedback.is_some() {
            // Escape cancels via the inputs' Dismissed; enter sends.
            if keystroke.key.as_str() == "enter" {
                self.submit_feedback(window, cx);
            }
            return;
        }
        if self.tag_editor.is_some() {
            // Same as rename: the input handles escape (Dismissed → cancel);
            // enter commits the tag.
            if keystroke.key.as_str() == "enter" {
                self.commit_tag_editor(window, cx);
            }
        }
        // Selection/navigation now comes from the editable action registry.
    }

    /// Keep the menu on screen: pull the anchor back from the right and
    /// bottom edges by the menu's size (estimated height — items are
    /// fixed-height so the estimate is close).
    pub(super) fn clamped_menu_position(
        raw: Point<Pixels>,
        est_height: f32,
        window: &Window,
    ) -> Point<Pixels> {
        let viewport = window.viewport_size();
        let max_x = viewport.width - px(ui::menu::MENU_WIDTH + 8.);
        let max_y = viewport.height - px(est_height);
        gpui::point(raw.x.min(max_x).max(px(0.)), raw.y.min(max_y).max(px(0.)))
    }
}

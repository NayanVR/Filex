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
        if self.settings_open {
            // Keys belong to the Settings modal and its text fields.
            return;
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
        let (w, h) = (f32::from(viewport.width), f32::from(viewport.height));
        let (x, y) = menu_origin(
            (f32::from(raw.x), f32::from(raw.y)),
            (ui::menu::MENU_WIDTH, est_height),
            (w, h),
        );
        gpui::point(px(x), px(y))
    }
}

/// Where a menu of `size` opened at `at` goes in a window of `window`:
/// below and right of the pointer when it fits, otherwise flipped to open
/// up or left from it (as native menus do), never closer than a margin to
/// an edge. The height estimate gets slack for the card's padding and
/// shadow, which it doesn't count.
fn menu_origin(at: (f32, f32), size: (f32, f32), window: (f32, f32)) -> (f32, f32) {
    const MARGIN: f32 = 8.;
    let (w, h) = (size.0, size.1 + MARGIN);
    let place = |pos: f32, len: f32, room: f32| {
        let pos = if pos + len + MARGIN > room {
            pos - len
        } else {
            pos
        };
        pos.min(room - len - MARGIN).max(MARGIN)
    };
    (place(at.0, w, window.0), place(at.1, h, window.1))
}

#[cfg(test)]
mod menu_origin_tests {
    use super::menu_origin;

    /// Regression: a background menu opened near the bottom-right corner
    /// sat 6px from the edges, over the status bar.
    #[test]
    fn a_menu_near_a_corner_opens_up_and_left() {
        let (x, y) = menu_origin((1100., 740.), (248., 160.), (1120., 760.));
        assert_eq!((x, y), (1100. - 248., 740. - 168.));
        // Room to spare: it opens at the pointer.
        assert_eq!(
            menu_origin((100., 100.), (248., 160.), (1120., 760.)),
            (100., 100.)
        );
        // Taller than the window: pinned to the top margin.
        assert_eq!(menu_origin((10., 10.), (248., 900.), (1120., 760.)).1, 8.);
    }
}

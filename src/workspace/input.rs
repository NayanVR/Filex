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
        if self.tag_editor.is_some() {
            // Same as rename: the input handles escape (Dismissed → cancel);
            // enter commits the tag.
            if keystroke.key.as_str() == "enter" {
                self.commit_tag_editor(window, cx);
            }
            return;
        }
        let extend = keystroke.modifiers.shift;
        match keystroke.key.as_str() {
            "up" => self.move_selection(-1, extend, cx),
            "down" => self.move_selection(1, extend, cx),
            "enter" => self.activate_selected(cx),
            _ => {}
        }
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

    pub(super) fn open_entry_menu(
        &mut self,
        ix: usize,
        from_search: bool,
        position: Point<Pixels>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if self.renaming.is_some() || self.conflict.is_some() {
            return;
        }
        let target = if from_search {
            let Some(row) = self.results.get(ix) else {
                return;
            };
            MenuTarget::Entry {
                ix,
                path: row.target.clone(),
                name: row.name.to_string(),
                is_dir: row.is_dir,
                from_search,
            }
        } else {
            let Some(entry) = self.entries.get(ix) else {
                return;
            };
            MenuTarget::Entry {
                ix,
                path: entry.path.clone(),
                name: entry.name.clone(),
                is_dir: entry.is_dir,
                from_search,
            }
        };
        // Right-clicking a row outside the current selection selects
        // just it; right-clicking one already in a multi-selection keeps
        // the whole set, so the menu can act on all of it.
        if !self.active_selection().contains(ix) {
            self.active_selection_mut().select_one(ix);
        }
        self.context_menu = Some(ContextMenu {
            position: Self::clamped_menu_position(position, 280., window),
            target,
        });
        cx.notify();
    }

    pub(super) fn open_root_menu(
        &mut self,
        path: PathBuf,
        position: Point<Pixels>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        self.context_menu = Some(ContextMenu {
            position: Self::clamped_menu_position(position, 90., window),
            target: MenuTarget::Root { path },
        });
        cx.notify();
    }

    pub(super) fn open_favorite_menu(
        &mut self,
        path: PathBuf,
        position: Point<Pixels>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        self.context_menu = Some(ContextMenu {
            position: Self::clamped_menu_position(position, 130., window),
            target: MenuTarget::Favorite { path },
        });
        cx.notify();
    }

    pub(super) fn close_menu(&mut self, cx: &mut Context<Self>) {
        if self.context_menu.take().is_some() {
            cx.notify();
        }
    }
}

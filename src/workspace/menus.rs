//! Context-menu controller: opening menus built by `filex::context_menu`,
//! opening submenus when the pointer rests on their row, arrow-key
//! navigation, and running the chosen [`Command`]. Drawing lives in
//! `render_menus`.

use super::*;
use menu_model::{Command, Focus, Row, Submenu};

/// Keyboard moves inside an open menu (the `ContextMenu` key context).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MenuKey {
    Prev,
    Next,
    Left,
    Right,
    Confirm,
    Dismiss,
}

impl Workspace {
    /// Show a menu of `rows` at `position` and give it keyboard focus.
    /// Replacing an open menu (Folder Icon… swaps in its choices) keeps
    /// the original focus to return to.
    fn show_menu(
        &mut self,
        position: Point<Pixels>,
        target: MenuTarget,
        rows: Vec<Row>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let restore_focus = self
            .menu_focus_restore
            .take()
            .map(|(_, restore)| restore)
            .or_else(|| self.context_menu.take().and_then(|m| m.restore_focus))
            .or_else(|| window.focused(cx));
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle, cx);
        // The right-click that opened us is still bubbling; the workspace
        // root focuses itself on mouse down and would take focus straight
        // back from the menu.
        cx.stop_propagation();
        self.menu_serial += 1;
        self.context_menu = Some(ContextMenu {
            position: Self::clamped_menu_position(position, ui::menu::panel_height(&rows), window),
            target,
            rows,
            focus: None,
            hover: menu_model::HoverIntent::default(),
            submenu: None,
            apps: AppsLookup::Unused,
            wants_apps: None,
            focus_handle,
            restore_focus,
        });
        cx.notify();
    }

    pub(super) fn open_entry_menu(
        &mut self,
        ix: usize,
        from_search: bool,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The browse pane under the row opens the background menu on the
        // same right-click; the row wins.
        cx.stop_propagation();
        if self.renaming.is_some() || self.conflict.is_some() {
            return;
        }
        let (path, name, is_dir) = if from_search {
            let Some(row) = self.results.get(ix) else {
                return;
            };
            (row.target.clone(), row.name.to_string(), row.is_dir)
        } else {
            let Some(entry) = self.entries.get(ix) else {
                return;
            };
            (entry.path.clone(), entry.name.clone(), entry.is_dir)
        };
        // Right-clicking a row outside the current selection selects
        // just it; right-clicking one already in a multi-selection keeps
        // the whole set, so the menu can act on all of it.
        if !self.active_selection().contains(ix) {
            self.active_selection_mut().select_one(ix);
            self.refresh_preview(cx);
        }
        let count = self.active_selection().len();
        let facts = menu_model::EntryFacts {
            // The bare name (no quotes — unlike a dialog sentence, the
            // menu heading needs no punctuation to set the name apart).
            heading: match count {
                1 => name,
                n => format!("{n} items"),
            },
            count,
            is_dir,
            from_search,
            favorite: is_dir && self.is_favorite(&path, cx),
        };
        let rows = menu_model::entry_menu(&facts, menu_model::Capabilities::current());
        let target = MenuTarget::Entry {
            ix,
            path: path.clone(),
            is_dir,
            from_search,
        };
        self.show_menu(position, target, rows, window, cx);
        if count <= 1 && !is_dir {
            self.lookup_apps(path, window, cx);
        }
    }

    /// Right-click on empty space in the folder on screen.
    pub(super) fn open_background_menu(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.renaming.is_some()
            || self.conflict.is_some()
            || self.at_this_pc()
            || self.load_error.is_some()
        {
            return;
        }
        // Like Explorer: the menu is about the folder, not the selection.
        self.selection.clear();
        self.refresh_preview(cx);
        let facts = menu_model::BackgroundFacts {
            can_paste: self.clipboard.is_some(),
        };
        let rows = menu_model::background_menu(facts, menu_model::Capabilities::current());
        self.show_menu(position, MenuTarget::Background, rows, window, cx);
    }

    pub(super) fn open_favorite_menu(
        &mut self,
        path: PathBuf,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let rows = menu_model::favorite_menu(filex::listing::location_label(&path));
        self.show_menu(position, MenuTarget::Favorite { path }, rows, window, cx);
    }

    /// The right-click menu of a network drive (sidebar or This PC).
    pub(super) fn open_network_drive_menu(
        &mut self,
        drive: &Drive,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let rows = menu_model::network_drive_menu(drive.name.clone());
        let target = MenuTarget::NetworkDrive {
            path: drive.path.clone(),
        };
        self.show_menu(position, target, rows, window, cx);
    }

    fn open_folder_icon_menu(
        &mut self,
        path: PathBuf,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = self
            .settings
            .read(cx)
            .settings()
            .folder_icons
            .get(&path)
            .copied();
        let rows = menu_model::folder_icon_menu(current);
        self.show_menu(position, MenuTarget::FolderIcon { path }, rows, window, cx);
    }

    pub(super) fn close_menu(&mut self, cx: &mut Context<Self>) {
        if let Some(menu) = self.context_menu.take() {
            let restore = menu
                .restore_focus
                .unwrap_or_else(|| self.focus_handle.clone());
            self.menu_focus_restore = Some((menu.focus_handle, restore));
            cx.notify();
        }
    }

    /// Render-time follow-ups that need a `Window`: return focus from a
    /// closed menu, and start renaming a just-created folder once listed.
    pub(super) fn apply_menu_aftermath(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((menu, restore)) = self.menu_focus_restore.take()
            && menu.is_focused(window)
        {
            window.focus(&restore, cx);
        }
        if let Some(name) = &self.pending_rename
            && self.entries.iter().any(|entry| &entry.name == name)
        {
            self.pending_rename = None;
            self.start_rename(window, cx);
        }
    }

    /// Look up the Open With apps for `path` off the UI thread, so the
    /// submenu is ready by the time the pointer rests on its row.
    fn lookup_apps(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let Some(menu) = self.context_menu.as_mut() else {
            return;
        };
        menu.apps = AppsLookup::Pending;
        let serial = self.menu_serial;
        cx.spawn_in(window, async move |this, cx| {
            let apps = cx
                .background_executor()
                .spawn(async move { open_with_choices(&path) })
                .await;
            this.update_in(cx, |this, window, cx| {
                if this.menu_serial != serial {
                    return;
                }
                let Some(menu) = this.context_menu.as_mut() else {
                    return;
                };
                menu.apps = AppsLookup::Ready(apps);
                if let Some((row, keyboard)) = menu.wants_apps.take() {
                    this.open_submenu(row, keyboard, window, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Show the submenu of main-panel row `row` beside the menu. From the
    /// keyboard (`keyboard`), its first entry is highlighted too.
    fn open_submenu(
        &mut self,
        row: usize,
        keyboard: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let settings = self.settings.read(cx).settings();
        let (view, sort) = (settings.view, settings.sort);
        let Some(menu) = self.context_menu.as_mut() else {
            return;
        };
        let Some(Row::Item(item)) = menu.rows.get(row) else {
            return;
        };
        let Some(kind) = item.submenu else {
            return;
        };
        if let Some(open) = menu.submenu.as_mut().filter(|s| s.parent_row == row) {
            if keyboard {
                open.focus = menu_model::step(&open.rows, None, 1);
            }
            cx.notify();
            return;
        }
        let rows = match (kind, &menu.apps) {
            (Submenu::OpenWith, AppsLookup::Ready(apps)) => {
                let names: Option<Vec<String>> = apps
                    .as_ref()
                    .map(|apps| apps.iter().map(|a| a.name.clone()).collect());
                // No list at all (Linux couldn't type the file): still
                // offer the OS chooser.
                let other = OPEN_WITH_OTHER.or(names.is_none().then_some("Choose Application…"));
                menu_model::open_with_rows(names.as_deref(), other)
            }
            (Submenu::OpenWith, AppsLookup::Pending) => {
                menu.wants_apps = Some((row, keyboard));
                return;
            }
            (Submenu::OpenWith, AppsLookup::Unused) => return,
            (other, _) => menu_model::static_submenu(other, view, sort),
        };
        let viewport = window.viewport_size();
        let x = submenu_x(menu.position.x, viewport.width);
        let anchor =
            menu.position.y + px(ui::menu::row_top(&menu.rows, row) - ui::menu::row_top(&rows, 0));
        let (y, max_height) =
            submenu_frame(anchor, px(ui::menu::panel_height(&rows)), viewport.height);
        let focus = if keyboard {
            menu_model::step(&rows, None, 1)
        } else {
            None
        };
        menu.wants_apps = None;
        menu.submenu = Some(OpenSubmenu {
            parent_row: row,
            rows,
            position: gpui::point(x, y),
            max_height,
            focus,
        });
        cx.notify();
    }

    fn close_submenu(&mut self, cx: &mut Context<Self>) {
        if let Some(menu) = self.context_menu.as_mut() {
            menu.wants_apps = None;
            if menu.submenu.take().is_some() {
                cx.notify();
            }
        }
    }

    /// The pointer entered a row (`on_hover(true)`): highlight it, and on
    /// the main panel arm the hover-intent timer that opens that row's
    /// submenu — or closes the open one — once the pointer rests.
    pub(super) fn menu_hover(
        &mut self,
        panel: MenuPanel,
        focus: Focus,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let serial = self.menu_serial;
        let Some(menu) = self.context_menu.as_mut() else {
            return;
        };
        let ticket = match panel {
            MenuPanel::Sub => {
                menu.hover.settle();
                if let Some(sub) = menu.submenu.as_mut() {
                    sub.focus = Some(focus);
                }
                None
            }
            MenuPanel::Main => {
                menu.focus = Some(focus);
                if let Some(sub) = menu.submenu.as_mut() {
                    sub.focus = None;
                }
                let target = menu_model::item_at(&menu.rows, focus)
                    .filter(|item| item.submenu.is_some())
                    .map(|_| focus.row);
                let showing = menu.submenu.as_ref().map(|sub| sub.parent_row);
                if target.is_some() && target == showing {
                    menu.hover.settle();
                    None
                } else {
                    menu.hover.hover(target)
                }
            }
        };
        cx.notify();
        let Some(ticket) = ticket else {
            return;
        };
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor()
                .timer(menu_model::SUBMENU_HOVER_DELAY)
                .await;
            this.update_in(cx, |this, window, cx| {
                if this.menu_serial != serial {
                    return;
                }
                let Some(menu) = this.context_menu.as_mut() else {
                    return;
                };
                match menu.hover.fire(ticket) {
                    Some(Some(row)) => this.open_submenu(row, false, window, cx),
                    Some(None) => this.close_submenu(cx),
                    None => {}
                }
            })
            .ok();
        })
        .detach();
    }

    /// A click on a row. Submenu rows open at once (no hover wait).
    pub(super) fn menu_click(
        &mut self,
        panel: MenuPanel,
        focus: Focus,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The overlay behind the panel closes the menu on click.
        cx.stop_propagation();
        self.menu_activate(panel, focus, false, window, cx);
    }

    fn menu_activate(
        &mut self,
        panel: MenuPanel,
        focus: Focus,
        keyboard: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(menu) = self.context_menu.as_mut() else {
            return;
        };
        menu.hover.settle();
        let rows = match (panel, &menu.submenu) {
            (MenuPanel::Main, _) => &menu.rows,
            (MenuPanel::Sub, Some(sub)) => &sub.rows,
            (MenuPanel::Sub, None) => return,
        };
        let Some(item) = menu_model::item_at(rows, focus) else {
            return;
        };
        if item.submenu.is_some() {
            menu.focus = Some(focus);
            self.open_submenu(focus.row, keyboard, window, cx);
            return;
        }
        let command = item.command;
        self.run_menu_command(command, window, cx);
    }

    /// Arrow keys, Enter and Escape while a menu is open. Up/Down move
    /// through the panel the keyboard is in, wrapping; Right opens the
    /// highlighted row's submenu (or moves along the icon bar); Left
    /// closes the submenu; Escape closes one level.
    pub(super) fn menu_key(&mut self, key: MenuKey, window: &mut Window, cx: &mut Context<Self>) {
        let Some(menu) = self.context_menu.as_mut() else {
            return;
        };
        menu.hover.settle();
        let in_sub = menu.submenu.as_ref().is_some_and(|sub| sub.focus.is_some());
        match key {
            MenuKey::Prev | MenuKey::Next => {
                let delta = if key == MenuKey::Prev { -1 } else { 1 };
                match menu.submenu.as_mut().filter(|_| in_sub) {
                    Some(sub) => sub.focus = menu_model::step(&sub.rows, sub.focus, delta),
                    None => {
                        menu.focus = menu_model::step(&menu.rows, menu.focus, delta);
                        menu.submenu = None;
                        menu.wants_apps = None;
                    }
                }
            }
            MenuKey::Right if !in_sub => {
                let Some(focus) = menu.focus else {
                    menu.focus = menu_model::step(&menu.rows, None, 1);
                    cx.notify();
                    return;
                };
                if let Some(next) = menu_model::step_in_row(&menu.rows, focus, 1) {
                    menu.focus = Some(next);
                } else if menu_model::item_at(&menu.rows, focus)
                    .is_some_and(|i| i.submenu.is_some())
                {
                    self.open_submenu(focus.row, true, window, cx);
                    return;
                }
            }
            MenuKey::Right => {}
            MenuKey::Left if in_sub => {
                menu.submenu = None;
            }
            MenuKey::Left => {
                if let Some(prev) = menu
                    .focus
                    .and_then(|focus| menu_model::step_in_row(&menu.rows, focus, -1))
                {
                    menu.focus = Some(prev);
                }
            }
            MenuKey::Confirm => {
                let (panel, focus) = if in_sub {
                    (MenuPanel::Sub, menu.submenu.as_ref().and_then(|s| s.focus))
                } else {
                    (MenuPanel::Main, menu.focus)
                };
                if let Some(focus) = focus {
                    self.menu_activate(panel, focus, true, window, cx);
                }
                return;
            }
            MenuKey::Dismiss => {
                if menu.submenu.is_some() || menu.wants_apps.is_some() {
                    menu.submenu = None;
                    menu.wants_apps = None;
                } else {
                    self.close_menu(cx);
                    return;
                }
            }
        }
        cx.notify();
    }

    /// Run a menu command against the open menu's target, closing the
    /// menu (or, for Folder Icon…, replacing it with the icon choices).
    fn run_menu_command(&mut self, command: Command, window: &mut Window, cx: &mut Context<Self>) {
        let Some(menu) = self.context_menu.as_ref() else {
            return;
        };
        let position = menu.position;
        let (path, is_dir, from_search, ix) = match &menu.target {
            MenuTarget::Entry {
                ix,
                path,
                is_dir,
                from_search,
            } => (Some(path.clone()), *is_dir, *from_search, Some(*ix)),
            MenuTarget::Favorite { path }
            | MenuTarget::FolderIcon { path }
            | MenuTarget::NetworkDrive { path } => (Some(path.clone()), true, false, None),
            MenuTarget::Background => (None, true, false, None),
        };
        let app = match (command, &menu.apps) {
            (Command::OpenInApp(n), AppsLookup::Ready(Some(apps))) => apps.get(n).cloned(),
            _ => None,
        };
        if let (Command::FolderIcon, Some(path)) = (command, &path) {
            self.open_folder_icon_menu(path.clone(), position, window, cx);
            return;
        }
        self.close_menu(cx);
        // The folder a folder-level command acts on: the right-clicked
        // folder, or the one on screen for the background menu.
        let folder = path.clone().unwrap_or_else(|| self.cwd.clone());
        match command {
            Command::Cut => self.clip_selected(ClipMode::Cut, cx),
            Command::Copy => self.clip_selected(ClipMode::Copy, cx),
            Command::Rename => {
                if let Some(ix) = ix {
                    self.active_selection_mut().select_one(ix);
                    self.start_rename(window, cx);
                }
            }
            Command::Share => self.share_selected(window, cx),
            Command::Delete => self.trash_selected(cx),
            Command::Open => {
                if let Some(path) = path {
                    self.open_target(path, is_dir, from_search, cx);
                }
            }
            Command::Reveal => {
                if let Some(path) = path {
                    self.reveal(path, cx);
                }
            }
            Command::AddToFavorites => {
                if let Some(path) = path {
                    self.pin_favorite(path, cx);
                }
            }
            Command::RemoveFromFavorites => {
                if let Some(path) = path {
                    self.unpin_favorite(&path, cx);
                }
            }
            Command::CompressToZip => self.compress_selected(cx),
            Command::CopyAsPath => self.copy_selected_paths(cx),
            Command::IndexFolder => {
                if let Some(path) = path {
                    self.add_root(path, cx);
                }
            }
            Command::Properties => {
                // A row's menu acts on the whole selection; the background
                // and sidebar menus on their one folder.
                let paths = if ix.is_some() {
                    self.selected_paths().into_iter().map(|(p, _)| p).collect()
                } else {
                    vec![folder]
                };
                self.show_properties(paths, window, cx);
            }
            Command::Paste => self.paste_clipboard(cx),
            Command::OpenInTerminal => {
                let dir = folder;
                if let Err(err) = open_terminal(&dir) {
                    self.notice = Some(format!("couldn't open a terminal: {err}").into());
                    cx.notify();
                }
            }
            Command::NewFolder => self.new_folder(cx),
            Command::SetView(view) => self.update_settings(cx, |s| s.view = view),
            Command::SetSort(by) => self.update_settings(cx, |s| s.sort.by = by),
            Command::SetAscending(ascending) => {
                self.update_settings(cx, |s| s.sort.ascending = ascending)
            }
            Command::OpenInApp(_) => {
                if let (Some(path), Some(app)) = (path, app)
                    && self.validate_search_paths(std::slice::from_ref(&path), cx)
                {
                    self.open_in_app(path, app.id, cx);
                }
            }
            Command::ChooseOtherApp => {
                if let Some(path) = path {
                    self.choose_other_app(path, cx);
                }
            }
            Command::MoveFavorite(delta) => {
                if let Some(path) = path {
                    self.move_favorite(&path, delta, cx);
                }
            }
            Command::SetFolderIcon(choice) => {
                if let Some(path) = path {
                    self.update_settings(cx, |s| match choice {
                        Some(icon) => {
                            s.folder_icons.insert(path, icon);
                        }
                        None => {
                            s.folder_icons.remove(&path);
                        }
                    });
                }
            }
            Command::Disconnect => {
                if let Some(path) = path {
                    self.disconnect_drive(path, cx);
                }
            }
            Command::FolderIcon | Command::OpenSubmenu => {}
        }
    }

    fn update_settings(
        &mut self,
        cx: &mut Context<Self>,
        edit: impl FnOnce(&mut filex::settings::Settings),
    ) {
        self.settings.update(cx, |store, cx| store.update(cx, edit));
    }

    /// Ctrl+Shift+C (and the menu's Copy as Path).
    pub(super) fn copy_as_path_action(&mut self, cx: &mut Context<Self>) {
        if self.renaming.is_some() || self.settings_open {
            return;
        }
        self.copy_selected_paths(cx);
    }

    /// Alt+Enter (and the menu's Properties): the selection's properties,
    /// or the open folder's when nothing is selected.
    pub(super) fn properties_action(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.renaming.is_some() || self.settings_open || self.at_this_pc() {
            return;
        }
        let mut paths: Vec<PathBuf> = self.selected_paths().into_iter().map(|(p, _)| p).collect();
        if paths.is_empty() {
            paths.push(self.cwd.clone());
        }
        self.show_properties(paths, window, cx);
    }

    /// Windows: the Shell's Properties dialog. macOS/Linux have no
    /// desktop-neutral equivalent, so Filex's own details panel opens.
    fn show_properties(&mut self, paths: Vec<PathBuf>, window: &Window, cx: &mut Context<Self>) {
        if !self.validate_search_paths(&paths, cx) {
            return;
        }
        #[cfg(target_os = "windows")]
        {
            shell_windows::show_properties(platform::native_window(window), paths);
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = (paths, window);
            if !self.settings.read(cx).settings().preview_open {
                self.toggle_preview(cx);
            }
        }
    }

    /// Share the selected files through the OS share sheet (Windows).
    fn share_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths: Vec<PathBuf> = self
            .selected_paths()
            .into_iter()
            .map(|(p, _)| p)
            .filter(|p| !p.is_dir())
            .collect();
        if paths.is_empty() || !self.validate_search_paths(&paths, cx) {
            return;
        }
        #[cfg(target_os = "windows")]
        {
            let shared = platform::native_window(window)
                .ok_or_else(|| anyhow::anyhow!("no native window"))
                .and_then(|hwnd| shell_windows::share(hwnd, paths));
            if let Err(err) = shared {
                self.notice = Some(format!("couldn't share: {err:#}").into());
                cx.notify();
            }
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = (window, paths);
        }
    }

    /// New › Folder: make "New folder" (or the next free "New folder 2")
    /// in the folder on screen, then rename it in place, as Explorer does.
    fn new_folder(&mut self, cx: &mut Context<Self>) {
        if self.at_this_pc() || !self.query.is_empty() {
            return;
        }
        let dir = self.cwd.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let mut path = dir.join(NEW_FOLDER_NAME);
                    if std::fs::symlink_metadata(&path).is_ok() {
                        path = ops::next_free_name(&path)?;
                    }
                    ops::apply(&FileOp::CreateFolder { path })
                })
                .await;
            this.update(cx, |this, cx| match result {
                Ok(applied) => {
                    if let ops::AppliedOp::Created { path } = &applied {
                        let name = path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        this.pending_rename = Some(name.clone());
                        let cwd = this.cwd.clone();
                        this.load_dir_selecting(&cwd, Some(name), cx);
                    }
                    this.journal.record(vec![applied]);
                }
                Err(err) => {
                    this.notice = Some(format!("couldn't create a folder: {err:#}").into());
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Compress to ZIP File: pack the selection into `<first item>.zip`
    /// beside it, as a cancellable job in the jobs bar; undo trashes the
    /// archive.
    fn compress_selected(&mut self, cx: &mut Context<Self>) {
        let sources: Vec<PathBuf> = self.selected_paths().into_iter().map(|(p, _)| p).collect();
        if sources.is_empty() || !self.validate_search_paths(&sources, cx) {
            return;
        }
        let progress = std::sync::Arc::new(ops::OpProgress::default());
        let job_id = self.next_job_id;
        self.next_job_id += 1;
        self.jobs.push(Job {
            id: job_id,
            label: format!(
                "compressing {} {}",
                sources.len(),
                plural_items(sources.len())
            )
            .into(),
            progress: progress.clone(),
            // The archive is written beside the sources.
            paths: sources.clone(),
        });
        self.spawn_job_ticker(job_id, cx);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let dest = filex::archive::archive_path(&sources)?;
                    filex::archive::compress(&sources, &dest, &progress)?;
                    anyhow::Ok(dest)
                })
                .await;
            this.update(cx, |this, cx| {
                this.jobs.retain(|job| job.id != job_id);
                match result {
                    Ok(dest) => {
                        let applied = ops::AppliedOp::Created { path: dest };
                        this.notice = Some(applied.describe().into());
                        this.journal.record(vec![applied]);
                    }
                    Err(err) if err.is::<ops::OpCanceled>() => {
                        this.notice = Some("canceled".into());
                    }
                    Err(err) => this.notice = Some(format!("couldn't compress: {err:#}").into()),
                }
                this.refresh_after_op(cx);
            })
            .ok();
        })
        .detach();
    }
}

/// Explorer's name for a new folder ("New folder", then "New folder 2"…
/// via `ops::next_free_name`).
const NEW_FOLDER_NAME: &str = "New folder";

/// A submenu opens to the right of the menu, or to its left when that
/// would run past the window's right edge.
fn submenu_x(menu_x: Pixels, viewport_width: Pixels) -> Pixels {
    let width = px(ui::menu::MENU_WIDTH);
    let right = menu_x + width - px(4.);
    if right + width > viewport_width {
        (menu_x - width + px(4.)).max(px(0.))
    } else {
        right
    }
}

/// Vertical placement of a submenu: its first row level with the parent
/// row (`anchor_y` is where the panel's top must be for that), shifted up
/// to stay on screen, and capped to the window height — the list scrolls
/// past that.
fn submenu_frame(anchor_y: Pixels, height: Pixels, viewport_height: Pixels) -> (Pixels, Pixels) {
    const MARGIN: f32 = 8.;
    let height = height.min(viewport_height - px(2. * MARGIN)).max(px(0.));
    let y = anchor_y
        .min(viewport_height - height - px(MARGIN))
        .max(px(MARGIN));
    (y, height)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_submenu_lines_up_with_its_row() {
        assert_eq!(
            submenu_frame(px(200.), px(100.), px(700.)),
            (px(200.), px(100.))
        );
    }

    #[test]
    fn long_submenu_low_on_screen_moves_up_and_stays_inside() {
        let (y, h) = submenu_frame(px(450.), px(500.), px(680.));
        assert!(y + h <= px(672.), "{y:?} + {h:?} runs off the bottom");
    }

    #[test]
    fn submenu_taller_than_window_is_capped_to_scroll() {
        assert_eq!(
            submenu_frame(px(450.), px(1400.), px(680.)),
            (px(8.), px(664.))
        );
    }

    #[test]
    fn submenu_flips_left_at_the_right_edge() {
        let width = ui::menu::MENU_WIDTH;
        assert_eq!(submenu_x(px(100.), px(1200.)), px(100. + width - 4.));
        let x = submenu_x(px(1000.), px(1200.));
        assert_eq!(x, px(1000. - width + 4.));
    }
}

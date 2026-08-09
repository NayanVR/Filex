//! Moving through the tree: loading a directory, opening/revealing
//! items, history (up/back/forward), and list selection.

use super::*;

impl Workspace {
    pub(super) fn load_dir(&mut self, path: &Path, cx: &mut Context<Self>) {
        let settings = self.settings.read(cx).settings();
        let (sort, show_hidden) = (settings.sort, settings.show_hidden_files);
        match read_dir_sorted(path, &sort) {
            Ok(mut entries) => {
                if !show_hidden {
                    entries.retain(|entry| !entry.is_hidden);
                }
                let changed = self.cwd != path;
                self.cwd = path.to_path_buf();
                self.entries = entries;
                self.load_error = None;
                self.selection.clear();
                // Any in-flight rename or armed delete points at rows
                // that no longer exist; drop them.
                self.renaming = None;
                self.pending_delete = None;
                // Only jump to the top when this is a real navigation. A
                // same-directory refresh (after a delete, paste, or rename)
                // must keep the user's scroll position — yanking back to
                // row 0 after every file op was jarring.
                if changed {
                    self.browse_scroll.scroll_to_item(0, ScrollStrategy::Top);
                }
                // A "Current Dir" search follows the folder you're in, so
                // navigating with a scoped query live must re-scope it to
                // the new directory. "Anywhere" is cwd-independent, so it
                // is left untouched.
                if changed && self.search_scope == SearchScope::CurrentDir && !self.query.is_empty()
                {
                    self.update_search(cx);
                }
            }
            Err(err) => {
                self.load_error = Some(format!("{err:#}").into());
            }
        }
    }

    /// Schedule a thumbnail decode for a visible image row (no-op if
    /// cached or in flight). Called from the list processor, so only
    /// rows that actually render ever spawn work.
    pub(super) fn request_thumbnail(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.thumbnails.contains_key(&path) {
            return;
        }
        if self.thumbnails.len() >= thumbnails::CACHE_CAP {
            self.thumbnails.clear(); // visible rows repopulate immediately
        }
        self.thumbnails
            .insert(path.clone(), ThumbnailState::Loading);
        cx.spawn(async move |this, cx| {
            let decoded = cx
                .background_executor()
                .spawn({
                    let path = path.clone();
                    async move { thumbnails::decode_thumbnail(&path) }
                })
                .await;
            this.update(cx, |this, cx| {
                let state = match decoded {
                    Ok(imagery) => ThumbnailState::Ready(imagery),
                    Err(_) => ThumbnailState::Failed,
                };
                this.thumbnails.insert(path, state);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Length of whichever list selection currently applies to.
    pub(super) fn active_list_len(&self) -> usize {
        if self.query.is_empty() {
            self.entries.len()
        } else {
            self.results.len()
        }
    }

    /// The selection for the active list: the global search selection
    /// while a query is live, otherwise the (per-tab) browse selection.
    pub(super) fn active_selection(&self) -> &Selection {
        if self.query.is_empty() {
            &self.selection
        } else {
            &self.search_selection
        }
    }

    pub(super) fn active_selection_mut(&mut self) -> &mut Selection {
        if self.query.is_empty() {
            &mut self.selection
        } else {
            &mut self.search_selection
        }
    }

    /// Arrow-key navigation. `extend` (shift held) grows the range from
    /// the anchor instead of moving a single selection.
    pub(super) fn move_selection(&mut self, delta: isize, extend: bool, cx: &mut Context<Self>) {
        let len = self.active_list_len();
        let next = if extend {
            self.active_selection_mut().extend_lead(delta, len)
        } else {
            self.active_selection_mut().move_lead(delta, len)
        };
        if let Some(next) = next {
            let handle = if self.query.is_empty() {
                &self.browse_scroll
            } else {
                &self.results_scroll
            };
            handle.scroll_to_item(next, ScrollStrategy::Center);
        }
        self.refresh_preview(cx);
        cx.notify();
    }

    /// Enter / double-click: directories navigate, files open with the
    /// platform's default application.
    pub(super) fn activate(&mut self, ix: usize, cx: &mut Context<Self>) {
        let (path, is_dir, from_search) = if self.query.is_empty() {
            let Some(entry) = self.entries.get(ix) else {
                return;
            };
            (entry.path.clone(), entry.is_dir, false)
        } else {
            let Some(row) = self.results.get(ix) else {
                return;
            };
            (row.target.clone(), row.is_dir, true)
        };
        self.open_target(path, is_dir, from_search, cx);
    }

    pub(super) fn open_target(
        &mut self,
        path: PathBuf,
        is_dir: bool,
        from_search: bool,
        cx: &mut Context<Self>,
    ) {
        if is_dir {
            self.navigate(path, cx);
            if from_search {
                self.clear_search(cx);
            }
        } else {
            if let Err(err) = open_with_default_app(&path) {
                self.notice = Some(format!("couldn't open {}: {err}", path.display()).into());
            }
            self.record_recent(path, cx);
            if from_search {
                self.clear_search(cx);
            }
            cx.notify();
        }
    }

    /// Show the OS "Open with…" chooser for a path (context-menu action),
    /// letting the user pick a program other than the default.
    pub(super) fn open_with(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if let Err(err) = open_with_dialog(&path) {
            self.notice =
                Some(format!("couldn't open {} with another app: {err}", path.display()).into());
            cx.notify();
        }
    }

    /// Jump to a file's parent directory and select it there (context
    /// menu "Reveal in Folder" on search results).
    pub(super) fn reveal(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let Some(parent) = path.parent().map(Path::to_path_buf) else {
            return;
        };
        self.clear_search(cx);
        self.navigate(parent, cx);
        if let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned())
            && let Some(ix) = self.entries.iter().position(|entry| entry.name == name)
        {
            self.selection.select_one(ix);
            self.browse_scroll
                .scroll_to_item(ix, ScrollStrategy::Center);
        }
        self.refresh_preview(cx);
        cx.notify();
    }

    pub(super) fn activate_selected(&mut self, cx: &mut Context<Self>) {
        if let Some(ix) = self.active_selection().lead() {
            self.activate(ix, cx);
        }
    }

    /// A left click on row `ix`, dispatched by its keyboard modifiers:
    /// cmd/ctrl toggles, shift ranges from the anchor, plain click
    /// selects only it.
    pub(super) fn select_click(
        &mut self,
        ix: usize,
        modifiers: gpui::Modifiers,
        cx: &mut Context<Self>,
    ) {
        if modifiers.secondary() {
            self.active_selection_mut().toggle(ix);
        } else if modifiers.shift {
            self.active_selection_mut().range_to(ix);
        } else {
            self.active_selection_mut().select_one(ix);
        }
        self.refresh_preview(cx);
        cx.notify();
    }

    pub(super) fn select_all(&mut self, cx: &mut Context<Self>) {
        if self.renaming.is_some() || self.settings_open {
            return;
        }
        let len = self.active_list_len();
        self.active_selection_mut().select_all(len);
        self.refresh_preview(cx);
        cx.notify();
    }

    pub(super) fn navigate(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if path == self.cwd {
            return;
        }
        // A new destination severs the forward history (the tab owns
        // both stacks).
        self.history_back.push(self.cwd.clone());
        self.history_forward.clear();
        self.load_dir(&path, cx);
        self.record_recent(path, cx);
        cx.notify();
    }

    pub(super) fn go_up(&mut self, cx: &mut Context<Self>) {
        if let Some(parent) = self.cwd.parent().map(Path::to_path_buf) {
            self.navigate(parent, cx);
        }
    }

    /// Back/forward through the active tab's history.
    pub(super) fn go_back(&mut self, cx: &mut Context<Self>) {
        if let Some(prev) = self.history_back.pop() {
            self.history_forward.push(self.cwd.clone());
            self.load_dir(&prev, cx);
            cx.notify();
        }
    }

    pub(super) fn go_forward(&mut self, cx: &mut Context<Self>) {
        if let Some(next) = self.history_forward.pop() {
            self.history_back.push(self.cwd.clone());
            self.load_dir(&next, cx);
            cx.notify();
        }
    }

    /// The (path, name) at a list index in whichever list is active
    /// (browse entries, or search results while a query is live).
    pub(super) fn path_at(&self, ix: usize) -> Option<(PathBuf, String)> {
        if self.query.is_empty() {
            let entry = self.entries.get(ix)?;
            Some((entry.path.clone(), entry.name.clone()))
        } else {
            let row = self.results.get(ix)?;
            Some((row.target.clone(), row.name.to_string()))
        }
    }

    /// Every selected item's (path, name), in list order.
    pub(super) fn selected_paths(&self) -> Vec<(PathBuf, String)> {
        self.active_selection()
            .iter()
            .filter_map(|ix| self.path_at(ix))
            .collect()
    }

    /// The lead item (path, name, is_dir) — the details panel's subject
    /// and the natural single target.
    pub(super) fn lead_item(&self) -> Option<(PathBuf, String, bool)> {
        let ix = self.active_selection().lead()?;
        if self.query.is_empty() {
            let entry = self.entries.get(ix)?;
            Some((entry.path.clone(), entry.name.clone(), entry.is_dir))
        } else {
            let row = self.results.get(ix)?;
            Some((row.target.clone(), row.name.to_string(), row.is_dir))
        }
    }

    /// Toggle the details panel (top-bar button / cmd-i). Opening it
    /// kicks off the metadata fetch for the current item.
    pub(super) fn toggle_preview(&mut self, cx: &mut Context<Self>) {
        self.settings.update(cx, |store, cx| {
            store.update(cx, |settings| {
                settings.preview_open = !settings.preview_open
            });
        });
        self.refresh_preview(cx);
    }

    /// Ensure `preview_meta` describes the current lead item, fetching it
    /// off-thread when the selection moved (or clearing it when the panel
    /// is closed / nothing is selected). Called after selection changes.
    pub(super) fn refresh_preview(&mut self, cx: &mut Context<Self>) {
        if !self.settings.read(cx).settings().preview_open {
            self.preview_meta = None;
            self.clear_tag_state();
            return;
        }
        let Some((path, _, _)) = self.lead_item() else {
            self.preview_meta = None;
            self.clear_tag_state();
            return;
        };
        if self.preview_meta.as_ref().map(|m| m.path.as_path()) == Some(path.as_path()) {
            return; // already have this item's metadata (and tags)
        }
        // A new item is selected: drop stale tag state and reload its tags.
        self.clear_tag_state();
        self.spawn_refresh_tags(path.clone(), cx);
        self.preview_meta = None; // drop stale while the fetch is in flight
        cx.spawn(async move |this, cx| {
            let meta = cx
                .background_executor()
                .spawn({
                    let path = path.clone();
                    async move { fetch_preview_meta(&path) }
                })
                .await;
            this.update(cx, |this, cx| {
                // Apply only if the lead still points at the same item.
                if this.lead_item().map(|(p, _, _)| p).as_deref() == Some(path.as_path()) {
                    this.preview_meta = Some(meta);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }
}

//! Sidebar state: favorites, collapsible sections, and recents.

use super::*;

impl Workspace {
    pub(super) fn is_favorite(&self, path: &Path, cx: &App) -> bool {
        self.settings
            .read(cx)
            .settings()
            .favorites
            .iter()
            .any(|p| p == path)
    }

    /// Pin a folder to the sidebar's Favorites (idempotent).
    pub(super) fn pin_favorite(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.settings.update(cx, |store, cx| {
            store.update(cx, |settings| {
                if !settings.favorites.iter().any(|p| p == &path) {
                    settings.favorites.push(path.clone());
                }
            });
        });
    }

    pub(super) fn unpin_favorite(&mut self, path: &Path, cx: &mut Context<Self>) {
        let path = path.to_path_buf();
        self.settings.update(cx, |store, cx| {
            store.update(cx, |settings| settings.favorites.retain(|p| p != &path));
        });
    }

    /// Move a favorite up (`delta < 0`) or down in the list.
    pub(super) fn move_favorite(&mut self, path: &Path, delta: isize, cx: &mut Context<Self>) {
        let path = path.to_path_buf();
        self.settings.update(cx, |store, cx| {
            store.update(cx, |settings| {
                let favorites = &mut settings.favorites;
                if let Some(i) = favorites.iter().position(|p| p == &path) {
                    let j = (i as isize + delta).clamp(0, favorites.len() as isize - 1) as usize;
                    if i != j {
                        let item = favorites.remove(i);
                        favorites.insert(j, item);
                    }
                }
            });
        });
    }

    pub(super) fn is_section_collapsed(&self, id: &str, cx: &App) -> bool {
        self.settings
            .read(cx)
            .settings()
            .collapsed_sections
            .iter()
            .any(|s| s == id)
    }

    pub(super) fn toggle_section(&mut self, id: &'static str, cx: &mut Context<Self>) {
        self.settings.update(cx, |store, cx| {
            store.update(cx, |settings| {
                let sections = &mut settings.collapsed_sections;
                if let Some(i) = sections.iter().position(|s| s == id) {
                    sections.remove(i);
                } else {
                    sections.push(id.to_string());
                }
            });
        });
    }

    /// Note `path` as recently opened and persist the log off-thread.
    pub(super) fn record_recent(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.recents.record(path);
        self.refresh_frecency();
        self.persist_recents(cx);
    }

    /// Rebuild the cached frecency table after `recents` changed.
    pub(super) fn refresh_frecency(&mut self) {
        self.frecency = std::sync::Arc::new(self.recents.score_table(filex::frecency::now_secs()));
    }

    pub(super) fn persist_recents(&self, cx: &Context<Self>) {
        let Some(file) = filex::recents::default_recents_file() else {
            return;
        };
        let recents = self.recents.clone();
        cx.background_executor()
            .spawn(async move {
                if let Err(err) = recents.save(&file) {
                    tracing::error!("failed to save recents: {err:#}");
                }
            })
            .detach();
    }

    pub(super) fn clear_recents(&mut self, cx: &mut Context<Self>) {
        self.recents.clear();
        self.refresh_frecency();
        self.persist_recents(cx);
        cx.notify();
    }

    /// Recompute the sidebar's distinct-tag list off-thread (the store
    /// scan/clone must not run on the UI thread) and cache it. Called at
    /// startup and after any change to the store.
    pub(super) fn refresh_sidebar_tags(&self, cx: &mut Context<Self>) {
        let store = self.tags.clone();
        cx.spawn(async move |this, cx| {
            let distinct = cx
                .background_executor()
                .spawn(async move {
                    let all = store.all();
                    filex::tags::distinct_tags(all.iter().flat_map(|(_, tags)| tags.iter()))
                })
                .await;
            this.update(cx, |this, cx| {
                this.sidebar_tags = distinct;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Navigate a recent folder, or open a recent file with its default
    /// app (a stat on click decides which).
    pub(super) fn open_recent(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if path.is_dir() {
            self.navigate(path, cx);
        } else {
            self.open_target(path, false, false, cx);
        }
    }
}

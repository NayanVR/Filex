//! Root folders: adding/removing roots and their per-root slots, and
//! reacting to filesystem changes under them.

use super::*;

impl Workspace {
    /// Roots to index: from settings, defaulting to the home directory
    /// on a fresh install.
    pub(super) fn configured_roots(&self, cx: &App) -> Vec<PathBuf> {
        let configured = self.settings.read(cx).settings().roots.clone();
        if configured.is_empty() {
            // Fresh install: index the platform default (all fixed drives
            // on Windows, the home directory on macOS/Linux).
            filex::drives::default_index_roots()
        } else {
            configured
        }
    }

    /// Canonicalize, create the slot, and start indexing. Invalid paths
    /// become Failed slots so the config line stays visible rather than
    /// silently vanishing.
    pub(super) fn add_root_slot(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        match path.canonicalize() {
            Ok(canonical) => {
                if self.roots.iter().any(|slot| slot.path == canonical) {
                    return;
                }
                self.roots.push(RootSlot::new(canonical.clone()));
                self.spawn_root(canonical, cx);
            }
            Err(err) => {
                let mut slot = RootSlot::new(path.clone());
                slot.state = RootState::Failed(format!("{}: {err}", path.display()).into());
                self.roots.push(slot);
            }
        }
    }

    pub(super) fn slot_mut(&mut self, path: &Path) -> Option<&mut RootSlot> {
        self.roots.iter_mut().find(|slot| slot.path == path)
    }

    /// Index one root on the background executor, then keep listening for
    /// its writer's change notifications. The UI thread never blocks.
    pub(super) fn spawn_root(&self, path: PathBuf, cx: &mut Context<Self>) {
        // Exclude OS/system folders (C:\Windows, …) unless the user opted to
        // index them. Read on the UI thread before crossing to the executor.
        let exclude_system_dirs = !self.settings.read(cx).settings().index_system_files;
        cx.spawn(async move |this, cx| {
            let (change_tx, mut change_rx) = futures::channel::mpsc::unbounded::<()>();
            let result = cx
                .background_executor()
                .spawn({
                    let path = path.clone();
                    async move {
                        #[cfg(feature = "observability")]
                        let started = std::time::Instant::now();
                        let result = start_live_index(&path, exclude_system_dirs, move || {
                            change_tx.unbounded_send(()).ok();
                        })
                        .map(|live| {
                            let files = read_index(&live.index).len();
                            (live, files)
                        });
                        // Report how long this root's initial whole-drive
                        // walk took (once per root, off the UI thread).
                        #[cfg(feature = "observability")]
                        if let Ok((_, files)) = &result {
                            filex::observability::record_index_bootstrap(
                                started.elapsed().as_millis() as u64,
                                *files,
                            );
                        }
                        result
                    }
                })
                .await;

            let updated = this.update(cx, |this, cx| {
                let Some(slot) = this.slot_mut(&path) else {
                    return; // root was removed while indexing
                };
                match result {
                    Ok((live, files)) => {
                        slot.state = RootState::Ready { live, files };
                        // A query typed while indexing can now be answered.
                        this.update_search(cx);
                    }
                    Err(err) => {
                        slot.state = RootState::Failed(format!("{err:#}").into());
                    }
                }
                cx.notify();
            });
            if updated.is_err() {
                return; // workspace is gone
            }

            // Live-update loop for this root.
            while change_rx.next().await.is_some() {
                while change_rx.try_recv().is_ok() {} // drain bursts
                let alive = this.update(cx, |this, cx| {
                    this.refresh_after_fs_change(path.clone(), cx)
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    /// React to filesystem activity under `path`, at most once per
    /// [`FS_REFRESH_INTERVAL`].
    ///
    /// Both things this does are O(arena): the file count walks every
    /// entry, and re-running the query is a full parallel scan. Neither
    /// is something a `node_modules` write should be able to trigger at
    /// event rate — unthrottled, an active home directory kept both
    /// running continuously.
    pub(super) fn refresh_after_fs_change(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.fs_refresh_pending {
            return; // already scheduled; this burst folds into it
        }
        self.fs_refresh_pending = true;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(FS_REFRESH_INTERVAL).await;
            this.update(cx, |this, cx| {
                this.fs_refresh_pending = false;
                this.refresh_root_stats(path, cx);
                if !this.query.is_empty() {
                    this.update_search(cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Recompute one root's file count off-thread (it walks the arena).
    pub(super) fn refresh_root_stats(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let Some(index) = self.slot_mut(&path).and_then(|s| s.ready_index()) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let files = cx
                .background_executor()
                .spawn(async move { read_index(&index).len() })
                .await;
            this.update(cx, |this, cx| {
                if let Some(slot) = this.slot_mut(&path)
                    && let RootState::Ready {
                        files: slot_files, ..
                    } = &mut slot.state
                {
                    *slot_files = files;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Add the directory currently being browsed as a new indexed root.
    pub(super) fn add_current_folder(&mut self, cx: &mut Context<Self>) {
        let cwd = self.cwd.clone();
        self.add_root(cwd, cx);
    }

    /// Validate and start indexing a new root.
    pub(super) fn add_root(&mut self, candidate: PathBuf, cx: &mut Context<Self>) {
        if self.service_mode() {
            self.notice =
                Some("roots are managed by the filex index service (filex-indexd)".into());
            cx.notify();
            return;
        }
        let existing: Vec<PathBuf> = self.roots.iter().map(|slot| slot.path.clone()).collect();
        match manager::validate_new_root(&existing, &candidate) {
            Ok(canonical) => {
                self.notice = None;
                self.roots.push(RootSlot::new(canonical.clone()));
                self.persist_roots(cx);
                self.spawn_root(canonical, cx);
            }
            Err(err) => {
                self.notice = Some(format!("{err:#}").into());
            }
        }
        cx.notify();
    }

    /// Roots live in settings (the store persists + notifies).
    pub(super) fn persist_roots(&self, cx: &mut Context<Self>) {
        let roots: Vec<PathBuf> = self.roots.iter().map(|slot| slot.path.clone()).collect();
        self.settings.update(cx, |store, cx| {
            store.update(cx, |settings| settings.roots = roots);
        });
    }

    /// Stop indexing a root. Dropping the slot drops its LiveIndex —
    /// the watcher stops and a final snapshot saves on drop.
    pub(super) fn remove_root(&mut self, path: &Path, cx: &mut Context<Self>) {
        let before = self.roots.len();
        self.roots.retain(|slot| slot.path != path);
        if self.roots.len() == before {
            return;
        }
        self.persist_roots(cx);
        self.update_search(cx);
        self.notice = Some(format!("removed {} from the index", path.display()).into());
        cx.notify();
    }
}

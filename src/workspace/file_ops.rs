//! File operations: copy/paste/trash/delete/rename, drag-and-drop, the
//! job queue, conflict resolution, and undo.

use super::*;

impl Workspace {
    /// Write every selected item's path to the OS clipboard (one per
    /// line), for pasting into a terminal or another app.
    pub(super) fn copy_selected_paths(&mut self, cx: &mut Context<Self>) {
        let paths: Vec<String> = self
            .selected_paths()
            .iter()
            .map(|(path, _)| path.display().to_string())
            .collect();
        if paths.is_empty() {
            return;
        }
        let notice = if paths.len() == 1 {
            "path copied".to_string()
        } else {
            format!("{} paths copied", paths.len())
        };
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(paths.join("\n")));
        self.notice = Some(notice.into());
        cx.notify();
    }

    /// Trash every selected item (context-menu "Move to Trash" — no
    /// two-press confirm, the click is already explicit).
    pub(super) fn trash_selected(&mut self, cx: &mut Context<Self>) {
        let paths = self
            .selected_paths()
            .into_iter()
            .map(|(path, _)| path)
            .collect();
        self.delete_paths(paths, cx);
    }

    /// Begin renaming the selected browse entry (F2): the row's name
    /// cell becomes an input prefilled with the current name, content
    /// preselected so typing replaces it.
    pub(super) fn start_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.query.is_empty() || self.settings_open {
            return;
        }
        let Some(ix) = self.active_selection().lead() else {
            return;
        };
        let Some(entry) = self.entries.get(ix) else {
            return;
        };
        let name = entry.name.clone();
        let input = cx.new(SearchInput::new);
        input.update(cx, |input, cx| {
            input.set_placeholder("new name", cx);
            input.set_text(name, cx);
            input.select_all_text(cx);
        });
        let subscription = cx.subscribe_in(&input, window, |this, _input, event, window, cx| {
            if matches!(event, SearchInputEvent::Dismissed) {
                this.cancel_rename(window, cx);
            }
        });
        window.focus(&input.focus_handle(cx));
        self.renaming = Some(RenameState {
            ix,
            input,
            _subscription: subscription,
        });
        cx.notify();
    }

    pub(super) fn cancel_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.renaming.take().is_some() {
            window.focus(&self.search_input.focus_handle(cx));
            cx.notify();
        }
    }

    pub(super) fn commit_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(RenameState { ix, input, .. }) = self.renaming.take() else {
            return;
        };
        window.focus(&self.search_input.focus_handle(cx));
        cx.notify();
        let Some(entry) = self.entries.get(ix) else {
            return;
        };
        let new_name = input.read(cx).text().trim().to_string();
        if new_name.is_empty() || new_name == entry.name {
            return; // nothing to do — treated as cancel
        }
        self.run_op(
            FileOp::Rename {
                path: entry.path.clone(),
                new_name,
            },
            cx,
        );
    }

    /// cmd-c / cmd-x (reaches us only while the search input is empty —
    /// otherwise the keys edit query text). Captures every selected item.
    pub(super) fn clip_selected(&mut self, mode: ClipMode, cx: &mut Context<Self>) {
        if self.renaming.is_some() || self.settings_open {
            return;
        }
        let items = self.selected_paths();
        let Some(label) = describe_items(&items) else {
            return;
        };
        self.notice = Some(
            match mode {
                ClipMode::Copy => format!("copied {label} — paste into a folder"),
                ClipMode::Cut => format!("cut {label} — paste to move"),
            }
            .into(),
        );
        self.clipboard = Some((items.into_iter().map(|(path, _)| path).collect(), mode));
        cx.notify();
    }

    /// cmd-v: paste the file clipboard into the current directory; with
    /// no file on it, fall back to pasting clipboard text into the
    /// search box (the input bubbled the action because it was empty).
    pub(super) fn paste_clipboard(&mut self, cx: &mut Context<Self>) {
        if self.renaming.is_some() || self.settings_open {
            return;
        }
        let Some((sources, mode)) = self.clipboard.clone() else {
            if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                self.search_input.update(cx, |input, cx| {
                    input.set_text(text.replace('\n', " "), cx);
                });
            }
            return;
        };
        // One item keeps the interactive conflict dialog; a multi-paste
        // auto-resolves conflicts (keep-both) and lands as one undo.
        if let [source] = &sources[..] {
            let source = source.clone();
            let Some(file_name) = source.file_name() else {
                return;
            };
            let dest = self.cwd.join(file_name);
            if dest == source {
                self.notice = Some(match mode {
                    ClipMode::Copy => "already here — copy conflicts get options soon".into(),
                    ClipMode::Cut => "already here".into(),
                });
                if mode == ClipMode::Cut {
                    self.clipboard = None;
                }
                cx.notify();
                return;
            }
            let op = match mode {
                ClipMode::Copy => FileOp::Copy {
                    from: source,
                    to: dest,
                },
                ClipMode::Cut => FileOp::Move {
                    from: source,
                    to: dest,
                },
            };
            if mode == ClipMode::Cut {
                self.clipboard = None; // a move can only happen once
            }
            self.run_op(op, cx);
            return;
        }
        if mode == ClipMode::Cut {
            self.clipboard = None;
        }
        let dest = self.cwd.clone();
        self.spawn_transfer_batch(sources, mode, dest, cx);
    }

    /// Paths carried by a drag that starts on row/card `ix`: the whole
    /// current selection when the dragged item is part of it (so a
    /// multi-select drags as a group), otherwise just that one item.
    pub(super) fn drag_source_paths(&self, ix: usize) -> Vec<PathBuf> {
        if self.active_selection().contains(ix) && self.active_selection().iter().count() > 1 {
            self.active_selection()
                .iter()
                .filter_map(|i| self.path_at(i).map(|(p, _)| p))
                .collect()
        } else {
            self.path_at(ix)
                .map(|(p, _)| vec![p])
                .into_iter()
                .flatten()
                .collect()
        }
    }

    /// Build the drag payload for a browse row/card, or `None` when the
    /// index has no path. `theme` rides along so the drag pill can paint
    /// itself without a context.
    pub(super) fn drag_items(&self, ix: usize, theme: Theme) -> Option<DragItems> {
        let paths = self.drag_source_paths(ix);
        if paths.is_empty() {
            return None;
        }
        let label: SharedString = match paths.as_slice() {
            [one] => one
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
                .into(),
            many => format!("{} items", many.len()).into(),
        };
        Some(DragItems {
            paths,
            label,
            theme,
            position: Point::default(),
        })
    }

    /// Handle a drop of in-app items or OS files onto `dest_dir`. Internal
    /// drags move; OS-file drops (`ExternalPaths`) copy. Sources whose
    /// parent is already `dest_dir`, and any attempt to drop a directory
    /// into itself or a descendant, are filtered out first.
    pub(super) fn drop_onto(
        &mut self,
        dest_dir: PathBuf,
        sources: Vec<PathBuf>,
        mode: ClipMode,
        cx: &mut Context<Self>,
    ) {
        if !dest_dir.is_dir() {
            return;
        }
        let sources: Vec<PathBuf> = sources
            .into_iter()
            .filter(|src| is_valid_drop(&dest_dir, src))
            .collect();
        if sources.is_empty() {
            return;
        }
        self.spawn_transfer_batch(sources, mode, dest_dir, cx);
    }

    /// Decorate a folder row/card so it accepts drops: an in-app drag of
    /// items ([`DragItems`], moved) or OS files ([`ExternalPaths`],
    /// copied). Highlights while a compatible drag hovers, and refuses a
    /// drag made entirely of items already living in `dest`.
    pub(super) fn folder_drop_target(
        &self,
        row: gpui::Stateful<gpui::Div>,
        dest: PathBuf,
        theme: Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let highlight = theme.accent_selection;
        let can_dest = dest.clone();
        row.drag_over::<DragItems>(move |s, _dragged, _window, _cx| s.bg(highlight))
            .drag_over::<ExternalPaths>(move |s, _dragged, _window, _cx| s.bg(highlight))
            .can_drop(move |dragged, _window, _cx| {
                // OS-file drops are always welcome; an in-app drag is
                // rejected only when every item is already in `dest` or is
                // `dest` (or an ancestor of it).
                match dragged.downcast_ref::<DragItems>() {
                    Some(items) => items.paths.iter().any(|src| is_valid_drop(&can_dest, src)),
                    None => true,
                }
            })
            .on_drop(cx.listener({
                let dest = dest.clone();
                move |this, items: &DragItems, _window, cx| {
                    this.drop_onto(dest.clone(), items.paths.clone(), ClipMode::Cut, cx);
                }
            }))
            .on_drop(cx.listener(move |this, ext: &ExternalPaths, _window, cx| {
                this.drop_onto(dest.clone(), ext.paths().to_vec(), ClipMode::Copy, cx);
            }))
    }

    /// Reload the active directory (refresh button / cmd-r / F5). A no-op
    /// while a search is active — the results are recomputed live.
    pub(super) fn reload_dir(&mut self, cx: &mut Context<Self>) {
        if !self.query.is_empty() || self.settings_open {
            return;
        }
        let cwd = self.cwd.clone();
        self.load_dir(&cwd, cx);
        self.notice = Some("refreshed".into());
        cx.notify();
    }

    /// Move or copy many items into `dest_dir` as one undo batch. Runs
    /// sequentially off-thread; an occupied destination auto-resolves to
    /// the next free "name 2" variant (no per-item prompt), and items
    /// already in `dest_dir` are skipped. One job tracks the run; byte
    /// progress reflects the item currently copying. Shared by clipboard
    /// paste (dest = cwd) and drag-and-drop (dest = the dropped-on folder).
    pub(super) fn spawn_transfer_batch(
        &mut self,
        sources: Vec<PathBuf>,
        mode: ClipMode,
        dest_dir: PathBuf,
        cx: &mut Context<Self>,
    ) {
        let progress = std::sync::Arc::new(ops::OpProgress::default());
        let job_id = self.next_job_id;
        self.next_job_id += 1;
        let verb = if mode == ClipMode::Copy {
            "copying"
        } else {
            "moving"
        };
        self.jobs.push(Job {
            id: job_id,
            label: format!("{verb} {} {}", sources.len(), plural_items(sources.len())).into(),
            progress: progress.clone(),
        });
        self.spawn_job_ticker(job_id, cx);
        cx.notify();
        let tags = self.tags.clone();
        cx.spawn(async move |this, cx| {
            let applied = cx
                .background_executor()
                .spawn({
                    let progress = progress.clone();
                    async move {
                        let mut applied = Vec::new();
                        for source in sources {
                            let Some(name) = source.file_name() else {
                                continue;
                            };
                            let mut dest = dest_dir.join(name);
                            if dest == source {
                                continue; // pasting into the same folder
                            }
                            if std::fs::symlink_metadata(&dest).is_ok() {
                                match ops::next_free_name(&dest) {
                                    Ok(free) => dest = free,
                                    Err(_) => continue,
                                }
                            }
                            let op = match mode {
                                ClipMode::Copy => FileOp::Copy {
                                    from: source,
                                    to: dest,
                                },
                                ClipMode::Cut => FileOp::Move {
                                    from: source,
                                    to: dest,
                                },
                            };
                            if let Ok(mut done) = ops::apply_with_progress(&op, &progress) {
                                migrate_tags(&tags, &mut done);
                                applied.push(done);
                            }
                        }
                        applied
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                this.jobs.retain(|job| job.id != job_id);
                if !applied.is_empty() {
                    let verb = if mode == ClipMode::Copy {
                        "copied"
                    } else {
                        "moved"
                    };
                    this.notice = Some(
                        format!("{verb} {} {}", applied.len(), plural_items(applied.len())).into(),
                    );
                    this.journal.record(applied);
                }
                let cwd = this.cwd.clone();
                this.load_dir(&cwd, cx);
                // A file op may have migrated tags (moved/copied/dropped
                // keys), so refresh the sidebar's distinct-tag list.
                this.refresh_sidebar_tags(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Move the selected browse entry to the OS trash. With
    /// confirm_delete on (the default), the first press arms and the
    /// second press on the same entry deletes — a modal dialog can
    /// replace this once conflict prompts exist.
    pub(super) fn delete_selected(&mut self, cx: &mut Context<Self>) {
        if self.renaming.is_some() || !self.query.is_empty() || self.settings_open {
            return;
        }
        let items = self.selected_paths();
        let Some(label) = describe_items(&items) else {
            return;
        };
        let paths: Vec<PathBuf> = items.into_iter().map(|(path, _)| path).collect();
        let confirm = self.settings.read(cx).settings().confirm_delete;
        if confirm && self.pending_delete.as_deref() != Some(&paths[..]) {
            self.pending_delete = Some(paths);
            self.notice = Some(format!("press again to move {label} to the trash").into());
            cx.notify();
            return;
        }
        self.pending_delete = None;
        self.delete_paths(paths, cx);
    }

    /// Move every path to the OS trash as one undo batch (off-thread;
    /// each trash op is instant but there can be many).
    pub(super) fn delete_paths(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        if paths.is_empty() {
            return;
        }
        let tags = self.tags.clone();
        cx.spawn(async move |this, cx| {
            let applied = cx
                .background_executor()
                .spawn(async move {
                    let mut applied = Vec::new();
                    for path in paths {
                        if let Ok(mut done) = ops::apply(&FileOp::Delete { path }) {
                            migrate_tags(&tags, &mut done);
                            applied.push(done);
                        }
                    }
                    applied
                })
                .await;
            this.update(cx, |this, cx| {
                if !applied.is_empty() {
                    this.notice = Some(
                        if applied.len() == 1 {
                            applied[0].describe()
                        } else {
                            format!("moved {} items to the trash", applied.len())
                        }
                        .into(),
                    );
                    this.journal.record(applied);
                }
                let cwd = this.cwd.clone();
                this.load_dir(&cwd, cx);
                // A file op may have migrated tags (moved/copied/dropped
                // keys), so refresh the sidebar's distinct-tag list.
                this.refresh_sidebar_tags(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Run a file operation, first probing its destination off-thread:
    /// an occupied one opens the conflict dialog instead of failing
    /// mid-apply.
    pub(super) fn run_op(&mut self, op: FileOp, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let occupied = cx
                .background_executor()
                .spawn({
                    let dest = op.destination();
                    async move { dest.filter(|dest| std::fs::symlink_metadata(dest).is_ok()) }
                })
                .await;
            this.update(cx, |this, cx| match occupied {
                Some(dest) => {
                    this.conflict = Some(ConflictState { op, dest });
                    cx.notify();
                }
                None => this.spawn_apply(op, cx),
            })
            .ok();
        })
        .detach();
    }

    /// Resolve the open conflict dialog: keep both (retarget to the
    /// first free "name 2" variant) or cancel.
    pub(super) fn resolve_conflict(&mut self, keep_both: bool, cx: &mut Context<Self>) {
        let Some(ConflictState { op, dest }) = self.conflict.take() else {
            return;
        };
        cx.notify();
        if !keep_both {
            return;
        }
        cx.spawn(async move |this, cx| {
            let retargeted = cx
                .background_executor()
                .spawn(
                    async move { ops::next_free_name(&dest).map(|free| op.with_destination(free)) },
                )
                .await;
            this.update(cx, |this, cx| match retargeted {
                Ok(op) => this.spawn_apply(op, cx),
                Err(err) => {
                    this.notice = Some(format!("{err:#}").into());
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Execute a file operation on the background executor; success
    /// lands in the undo journal and refreshes the listing. Ops never
    /// touch the index — the watchers pick the change up as deltas.
    /// Copies and moves (the potentially long ones) appear in the jobs
    /// bar with progress and a cancel control while they run.
    pub(super) fn spawn_apply(&mut self, op: FileOp, cx: &mut Context<Self>) {
        let progress = std::sync::Arc::new(ops::OpProgress::default());
        let job_id = self.next_job_id;
        if matches!(op, FileOp::Copy { .. } | FileOp::Move { .. }) {
            self.next_job_id += 1;
            let (verb, source) = match &op {
                FileOp::Copy { from, .. } => ("copying", from),
                FileOp::Move { from, .. } => ("moving", from),
                _ => unreachable!(),
            };
            let name = source
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            self.jobs.push(Job {
                id: job_id,
                label: format!("{verb} {name}").into(),
                progress: progress.clone(),
            });
            self.spawn_job_ticker(job_id, cx);
            cx.notify();
        }
        let tags = self.tags.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn({
                    let progress = progress.clone();
                    async move {
                        let mut applied = ops::apply_with_progress(&op, &progress)?;
                        migrate_tags(&tags, &mut applied);
                        anyhow::Ok(applied)
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                this.jobs.retain(|job| job.id != job_id);
                match result {
                    Ok(applied) => {
                        this.notice = Some(applied.describe().into());
                        this.journal.record(vec![applied]);
                    }
                    Err(err) if err.is::<ops::OpCanceled>() => {
                        this.notice = Some("canceled".into());
                    }
                    Err(err) => this.notice = Some(format!("{err:#}").into()),
                }
                let cwd = this.cwd.clone();
                this.load_dir(&cwd, cx);
                // A file op may have migrated tags (moved/copied/dropped
                // keys), so refresh the sidebar's distinct-tag list.
                this.refresh_sidebar_tags(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Refresh the UI while job `id` runs, so its progress bar moves.
    /// Exits as soon as the job leaves the list.
    pub(super) fn spawn_job_ticker(&self, id: u64, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(100))
                    .await;
                let alive = this.update(cx, |this, cx| {
                    let alive = this.jobs.iter().any(|job| job.id == id);
                    if alive {
                        cx.notify();
                    }
                    alive
                });
                if !matches!(alive, Ok(true)) {
                    break;
                }
            }
        })
        .detach();
    }

    pub(super) fn cancel_job(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some(job) = self.jobs.iter().find(|job| job.id == id) {
            job.progress.request_cancel();
            self.notice = Some("canceling…".into());
            cx.notify();
        }
    }

    /// Undo the most recent file operation (cmd-z / ctrl-z). The disk
    /// work runs off-thread; a failed undo goes back on the journal so
    /// the user can fix the cause and retry.
    pub(super) fn undo_last(&mut self, cx: &mut Context<Self>) {
        let Some(batch) = self.journal.pop() else {
            self.notice = Some("nothing to undo".into());
            cx.notify();
            return;
        };
        let tags = self.tags.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn({
                    let batch = batch.clone();
                    async move {
                        ops::undo_batch(&batch)?;
                        // Only reverse tag migration once the files are
                        // back; a failed file undo leaves both untouched.
                        for op in batch.iter().rev() {
                            if let Err(err) = tags.undo_applied(op) {
                                tracing::error!("failed to undo tag migration: {err:#}");
                            }
                        }
                        anyhow::Ok(())
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        let label = if batch.len() == 1 {
                            batch[0].describe()
                        } else {
                            format!("{} operations", batch.len())
                        };
                        this.notice = Some(format!("undid: {label}").into());
                    }
                    Err(err) => {
                        this.notice = Some(format!("undo failed: {err:#}").into());
                        this.journal.restore(batch);
                    }
                }
                let cwd = this.cwd.clone();
                this.load_dir(&cwd, cx);
                // A file op may have migrated tags (moved/copied/dropped
                // keys), so refresh the sidebar's distinct-tag list.
                this.refresh_sidebar_tags(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

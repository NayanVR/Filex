//! Search and Magic: query updates, running searches, scope, and the
//! Magic plan (build / toggle / confirm).

use super::*;

impl Workspace {
    /// Move focus into the search box (the `/` shortcut and the search
    /// affordance). Selecting the existing text means the next keystroke
    /// replaces a stale query rather than appending to it.
    pub(super) fn focus_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let handle = self.search_input.focus_handle(cx);
        window.focus(&handle);
        self.search_input
            .update(cx, |input, cx| input.select_all_text(cx));
    }

    /// Run a `tag:NAME` search (clicking a sidebar tag), focusing the
    /// search field so it can be refined.
    pub(super) fn search_tag(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.search_input.focus_handle(cx));
        self.search_input.update(cx, |input, cx| {
            input.set_text(format!("tag:{name}"), cx);
        });
    }

    /// Remove one recognized filter token from the query (clicking its
    /// chip) by rewriting the search input's text.
    pub(super) fn remove_filter_token(&mut self, token: &str, cx: &mut Context<Self>) {
        let rewritten = filex::search_filter::without_token(&self.query, token);
        self.search_input
            .update(cx, |input, cx| input.set_text(rewritten, cx));
    }

    /// Remove an inferred natural-language phrase (clicking its chip) by
    /// stripping the words that produced it.
    pub(super) fn remove_phrase(&mut self, source: &str, cx: &mut Context<Self>) {
        let rewritten = filex::phrases::without_phrase(&self.query, source);
        self.search_input
            .update(cx, |input, cx| input.set_text(rewritten, cx));
    }

    /// Run the checked ops as one undo batch — the same background
    /// executor, `apply_with_progress` and `Journal::record` path that
    /// paste and drag-and-drop already use, so a Magic plan undoes with
    /// the same Ctrl+Z as anything else (`docs/design-magic-mode.md` §3).
    ///
    /// Conflicts are resolved the way a multi-item paste resolves them —
    /// an occupied destination retargets to the next free "name 2"
    /// variant rather than prompting per file. A plan is reviewed as a
    /// whole; stopping midway to ask about file 40 of 200 would be a
    /// worse experience than the batch being uniformly predictable.
    pub(super) fn confirm_magic(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.magic.as_ref() else {
            return;
        };
        let ops = state.selected_ops();
        if ops.is_empty() {
            return;
        }
        let verb = state.command.verb;
        let progress = std::sync::Arc::new(ops::OpProgress::default());
        let job_id = self.next_job_id;
        self.next_job_id += 1;
        self.jobs.push(Job {
            id: job_id,
            label: format!(
                "{} {} {}",
                verb.label().to_lowercase(),
                ops.len(),
                plural_items(ops.len())
            )
            .into(),
            progress: progress.clone(),
        });
        self.spawn_job_ticker(job_id, cx);
        // The card's work is done; clearing the query also drops the card
        // and returns the user to where they were.
        self.clear_search(cx);
        cx.notify();

        let tags = self.tags.clone();
        cx.spawn(async move |this, cx| {
            let applied = cx
                .background_executor()
                .spawn({
                    let progress = progress.clone();
                    async move {
                        let mut applied = Vec::new();
                        for mut op in ops {
                            if let Some(dest) = op.destination()
                                && std::fs::symlink_metadata(&dest).is_ok()
                            {
                                match ops::next_free_name(&dest) {
                                    Ok(free) => op = op.with_destination(free),
                                    Err(_) => continue,
                                }
                            }
                            match ops::apply_with_progress(&op, &progress) {
                                Ok(mut done) => {
                                    migrate_tags(&tags, &mut done);
                                    applied.push(done);
                                }
                                Err(err) => {
                                    tracing::warn!("magic op failed: {err:#}");
                                }
                            }
                        }
                        applied
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                this.jobs.retain(|job| job.id != job_id);
                if !applied.is_empty() {
                    this.notice = Some(
                        format!(
                            "{} {} {}",
                            verb.past_tense(),
                            applied.len(),
                            plural_items(applied.len())
                        )
                        .into(),
                    );
                    this.journal.record(applied);
                }
                let cwd = this.cwd.clone();
                this.load_dir(&cwd, cx);
                this.refresh_sidebar_tags(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn clear_search(&mut self, cx: &mut Context<Self>) {
        // A forced mode is scoped to the query that prompted it: clearing
        // the box returns to Auto so the next, unrelated query decides
        // afresh rather than inheriting a stuck On/Off.
        self.magic_mode = MagicMode::Auto;
        // The input owns the text; its Changed event clears our mirror
        // and re-runs the (now empty) search.
        self.search_input.update(cx, |input, cx| {
            if !input.is_empty() {
                input.set_text("", cx);
            }
        });
    }

    /// The search-bar toggle. Clicking it means "give me the other mode
    /// than what I'm seeing": from any magic view (forced or auto) to a
    /// forced-off normal search, and from any normal search to forced-on
    /// magic. Without the forced-off state, clicking the toggle on an
    /// auto-switched command would set `On` and look like a no-op, and
    /// there'd be no way back to plain search without editing the query.
    pub(super) fn toggle_magic_mode(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.magic_mode = if self.in_magic_view() {
            MagicMode::Off
        } else {
            MagicMode::On
        };
        window.focus(&self.search_input.focus_handle(cx));
        self.update_search(cx);
        cx.notify();
    }

    /// Open the scope dropdown anchored at the click position (same
    /// overlay pattern as the context menu).
    pub(super) fn open_scope_menu(
        &mut self,
        position: Point<Pixels>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        self.scope_menu = Some(Self::clamped_menu_position(position, 88., window));
        cx.notify();
    }

    pub(super) fn close_scope_menu(&mut self, cx: &mut Context<Self>) {
        if self.scope_menu.take().is_some() {
            cx.notify();
        }
    }

    /// Pick a search scope and re-run the query under it. No-op (beyond
    /// closing the menu) when the scope is unchanged.
    pub(super) fn set_scope(&mut self, scope: SearchScope, cx: &mut Context<Self>) {
        self.scope_menu = None;
        if self.search_scope != scope {
            self.search_scope = scope;
            self.update_search(cx);
        }
        cx.notify();
    }

    /// Whether the plan view replaces the results list right now. The one
    /// predicate the render path and the search path share, so they can't
    /// disagree about which mode is active.
    pub(super) fn in_magic_view(&self) -> bool {
        match self.magic_mode {
            MagicMode::On => true,
            MagicMode::Off => false,
            // Auto follows whether a command actually parsed.
            MagicMode::Auto => self.magic.is_some(),
        }
    }

    /// Schedule a search for the current query, coalescing keystrokes.
    ///
    /// Every caller wanting results should come through here rather than
    /// [`run_search`](Self::run_search). A search is a full parallel scan
    /// of every root's arena, and it is **not cancellable once started**:
    /// the generation check drops a stale scan's *results*, but the scan
    /// itself still runs to completion on the shared rayon pool. So firing
    /// one per keystroke meant a 12-character query launched 12 full
    /// scans, 11 of them pure waste, all competing for the same cores —
    /// measured at ~124 ms of scan work against a 1.2M-entry index, which
    /// is latency the user waits through before seeing what they typed.
    ///
    /// [`SEARCH_DEBOUNCE`] collapses that to one scan per settled query.
    pub(super) fn update_search(&mut self, cx: &mut Context<Self>) {
        // Bump now, not in `run_search`: a query that has already changed
        // must invalidate scans still in flight immediately, so their
        // results can't land under the newer query.
        self.search_generation += 1;
        // Signal any scan already running on the pool to stop, then hand
        // the next scan a fresh flag. The generation check discards a
        // stale scan's *results*; this stops it spending CPU and holding
        // the arena read lock to produce them. Without it, a burst of
        // per-keystroke searches on a large index piles up and convoys
        // with the FS writer — measured at multi-second stalls.
        self.search_cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.search_cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        // Coalesce the burst. Dropping the previous task cancels its
        // timer, so an N-character word costs one scan, not N.
        self.search_debounce = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SEARCH_DEBOUNCE).await;
            this.update(cx, |this, cx| this.run_search(cx)).ok();
        }));
        cx.notify();
    }

    /// Run a merged query across every ready root on the background
    /// executor. Stale completions are dropped by generation check.
    ///
    /// Call [`update_search`](Self::update_search) instead — this is the
    /// debounced tail, and calling it directly puts a full scan on every
    /// keystroke again.
    ///
    /// The raw query is split (a pure
    /// [`parse_query`](filex::search_filter::parse_query)) into filename
    /// text, index-evaluable filters (`kind:`/`ext:`/`size:`/`modified:`),
    /// and `tag:` filters. The text + index filters run in the index scan
    /// (`search_filtered`); the results are then intersected with the
    /// paths carrying every named tag (the sidecar store). A query with no
    /// filename text and no index filters — only `tag:` — lists the tagged
    /// files straight from the sidecar, skipping the index scan.
    pub(super) fn run_search(&mut self, cx: &mut Context<Self>) {
        let generation = self.search_generation;

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        // A command-shaped query searches for what it *targets*, not for
        // its own words: "delete screenshots older than 30 days" as a
        // literal search matches nothing, while the files the plan would
        // act on are exactly what the user needs to see under the card.
        //
        // The gate follows the entry (`magic::parse_with_gate`):
        // - On:   the user opted in, so `delete old logs` parses (gate off).
        // - Auto: a command needs structured evidence before it may
        //         auto-switch the app into magic mode (gate on).
        // - Off:  the user forced normal search; don't read it as a command
        //         at all.
        let command = match self.magic_mode {
            MagicMode::Off => None,
            MagicMode::On => filex::magic::parse_with_gate(&self.query, now, false),
            MagicMode::Auto => filex::magic::parse_with_gate(&self.query, now, true),
        };
        // Re-searching the *same* command must not disturb the card. The
        // live-update loop re-runs this on every filesystem event burst,
        // and blanking `outcome` made the card flip back to "finding
        // matches…" each time — a card on a busy home directory never
        // settled. Worse, it is `checked` that must survive: resetting it
        // silently re-ticks rows the user deliberately unticked, on a
        // batch that is one click from being executed.
        let previous = self.magic.take();
        self.magic = command.as_ref().map(|command| match previous {
            Some(state) if state.command == *command => state,
            _ => MagicState {
                command: command.clone(),
                outcome: None,
                checked: Vec::new(),
            },
        });

        let (text, all_filters, limit) = match &command {
            Some(command) => (
                command.selection.text.clone(),
                command.selection.filters.clone(),
                // Deliberately *not* SEARCH_RESULT_LIMIT. A plan is built
                // from these rows, so a truncated search would silently
                // become a partial delete — and `magic::build`'s
                // too-many-to-review guard would never fire, because it
                // would only ever see the truncated count. Fetching one
                // past the cap is what lets that guard work.
                filex::magic::MAX_PLAN_OPS + 1,
            ),
            None => {
                // Forced-on magic mode with no command yet (empty box, or a
                // half-typed command): show nothing rather than a normal
                // search. The magic view renders its own "type a command"
                // hint — running a filename search here would repopulate
                // exactly the results list the mode is meant to replace.
                if self.magic_mode == MagicMode::On {
                    self.results.clear();
                    self.search_selection.clear();
                    return;
                }
                let parsed = filex::search_filter::parse_query(&self.query, now);
                // Natural-language phrases in whatever text the `key:value`
                // parse left over ("photos from last week"). Pure and
                // rule-based; the recognized phrases are shown as removable
                // chips, never applied invisibly.
                let expansion = filex::phrases::expand(&parsed.text, now);
                let text = expansion.text.clone();
                if text.is_empty() && parsed.filters.is_empty() && expansion.is_empty() {
                    self.results.clear();
                    // Leave the browse selection intact; only the search's
                    // own selection goes away with the results.
                    self.search_selection.clear();
                    return;
                }
                let filters = parsed
                    .filters
                    .into_iter()
                    .chain(expansion.filters())
                    .collect::<Vec<_>>();
                (text, filters, SEARCH_RESULT_LIMIT)
            }
        };

        // Tags live in the sidecar (intersected after the scan); the rest
        // are evaluated inside the index scan.
        let mut tags_required = Vec::new();
        let mut index_filters = Vec::new();
        for filter in all_filters {
            match filter {
                Filter::Tag(name) => tags_required.push(name),
                other if !index_filters.contains(&other) => index_filters.push(other),
                _ => {}
            }
        }

        // Tag-only query (no text, no index filters): list the tagged files
        // straight from the sidecar (works the same in service mode — tags
        // are always a client-side store).
        if text.is_empty() && index_filters.is_empty() {
            let store = self.tags.clone();
            let required = tags_required;
            // Scope tag results the same way as everything else, so the
            // dropdown means one thing across query kinds.
            let scope_dir = match self.search_scope {
                SearchScope::Anywhere => None,
                SearchScope::CurrentDir => Some(self.cwd.clone()),
            };
            cx.spawn(async move |this, cx| {
                let rows = cx
                    .background_executor()
                    .spawn(async move {
                        let mut paths = store.paths_with_all_tags(&required);
                        if let Some(dir) = &scope_dir {
                            paths.retain(|p| p.starts_with(dir));
                        }
                        rows_from_tagged_paths(paths, limit)
                    })
                    .await;
                this.update(cx, |this, cx| {
                    if this.search_generation == generation {
                        this.results = rows;
                        this.rebuild_magic_plan();
                        this.select_first_result();
                        this.refresh_preview(cx);
                        cx.notify();
                    }
                })
                .ok();
            })
            .detach();
            return;
        }

        // Service mode: the text query + index filters go over IPC and are
        // applied service-side (where the index with size/mtime lives); a
        // failed roundtrip falls back to local indexing and re-runs. `tag:`
        // stays client-side — the service has no sidecar.
        #[cfg(target_os = "windows")]
        if let Some(client) = self.service.clone() {
            let store = self.tags.clone();
            let text = text.clone();
            let tags_required = tags_required.clone();
            let index_filters = index_filters.clone();
            cx.spawn(async move |this, cx| {
                let result = cx
                    .background_executor()
                    .spawn(async move {
                        // KNOWN GAP: unlike the local path below, these hits
                        // carry no `MatchKind` over the wire, so
                        // `usable_in_plan` cannot be applied and a command
                        // query in service mode can still plan against fuzzy
                        // matches. Closing it means adding the kind to the
                        // IPC hit; until then Magic is only fully safe on
                        // the local index.
                        client
                            .search(&text, &index_filters, limit as u32)
                            .map(|hits| {
                                let rows = hits
                                    .into_iter()
                                    .map(|hit| SearchRow {
                                        name: hit.name.into(),
                                        path_label: hit.path.display().to_string().into(),
                                        is_dir: hit.is_dir,
                                        target: hit.path,
                                    })
                                    .collect();
                                filter_rows_by_tags(rows, &store, &tags_required)
                            })
                    })
                    .await;
                this.update(cx, |this, cx| {
                    if this.search_generation != generation {
                        return;
                    }
                    match result {
                        Ok(rows) => {
                            this.results = rows;
                            this.rebuild_magic_plan();
                            this.select_first_result();
                            this.refresh_preview(cx);
                            cx.notify();
                        }
                        Err(err) => {
                            tracing::warn!("service search failed ({err:#})");
                            this.service_disconnected(cx);
                        }
                    }
                })
                .ok();
            })
            .detach();
            return;
        }

        let indexes: Vec<SharedIndex> = self
            .roots
            .iter()
            .filter_map(RootSlot::ready_index)
            .collect();
        if indexes.is_empty() {
            return; // still building; root readiness re-runs the query
        }
        let store = self.tags.clone();
        let frecency = self.frecency.clone();
        let command_query = command.is_some();
        let cancel = self.search_cancel.clone();
        // "Current Dir" scopes the scan to cwd; "Anywhere" leaves it open.
        // The path is resolved to a subtree per-index inside the scan (off
        // the UI thread), so this is just the directory to hand down.
        let scope_dir = match self.search_scope {
            SearchScope::Anywhere => None,
            SearchScope::CurrentDir => Some(self.cwd.clone()),
        };

        cx.spawn(async move |this, cx| {
            let rows = cx
                .background_executor()
                .spawn(async move {
                    // Per-scan latency, the number the CLAUDE.md
                    // search-as-you-type rule is about. Invisible from
                    // outside the process otherwise — run the app with
                    // `RUST_LOG=filex=debug` to see it.
                    let started = std::time::Instant::now();
                    let rows: Vec<SearchRow> = manager::search_all_scoped(
                        &indexes,
                        &text,
                        &index_filters,
                        limit,
                        &frecency,
                        &cancel,
                        scope_dir.as_deref(),
                    )
                        .into_iter()
                        // A command's plan is built from these rows, so a
                        // fuzzy hit here becomes a file the batch acts on.
                        // Subsequence matching is far too loose for that:
                        // `rename gravloc to …` matched 148 files of which
                        // 4 contained "gravloc" — the rest were things like
                        // `xstate-graph.development.cjs.js`, which a plan
                        // would happily have renamed. Fuzzy is a good way to
                        // *find* something you then look at; it is not
                        // evidence you meant to modify it.
                        .filter(|hit| usable_in_plan(hit.score.kind, command_query))
                        .map(|hit| SearchRow {
                            name: hit.name.into(),
                            path_label: hit.path.display().to_string().into(),
                            is_dir: hit.is_dir,
                            target: hit.path,
                        })
                        .collect();
                    let elapsed_ms = started.elapsed().as_millis() as u64;
                    // A scan of even a 2M-entry arena is tens of
                    // milliseconds (docs/design-search-ranking.md). Anything
                    // near a second is not scan *work* — it is almost
                    // certainly `search_all` blocked acquiring the read lock
                    // while the writer holds it for a big rescan. Surfaced at
                    // warn so it lands in the log with no `RUST_LOG` set,
                    // which is the only way to see it in a shipped .app.
                    if elapsed_ms >= SLOW_OP_MS {
                        tracing::warn!(
                            query = %text,
                            limit,
                            hits = rows.len(),
                            elapsed_ms,
                            "slow search — scan or lock wait over budget"
                        );
                        // Only the slow scans are sampled to Sentry — the
                        // ones worth investigating — so search-as-you-type
                        // never pays a per-keystroke measurement cost.
                        #[cfg(feature = "observability")]
                        filex::observability::record_search_latency(elapsed_ms, rows.len());
                    } else {
                        tracing::debug!(query = %text, limit, hits = rows.len(), elapsed_ms, "index scan");
                    }
                    filter_rows_by_tags(rows, &store, &tags_required)
                })
                .await;
            this.update(cx, |this, cx| {
                if this.search_generation == generation {
                    this.results = rows;
                    this.rebuild_magic_plan();
                    this.select_first_result();
                    this.refresh_preview(cx);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Resolve the pending Magic command against the rows the search just
    /// returned. No-op unless the query parsed as a command.
    ///
    /// Plans are built from `results`, which is why a Magic query raises
    /// the search limit to `MAX_PLAN_OPS + 1` — see `update_search`. The
    /// one-past-the-cap fetch is what lets `build` distinguish "1000
    /// files, reviewable" from "more than we will act on blind".
    pub(super) fn rebuild_magic_plan(&mut self) {
        let Some(state) = self.magic.as_mut() else {
            return;
        };
        let matches: Vec<PathBuf> = self.results.iter().map(|row| row.target.clone()).collect();
        let ctx = filex::magic::PlanContext {
            cwd: &self.cwd,
            dirs: &self.user_dirs,
        };
        let outcome = filex::magic::build(&state.command, &matches, &ctx);
        // Only re-tick when the plan actually changed. This runs on every
        // filesystem event burst, not just on a new query, so rebuilding
        // `checked` unconditionally would keep restoring rows the user had
        // unticked — silently re-arming a destructive batch under them.
        // Comparing the ops (not just their count) is what makes "same
        // plan" mean the same files in the same order.
        let unchanged = matches!(
            (&outcome, &state.outcome),
            (Ok(new), Some(Ok(old))) if new.ops == old.ops
        );
        if !unchanged {
            state.checked = match &outcome {
                Ok(plan) => vec![true; plan.ops.len()],
                Err(_) => Vec::new(),
            };
        }
        state.outcome = Some(outcome);
    }

    /// Toggle one op's checkbox in the Magic card.
    pub(super) fn toggle_magic_op(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(state) = self.magic.as_mut()
            && let Some(checked) = state.checked.get_mut(ix)
        {
            *checked = !*checked;
            cx.notify();
        }
    }

    /// Check or uncheck every op in the Magic plan at once — the card's
    /// Select all / Deselect all control, so a large batch doesn't have to
    /// be un-ticked one row at a time.
    pub(super) fn set_all_magic_ops(&mut self, checked: bool, cx: &mut Context<Self>) {
        if let Some(state) = self.magic.as_mut() {
            for flag in &mut state.checked {
                *flag = checked;
            }
            cx.notify();
        }
    }

    /// New results select the first hit (Spotlight-style), so Enter
    /// immediately opens the top match.
    pub(super) fn select_first_result(&mut self) {
        if self.results.is_empty() {
            self.search_selection.clear();
        } else {
            self.search_selection.select_one(0);
        }
        self.results_scroll.scroll_to_item(0, ScrollStrategy::Top);
    }

    pub(super) fn any_root_ready(&self) -> bool {
        if self.service_mode() {
            return true;
        }
        self.roots
            .iter()
            .any(|slot| matches!(slot.state, RootState::Ready { .. }))
    }

    pub(super) fn index_status_text(&self) -> SharedString {
        #[cfg(target_os = "windows")]
        if self.service_mode() {
            let files: u64 = self.service_status.iter().map(|r| r.files).sum();
            return ui::status_bar::service_index_status(files, self.service_status.len()).into();
        }
        let total = self.roots.len();
        let mut ready = 0usize;
        let mut failed = 0usize;
        let mut files = 0usize;
        for slot in &self.roots {
            match &slot.state {
                RootState::Ready { files: f, .. } => {
                    ready += 1;
                    files += f;
                }
                RootState::Failed(_) => failed += 1,
                RootState::Building => {}
            }
        }
        let degraded = self.roots.iter().any(|slot| match &slot.state {
            RootState::Ready { live, .. } => live.coverage_degraded(),
            _ => false,
        });
        ui::status_bar::local_index_status(ready, total, failed, files, degraded).into()
    }
}

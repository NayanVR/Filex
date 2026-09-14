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

    /// Run the checked ops as one undo batch, through the same
    /// `apply_with_progress` + `Journal::record` path paste and drag use,
    /// so a Magic plan undoes with the same Ctrl+Z as anything else.
    ///
    /// Conflicts resolve as a multi-item paste does — an occupied
    /// destination retargets to the next free "name 2" rather than
    /// prompting per file. A plan is reviewed as a whole; stopping to ask
    /// about file 40 of 200 would be worse than uniform predictability.
    pub(super) fn confirm_magic(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.magic.as_ref() else {
            return;
        };
        let ops = state.selected_ops();
        if ops.is_empty() {
            return;
        }
        let verb = state.command.verb;
        let identities = self.search_hits.clone();
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
            let (applied, failure) = cx
                .background_executor()
                .spawn({
                    let progress = progress.clone();
                    async move {
                        let mut applied = Vec::new();
                        let mut failure = None;
                        for op in &ops {
                            let checked = identities
                                .get(op.source())
                                .ok_or_else(|| anyhow::anyhow!("search target expired"))
                                .and_then(filex::daemon::query::verify);
                            if let Err(e) = checked {
                                return (
                                    applied,
                                    Some(format!(
                                        "Plan changed: {e}. Search again before applying."
                                    )),
                                );
                            }
                        }
                        for mut op in ops {
                            let hit = &identities[op.source()];
                            if let Err(e) = filex::daemon::query::verify(hit) {
                                failure =
                                    Some(format!("Stopped because a search target changed: {e}"));
                                break;
                            }

                            if let Some(dest) = op.destination()
                                && std::fs::symlink_metadata(&dest).is_ok()
                            {
                                match ops::next_free_name(&dest) {
                                    Ok(free) => op = op.with_destination(free),
                                    Err(e) => {
                                        failure = Some(e.to_string());
                                        break;
                                    }
                                }
                            }
                            match ops::apply_with_progress(&op, &progress) {
                                Ok(mut done) => {
                                    migrate_tags(&tags, &mut done);
                                    applied.push(done);
                                }
                                Err(err) => {
                                    failure = Some(format!("Plan stopped: {err:#}"));
                                    break;
                                }
                            }
                        }
                        (applied, failure)
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
                if let Some(error) = failure {
                    this.notice = Some(error.into());
                }
                this.refresh_after_op(cx);
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

    /// The search-bar toggle: "give me the other mode than what I'm
    /// seeing". Any magic view → forced-off normal, any normal search →
    /// forced-on magic. Without forced-off, toggling an auto-switched
    /// command would set `On` and look like a no-op, with no way back to
    /// plain search except editing the query.
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
    /// Every caller wanting results comes through here, not
    /// [`run_search`](Self::run_search).
    ///
    /// Debounce keystrokes and cancel the preceding daemon request.
    pub(super) fn update_search(&mut self, cx: &mut Context<Self>) {
        // Bump now, not in `run_search`: a query that has already changed
        // must invalidate scans still in flight immediately, so their
        // results can't land under the newer query.
        self.search_generation += 1;
        self.notice = None;
        if let Some(state) = &mut self.magic {
            state.loading = true;
            state.error = None;
        }
        self.search_page_query = None;
        self.search_paging = false;
        self.search_more = false;
        if let Some(client) = self.service.clone() {
            let client_id = self.search_client_id;
            let before_request = self.search_generation;
            cx.background_executor()
                .spawn(async move {
                    let _ = client.call(filex::daemon::ipc::Command::Cancel {
                        client: client_id,
                        before_request,
                    });
                })
                .detach();
        }

        // Cancel in-flight work immediately. The generation check below also
        // prevents a completed older request from replacing the current results.
        self.search_cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.search_cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        // Restart the 50 ms timer after each edit. Only keystrokes within that
        // interval are coalesced; ordinary typing can still issue several queries.
        self.search_debounce = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SEARCH_DEBOUNCE).await;
            this.update(cx, |this, cx| this.run_search(cx)).ok();
        }));
        cx.notify();
    }

    /// Parse filters and request a bounded page or an exhaustive Magic stream.
    pub(super) fn run_search(&mut self, cx: &mut Context<Self>) {
        let generation = self.search_generation;

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        // A command-shaped query searches for what it *targets*, not its
        // own words: "delete screenshots older than 30 days" matches
        // nothing literally, while the files the plan would act on are
        // exactly what belongs under the card. The gate follows the entry
        // (`magic::parse_with_gate`): On = user opted in (gate off), Auto =
        // needs structured evidence to auto-switch (gate on), Off = don't
        // read it as a command.
        let command = match self.magic_mode {
            MagicMode::Off => None,
            MagicMode::On => filex::magic::parse_with_gate(&self.query, now, false),
            MagicMode::Auto => filex::magic::parse_with_gate(&self.query, now, true),
        };
        // Re-searching the *same* command must not disturb the card. The
        // live-update loop re-runs this on every FS event burst, so
        // blanking `outcome` made the card flip back to "finding matches…"
        // forever on a busy directory. Worse, `checked` must survive —
        // resetting it silently re-ticks rows the user unticked, on a batch
        // one click from executing.
        let previous = self.magic.take();
        self.magic = command.as_ref().map(|command| match previous {
            Some(mut state) if state.source_query == self.query => {
                // Relative dates resolve against a newer clock on refresh. The
                // unchanged query still owns the user's per-operation choices.
                state.command = command.clone();
                state
            }
            _ => MagicState {
                source_query: self.query.clone(),
                command: command.clone(),
                outcome: None,
                checked: Vec::new(),
                loading: true,
                error: None,
                progress: Default::default(),
            },
        });

        let progress = std::sync::Arc::new(MagicProgress::default());
        if let Some(state) = &mut self.magic {
            self.search_selection.clear();
            state.loading = true;
            state.error = None;
            state.progress = progress.clone();
            cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(150))
                        .await;
                    let running = this
                        .update(cx, |this, cx| {
                            let running = this.search_generation == generation
                                && this.magic.as_ref().is_some_and(|s| s.loading);
                            if running {
                                cx.notify();
                            }
                            running
                        })
                        .unwrap_or(false);
                    if !running {
                        break;
                    }
                }
            })
            .detach();
        }
        let (text, all_filters, limit) = match &command {
            Some(command) => (
                command.selection.text.clone(),
                command.selection.filters.clone(),
                // Deliberately *not* SEARCH_RESULT_LIMIT: the plan is
                // built from these rows, so truncation would become a
                // silent partial delete, and `magic::build`'s
                // too-many-to-review guard would never fire on the
                // truncated count. One past the cap is what arms it.
                filex::magic::MAX_PLAN_OPS + 1,
            ),
            None => {
                // Forced-on magic with no command yet: show nothing rather
                // than a normal search. The magic view renders its own
                // hint, and a filename search here would repopulate exactly
                // the list the mode replaces.
                if self.magic_mode == MagicMode::On {
                    self.results.clear();
                    self.search_selection.clear();
                    cx.notify();
                    return;
                }
                let parsed = filex::search_filter::parse_query(&self.query, now);
                // Natural-language phrases in the text the `key:value`
                // parse left over. Rule-based, and shown as removable chips
                // rather than applied invisibly.
                let expansion = filex::phrases::expand(&parsed.text, now);
                let text = expansion.text.clone();
                if text.is_empty() && parsed.filters.is_empty() && expansion.is_empty() {
                    self.results.clear();
                    // Leave the browse selection intact; only the search's
                    // own selection goes away with the results.
                    self.search_selection.clear();
                    cx.notify();
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

        // Resolve tag membership before daemon ranking and truncation.
        let mut tags_required = Vec::new();
        let mut index_filters = Vec::new();
        for filter in all_filters {
            match filter {
                Filter::Tag(name) => tags_required.push(name),
                other if !index_filters.contains(&other) => index_filters.push(other),
                _ => {}
            }
        }

        let Some(client) = self.service.clone() else {
            self.results.clear();
            self.search_hits.clear();
            self.notice = Some("Search unavailable — reconnecting to filex-indexd".into());
            if let Some(state) = &mut self.magic {
                state.loading = false;
                state.error = Some("Search is reconnecting. Try again shortly.".into());
            }
            cx.notify();
            return;
        };
        let store = self.tags.clone();
        let cancel = self.search_cancel.clone();
        let command_query = command.is_some();
        let cwd = self.cwd.clone();
        let dirs = self.user_dirs.clone();
        let scope = match self.search_scope {
            SearchScope::Anywhere => None,
            SearchScope::CurrentDir => Some(self.cwd.clone()),
        };
        let client_id = self.search_client_id;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let allowed = if tags_required.is_empty() {
                        None
                    } else {
                        Some(store.paths_with_all_tags(&tags_required))
                    };
                    let query = filex::daemon::ipc::Query {
                        text,
                        filters: index_filters,
                        scope,
                        allowed,
                        limit: limit.min(1000),
                        offset: 0,
                        fuzzy: !command_query,
                        client: client_id,
                        request: generation,
                        epoch_hint: None,
                    };
                    if command_query {
                        let mut hits = Vec::new();
                        let mut epoch = 0;
                        client.stream_matches(query, &cancel, |batch| {
                            epoch = batch.epoch;
                            progress
                                .scanned
                                .store(batch.scanned, std::sync::atomic::Ordering::Relaxed);
                            hits.extend(batch.hits);
                            progress
                                .matched
                                .store(hits.len() as u64, std::sync::atomic::Ordering::Relaxed);
                            hits.len() <= filex::magic::MAX_PLAN_OPS
                        })?;
                        hits.truncate(filex::magic::MAX_PLAN_OPS + 1);
                        // Preview uses the index snapshot. Identity checks still run
                        // before the batch and immediately before each operation.
                        let paths = hits.iter().map(|hit| hit.path.clone()).collect::<Vec<_>>();
                        let plan = command.as_ref().map(|command| {
                            filex::magic::build(
                                command,
                                &paths,
                                &filex::magic::PlanContext {
                                    cwd: &cwd,
                                    dirs: &dirs,
                                },
                            )
                        });
                        Ok::<_, anyhow::Error>((hits, epoch, false, None, false, plan))
                    } else {
                        let page = client.search(query.clone(), &cancel)?;
                        Ok((
                            page.hits,
                            page.epoch,
                            page.more,
                            Some(query),
                            page.partial,
                            None,
                        ))
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                if this.search_generation != generation {
                    return;
                }
                match result {
                    Ok((hits, epoch, more, query, partial, plan)) => {
                        this.search_page_query = query.map(|mut q| {
                            q.epoch_hint = Some(epoch);
                            q
                        });
                        this.notice = partial
                            .then(|| "Search limit reached; refine your query or scope".into());
                        this.search_more = more;
                        this.search_hits =
                            hits.iter().map(|h| (h.path.clone(), h.clone())).collect();
                        this.results = hits
                            .into_iter()
                            .map(|hit| SearchRow {
                                name: hit.name.into(),
                                path_label: hit.path.display().to_string().into(),
                                is_dir: hit.is_dir,
                                target: hit.path,
                            })
                            .collect();
                        if let (Some(state), Some(plan)) = (&mut this.magic, plan) {
                            state.install_plan(plan);
                        } else {
                            this.select_first_result();
                            this.refresh_preview(cx);
                        }
                    }
                    Err(e) => {
                        this.results.clear();
                        this.search_hits.clear();
                        this.notice = Some(e.to_string().into());
                        if let Some(state) = &mut this.magic {
                            state.loading = false;
                            state.error = Some(e.to_string());
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn load_more_results(&mut self, cx: &mut Context<Self>) {
        if self.search_paging || !self.search_more {
            return;
        }
        let (Some(client), Some(mut query)) =
            (self.service.clone(), self.search_page_query.clone())
        else {
            return;
        };
        query.offset = self.results.len();
        let generation = self.search_generation;
        let cancel = self.search_cancel.clone();
        self.search_paging = true;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { client.search(query, &cancel) })
                .await;
            this.update(cx, |this, cx| {
                if this.search_generation != generation {
                    return;
                }
                this.search_paging = false;
                match result {
                    Ok(page) => {
                        this.search_more = page.more && !page.hits.is_empty();
                        for hit in page.hits {
                            if this.search_hits.contains_key(&hit.path) {
                                continue;
                            }
                            this.results.push(SearchRow {
                                name: hit.name.clone().into(),
                                path_label: hit.path.display().to_string().into(),
                                is_dir: hit.is_dir,
                                target: hit.path.clone(),
                            });
                            this.search_hits.insert(hit.path.clone(), hit);
                        }
                        if page.partial {
                            this.notice =
                                Some("Search limit reached; refine your query or scope".into());
                        } else {
                            this.notice = None;
                        }
                    }
                    Err(e) => {
                        this.notice = Some(e.to_string().into());
                        this.update_search(cx);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
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
        self.service.is_some()
            && (!self.daemon_status.building
                || self.daemon_status.roots.iter().any(|r| r.files > 0))
    }
    pub(super) fn index_status_text(&self) -> SharedString {
        if self.service.is_none() {
            return "Search unavailable — browsing is available".into();
        }
        if let Some(error) = &self.daemon_status.error {
            return error.clone().into();
        }
        if self.daemon_status.building {
            return "Building index".into();
        }
        let files: u64 = self.daemon_status.roots.iter().map(|r| r.files).sum();
        format!(
            "{} files indexed{}",
            files,
            if self.search_more {
                " · more matches available"
            } else {
                ""
            }
        )
        .into()
    }
}

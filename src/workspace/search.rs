//! Search and Magic: query updates, running searches, scope, and the
//! Magic plan (build / toggle / confirm).

use super::*;

impl Workspace {
    /// Move focus into the search box (the `/` shortcut and the search
    /// affordance). Selecting the existing text means the next keystroke
    /// replaces a stale query rather than appending to it.
    pub(super) fn focus_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let handle = self.search_input.focus_handle(cx);
        window.focus(&handle, cx);
        self.search_input
            .update(cx, |input, cx| input.select_all_text(cx));
    }

    /// Run a `tag:NAME` search (clicking a sidebar tag), focusing the
    /// search field so it can be refined.
    pub(super) fn search_tag(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.search_input.focus_handle(cx), cx);
        self.search_input.update(cx, |input, cx| {
            input.set_text(format!("tag:{name}"), cx);
        });
    }

    /// Remove one recognized filter token from the query (clicking its
    /// chip) by rewriting the search input's text.
    pub(super) fn remove_filter_token(&mut self, token: &str, cx: &mut Context<Self>) {
        let rewritten = filex::search::filter::without_token(&self.query, token);
        self.search_input
            .update(cx, |input, cx| input.set_text(rewritten, cx));
    }

    /// Apply the current query's phrases as filters even though file
    /// names matched it as typed — the suggestion chips' action. Holds
    /// for this exact query only; editing it starts fresh.
    pub(super) fn use_phrases_as_filters(&mut self, cx: &mut Context<Self>) {
        self.phrases_forced = Some(self.query.clone());
        self.update_search(cx);
    }

    /// Remove an inferred natural-language phrase (clicking its chip) by
    /// stripping the words that produced it.
    pub(super) fn remove_phrase(&mut self, source: &str, cx: &mut Context<Self>) {
        let rewritten = filex::search::phrases::without_phrase(&self.query, source);
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
                    this.sync_folder_icons(&applied, false, cx);
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
        window.focus(&self.search_input.focus_handle(cx), cx);
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
        let (text, all_filters, limit, literal) = match &command {
            Some(command) => (
                command.selection.text.clone(),
                command.selection.filters.clone(),
                // Deliberately *not* SEARCH_RESULT_LIMIT: the plan is
                // built from these rows, so truncation would become a
                // silent partial delete, and `magic::build`'s
                // too-many-to-review guard would never fire on the
                // truncated count. One past the cap is what arms it.
                filex::magic::MAX_PLAN_OPS + 1,
                None,
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
                let parsed = filex::search::filter::parse_query(&self.query, now);
                // Natural-language phrases in the text the `key:value`
                // parse left over. Rule-based, and shown as removable chips
                // rather than applied invisibly.
                let expansion = filex::search::phrases::expand(&parsed.text, now);
                let text = expansion.text.clone();
                if text.is_empty() && parsed.filters.is_empty() && expansion.is_empty() {
                    self.results.clear();
                    // Leave the browse selection intact; only the search's
                    // own selection goes away with the results.
                    self.search_selection.clear();
                    cx.notify();
                    return;
                }
                // The words read as a plain filename, tried first unless
                // the user already chose the filters for this query. Tags
                // are the same either way: phrases never add one.
                let literal = (!expansion.is_empty()
                    && self.phrases_forced.as_deref() != Some(self.query.as_str()))
                .then(|| {
                    let filters = parsed
                        .filters
                        .iter()
                        .filter(|f| !matches!(f, Filter::Tag(_)))
                        .cloned()
                        .collect::<Vec<_>>();
                    (parsed.text.clone(), filters)
                });
                let filters = parsed
                    .filters
                    .into_iter()
                    .chain(expansion.filters())
                    .collect::<Vec<_>>();
                (text, filters, SEARCH_RESULT_LIMIT, literal)
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
        // Same query, scope and phrase choice as the rows on screen means
        // this is a background refresh (index epoch moved, a file op
        // landed), not a new search: refetch every loaded row and leave
        // the scroll position and selection where the user put them.
        let key = ResultsKey {
            query: self.query.clone(),
            scope: scope.clone(),
            phrases_forced: self.phrases_forced.as_deref() == Some(self.query.as_str()),
        };
        let refresh = !command_query && self.results_key.as_ref() == Some(&key);
        let limit = if refresh {
            limit.max(self.results.len())
        } else {
            limit
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
                    let mut query = filex::daemon::ipc::Query {
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
                        Ok::<_, anyhow::Error>((hits, epoch, false, None, false, plan, false))
                    } else {
                        let literal_won = match literal {
                            Some(literal) => prefer_literal(&client, &mut query, literal, &cancel)?,
                            None => false,
                        };
                        let page = client.search(query.clone(), &cancel)?;
                        Ok((
                            page.hits,
                            page.epoch,
                            page.more,
                            Some(query),
                            page.partial,
                            None,
                            literal_won,
                        ))
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                if this.search_generation != generation {
                    return;
                }
                match result {
                    Ok((hits, epoch, more, query, partial, plan, literal_won)) => {
                        this.phrases_literal = literal_won.then(|| this.query.clone());
                        let kept = refresh.then(|| this.selected_result_paths());
                        this.results_key = Some(key);
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
                        } else if let Some((selected, lead)) = kept {
                            this.reselect_results(selected, lead);
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
        // A refresh may have refetched every loaded row in one request;
        // pages after it go back to the normal size.
        query.limit = SEARCH_RESULT_LIMIT;
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
            state.checked.fill(checked);
            cx.notify();
        }
    }

    /// New results select the first hit (Spotlight-style), so Enter
    /// immediately opens the top match.
    /// The selected result rows' paths, and the lead's — what survives a
    /// background refresh, since indices don't.
    fn selected_result_paths(&self) -> (Vec<PathBuf>, Option<PathBuf>) {
        let path = |ix: usize| self.results.get(ix).map(|row| row.target.clone());
        let selected = self.search_selection.iter().filter_map(path).collect();
        (selected, self.search_selection.lead().and_then(path))
    }

    fn reselect_results(&mut self, selected: Vec<PathBuf>, lead: Option<PathBuf>) {
        self.search_selection = selection_for_paths(&self.results, &selected, lead.as_ref());
    }

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

/// When a query both expanded into filters and appears as typed inside a
/// real file name, trust the name: "the music man" with a `The Music Man.mp4` on
/// disk is that file, not "audio files named 'the man'". A one-hit,
/// literal-only probe decides; on a hit `query` becomes the plain filename
/// search and the phrases are offered as chips instead. Costs one extra
/// daemon round trip, and only for queries that expanded.
fn prefer_literal(
    client: &filex::daemon::ipc::Client,
    query: &mut filex::daemon::ipc::Query,
    (text, filters): (String, Vec<Filter>),
    cancel: &std::sync::atomic::AtomicBool,
) -> anyhow::Result<bool> {
    let probe = filex::daemon::ipc::Query {
        text: text.clone(),
        filters: filters.clone(),
        limit: 1,
        fuzzy: false,
        ..query.clone()
    };
    if client.search(probe, cancel)?.hits.is_empty() {
        return Ok(false);
    }
    query.text = text;
    query.filters = filters;
    Ok(true)
}

/// Point a selection back at the same files after a refresh replaced the
/// rows. Files no longer in the results drop out; the lead is toggled last
/// so it stays the keyboard cursor.
fn selection_for_paths(
    rows: &[SearchRow],
    selected: &[PathBuf],
    lead: Option<&PathBuf>,
) -> Selection {
    let index: std::collections::HashMap<&PathBuf, usize> = rows
        .iter()
        .enumerate()
        .map(|(ix, row)| (&row.target, ix))
        .collect();
    let mut kept = Selection::default();
    for path in selected.iter().filter(|p| Some(*p) != lead) {
        if let Some(&ix) = index.get(path) {
            kept.toggle(ix);
        }
    }
    if let Some(&ix) = lead.and_then(|p| index.get(p)) {
        kept.toggle(ix);
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(names: &[&str]) -> Vec<SearchRow> {
        names
            .iter()
            .map(|name| SearchRow {
                name: (*name).to_string().into(),
                path_label: format!("/t/{name}").into(),
                is_dir: false,
                target: PathBuf::from(format!("/t/{name}")),
            })
            .collect()
    }

    #[test]
    fn a_refresh_keeps_the_selection_on_the_same_files() {
        // Regression: every index epoch change re-ran the search and
        // reselected row 0, which also scrolled the list to the top.
        let before = rows(&["a", "b", "c", "d"]);
        let after = rows(&["new", "a", "b", "c", "d"]);
        let selected = vec![before[1].target.clone(), before[3].target.clone()];
        let kept = selection_for_paths(&after, &selected, Some(&before[3].target));
        assert_eq!(kept.iter().collect::<Vec<_>>(), [2, 4]);
        assert_eq!(kept.lead(), Some(4));
    }

    #[test]
    fn files_gone_from_the_results_drop_out_of_the_selection() {
        let before = rows(&["a", "b"]);
        let after = rows(&["a"]);
        let selected = vec![before[0].target.clone(), before[1].target.clone()];
        let kept = selection_for_paths(&after, &selected, Some(&before[1].target));
        assert_eq!(kept.iter().collect::<Vec<_>>(), [0]);
    }
}

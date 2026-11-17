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
    /// Compress and tag plans have no ops and hand off to their own jobs.
    ///
    /// Conflicts follow the plan's [`ConflictPolicy`] (keep both unless
    /// the user chose otherwise) rather than prompting per file. A plan is
    /// reviewed as a whole; stopping to ask about file 40 of 200 would be
    /// worse than uniform predictability.
    ///
    /// [`ConflictPolicy`]: filex::magic::ConflictPolicy
    pub(super) fn confirm_magic(&mut self, cx: &mut Context<Self>) {
        use filex::magic::{Action, ConflictPolicy};
        if self.magic.as_ref().is_none_or(|s| s.selection_count() == 0) {
            return;
        }
        // Applied from onboarding's live Magic step: drop into the app first
        // so the job, any conflict dialog and undo are visible.
        self.hand_off_onboarding(cx);
        let Some(state) = self.magic.as_ref() else {
            return;
        };
        let Some(Ok(plan)) = &state.outcome else {
            return;
        };
        let verb = state.command.verb;
        let local = state.local;
        match &state.command.action {
            action @ (Action::Tag(_) | Action::Untag(_)) => {
                let (targets, action) = (state.selected_targets(), action.clone());
                self.clear_search(cx);
                self.spawn_tag_batch(targets, action, cx);
                return;
            }
            Action::Compress => {
                let (targets, archive) = (state.selected_targets(), plan.archive.clone());
                if !local && !self.validate_search_paths(&targets, cx) {
                    return;
                }
                self.clear_search(cx);
                self.spawn_compress(targets, archive, cx);
                return;
            }
            _ => {}
        }
        let ops = state.selected_ops();
        if ops.is_empty() {
            return;
        }
        let creates = plan.creates.clone();
        // Only a move or copy has a destination the user was asked about.
        let policy = match state.command.action {
            Action::Move(_) | Action::Copy(_) => self.magic_conflicts,
            _ => ConflictPolicy::KeepBoth,
        };
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
            paths: ops
                .iter()
                .flat_map(|op| [Some(op.source().to_path_buf()), op.destination()])
                .flatten()
                .collect(),
        });
        self.spawn_job_ticker(job_id, cx);
        // The card's work is done; clearing the query also drops the card
        // and returns the user to where they were.
        self.clear_search(cx);
        cx.notify();

        let tags = self.tags.clone();
        cx.spawn(async move |this, cx| {
            let (applied, done, failure) = cx
                .background_executor()
                .spawn({
                    let progress = progress.clone();
                    async move {
                        apply_plan(
                            ops,
                            creates,
                            policy,
                            (!local).then_some(&identities),
                            |done| migrate_tags(&tags, done),
                            &progress,
                        )
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                this.jobs.retain(|job| job.id != job_id);
                if !applied.is_empty() {
                    this.notice =
                        Some(format!("{} {done} {}", verb.past_tense(), plural_items(done)).into());
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
        self.sync_magic_placeholder(cx);
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
        // Turning Magic off on a `>` query would be undone by the next
        // keystroke; the `>` goes with it.
        if self.magic_mode == MagicMode::Off && self.query.starts_with('>') {
            let text = self.magic_text().to_owned();
            self.search_input
                .update(cx, |input, cx| input.set_text(text, cx));
        }
        self.sync_magic_placeholder(cx);
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
        // Dropped below the field so the menu never covers its trigger.
        // Under the trigger, its checkmark gutter left of the label, so the
        // checked choice's text lines up with "Anywhere" above it.
        // ponytail: offset from the click, which lands mid-label; anchor to
        // the trigger's laid-out bounds if clicks at its edges look off.
        let below = gpui::point(position.x - px(70.), position.y + px(22.));
        self.scope_menu = Some(Self::clamped_menu_position(below, 88., window));
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
        // On also reads paraphrases and typos (`magic::understand`); Auto
        // stays strict, since it acts on text nobody marked as a command.
        let (intent, read_as) = match self.magic_mode {
            MagicMode::Off => (None, None),
            MagicMode::On => filex::magic::understand(self.magic_text(), now)
                .map_or((None, None), |read| (Some(read.intent), read.read_as)),
            MagicMode::Auto => (
                filex::magic::parse_with_gate(&self.query, now, true)
                    .map(filex::magic::Intent::Plan),
                None,
            ),
        };
        // Offered only where a slip could explain an empty result: a
        // sentence that didn't read, or a plan that may match nothing.
        self.magic_did_you_mean = (self.magic_mode == MagicMode::On
            && read_as.is_none()
            && matches!(intent, None | Some(filex::magic::Intent::Plan(_))))
        .then(|| filex::magic::did_you_mean(self.magic_text(), now))
        .flatten();
        // A command's card already names it in the app's words.
        self.magic_read_as =
            read_as.filter(|_| !matches!(intent, Some(filex::magic::Intent::App(_))));
        self.magic_cursor = 0;
        self.magic_instant = None;
        self.magic_suggestions = match &intent {
            None if self.magic_mode == MagicMode::On => filex::magic::suggest(self.magic_text()),
            // A command brings its neighbours in the table: "dark mode"
            // lists the other themes under it.
            Some(filex::magic::Intent::App(command)) => {
                let label = super::magic::app_label(command);
                let family = label.rsplit(' ').next().unwrap_or_default();
                filex::magic::suggest(family)
                    .into_iter()
                    .filter(|related| related.label != label)
                    .collect()
            }
            _ => Vec::new(),
        };
        let command = match intent {
            Some(filex::magic::Intent::Plan(command)) => Some(command),
            // Going somewhere, an app command, a question: no plan and
            // no result rows — it runs on Enter or answers itself.
            Some(instant) => {
                self.magic = None;
                self.results.clear();
                self.search_selection.clear();
                self.begin_instant(instant, generation, cx);
                cx.notify();
                return;
            }
            None => None,
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
            _ => {
                // A new command starts from the safe answer to "that name
                // is taken", not whatever the last plan was told.
                self.magic_conflicts = Default::default();
                MagicState::new(&self.query, command.clone())
            }
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
        // The selection, or a folder being organized: nothing to search.
        if let Some(command) = &command
            && (command.selection.picked || command.verb == filex::magic::Verb::Organize)
        {
            self.spawn_local_plan(command.clone(), generation, cx);
            return;
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
        let conflicts = self.magic_conflicts;
        let scope = match self.search_scope {
            SearchScope::Anywhere => None,
            // This PC holds every drive, so its "current dir" is everywhere.
            SearchScope::CurrentDir if self.at_this_pc() => None,
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
        let issued = std::time::Instant::now();
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
                    if let Some(command) = &command {
                        let ctx = filex::magic::PlanContext {
                            cwd: &cwd,
                            dirs: &dirs,
                            conflicts,
                        };
                        // "… from Downloads": that folder is the scope. One
                        // that doesn't resolve is the plan's answer, not a
                        // reason to search everywhere instead.
                        if let Some(within) = &command.selection.within {
                            match filex::magic::resolve_destination(within, &ctx) {
                                Ok(folder) => query.scope = Some(folder),
                                Err(err) => {
                                    return Ok((
                                        Vec::new(),
                                        0,
                                        false,
                                        None,
                                        false,
                                        Some(Err(err)),
                                        false,
                                        Default::default(),
                                    ));
                                }
                            }
                        }
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
                        let plan = Some(filex::magic::build(command, &paths, &ctx));
                        // One stat per file, here off the UI thread; capped
                        // by MAX_PLAN_OPS like the plan itself.
                        let meta = paths
                            .iter()
                            .filter_map(|path| {
                                let meta = std::fs::symlink_metadata(path).ok()?;
                                Some((path.clone(), (meta.len(), meta.modified().ok())))
                            })
                            .collect::<std::collections::HashMap<_, _>>();
                        Ok::<_, anyhow::Error>((hits, epoch, false, None, false, plan, false, meta))
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
                            Default::default(),
                        ))
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                if this.search_generation != generation {
                    return;
                }
                match result {
                    Ok((mut hits, epoch, more, query, partial, plan, literal_won, meta)) => {
                        // ponytail: re-ranks the first page only; later
                        // pages append in the index's order. Move the
                        // locality prior into the daemon's merge if the
                        // seam ever shows.
                        if this.magic.is_none() {
                            let cwd = this.cwd.clone();
                            hits.sort_by_key(|hit| (hit.tier, locality(&hit.path, &cwd)));
                        }
                        this.phrases_literal = literal_won.then(|| this.query.clone());
                        let kept = refresh.then(|| this.selected_result_paths());
                        this.results_key = Some(key);
                        let took = issued.elapsed();
                        tracing::debug!(
                            chars = this.query.chars().count(),
                            hits = hits.len(),
                            ms = took.as_secs_f64() * 1000.,
                            "search answered"
                        );
                        record_latency(&mut this.search_latency, &this.query, took);
                        this.search_page_query = query.map(|mut q| {
                            q.epoch_hint = Some(epoch);
                            q
                        });
                        this.notice = partial
                            .then(|| "Search limit reached; refine your query or scope".into());
                        this.search_more = more;
                        this.search_hits =
                            hits.iter().map(|h| (h.path.clone(), h.clone())).collect();
                        let rows: Vec<SearchRow> = hits
                            .into_iter()
                            .filter(|hit| this.picker_shows(&hit.name, hit.is_dir))
                            .map(|hit| SearchRow {
                                name: hit.name.into(),
                                path_label: this.folder_label(&hit.path),
                                is_dir: hit.is_dir,
                                target: hit.path,
                            })
                            .collect();
                        let paths = rows.iter().map(|row| row.target.clone()).collect();
                        this.results = rows;
                        this.search_meta.clear();
                        this.stat_results(paths, generation, cx);
                        if let (Some(state), Some(plan)) = (&mut this.magic, plan) {
                            state.meta = meta;
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
                            if this.search_hits.contains_key(&hit.path)
                                || !this.picker_shows(&hit.name, hit.is_dir)
                            {
                                continue;
                            }
                            this.results.push(SearchRow {
                                name: hit.name.clone().into(),
                                path_label: this.folder_label(&hit.path),
                                is_dir: hit.is_dir,
                                target: hit.path.clone(),
                            });
                            this.search_hits.insert(hit.path.clone(), hit);
                        }
                        let fresh = this
                            .results
                            .iter()
                            .filter(|row| !this.search_meta.contains_key(&row.target))
                            .map(|row| row.target.clone())
                            .collect();
                        this.stat_results(fresh, generation, cx);
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

    /// The folder a hit lives in, as its row's second line: the name is
    /// already on the first, and home is written `~`.
    fn folder_label(&self, path: &Path) -> SharedString {
        tilde(
            display_path(path.parent().unwrap_or(path)),
            self.user_dirs.get("home"),
        )
        .into()
    }

    /// Read size and modified time for `paths` on the background
    /// executor (one stat each; a page is at most 1000), then fill the
    /// Modified and Size columns. Dropped if the search moved on.
    fn stat_results(&mut self, paths: Vec<PathBuf>, generation: u64, cx: &mut Context<Self>) {
        if paths.is_empty() {
            return;
        }
        cx.spawn(async move |this, cx| {
            let meta: Vec<_> = cx
                .background_executor()
                .spawn(async move {
                    paths
                        .into_iter()
                        .filter_map(|path| {
                            let meta = std::fs::metadata(&path).ok()?;
                            Some((path, (meta.len(), meta.modified().ok())))
                        })
                        .collect()
                })
                .await;
            this.update(cx, |this, cx| {
                if this.search_generation == generation {
                    this.search_meta.extend(meta);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn any_root_ready(&self) -> bool {
        self.service.is_some()
            && (!self.daemon_status.building
                || self.daemon_status.roots.iter().any(|r| r.files > 0))
    }
    /// The status bar's index dot: attention when search is unavailable
    /// or the index reported a problem, accent while building, else ready.
    /// Skipped (unreadable) entries are expected, not a problem: still
    /// ready.
    pub(super) fn index_status_color(&self, theme: &Theme) -> gpui::Rgba {
        if self.service.is_none() || self.daemon_status.error.is_some() {
            theme.warn
        } else if self.daemon_status.building {
            theme.accent
        } else {
            theme.success
        }
    }

    pub(super) fn index_status_text(&self) -> SharedString {
        if self.service.is_none() {
            return "Search unavailable — browsing is available".into();
        }
        // The raw error is developer text; it goes in the hover tip.
        if self.daemon_status.error.is_some() {
            return "Index incomplete".into();
        }
        if self.daemon_status.building {
            return "Building index".into();
        }
        let files: u64 = self.daemon_status.roots.iter().map(|r| r.files).sum();
        ui::status_bar::index_summary(files, self.daemon_status.skipped, self.search_more).into()
    }

    /// Hover text for the index status: what "N skipped" means.
    pub(super) fn index_status_tip(&self) -> Option<SharedString> {
        if let Some(error) = &self.daemon_status.error {
            return Some(error.clone().into());
        }
        self.service.as_ref()?;
        ui::status_bar::skipped_explanation(self.daemon_status.skipped).map(Into::into)
    }
}

/// Where a hit sits relative to the open folder, for ordering hits of
/// equal match quality: inside it first, then the rest of the disk, with
/// anything under a hidden folder (caches, tool state) after its visible
/// peers. Lower sorts first.
fn locality(path: &Path, cwd: &Path) -> u8 {
    let hidden = path
        .parent()
        .into_iter()
        .flat_map(Path::components)
        .any(|part| part.as_os_str().to_string_lossy().starts_with('.'));
    match (path.starts_with(cwd), hidden) {
        (true, false) => 0,
        (false, false) => 1,
        (true, true) => 2,
        (false, true) => 3,
    }
}

/// Run a reviewed plan's ops in order. Returns what was applied (for the
/// undo journal), how many of the plan's own ops that was — the journal
/// also holds the folders made and files replaced along the way — and why
/// it stopped, if it did. Blocking.
///
/// `identities` are the index hits a search-built plan came from: each
/// file is checked against its hit before anything runs and again right
/// before its op, so a file swapped since the preview is never touched.
/// A plan built from the selection has none; its ops fail on their own if
/// a file is gone.
fn apply_plan(
    ops: Vec<FileOp>,
    creates: Vec<PathBuf>,
    policy: filex::magic::ConflictPolicy,
    identities: Option<&std::collections::HashMap<PathBuf, filex::daemon::ipc::Hit>>,
    // Told of each applied op (tag migration, index hints) before it is
    // journaled.
    mut on_applied: impl FnMut(&mut ops::AppliedOp),
    progress: &ops::OpProgress,
) -> (Vec<ops::AppliedOp>, usize, Option<String>) {
    use filex::magic::ConflictPolicy;
    let verify = |op: &FileOp| match identities {
        Some(identities) => identities
            .get(op.source())
            .ok_or_else(|| anyhow::anyhow!("search target expired"))
            .and_then(filex::daemon::query::verify),
        None => Ok(()),
    };
    let mut applied = Vec::new();
    let mut done = 0;
    for op in &ops {
        if let Err(e) = verify(op) {
            return (
                applied,
                done,
                Some(format!("Plan changed: {e}. Search again before applying.")),
            );
        }
    }
    // Only the folders a checked op actually lands in.
    for dir in creates {
        let needed = ops.iter().any(|op| {
            op.destination()
                .is_some_and(|dest| dest.parent() == Some(&dir))
        });
        if !needed || dir.is_dir() {
            continue;
        }
        match ops::apply(&FileOp::CreateFolder { path: dir }) {
            Ok(made) => applied.push(made),
            Err(err) => return (applied, done, Some(format!("Plan stopped: {err:#}"))),
        }
    }
    for mut op in ops {
        if let Err(e) = verify(&op) {
            return (
                applied,
                done,
                Some(format!("Stopped because a search target changed: {e}")),
            );
        }
        if let Some(dest) = op.destination()
            && std::fs::symlink_metadata(&dest).is_ok()
        {
            let cleared = match policy {
                ConflictPolicy::Skip => continue,
                // To the trash, in the same undo batch: undoing the plan
                // puts the replaced file back.
                ConflictPolicy::Replace => {
                    ops::apply(&FileOp::Delete { path: dest }).map(|mut gone| {
                        on_applied(&mut gone);
                        applied.push(gone);
                    })
                }
                ConflictPolicy::KeepBoth => ops::next_free_name(&dest).map(|free| {
                    op = op.clone().with_destination(free);
                }),
            };
            if let Err(err) = cleared {
                return (applied, done, Some(format!("Plan stopped: {err:#}")));
            }
        }
        match ops::apply_with_progress(&op, progress) {
            Ok(mut finished) => {
                on_applied(&mut finished);
                applied.push(finished);
                done += 1;
            }
            Err(err) => return (applied, done, Some(format!("Plan stopped: {err:#}"))),
        }
    }
    (applied, done, None)
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
mod apply_plan_tests {
    use super::*;
    use filex::magic::ConflictPolicy;

    fn run(
        ops: Vec<FileOp>,
        creates: Vec<PathBuf>,
        policy: ConflictPolicy,
    ) -> (Vec<ops::AppliedOp>, usize, Option<String>) {
        apply_plan(
            ops,
            creates,
            policy,
            None,
            |_| {},
            &ops::OpProgress::default(),
        )
    }

    fn mv(from: &Path, to: &Path) -> FileOp {
        FileOp::Move {
            from: from.to_path_buf(),
            to: to.to_path_buf(),
        }
    }

    #[test]
    fn folders_are_made_only_for_ops_that_run_and_undo_with_the_batch() {
        let dir = tempfile::tempdir().unwrap();
        let (a, images, docs) = (
            dir.path().join("a.png"),
            dir.path().join("Images"),
            dir.path().join("Documents"),
        );
        std::fs::write(&a, b"x").unwrap();
        // The plan would make two folders; the op into Documents was
        // unchecked, so only Images is needed.
        let (applied, done, failure) = run(
            vec![mv(&a, &images.join("a.png"))],
            vec![images.clone(), docs.clone()],
            ConflictPolicy::KeepBoth,
        );
        assert_eq!(failure, None);
        assert_eq!(done, 1, "the folder is not one of the plan's ops");
        assert!(images.join("a.png").is_file());
        assert!(!docs.exists());
        // Folder first in the journal, so undo moves the file back out
        // before the folder goes.
        assert!(matches!(applied[0], ops::AppliedOp::Created { .. }));
        assert!(matches!(applied[1], ops::AppliedOp::Moved { .. }));
    }

    #[test]
    fn a_name_taken_since_the_preview_follows_the_policy() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("dest");
        std::fs::create_dir(&dest).unwrap();
        std::fs::write(dest.join("a.txt"), b"old").unwrap();
        let a = dir.path().join("a.txt");
        std::fs::write(&a, b"new").unwrap();

        // Skip: nothing moves, nothing is overwritten.
        let (applied, done, failure) = run(
            vec![mv(&a, &dest.join("a.txt"))],
            vec![],
            ConflictPolicy::Skip,
        );
        assert_eq!((applied.len(), done, failure), (0, 0, None));
        assert!(a.is_file());

        // Keep both: the mover takes the next free name.
        let (_, done, failure) = run(
            vec![mv(&a, &dest.join("a.txt"))],
            vec![],
            ConflictPolicy::KeepBoth,
        );
        assert_eq!((done, failure), (1, None));
        assert_eq!(std::fs::read(dest.join("a.txt")).unwrap(), b"old");
        assert_eq!(std::fs::read(dest.join("a 2.txt")).unwrap(), b"new");
    }

    #[test]
    fn a_failed_op_stops_the_plan_and_reports_what_already_ran() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.txt");
        std::fs::write(&a, b"x").unwrap();
        let (applied, done, failure) = run(
            vec![
                mv(&a, &dir.path().join("b.txt")),
                mv(&dir.path().join("gone.txt"), &dir.path().join("c.txt")),
            ],
            vec![],
            ConflictPolicy::KeepBoth,
        );
        assert_eq!((applied.len(), done), (1, 1));
        assert!(failure.is_some_and(|reason| reason.starts_with("Plan stopped")));
    }
}

#[cfg(test)]
mod tests {
    /// Regression: a `cargo.js` in a tool cache outranked the open
    /// project's own `Cargo.toml`.
    #[test]
    fn hits_in_the_open_folder_lead_and_hidden_ones_trail() {
        let cwd = std::path::Path::new("/u/me/proj");
        let rank = |path: &str| super::locality(std::path::Path::new(path), cwd);
        assert!(rank("/u/me/proj/Cargo.toml") < rank("/u/me/other/Cargo.toml"));
        assert!(rank("/u/me/other/Cargo.toml") < rank("/u/me/proj/.cache/Cargo.toml"));
        assert!(rank("/u/me/proj/.cache/Cargo.toml") < rank("/u/me/.bun/cargo.js"));
        // A dot-file is not hidden *placement*; only its folders count.
        assert_eq!(rank("/u/me/proj/.gitignore"), 0);
    }

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

/// Keep the first timing taken for `query`. Live index refreshes re-run the
/// same query in the background; recording those would make the number
/// change while the user isn't typing.
fn record_latency(
    slot: &mut Option<(String, std::time::Duration)>,
    query: &str,
    took: std::time::Duration,
) {
    if slot.as_ref().is_none_or(|(timed, _)| timed != query) {
        *slot = Some((query.to_owned(), took));
    }
}

#[cfg(test)]
mod latency_tests {
    use super::record_latency;
    use std::time::Duration;

    #[test]
    fn a_background_refresh_does_not_change_the_shown_latency() {
        let mut slot = None;
        record_latency(&mut slot, "resume pdf", Duration::from_millis(2));
        // Same query again: an index refresh, not the user.
        record_latency(&mut slot, "resume pdf", Duration::from_millis(9));
        assert_eq!(slot, Some(("resume pdf".into(), Duration::from_millis(2))));
        // A new query is timed afresh.
        record_latency(&mut slot, "resume", Duration::from_millis(3));
        assert_eq!(slot, Some(("resume".into(), Duration::from_millis(3))));
    }
}

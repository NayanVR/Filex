//! Magic beyond file plans: going places, app commands, questions, the
//! suggestion list, and the plans that are built without a search (the
//! selection, a folder being organized). The grammar is
//! [`filex::magic`]; this maps what it read onto what the app already
//! does. Plan review and execution live in `search.rs`.

use super::*;
use filex::magic::{Action, AppCommand, Go, Intent, Question, SuggestionAction, Verb};

/// Rows an answer lists under its headline.
const ANSWER_ROWS: usize = 50;

/// Neighbouring commands listed under one that is about to run.
const RELATED_ROWS: usize = 8;

/// What the empty bar says while it takes commands.
const MAGIC_PLACEHOLDER: &str = "Type a command or a question";

impl Workspace {
    /// The query as Magic reads it: without the `>` that asked for it.
    pub(super) fn magic_text(&self) -> &str {
        self.query.trim_start_matches('>').trim()
    }

    /// The query rewritten to make the folder a refused plan couldn't
    /// find — what the "Create … and continue" button and its shortcut
    /// run. `None` unless that is why there is no plan.
    pub(super) fn missing_destination_fix(&self) -> Option<(String, String)> {
        let state = self.magic.as_ref()?;
        let Some(Err(filex::magic::PlanError::UnknownDestination(name))) = &state.outcome else {
            return None;
        };
        Some((name.clone(), with_new_folder(self.magic_text(), name)?))
    }

    pub(super) fn create_missing_destination(&mut self, cx: &mut Context<Self>) {
        if let Some((_, rewritten)) = self.missing_destination_fix() {
            self.magic_mode = MagicMode::On;
            self.search_input
                .update(cx, |input, cx| input.set_text(rewritten, cx));
        }
    }

    /// Make the empty bar say what it takes right now.
    // ponytail: leaving Magic restores the default placeholder, not one
    // onboarding had set; thread the previous text through if that shows.
    pub(super) fn sync_magic_placeholder(&self, cx: &mut Context<Self>) {
        let magic = self.magic_mode == MagicMode::On;
        self.search_input.update(cx, |input, cx| {
            if magic {
                input.set_placeholder(MAGIC_PLACEHOLDER, cx);
            } else if input.placeholder() == MAGIC_PLACEHOLDER {
                input.set_placeholder(search_input::DEFAULT_PLACEHOLDER, cx);
            }
        });
    }

    /// Hand `text` to the ordinary filename search — what "find X" asks
    /// for, and the way out when Magic can't read a sentence.
    fn search_names_for(&mut self, text: String, cx: &mut Context<Self>) {
        self.magic_mode = MagicMode::Off;
        self.sync_magic_placeholder(cx);
        self.search_input
            .update(cx, |input, cx| input.set_text(text, cx));
        // Same text as before (minus nothing): the input fires no change.
        self.update_search(cx);
    }

    /// Replace the query with `text`, still as a Magic request.
    pub(super) fn ask_instead(&mut self, text: String, cx: &mut Context<Self>) {
        self.magic_mode = MagicMode::On;
        self.search_input
            .update(cx, |input, cx| input.set_text(text, cx));
    }

    /// Drop out of the search/magic overlay *now*, so whatever runs next
    /// sees the browse view. [`clear_search`](Self::clear_search) alone
    /// leaves `query` set until the input's change event lands.
    fn leave_magic(&mut self, cx: &mut Context<Self>) {
        self.search_generation += 1;
        self.query.clear();
        self.magic = None;
        self.magic_instant = None;
        self.magic_suggestions.clear();
        self.results.clear();
        self.search_selection.clear();
        self.clear_search(cx);
        cx.notify();
    }

    /// The files selected in the browse list — what "selected" means in a
    /// command, since typing the command left that selection alone.
    fn picked_paths(&self) -> Vec<PathBuf> {
        self.selection
            .iter()
            .filter_map(|ix| self.entries.get(ix))
            .map(|entry| entry.path.clone())
            .collect()
    }

    /// Start showing a non-plan intent: look up where a `go` leads, or
    /// work out an answer, off the UI thread.
    pub(super) fn begin_instant(
        &mut self,
        intent: Intent,
        generation: u64,
        cx: &mut Context<Self>,
    ) {
        match intent {
            Intent::Plan(_) => {}
            Intent::App(command) => self.magic_instant = Some(MagicInstant::App(command)),
            Intent::Find(what) => self.magic_instant = Some(MagicInstant::Find(what)),
            Intent::Go(go) => {
                self.magic_instant = Some(MagicInstant::Go {
                    go: go.clone(),
                    target: None,
                });
                self.spawn_go(go, generation, cx);
            }
            Intent::Ask(question) => {
                let progress = std::sync::Arc::new(MagicProgress::default());
                self.magic_instant = Some(MagicInstant::Ask {
                    answer: None,
                    progress: progress.clone(),
                });
                self.spawn_answer(question, progress, generation, cx);
            }
        }
    }

    /// Where the bar is looking, for the scope control and the pane's
    /// subtitle. A command that names its own place ("from Downloads",
    /// the selection, this folder) says that, not the dropdown's setting
    /// it is overriding.
    pub(super) fn scope_label(&self) -> SharedString {
        let Some(state) = &self.magic else {
            return self.search_scope.label().into();
        };
        let selection = &state.command.selection;
        if selection.picked {
            "Selection".into()
        } else if let Some(within) = &selection.within {
            within.name.clone().into()
        } else if state.local {
            "This folder".into()
        } else {
            self.search_scope.label().into()
        }
    }

    /// The scope a search-backed Magic request starts from.
    fn magic_scope(&self) -> Option<PathBuf> {
        match self.search_scope {
            SearchScope::CurrentDir if !self.at_this_pc() => Some(self.cwd.clone()),
            _ => None,
        }
    }

    fn spawn_go(&mut self, go: Go, generation: u64, cx: &mut Context<Self>) {
        let cwd = self.cwd.clone();
        let dirs = self.user_dirs.clone();
        // Places the user already has a name for, before asking the index.
        let known: Vec<PathBuf> = self
            .settings
            .read(cx)
            .settings()
            .favorites
            .iter()
            .cloned()
            .chain(self.drives.iter().map(|drive| drive.path.clone()))
            .collect();
        let drive_names: Vec<(String, PathBuf)> = self
            .drives
            .iter()
            .map(|drive| (drive.name.clone(), drive.path.clone()))
            .collect();
        let client = self.service.clone();
        let client_id = self.search_client_id;
        let cancel = self.search_cancel.clone();
        cx.spawn(async move |this, cx| {
            let target = cx
                .background_executor()
                .spawn(async move {
                    resolve_go(&go.name, &cwd, &dirs, &known, &drive_names, |name| {
                        let client = client.as_ref()?;
                        let query = filex::daemon::ipc::Query {
                            text: name.to_owned(),
                            filters: vec![Filter::Kind(FileKind::Directory)],
                            limit: 1,
                            fuzzy: true,
                            client: client_id,
                            request: generation,
                            ..Default::default()
                        };
                        let page = client.search(query, &cancel).ok()?;
                        page.hits.into_iter().next().map(|hit| hit.path)
                    })
                    .map(|path| {
                        // Stat here, not on Enter: a slow share must not
                        // stall the UI thread.
                        let is_dir = path.is_dir();
                        (path, is_dir)
                    })
                })
                .await;
            this.update(cx, |this, cx| {
                if this.search_generation == generation
                    && let Some(MagicInstant::Go { target: slot, .. }) = &mut this.magic_instant
                {
                    *slot = Some(target);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn spawn_answer(
        &mut self,
        question: Question,
        progress: std::sync::Arc<MagicProgress>,
        generation: u64,
        cx: &mut Context<Self>,
    ) {
        let Some(client) = self.service.clone() else {
            if let Some(MagicInstant::Ask { answer, .. }) = &mut self.magic_instant {
                *answer = Some(Err("Search is reconnecting. Try again shortly.".into()));
            }
            return;
        };
        let request = AnswerRequest {
            question,
            cwd: self.cwd.clone(),
            dirs: self.user_dirs.clone(),
            scope: self.magic_scope(),
            picked: self.picked_paths(),
            client_id: self.search_client_id,
            generation,
        };
        let cancel = self.search_cancel.clone();
        let store = self.tags.clone();
        // Repaint while it counts, as the plan loader does.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(150))
                    .await;
                let running = this
                    .update(cx, |this, cx| {
                        let running = this.search_generation == generation
                            && matches!(
                                this.magic_instant,
                                Some(MagicInstant::Ask { answer: None, .. })
                            );
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
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    answer(&request, &client, &store, &progress, &cancel)
                        .map_err(|err| format!("{err:#}"))
                })
                .await;
            this.update(cx, |this, cx| {
                if this.search_generation == generation
                    && let Some(MagicInstant::Ask { answer, .. }) = &mut this.magic_instant
                {
                    *answer = Some(result);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Build a plan that needs no search: the selection, or the files of
    /// a folder being organized. The listing and the plan's own
    /// `read_dir`s run off the UI thread.
    pub(super) fn spawn_local_plan(
        &mut self,
        command: filex::magic::Command,
        generation: u64,
        cx: &mut Context<Self>,
    ) {
        let picked = command.selection.picked.then(|| self.picked_paths());
        let cwd = self.cwd.clone();
        let dirs = self.user_dirs.clone();
        let conflicts = self.magic_conflicts;
        if let Some(state) = &mut self.magic {
            state.local = true;
        }
        self.results.clear();
        self.search_hits.clear();
        cx.spawn(async move |this, cx| {
            let plan = cx
                .background_executor()
                .spawn(async move {
                    let ctx = filex::magic::PlanContext {
                        cwd: &cwd,
                        dirs: &dirs,
                        conflicts,
                    };
                    let files = match picked {
                        Some(files) if files.is_empty() => {
                            return Err(filex::magic::PlanError::NothingSelected);
                        }
                        Some(files) => files,
                        // Organize: one folder's own files, not its subtree —
                        // sorting a tree would flatten it.
                        None => {
                            let folder = match &command.selection.within {
                                Some(within) => filex::magic::resolve_destination(within, &ctx)?,
                                None if cwd.is_absolute() => cwd.clone(),
                                None => {
                                    return Err(filex::magic::PlanError::NoMatches);
                                }
                            };
                            std::fs::read_dir(&folder)
                                .into_iter()
                                .flatten()
                                .flatten()
                                .filter(|entry| entry.file_type().is_ok_and(|kind| !kind.is_dir()))
                                .map(|entry| entry.path())
                                .take(filex::magic::MAX_PLAN_OPS + 1)
                                .collect()
                        }
                    };
                    filex::magic::build(&command, &files, &ctx)
                })
                .await;
            this.update(cx, |this, cx| {
                if this.search_generation == generation
                    && let Some(state) = &mut this.magic
                {
                    state.install_plan(plan);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Enter in magic view. Things that change no files run; a file plan
    /// does not — that takes the apply shortcut or a click, so the key
    /// someone is already leaning on can't trash a batch.
    /// For a plan refused over a missing folder: the folders to offer
    /// instead (near misses, else the ones right here) and their heading.
    pub(super) fn refusal_folders(&self, missing: &str) -> (Vec<String>, String) {
        let close = self.folders_like(missing);
        if !close.is_empty() {
            return (close, "Did you mean".into());
        }
        let here = self
            .entries
            .iter()
            .filter(|entry| entry.is_dir)
            .take(3)
            .map(|entry| entry.name.to_string())
            .collect();
        (
            here,
            format!("Folders in “{}”", filex::listing::location_label(&self.cwd)),
        )
    }

    /// Rerun the refused sentence with `folder` in place of the missing
    /// one.
    pub(super) fn use_folder_instead(
        &mut self,
        missing: &str,
        folder: &str,
        cx: &mut Context<Self>,
    ) {
        let fixed = replace_last_ci(&self.query, missing, folder);
        self.ask_instead(fixed, cx);
    }

    pub(super) fn magic_enter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // A refusal over a missing folder: Return takes the first folder
        // offered (⌘↩ creates the missing one instead).
        if let Some(Some(Err(filex::magic::PlanError::UnknownDestination(missing)))) =
            self.magic.as_ref().map(|state| state.outcome.clone())
        {
            if let Some(folder) = self.refusal_folders(&missing).0.first().cloned() {
                self.use_folder_instead(&missing, &folder, cx);
            }
            return;
        }
        // The highlight moved off the command onto one of its neighbours.
        if self.magic_cursor > 0
            && !matches!(self.magic_instant, Some(MagicInstant::Ask { .. }) | None)
        {
            self.run_suggestion(self.magic_cursor - 1, window, cx);
            return;
        }
        // An answer: Enter shows the highlighted file where it lives.
        if let Some(MagicInstant::Ask {
            answer: Some(Ok(answer)),
            ..
        }) = &self.magic_instant
        {
            if let Some((path, _)) = answer.rows.get(self.magic_cursor).cloned() {
                self.reveal(path, cx);
            }
            return;
        }
        match self.magic_instant.take() {
            Some(MagicInstant::App(command)) => self.run_app_command(command, window, cx),
            Some(MagicInstant::Go {
                go,
                target: Some(Ok((path, is_dir))),
            }) => {
                self.leave_magic(cx);
                self.end_search_session(ev::SearchOutcome::MagicGo, None);
                fa::track(ev::MagicInstant {
                    kind: ev::MagicInstantKind::Go,
                    command: None,
                    new_tab: Some(go.new_tab),
                });
                if is_dir {
                    if go.new_tab {
                        self.open_tab(path, cx);
                    } else {
                        self.navigate(path, NavVia::Magic, cx);
                    }
                } else {
                    self.open_target(path, false, false, cx);
                }
            }
            Some(MagicInstant::Find(what)) => self.search_names_for(what, cx),
            // No such place: the name is still worth a search.
            Some(MagicInstant::Go {
                go,
                target: Some(Err(_)),
            }) => self.search_names_for(go.name, cx),
            // Still resolving, or an answer: nothing to run.
            other @ Some(_) => self.magic_instant = other,
            None if self.magic.is_none() && self.suggestion_takes_enter(self.magic_cursor) => {
                self.run_suggestion(self.magic_cursor, window, cx);
            }
            // Not understood: Enter does what the bar always did.
            None if self.magic.is_none()
                && self.magic_suggestions.is_empty()
                && !self.magic_text().is_empty() =>
            {
                self.search_names_for(self.magic_text().to_owned(), cx);
            }
            None => {}
        }
    }

    /// Whether Enter may choose suggestion `ix`. A command always; a
    /// template only while it completes what is typed — Enter must not
    /// replace half a sentence of the user's own with an example.
    fn suggestion_takes_enter(&self, ix: usize) -> bool {
        match self.magic_suggestions.get(ix).map(|s| &s.action) {
            Some(SuggestionAction::Run(_)) => true,
            Some(SuggestionAction::Fill(text)) => completes(self.magic_text(), text),
            None => false,
        }
    }

    /// Choose suggestion `ix`: run a command, or put a template in the
    /// bar to be edited.
    pub(super) fn run_suggestion(
        &mut self,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(suggestion) = self.magic_suggestions.get(ix).cloned() else {
            return;
        };
        match suggestion.action {
            SuggestionAction::Run(command) => self.run_app_command(command, window, cx),
            SuggestionAction::Fill(text) => {
                self.magic_mode = MagicMode::On;
                window.focus(&self.search_input.focus_handle(cx), cx);
                self.search_input
                    .update(cx, |input, cx| input.set_text(text, cx));
            }
        }
    }

    /// The idle list top to bottom: suggestions (`Ok`, by index), with a
    /// label (`Err`) wherever the kind changes — but only while the whole
    /// vocabulary is showing. Once the user types, it is a plain list of
    /// what fits.
    fn suggestion_lines(&self) -> Vec<Result<usize, &'static str>> {
        let grouped = self.magic_text().is_empty();
        let mut lines = Vec::with_capacity(self.magic_suggestions.len() + 4);
        let mut last = "";
        for (ix, suggestion) in self.magic_suggestions.iter().enumerate() {
            let section = suggestion_section(suggestion);
            if grouped && section != last {
                lines.push(Err(section));
                last = section;
            }
            lines.push(Ok(ix));
        }
        lines
    }

    /// Move the suggestion highlight (up/down in the idle Magic pane).
    pub(super) fn move_magic_cursor(&mut self, delta: isize, cx: &mut Context<Self>) {
        // Whichever list is showing: an answer's files, a command and its
        // neighbours (the command itself is row 0), or the suggestions.
        let (len, line) = match &self.magic_instant {
            Some(MagicInstant::Ask {
                answer: Some(Ok(answer)),
                ..
            }) => (answer.rows.len(), None),
            Some(MagicInstant::Ask { .. }) => return,
            Some(_) => (self.magic_suggestions.len().min(RELATED_ROWS) + 1, None),
            None => (self.magic_suggestions.len(), Some(())),
        };
        if len == 0 {
            return;
        }
        self.magic_cursor = (self.magic_cursor as isize + delta).rem_euclid(len as isize) as usize;
        let line = line
            .and_then(|()| {
                self.suggestion_lines()
                    .iter()
                    .position(|line| *line == Ok(self.magic_cursor))
            })
            .unwrap_or(self.magic_cursor);
        self.magic_scroll
            .scroll_to_item(line, ScrollStrategy::Center);
        cx.notify();
    }

    /// Run an app command exactly as its shortcut, menu item or setting
    /// would, from the browse view.
    pub(super) fn run_app_command(
        &mut self,
        command: AppCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.leave_magic(cx);
        window.focus(&self.focus_handle, cx);
        self.end_search_session(ev::SearchOutcome::MagicCommand, None);
        fa::track(ev::MagicInstant {
            kind: ev::MagicInstantKind::App,
            command: Some(command.id()),
            new_tab: None,
        });
        match command {
            AppCommand::NewTab => {
                let cwd = self.cwd.clone();
                self.open_tab(cwd, cx);
            }
            // As the shortcut does: the last tab closes the window.
            AppCommand::CloseTab if self.tabs.len() <= 1 => window.remove_window(),
            AppCommand::CloseTab => self.close_tab(self.active_tab, cx),
            AppCommand::NextTab => self.cycle_tab(1, cx),
            AppCommand::PrevTab => self.cycle_tab(-1, cx),
            AppCommand::Back => self.go_back(cx),
            AppCommand::Forward => self.go_forward(cx),
            AppCommand::Up => self.go_up(cx),
            AppCommand::Refresh => self.reload_dir(cx),
            AppCommand::Undo => self.undo_last(cx),
            AppCommand::Settings => {
                if !self.settings_open {
                    self.toggle_settings(window, cx);
                }
            }
            AppCommand::Shortcuts => self.open_keyboard_settings(window, cx),
            AppCommand::Theme(theme) => self.set_setting(cx, |s| s.theme = theme),
            AppCommand::Density(density) => self.set_setting(cx, |s| s.density = density),
            AppCommand::HiddenFiles(show) => self.set_setting(cx, |s| {
                s.show_hidden_files = show.unwrap_or(!s.show_hidden_files);
            }),
            AppCommand::Sort(by) => self.set_setting(cx, |s| s.sort.by = by),
            AppCommand::SortAscending(ascending) => {
                self.set_setting(cx, |s| s.sort.ascending = ascending);
            }
            AppCommand::View(view) => self.set_setting(cx, |s| s.view = view),
            AppCommand::TogglePreview => self.toggle_preview(cx),
            AppCommand::NewFolder(name) => self.new_folder_named(name, cx),
            AppCommand::Connect(address) => self.open_connect(address, None, window, cx),
            AppCommand::Eject(name) => {
                let drive = self.drives.iter().find(|drive| {
                    drive.kind == DriveKind::Network && drive.name.eq_ignore_ascii_case(&name)
                });
                match drive.map(|drive| drive.path.clone()) {
                    Some(mount) => self.disconnect_drive(mount, cx),
                    None => {
                        self.notice = Some(format!("No network drive called “{name}”").into());
                    }
                }
            }
            AppCommand::Terminal => {
                if let Err(err) = open_terminal(&self.cwd) {
                    self.notice = Some(format!("couldn't open a terminal: {err}").into());
                }
            }
            AppCommand::CopyPath => self.copy_as_path_action(cx),
            AppCommand::Properties => self.properties_action(window, cx),
            AppCommand::Favorite(pin) => {
                let cwd = self.cwd.clone();
                if self.at_this_pc() {
                    self.notice = Some("Open a folder to pin it".into());
                } else if pin {
                    self.pin_favorite(cwd, cx);
                } else {
                    self.unpin_favorite(&cwd, cx);
                }
            }
            AppCommand::Feedback => self.open_feedback(window, cx),
            AppCommand::Quit => window.dispatch_action(Box::new(Quit), cx),
        }
        cx.notify();
    }

    fn set_setting(
        &mut self,
        cx: &mut Context<Self>,
        edit: impl FnOnce(&mut filex::settings::Settings),
    ) {
        self.settings.update(cx, |store, cx| store.update(cx, edit));
    }

    /// Apply a tag plan: add the tag to (or strip tags from) each file.
    /// Not in the undo journal, which records file operations; the
    /// opposite command is the undo.
    pub(super) fn spawn_tag_batch(
        &mut self,
        paths: Vec<PathBuf>,
        action: Action,
        cx: &mut Context<Self>,
    ) {
        let store = self.tags.clone();
        let verb = match action {
            Action::Tag(_) => Verb::Tag,
            _ => Verb::Untag,
        };
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    // ponytail: one store write per file (each persists the
                    // sidecar). Fine to the 1000-row plan cap; add a batch
                    // write to `TagStore` if plans ever grow past it.
                    let mut changed = 0usize;
                    for path in &paths {
                        let current = store.tags(path);
                        let next = retagged(&current, &action);
                        if next != current {
                            store.set_tags(path, &next)?;
                            changed += 1;
                        }
                    }
                    anyhow::Ok(changed)
                })
                .await;
            this.update(cx, |this, cx| {
                this.notice = Some(match result {
                    Ok(0) => "Nothing to change — the tags were already like that".into(),
                    Ok(n) => format!("{} {n} {}", verb.past_tense(), plural_items(n)).into(),
                    Err(err) => format!("{err:#}").into(),
                });
                this.refresh_sidebar_tags(cx);
                if let Some((path, _, _)) = this.lead_item() {
                    this.spawn_refresh_tags(path, cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // -- rendering ------------------------------------------------------

    /// The Magic pane with no plan in it: an instant intent's card, or
    /// the list of what the query could become.
    pub(super) fn render_magic_idle(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = *cx.theme();
        match &self.magic_instant {
            Some(MagicInstant::App(command)) => {
                let label = app_label(command);
                let hint = if self.is_current(command, cx) {
                    "Current"
                } else {
                    "Run"
                };
                self.instant_card(&theme, suggestion_icon(&label), label, hint, true, cx)
            }
            Some(MagicInstant::Go { go, target }) => match target {
                None => self.instant_card(
                    &theme,
                    "icons/folder-search.svg",
                    format!("Looking for “{}”…", go.name),
                    "",
                    false,
                    cx,
                ),
                Some(Ok((path, _))) => self.instant_card(
                    &theme,
                    "icons/folder.svg",
                    format!(
                        "{} {}",
                        if go.new_tab {
                            "Open in a new tab:"
                        } else {
                            "Go to"
                        },
                        tilde(display_path(path), self.user_dirs.get("home"))
                    ),
                    "Go",
                    true,
                    cx,
                ),
                Some(Err(reason)) => self.instant_card(
                    &theme,
                    "icons/folder-search.svg",
                    reason.clone(),
                    "Search file names instead",
                    true,
                    cx,
                ),
            },
            Some(MagicInstant::Find(what)) => self.instant_card(
                &theme,
                "icons/search.svg",
                format!("Search file names for “{what}”"),
                "Search",
                true,
                cx,
            ),
            Some(MagicInstant::Ask {
                answer, progress, ..
            }) => self.render_answer(&theme, answer.as_ref(), progress, cx),
            None => self.render_suggestions(&theme, cx),
        }
    }

    /// The pane's title, the line under it and the controls beside it,
    /// for every Magic state. The title *is* the thing — "Move 153
    /// items", "27.7 GB" — so the pane below spends no rows repeating it.
    pub(super) fn magic_header(
        &self,
        cx: &mut Context<Self>,
    ) -> (SharedString, SharedString, gpui::Div) {
        let theme = *cx.theme();
        let mut controls = div().flex().flex_none().items_center().gap_3();
        let Some(state) = &self.magic else {
            let (title, subtitle): (SharedString, SharedString) = match &self.magic_instant {
                Some(MagicInstant::Ask {
                    answer: Some(Ok(answer)),
                    ..
                }) => (answer.headline.clone().into(), answer.detail.clone().into()),
                Some(MagicInstant::Ask { .. }) => {
                    ("Magic".into(), "Only looking · nothing changes".into())
                }
                // The sentence, as read, is the title: "Turn on dark mode".
                Some(MagicInstant::App(command)) => (
                    sentence_case(self.magic_text()).into(),
                    // A setting says what it is now, so the rows below read
                    // as the choices against it.
                    match command {
                        AppCommand::Theme(_) => format!(
                            "Theme · currently {}",
                            match self.settings.read(cx).settings().theme {
                                filex::settings::ThemeMode::System if theme.is_dark() => {
                                    "System (Dark)"
                                }
                                filex::settings::ThemeMode::System => "System (Light)",
                                filex::settings::ThemeMode::Light => "Light",
                                filex::settings::ThemeMode::Dark => "Dark",
                                filex::settings::ThemeMode::Oled => "OLED",
                            }
                        )
                        .into(),
                        _ => "Command".into(),
                    },
                ),
                Some(MagicInstant::Go { .. }) => ("Magic".into(), "Place".into()),
                Some(_) => ("Magic".into(), "Search".into()),
                None if self.magic_text().is_empty() => (
                    "Magic".into(),
                    "Move, rename, tag and tidy files · go anywhere · change any setting · ask how big things are".into(),
                ),
                None if self.magic_suggestions.is_empty() => ("Magic".into(), "".into()),
                None => ("Magic".into(), "Keep typing, or pick one".into()),
            };
            return (title, subtitle, controls);
        };
        let verb = state.command.verb;
        let selection = &state.command.selection;
        let mut subtitle = if selection.picked {
            "The selected items".to_owned()
        } else if state.local {
            // Organize: one folder's own files, whatever it was called.
            "The files".to_owned()
        } else {
            format!("Matching “{}”", selection.source)
        };
        let Some(Ok(plan)) = &state.outcome else {
            if !state.local {
                subtitle = format!("{subtitle} · {}", self.scope_label());
            }
            // A refusal leads with its reason ("No folder called
            // “Nowhere”"); the sentence it refused is under it. Still
            // loading: the sentence, in the grammar's words.
            let sentence = self
                .magic_read_as
                .clone()
                .unwrap_or_else(|| self.magic_text().to_owned());
            if let Some(Err(err)) = &state.outcome {
                let (reason, advice) = super::plan_error_advice(err);
                let advice = if matches!(err, filex::magic::PlanError::UnknownDestination(_)) {
                    format!(
                        "Check the spelling, or create it in “{}”.",
                        filex::listing::location_label(&self.cwd)
                    )
                } else {
                    advice.to_owned()
                };
                return (
                    reason.into(),
                    format!("{advice} Nothing has changed.").into(),
                    controls,
                );
            }
            return (sentence_case(&sentence).into(), subtitle.into(), controls);
        };
        // A searched plan's match is the chip row under the title and its
        // scope is the field's chip: the subtitle says only what neither
        // does. One folder: say it here; there are no group headers.
        let searched = !selection.picked && !state.local;
        if let [(folder, _)] = state.layout.groups.as_slice() {
            subtitle = if searched {
                format!("In “{}”", leaf(folder))
            } else {
                format!("{subtitle} in “{}”", leaf(folder))
            };
        } else if searched {
            subtitle = format!("from {} folders", state.layout.groups.len());
        }
        // What the checked files weigh, when the search read their sizes.
        let bytes: u64 = plan
            .ops
            .iter()
            .zip(&state.checked)
            .filter(|(_, checked)| **checked)
            .filter_map(|(op, _)| state.meta.get(op.source()).map(|(size, _)| *size))
            .sum();
        if bytes > 0 {
            subtitle = if subtitle.is_empty() {
                format_size(bytes)
            } else {
                format!("{} · {subtitle}", format_size(bytes))
            };
        }
        // A subtitle opens with a capital, whatever piece came first.
        if let Some(first) = subtitle.get_mut(..1) {
            first.make_ascii_uppercase();
        }
        let count = state.selection_count();
        // Names already taken where the files are going: say so, and let
        // the whole plan answer it one way. A single row that should stay
        // behind is what its checkbox is for.
        if plan.conflicts > 0 {
            use filex::magic::ConflictPolicy;
            let mut choices = div()
                .flex()
                .items_center()
                .gap_0p5()
                .p_0p5()
                .rounded_lg()
                .bg(theme.field);
            for (id, label, policy) in [
                ("magic-keep-both", "Keep both", ConflictPolicy::KeepBoth),
                ("magic-skip", "Skip", ConflictPolicy::Skip),
                ("magic-replace", "Replace", ConflictPolicy::Replace),
            ] {
                choices = choices.child(
                    ui::magic_card::segment(&theme, id, label, self.magic_conflicts == policy)
                        // The one answer that loses a file says so.
                        .when(
                            policy == ConflictPolicy::Replace && self.magic_conflicts == policy,
                            |segment| segment.text_color(theme.danger),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if this.magic_conflicts != policy {
                                this.magic_conflicts = policy;
                                this.update_search(cx);
                            }
                        })),
                );
            }
            // The group rows count every renamed file; some were renamed
            // because two files in the batch share a name, not because the
            // destination had it. Say both, so the numbers add up.
            let renamed = plan
                .ops
                .iter()
                .filter(|op| {
                    matches!(op, FileOp::Move { from, to } | FileOp::Copy { from, to }
                        if from.file_name() != to.file_name())
                })
                .count();
            let name = |n: usize| if n == 1 { "file" } else { "files" };
            // What the chosen answer does, in the group rows' own terms.
            // Settled (Keep both, Skip) reads as a note; only Replace,
            // which loses files, is a warning.
            let (label, warn) = match self.magic_conflicts {
                // Short: it shares the title's line, and the title must
                // never be cut. The group rows say where each one is.
                ConflictPolicy::KeepBoth => (format!("{renamed} renamed to avoid clashes"), false),
                ConflictPolicy::Skip => (
                    format!(
                        "{} {} skipped, name taken",
                        plan.conflicts,
                        name(plan.conflicts)
                    ),
                    false,
                ),
                ConflictPolicy::Replace => (
                    format!(
                        "{} {} will be replaced",
                        plan.conflicts,
                        name(plan.conflicts)
                    ),
                    true,
                ),
            };
            controls = controls
                .child(if warn {
                    ui::magic_card::refusal(&theme, label)
                } else {
                    ui::magic_card::subtitle(&theme, label)
                })
                .child(choices);
        }
        controls = controls.child(
            ui::top_bar::toolbar_button(
                &theme,
                "magic-refresh",
                "icons/refresh-cw.svg",
                theme.text_dim,
            )
            .tooltip(ui::tooltip::text_tooltip("Refresh the preview", theme))
            .on_click(cx.listener(|this, _, _, cx| this.update_search(cx))),
        );
        let mut title = plan_action(verb, count);
        if let Some(dest) = &state.layout.dest {
            title = format!("{title} to {}", self.dest_name(dest));
        }
        (title.into(), subtitle.into(), controls)
    }

    /// How a paraphrase or typo fix was read, as the strip under the
    /// title — shown before anything runs.
    fn read_as_strip(&self, theme: &Theme) -> Option<gpui::Div> {
        let read_as = self.magic_read_as.as_ref()?;
        // An answer whose title already says the same words.
        if let Some(MagicInstant::Ask {
            answer: Some(Ok(answer)),
            ..
        }) = &self.magic_instant
            && answer.headline.eq_ignore_ascii_case(read_as)
        {
            return None;
        }
        Some(ui::magic_card::notes(theme).child(ui::magic_card::note(
            theme,
            "icons/sparkles.svg",
            format!("Read as: {read_as}"),
        )))
    }

    /// Whether running `command` would change nothing: the setting it
    /// picks is already the one in use. Said beside it, so "dark mode"
    /// shows which theme is on before anyone presses Enter.
    fn is_current(&self, command: &AppCommand, cx: &App) -> bool {
        let settings = self.settings.read(cx).settings();
        match command {
            AppCommand::Theme(mode) => settings.theme == *mode,
            AppCommand::Density(density) => settings.density == *density,
            AppCommand::View(view) => settings.view == *view,
            AppCommand::Sort(by) => settings.sort.by == *by,
            _ => false,
        }
    }

    /// The one thing the sentence means, as the single highlighted row
    /// of the list it would otherwise be picked from: Enter (or a click)
    /// runs it.
    fn instant_card(
        &self,
        theme: &Theme,
        icon: &'static str,
        title: impl Into<SharedString>,
        hint: &'static str,
        runnable: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let cursor = self.magic_cursor;
        let row = ui::magic_card::suggestion_row(
            theme,
            0,
            cursor == 0,
            icon,
            title,
            // Only the row Enter would run says what Enter does.
            if cursor == 0 { hint } else { "" },
            runnable && cursor == 0,
        )
        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
            this.magic_cursor = 0;
            this.magic_enter(window, cx);
        }));
        // Its neighbours in the command table ("dark mode" → the other
        // themes), an arrow key away — `run_search` fills these.
        let mut list = ui::magic_card::pane_list().child(row);
        if !self.magic_suggestions.is_empty() {
            list = list.child(ui::magic_card::section_row(theme, "Related"));
        }
        for (ix, related) in self.magic_suggestions.iter().enumerate().take(RELATED_ROWS) {
            let highlighted = cursor == ix + 1;
            let current = matches!(&related.action,
                filex::magic::SuggestionAction::Run(command) if self.is_current(command, cx));
            list = list.child(
                ui::magic_card::suggestion_row(
                    theme,
                    ix + 1,
                    highlighted,
                    suggestion_icon(related.label),
                    related.label,
                    if current { "Current" } else { "" },
                    highlighted,
                )
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.run_suggestion(ix, window, cx);
                })),
            );
        }
        // A column that ends in the hint, so it sits at the pane's foot.
        let list = list.child(div().flex_1());
        let keys = if !runnable {
            "esc to close"
        } else if self.magic_suggestions.is_empty() {
            "↩ to run · esc to close"
        } else {
            "↑↓ to choose · ↩ to run · esc to close"
        };
        ui::magic_card::pane()
            .children(self.read_as_strip(theme))
            .child(list)
            .child(ui::magic_card::hint_bar(theme, keys))
            .into_any_element()
    }

    fn render_answer(
        &self,
        theme: &Theme,
        answer: Option<&Result<MagicAnswer, String>>,
        progress: &MagicProgress,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let answer = match answer {
            None => {
                let seen = progress.matched.load(std::sync::atomic::Ordering::Relaxed);
                return ui::magic_card::pane()
                    .child(
                        div()
                            .flex()
                            .flex_1()
                            .items_center()
                            .justify_center()
                            .gap_3()
                            .child(ui::icon::spinner(
                                "icons/loader-circle.svg",
                                theme.accent,
                                22.,
                                "magic-answering",
                            ))
                            .child(ui::magic_card::subtitle(
                                theme,
                                if seen == 0 {
                                    "Counting…".to_owned()
                                } else {
                                    format!("{seen} files so far…")
                                },
                            )),
                    )
                    .into_any_element();
            }
            Some(Err(reason)) => {
                return ui::magic_card::pane()
                    .child(ui::pane::message_state(
                        theme,
                        "icons/search.svg",
                        "Couldn’t answer that",
                        reason.clone(),
                    ))
                    .into_any_element();
            }
            Some(Ok(answer)) => answer,
        };
        let rows = answer.rows.clone();
        let largest = rows
            .iter()
            .filter_map(|row| row.1)
            .max()
            .unwrap_or(1)
            .max(1) as f32;
        let list = uniform_list(
            "magic-answer",
            rows.len(),
            cx.processor(move |this, range: Range<usize>, _window, cx| {
                let theme = *cx.theme();
                range
                    .filter_map(|ix| {
                        let (path, size) = rows.get(ix)?;
                        let target = path.clone();
                        Some(
                            ui::magic_card::answer_row(
                                &theme,
                                ix,
                                ix == this.magic_cursor,
                                path.file_name()
                                    .unwrap_or_default()
                                    .to_string_lossy()
                                    .into_owned(),
                                tilde(
                                    path.parent().map(display_path).unwrap_or_default(),
                                    this.user_dirs.get("home"),
                                ),
                                size.map(format_size).unwrap_or_default(),
                                size.map_or(0., |size| size as f32 / largest),
                            )
                            .on_click(cx.listener(
                                move |this, _: &ClickEvent, _, cx| this.reveal(target.clone(), cx),
                            )),
                        )
                    })
                    .collect()
            }),
        )
        .track_scroll(&self.magic_scroll)
        .pr(px(ui::scrollbar::GUTTER))
        .pb(px(24.))
        .with_decoration(
            ui::scrollbar::scrollbar(
                self.magic_scroll.clone(),
                self.magic_scrollbar.clone(),
                theme,
            )
            .outset(ui::scrollbar::GUTTER),
        )
        .flex_1();
        ui::magic_card::pane()
            // The number itself is the pane's title (`magic_header`).
            .children(self.read_as_strip(theme))
            .child(
                ui::magic_card::pane_list()
                    .child(ui::magic_card::answer_header(theme))
                    .child(list)
                    .child(ui::pane::bottom_fade(theme)),
            )
            .child(ui::magic_card::hint_bar(
                theme,
                "↑↓ to choose · ↩ to show in its folder · esc to close",
            ))
            .into_any_element()
    }

    /// "Did you mean …?" for a query a typo fix would make readable.
    /// A plan's destination as its title and button say it: a folder
    /// right in home by name ("Downloads"); a deeper one keeps its path
    /// so it can't be mistaken.
    pub(super) fn dest_name(&self, dest: &Path) -> String {
        let home = self.user_dirs.get("home");
        if dest.parent().is_some_and(|parent| Some(parent) == home) {
            leaf(dest)
        } else {
            tilde(short_path(dest), home)
        }
    }

    /// Folders a missing destination may have meant: this folder's own
    /// subfolders and the standard ones, closest spelling first.
    pub(super) fn folders_like(&self, name: &str) -> Vec<String> {
        const STANDARD: [&str; 7] = [
            "Desktop",
            "Documents",
            "Downloads",
            "Pictures",
            "Music",
            "Movies",
            "Videos",
        ];
        let limit = (name.len() / 3).max(2);
        let mut close: Vec<(usize, String)> = self
            .entries
            .iter()
            .filter(|entry| entry.is_dir)
            .map(|entry| entry.name.to_string())
            .chain(
                STANDARD
                    .iter()
                    .filter(|dir| self.user_dirs.get(&dir.to_lowercase()).is_some())
                    .map(|dir| dir.to_string()),
            )
            .filter_map(|folder| {
                let d = filex::magic::name_distance(&folder, name, limit);
                (d <= limit).then_some((d, folder))
            })
            .collect();
        close.sort();
        close.dedup_by(|a, b| a.1 == b.1);
        close
            .into_iter()
            .take(3)
            .map(|(_, folder)| folder)
            .collect()
    }

    pub(super) fn did_you_mean_button(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Stateful<gpui::Div>> {
        let fixed = self.magic_did_you_mean.clone()?;
        Some(
            ui::button::button(
                theme,
                "magic-did-you-mean",
                format!("Did you mean “{fixed}”?"),
                ui::button::Variant::Subtle,
                ui::button::Size::Regular,
            )
            .on_click(cx.listener(move |this, _, _, cx| this.ask_instead(fixed.clone(), cx))),
        )
    }

    /// What the query could become, highlighted row kept in view.
    fn render_suggestions(&self, theme: &Theme, cx: &mut Context<Self>) -> gpui::AnyElement {
        if self.magic_suggestions.is_empty() {
            let mut pane = ui::magic_card::pane().child(ui::pane::message_state(
                theme,
                "icons/sparkles.svg",
                "Not something Magic understands yet",
                "Press Enter to search file names for it, or clear the box to see what you can ask for.",
            ));
            if let Some(button) = self.did_you_mean_button(theme, cx) {
                pane = pane.child(ui::magic_card::pane_actions(theme).child(button));
            }
            return pane.into_any_element();
        }
        let lines = self.suggestion_lines();
        let list = uniform_list(
            "magic-suggestions",
            lines.len(),
            cx.processor(move |this, range: Range<usize>, _window, cx| {
                let theme = *cx.theme();
                range
                    .filter_map(|line| {
                        let ix = match *lines.get(line)? {
                            Ok(ix) => ix,
                            Err(label) => {
                                return Some(
                                    ui::magic_card::section_row(&theme, label).into_any_element(),
                                );
                            }
                        };
                        let suggestion = this.magic_suggestions.get(ix)?;
                        let highlighted = ix == this.magic_cursor;
                        Some(
                            ui::magic_card::suggestion_row(
                                &theme,
                                ix,
                                highlighted,
                                suggestion_icon(suggestion.label),
                                suggestion.label,
                                suggestion.hint,
                                highlighted && this.suggestion_takes_enter(ix),
                            )
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                this.run_suggestion(ix, window, cx);
                            }))
                            .into_any_element(),
                        )
                    })
                    .collect()
            }),
        )
        .track_scroll(&self.magic_scroll)
        .pr(px(ui::scrollbar::GUTTER))
        .pb(px(24.))
        .with_decoration(
            ui::scrollbar::scrollbar(
                self.magic_scroll.clone(),
                self.magic_scrollbar.clone(),
                theme,
            )
            .outset(ui::scrollbar::GUTTER),
        )
        .flex_1();
        ui::magic_card::pane()
            .child(
                ui::magic_card::pane_list()
                    .child(list)
                    .child(ui::pane::bottom_fade(theme)),
            )
            .child(ui::magic_card::hint_bar(
                theme,
                "↑↓ to choose · ↩ to use · esc to close",
            ))
            .into_any_element()
    }
}

/// Which part of the idle list a suggestion belongs under.
fn suggestion_section(suggestion: &filex::magic::Suggestion) -> &'static str {
    if matches!(suggestion.action, SuggestionAction::Run(_)) {
        return "Commands";
    }
    match suggestion.label.split(' ').next().unwrap_or_default() {
        "go" | "new" | "connect" => "Go and create",
        "how" | "largest" => "Ask",
        _ => "Files",
    }
}

/// An icon for a suggestion, from the word it starts with — so the list
/// can be scanned by kind instead of read.
fn suggestion_icon(label: &str) -> &'static str {
    let first = label.split(' ').next().unwrap_or_default();
    match first.to_ascii_lowercase().as_str() {
        "move" => "icons/arrow-right.svg",
        "copy" => "icons/copy.svg",
        "delete" => "icons/trash-2.svg",
        "rename" => "icons/pencil.svg",
        "zip" => "icons/archive.svg",
        "tag" => "icons/tag.svg",
        "organize" => "icons/layout-grid.svg",
        "go" | "open" => "icons/folder.svg",
        "how" | "largest" => "icons/info.svg",
        "new" => "icons/plus.svg",
        "connect" => "icons/server.svg",
        "settings" | "keyboard" => "icons/settings.svg",
        "dark" | "light" | "oled" | "system" => "icons/palette.svg",
        "sort" => "icons/arrow-up-down.svg",
        "list" | "grid" => "icons/list.svg",
        _ => "icons/sparkles.svg",
    }
}

/// Whether `template` begins with what is `typed` (ignoring case) — i.e.
/// choosing it finishes the user's sentence rather than replacing it.
fn completes(typed: &str, template: &str) -> bool {
    template
        .get(..typed.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(typed))
}

/// `current` after a tag plan's action.
fn retagged(current: &[Tag], action: &Action) -> Vec<Tag> {
    match action {
        Action::Tag(tag) => filex::tags::upsert_tag(current, None, tag.clone()),
        Action::Untag(Some(name)) => current
            .iter()
            .filter(|tag| !tag.name.eq_ignore_ascii_case(name))
            .cloned()
            .collect(),
        _ => Vec::new(),
    }
}

/// What a command is called: the suggestion table's name for it, or one
/// built from its argument.
pub(super) fn app_label(command: &AppCommand) -> String {
    if let Some(known) = filex::magic::suggest("")
        .into_iter()
        .find(|s| s.action == SuggestionAction::Run(command.clone()))
    {
        return known.label.to_owned();
    }
    match command {
        AppCommand::NewFolder(Some(name)) => format!("New folder “{name}”"),
        AppCommand::Connect(Some(address)) => format!("Connect to {address}"),
        AppCommand::Eject(name) => format!("Disconnect “{name}”"),
        other => format!("{other:?}"),
    }
}

/// Where a spelled place leads: a typed path, a folder here or a
/// well-known one, a favorite or drive by name, and only then whatever
/// folder the index ranks first for the name (`search`). Blocking: reads
/// the current folder and may ask the daemon.
fn resolve_go(
    name: &str,
    cwd: &Path,
    dirs: &filex::magic::UserDirs,
    known: &[PathBuf],
    drives: &[(String, PathBuf)],
    search: impl FnOnce(&str) -> Option<PathBuf>,
) -> Result<PathBuf, String> {
    if let Some(path) = filex::magic::typed_path(name, dirs) {
        return if path.exists() {
            Ok(path)
        } else {
            Err(format!("{} doesn’t exist", display_path(&path)))
        };
    }
    let ctx = filex::magic::PlanContext {
        cwd,
        dirs,
        conflicts: Default::default(),
    };
    let dest = filex::magic::Destination {
        name: name.to_owned(),
        create: false,
    };
    if let Ok(path) = filex::magic::resolve_destination(&dest, &ctx) {
        return Ok(path);
    }
    // "open report.pdf": a file right here opens.
    let here = cwd.join(name);
    if cwd.is_absolute() && here.exists() {
        return Ok(here);
    }
    let named = |path: &&PathBuf| {
        path.file_name()
            .is_some_and(|leaf| leaf.to_string_lossy().eq_ignore_ascii_case(name))
    };
    if let Some(path) = known.iter().find(named) {
        return Ok(path.clone());
    }
    if let Some((_, path)) = drives
        .iter()
        .find(|(drive, _)| drive.eq_ignore_ascii_case(name))
    {
        return Ok(path.clone());
    }
    search(name).ok_or_else(|| format!("No folder called “{name}”"))
}

struct AnswerRequest {
    question: Question,
    cwd: PathBuf,
    dirs: filex::magic::UserDirs,
    scope: Option<PathBuf>,
    picked: Vec<PathBuf>,
    client_id: u64,
    generation: u64,
}

/// Running totals for an answer: how many files, how many bytes, and the
/// largest seen. Sizes come from a stat per file — the index hit carries
/// none — so only [`Tally::measure`] pays for one.
#[derive(Default)]
struct Tally {
    count: u64,
    bytes: u64,
    rows: Vec<(PathBuf, Option<u64>)>,
}

impl Tally {
    fn count(&mut self, path: PathBuf) {
        self.count += 1;
        if self.rows.len() < ANSWER_ROWS {
            self.rows.push((path, None));
        }
    }

    fn measure(&mut self, path: PathBuf) {
        // A file that vanished since it was indexed counts for nothing.
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            return;
        };
        let size = size_on_disk(&meta);
        self.count += 1;
        self.bytes += size;
        self.rows.push((path, Some(size)));
        // Keep the heaviest; trimmed in bulk so this stays a sort per
        // few thousand files rather than a heap op per file.
        if self.rows.len() > ANSWER_ROWS * 40 {
            self.trim();
        }
    }

    fn trim(&mut self) {
        self.rows.sort_by_key(|row| std::cmp::Reverse(row.1));
        self.rows.truncate(ANSWER_ROWS);
    }
}

/// What a file takes up. A sparse file (a VM disk image) claims far more
/// than it holds — an 8 TB "file" on a 500 GB disk — so where the OS
/// says how much is allocated, that caps it.
fn size_on_disk(meta: &std::fs::Metadata) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        meta.len().min(meta.blocks() * 512)
    }
    // ponytail: Windows reports the logical size; GetCompressedFileSizeW
    // is the allocated one if sparse files show up there.
    #[cfg(not(unix))]
    meta.len()
}

/// Work out an answer. Blocking: streams matches from the index and, for
/// sizes, stats each one — cancelled with the query that asked.
fn answer(
    request: &AnswerRequest,
    client: &filex::daemon::ipc::Client,
    tags: &PlatformTags,
    progress: &MagicProgress,
    cancel: &std::sync::atomic::AtomicBool,
) -> anyhow::Result<MagicAnswer> {
    let selection = request.question.selection();
    let ctx = filex::magic::PlanContext {
        cwd: &request.cwd,
        dirs: &request.dirs,
        conflicts: Default::default(),
    };
    let what = if selection.source.is_empty() {
        "files"
    } else {
        &selection.source
    };
    let folder_named = |name: &str| {
        filex::magic::resolve_destination(
            &filex::magic::Destination {
                name: name.to_owned(),
                create: false,
            },
            &ctx,
        )
    };

    // What to look through: (scope, filename text, filters).
    let mut text = selection.text.clone();
    let mut filters = selection.filters.clone();
    let mut scope = request.scope.clone();
    let mut roots: Vec<PathBuf> = Vec::new();
    if selection.picked {
        anyhow::ensure!(!request.picked.is_empty(), "Nothing is selected");
        roots = request.picked.clone();
    } else if let Some(within) = &selection.within {
        scope = Some(folder_named(&within.name).map_err(|err| anyhow::anyhow!("{err}"))?);
    } else if matches!(request.question, Question::Size { folder: true, .. })
        && let Ok(folder) = folder_named(&selection.source)
    {
        // "how big is Downloads": the folder, not files described as
        // "downloads".
        scope = Some(folder);
        text.clear();
        filters.clear();
    }

    let stream = |scope: Option<PathBuf>,
                  filters: Vec<Filter>,
                  each: &mut dyn FnMut(filex::daemon::ipc::Hit) -> bool|
     -> anyhow::Result<()> {
        let mut tag_names = Vec::new();
        let mut index_filters = Vec::new();
        for filter in filters {
            match filter {
                Filter::Tag(name) => tag_names.push(name),
                other => index_filters.push(other),
            }
        }
        // Folders are left out of a count of files — unless folders are
        // what was asked about.
        let folders = index_filters.contains(&Filter::Kind(FileKind::Directory));
        let query = filex::daemon::ipc::Query {
            text: text.clone(),
            filters: index_filters,
            scope,
            allowed: (!tag_names.is_empty()).then(|| tags.paths_with_all_tags(&tag_names)),
            limit: 1000,
            fuzzy: false,
            client: request.client_id,
            request: request.generation,
            ..Default::default()
        };
        client.stream_matches(query, cancel, |batch| {
            batch
                .hits
                .into_iter()
                .all(|hit| (hit.is_dir && !folders) || each(hit))
        })
    };

    let mut tally = Tally::default();
    let bump = |tally: &Tally| {
        progress
            .matched
            .store(tally.count, std::sync::atomic::Ordering::Relaxed);
    };
    match &request.question {
        // About the selection itself: no search, just those items.
        Question::Count(_) if selection.picked => {
            roots.into_iter().for_each(|path| tally.count(path));
            Ok(MagicAnswer {
                headline: format!("{} selected", grouped(tally.count)),
                detail: String::new(),
                rows: tally.rows,
            })
        }
        Question::Largest { top, .. } if selection.picked => {
            roots.into_iter().for_each(|path| tally.measure(path));
            tally.trim();
            tally.rows.truncate(top.unwrap_or(ANSWER_ROWS));
            Ok(MagicAnswer {
                headline: "Largest in the selection".to_owned(),
                detail: String::new(),
                rows: tally.rows,
            })
        }
        Question::Count(_) => {
            stream(scope.clone(), filters, &mut |hit| {
                tally.count(hit.path);
                bump(&tally);
                true
            })?;
            // Sizes for the handful shown, not the whole set.
            for (path, size) in &mut tally.rows {
                *size = std::fs::symlink_metadata(path).ok().map(|meta| meta.len());
            }
            Ok(MagicAnswer {
                headline: format!("{} {}", grouped(tally.count), plural_files(tally.count)),
                detail: match &scope {
                    Some(folder) => format!("“{what}” in {}", display_path(folder)),
                    None => format!("“{what}”, everywhere"),
                },
                rows: tally.rows,
            })
        }
        Question::Size { .. } => {
            if roots.is_empty() {
                stream(scope.clone(), filters, &mut |hit| {
                    tally.measure(hit.path);
                    bump(&tally);
                    true
                })?;
            }
            for root in roots {
                if root.is_dir() {
                    stream(Some(root), Vec::new(), &mut |hit| {
                        tally.measure(hit.path);
                        bump(&tally);
                        true
                    })?;
                } else {
                    tally.measure(root);
                }
            }
            tally.trim();
            Ok(MagicAnswer {
                headline: format_size(tally.bytes),
                detail: format!(
                    "{} {} in {}",
                    grouped(tally.count),
                    plural_files(tally.count),
                    match (&scope, selection.picked) {
                        (_, true) => "the selection".to_owned(),
                        (Some(folder), _) if text.is_empty() && selection.filters.is_empty() =>
                            display_path(folder),
                        _ => format!("“{what}”"),
                    }
                ),
                rows: tally.rows,
            })
        }
        Question::Largest { top, .. } => {
            // The index can filter by size but not sort by it, so ask
            // for big files first and widen only until there are enough
            // — a full-disk "largest files" stats dozens of files, not
            // millions.
            let sized = filters.iter().any(|f| matches!(f, Filter::Size(_)));
            let floors: &[u64] = if sized {
                &[0]
            } else {
                &[1 << 30, 100 << 20, 10 << 20, 1 << 20, 0]
            };
            for &floor in floors {
                tally = Tally::default();
                let mut filters = filters.clone();
                if floor > 0 {
                    filters.push(Filter::Size(filex::search::filter::Bound::Ge(floor)));
                }
                stream(scope.clone(), filters, &mut |hit| {
                    tally.measure(hit.path);
                    bump(&tally);
                    true
                })?;
                if tally.count >= ANSWER_ROWS as u64 {
                    break;
                }
            }
            tally.trim();
            tally.rows.truncate(top.unwrap_or(ANSWER_ROWS));
            Ok(MagicAnswer {
                headline: format!("Largest {what}"),
                detail: if tally.rows.is_empty() {
                    "Nothing matched".to_owned()
                } else {
                    format!(
                        "These {} files take {} · {}",
                        tally.rows.len(),
                        format_size(tally.rows.iter().filter_map(|row| row.1).sum()),
                        match &scope {
                            Some(folder) => format!("in {}", display_path(folder)),
                            None => "Anywhere".to_owned(),
                        }
                    )
                },
                rows: tally.rows,
            })
        }
    }
}

fn plural_files(n: u64) -> &'static str {
    if n == 1 { "file" } else { "files" }
}

/// `1234567` → `1,234,567`.
fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (ix, digit) in digits.chars().enumerate() {
        if ix > 0 && (digits.len() - ix).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// `text` trimmed, with its first letter capitalised.
fn sentence_case(text: &str) -> String {
    let mut text = text.trim().to_owned();
    if let Some(first) = text.get_mut(..1) {
        first.make_ascii_uppercase();
    }
    text
}

/// What applying a plan does, as its title and its button: "Move 3 items".
/// A delete says where the files go — they are recoverable, and the word
/// "Delete" alone doesn't say so.
pub(super) fn plan_action(verb: filex::magic::Verb, count: usize) -> String {
    let items = plural_items(count);
    if verb == filex::magic::Verb::Delete {
        let bin = if cfg!(windows) {
            "the Recycle Bin"
        } else {
            "Trash"
        };
        format!("Move {count} {items} to {bin}")
    } else {
        format!("{} {count} {items}", verb.label())
    }
}

/// `text` with the last case-insensitive occurrence of `from` replaced by
/// `to` (unchanged when it doesn't occur).
fn replace_last_ci(text: &str, from: &str, to: &str) -> String {
    match text.to_lowercase().rfind(&from.to_lowercase()) {
        Some(at) if text.is_char_boundary(at) && text.is_char_boundary(at + from.len()) => {
            format!("{}{to}{}", &text[..at], &text[at + from.len()..])
        }
        _ => text.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enter_completes_a_template_but_never_replaces_other_words() {
        assert!(completes("", "go to Documents"));
        assert!(completes("Go to d", "go to Documents"));
        assert!(!completes("go to Desktop", "go to Documents"));
        assert!(!completes(
            "move invoices",
            "move pdfs from Downloads to Documents"
        ));
        // Longer than the template, or cut inside a multi-byte character.
        assert!(!completes("go to Documents now", "go to Documents"));
        assert!(!completes("é", "go"));
    }

    #[test]
    fn numbers_group_by_thousands() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(1_000), "1,000");
        assert_eq!(grouped(1_234_567), "1,234,567");
    }

    #[test]
    fn a_tally_keeps_only_the_heaviest_rows() {
        let dir = tempfile::tempdir().unwrap();
        let mut tally = Tally::default();
        for n in 0..(ANSWER_ROWS + 10) {
            let path = dir.path().join(format!("f{n}"));
            std::fs::write(&path, vec![0u8; n]).unwrap();
            tally.measure(path);
        }
        // A file that is gone adds nothing.
        tally.measure(dir.path().join("missing"));
        tally.trim();
        assert_eq!(tally.count, (ANSWER_ROWS + 10) as u64);
        assert_eq!(tally.rows.len(), ANSWER_ROWS);
        assert_eq!(tally.rows[0].1, Some((ANSWER_ROWS + 9) as u64));
        assert_eq!(tally.rows.last().unwrap().1, Some(10));
    }

    #[test]
    fn tag_plans_add_remove_one_or_clear() {
        let current = vec![Tag::new("Work"), Tag::colored("Hot", TagColor::Red)];
        assert_eq!(
            retagged(&current, &Action::Tag(Tag::new("Tax"))),
            vec![
                Tag::new("Work"),
                Tag::colored("Hot", TagColor::Red),
                Tag::new("Tax")
            ]
        );
        // Re-adding a name changes nothing but its colour.
        assert_eq!(retagged(&current, &Action::Tag(Tag::new("Work"))).len(), 2);
        assert_eq!(
            retagged(&current, &Action::Untag(Some("work".into()))),
            vec![Tag::colored("Hot", TagColor::Red)]
        );
        assert!(retagged(&current, &Action::Untag(None)).is_empty());
    }

    #[test]
    fn go_prefers_places_the_user_can_see_or_named_before_the_index() {
        let cwd = tempfile::tempdir().unwrap();
        let child = cwd.path().join("Projects");
        std::fs::create_dir(&child).unwrap();
        let dirs = filex::magic::UserDirs::default();
        let favorite = PathBuf::from("/srv/Media");
        let drives = [("Backup".to_owned(), PathBuf::from("/Volumes/Backup"))];
        let go = |name: &str, indexed: Option<&str>| {
            resolve_go(
                name,
                cwd.path(),
                &dirs,
                std::slice::from_ref(&favorite),
                &drives,
                |_| indexed.map(PathBuf::from),
            )
        };
        // A folder on screen wins even when the index has another.
        assert_eq!(go("projects", Some("/elsewhere/projects")), Ok(child));
        assert_eq!(go("media", None), Ok(favorite.clone()));
        assert_eq!(go("backup", None), Ok(PathBuf::from("/Volumes/Backup")));
        assert_eq!(
            go("filex", Some("/code/filex")),
            Ok(PathBuf::from("/code/filex"))
        );
        assert!(go("nowhere", None).is_err());
        // A file here is somewhere to go too (it opens).
        std::fs::write(cwd.path().join("notes.txt"), b"").unwrap();
        assert_eq!(go("notes.txt", None), Ok(cwd.path().join("notes.txt")));
        // A typed path is taken literally, never searched for.
        assert!(go("/definitely/not/here", Some("/code/filex")).is_err());
    }
}

#[cfg(test)]
mod replace_tests {
    #[test]
    fn the_misspelt_folder_is_swapped_for_the_chosen_one() {
        assert_eq!(
            super::replace_last_ci("move pdfs to Nowhere", "nowhere", "Notes"),
            "move pdfs to Notes"
        );
        assert_eq!(super::replace_last_ci("abc", "x", "y"), "abc");
    }
}

use std::ops::Range;
use std::path::{Path, PathBuf};

use futures::StreamExt as _;
use gpui::{
    App, Application, Bounds, ClickEvent, Context, FocusHandle, Focusable as _, KeyBinding,
    KeyDownEvent, ScrollStrategy, SharedString, TitlebarOptions, UniformListScrollHandle, Window,
    WindowBounds, WindowOptions, actions, div, prelude::*, px, rgb, size, uniform_list,
};

use filex::index::watcher::SharedIndex;
use filex::index::{LiveIndex, VolumeIndex, manager, start_live_index};
use filex::listing::{Entry, format_modified, format_size, path_segments, read_dir_sorted};
use filex::ops::{self, FileOp};
use filex::settings::SortBy;

mod settings_store;
mod thumbnails;
mod ui;
use filex::listing::FileKind;
use settings_store::{SettingsEvent, SettingsStore};
use thumbnails::ThumbnailState;
use ui::search_input::{self, SearchInput, SearchInputEvent};
use ui::theme::{ACCENT, BG, TEXT, TEXT_DIM, WARN};

actions!(filex, [Quit, CloseWindow, GoUp, ToggleSettings, RenameSelected, Undo]);

/// Open a file with the platform's default application. Detached — the
/// launched app owns its own lifetime.
fn open_with_default_app(path: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = std::process::Command::new("open");
        command.arg(path);
        command
    };
    #[cfg(target_os = "linux")]
    let mut command = {
        let mut command = std::process::Command::new("xdg-open");
        command.arg(path);
        command
    };
    #[cfg(target_os = "windows")]
    let mut command = {
        // `start` is a cmd builtin; the empty string is the window title
        // slot so paths with spaces aren't misparsed as a title.
        let mut command = std::process::Command::new("cmd");
        command.args(["/C", "start", ""]).arg(path);
        command
    };
    command.spawn().map(drop)
}

const SEARCH_RESULT_LIMIT: usize = 500;

fn read_index(index: &SharedIndex) -> std::sync::RwLockReadGuard<'_, VolumeIndex> {
    index.read().unwrap_or_else(std::sync::PoisonError::into_inner)
}

enum RootState {
    Building,
    Ready { live: LiveIndex, files: usize },
    Failed(SharedString),
}

/// One indexed root: its own LiveIndex (watcher + writer + snapshot).
struct RootSlot {
    /// Canonical path — the identity used to route async updates.
    path: PathBuf,
    label: SharedString,
    state: RootState,
}

impl RootSlot {
    fn new(path: PathBuf) -> Self {
        let label: SharedString = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string())
            .into();
        Self { path, label, state: RootState::Building }
    }

    fn ready_index(&self) -> Option<SharedIndex> {
        match &self.state {
            RootState::Ready { live, .. } => Some(live.index.clone()),
            _ => None,
        }
    }
}

/// A search hit prepared for display (paths pre-materialized off-thread).
struct SearchRow {
    name: SharedString,
    path_label: SharedString,
    target: PathBuf,
    is_dir: bool,
}

/// A rename-in-place in progress: which browse row is being edited and
/// the input that owns the edited text (the SearchInput element reused
/// as a transient editor, per docs/roadmap.md).
struct RenameState {
    ix: usize,
    input: gpui::Entity<SearchInput>,
    /// Watches for Dismissed (escape) to cancel.
    _subscription: gpui::Subscription,
}

struct Workspace {
    focus_handle: FocusHandle,
    cwd: PathBuf,
    entries: Vec<Entry>,
    load_error: Option<SharedString>,
    roots: Vec<RootSlot>,
    settings: gpui::Entity<SettingsStore>,
    _settings_subscription: gpui::Subscription,
    /// Transient user-facing message (e.g. why a root couldn't be added).
    notice: Option<SharedString>,
    #[cfg(target_os = "macos")]
    fda_missing: bool,
    /// Connection to filex-indexd, when the elevated service is running.
    /// Searches go over IPC and no local indexing happens.
    #[cfg(target_os = "windows")]
    service: Option<std::sync::Arc<filex::index::ipc::ServiceClient>>,
    #[cfg(target_os = "windows")]
    service_status: Vec<filex::index::ipc::RootStatus>,
    /// Mirror of the search input's content (the input entity owns it).
    query: String,
    search_input: gpui::Entity<SearchInput>,
    _search_input_subscription: gpui::Subscription,
    results: Vec<SearchRow>,
    search_generation: u64,
    /// Index into the active list (search results while searching,
    /// directory entries otherwise).
    selected: Option<usize>,
    /// The settings pane replaces the browse list while open (search
    /// still takes precedence, Spotlight-style).
    settings_open: bool,
    /// In-flight rename; `None` when no row is being edited.
    renaming: Option<RenameState>,
    /// Undo stack of completed file operations.
    journal: ops::Journal,
    browse_scroll: UniformListScrollHandle,
    results_scroll: UniformListScrollHandle,
    thumbnails: std::collections::HashMap<PathBuf, ThumbnailState>,
}

impl Workspace {
    /// Roots to index: from settings, defaulting to the home directory
    /// on a fresh install.
    fn configured_roots(&self, cx: &App) -> Vec<PathBuf> {
        let mut configured = self.settings.read(cx).settings().roots.clone();
        if configured.is_empty()
            && let Some(home) = std::env::home_dir()
        {
            configured.push(home);
        }
        configured
    }

    fn new(cx: &mut Context<Self>) -> Self {
        let cwd = std::env::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        let search_input = cx.new(SearchInput::new);
        let subscription = cx.subscribe(&search_input, |this, _input, event, cx| match event {
            SearchInputEvent::Changed(text) => {
                if this.query != *text {
                    this.query = text.clone();
                    this.update_search(cx);
                }
            }
            SearchInputEvent::BackspaceWhenEmpty => this.go_up(cx),
            SearchInputEvent::Dismissed => {} // escape just clears the query
        });
        let settings = cx.new(SettingsStore::new);
        // Settings changes re-derive everything visible that depends on
        // them (today: the hidden-file filter on the browse list).
        let settings_subscription =
            cx.subscribe(&settings, |this, _store, event, cx| match event {
                SettingsEvent::Changed => {
                    let cwd = this.cwd.clone();
                    this.load_dir(&cwd, cx);
                    cx.notify();
                }
            });
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            cwd: cwd.clone(),
            entries: Vec::new(),
            load_error: None,
            roots: Vec::new(),
            settings,
            _settings_subscription: settings_subscription,
            notice: None,
            #[cfg(target_os = "macos")]
            fda_missing: false,
            #[cfg(target_os = "windows")]
            service: None,
            #[cfg(target_os = "windows")]
            service_status: Vec::new(),
            query: String::new(),
            search_input,
            _search_input_subscription: subscription,
            results: Vec::new(),
            search_generation: 0,
            selected: None,
            settings_open: false,
            renaming: None,
            journal: ops::Journal::default(),
            browse_scroll: UniformListScrollHandle::new(),
            results_scroll: UniformListScrollHandle::new(),
            thumbnails: std::collections::HashMap::new(),
        };
        this.load_dir(&cwd, cx);
        // Windows probes for the elevated index service first and only
        // falls back to in-process indexing if it's absent; elsewhere
        // indexing is always in-process.
        #[cfg(target_os = "windows")]
        this.spawn_service_probe(cx);
        #[cfg(not(target_os = "windows"))]
        for path in this.configured_roots(cx) {
            this.add_root_slot(path, cx);
        }
        this.spawn_fda_check(cx);
        this
    }

    /// Probe for filex-indexd; on success run in service mode, otherwise
    /// start local indexing. Runs off-thread — the UI shows "indexing
    /// 0/0" briefly while probing.
    #[cfg(target_os = "windows")]
    fn spawn_service_probe(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let client = cx
                .background_executor()
                .spawn(async { filex::index::ipc::ServiceClient::try_connect().ok() })
                .await;
            this.update(cx, |this, cx| {
                match client {
                    Some(client) => {
                        this.service = Some(std::sync::Arc::new(client));
                        this.spawn_service_status_poll(cx);
                    }
                    None => {
                        for path in this.configured_roots(cx) {
                            this.add_root_slot(path, cx);
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Keep the service's root/file counts fresh; on IPC failure fall
    /// back to local indexing so search keeps working.
    #[cfg(target_os = "windows")]
    fn spawn_service_status_poll(&self, cx: &mut Context<Self>) {
        let Some(client) = self.service.clone() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            loop {
                let status = cx
                    .background_executor()
                    .spawn({
                        let client = client.clone();
                        async move { client.status() }
                    })
                    .await;
                let keep_polling = this.update(cx, |this, cx| match status {
                    Ok(status) => {
                        this.service_status = status.roots;
                        cx.notify();
                        true
                    }
                    Err(err) => {
                        tracing::warn!("index service lost ({err:#}); indexing locally");
                        this.service_disconnected(cx);
                        false
                    }
                });
                if !matches!(keep_polling, Ok(true)) {
                    break;
                }
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(5))
                    .await;
            }
        })
        .detach();
    }

    #[cfg(target_os = "windows")]
    fn service_disconnected(&mut self, cx: &mut Context<Self>) {
        self.service = None;
        self.service_status.clear();
        if self.roots.is_empty() {
            for path in self.configured_roots(cx) {
                self.add_root_slot(path, cx);
            }
        }
        self.update_search(cx);
        cx.notify();
    }

    #[cfg(target_os = "windows")]
    fn service_mode(&self) -> bool {
        self.service.is_some()
    }

    #[cfg(not(target_os = "windows"))]
    fn service_mode(&self) -> bool {
        false
    }

    /// Canonicalize, create the slot, and start indexing. Invalid paths
    /// become Failed slots so the config line stays visible rather than
    /// silently vanishing.
    fn add_root_slot(&mut self, path: PathBuf, cx: &mut Context<Self>) {
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

    fn slot_mut(&mut self, path: &Path) -> Option<&mut RootSlot> {
        self.roots.iter_mut().find(|slot| slot.path == path)
    }

    /// Index one root on the background executor, then keep listening for
    /// its writer's change notifications. The UI thread never blocks.
    fn spawn_root(&self, path: PathBuf, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let (change_tx, mut change_rx) = futures::channel::mpsc::unbounded::<()>();
            let result = cx
                .background_executor()
                .spawn({
                    let path = path.clone();
                    async move {
                        start_live_index(&path, move || {
                            change_tx.unbounded_send(()).ok();
                        })
                        .map(|live| {
                            let files = read_index(&live.index).len();
                            (live, files)
                        })
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
                    this.refresh_root_stats(path.clone(), cx);
                    if !this.query.is_empty() {
                        this.update_search(cx);
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    /// Recompute one root's file count off-thread (it walks the arena).
    fn refresh_root_stats(&mut self, path: PathBuf, cx: &mut Context<Self>) {
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
                    && let RootState::Ready { files: slot_files, .. } = &mut slot.state
                {
                    *slot_files = files;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn spawn_fda_check(&self, cx: &mut Context<Self>) {
        #[cfg(target_os = "macos")]
        cx.spawn(async move |this, cx| {
            let has_access = cx
                .background_executor()
                .spawn(async { filex::index::macos::has_full_disk_access() })
                .await;
            this.update(cx, |this, cx| {
                this.fda_missing = !has_access;
                cx.notify();
            })
            .ok();
        })
        .detach();
        #[cfg(not(target_os = "macos"))]
        let _ = cx;
    }

    /// Add the directory currently being browsed as a new indexed root.
    fn add_current_folder(&mut self, cx: &mut Context<Self>) {
        if self.service_mode() {
            self.notice =
                Some("roots are managed by the filex index service (filex-indexd)".into());
            cx.notify();
            return;
        }
        let existing: Vec<PathBuf> = self.roots.iter().map(|slot| slot.path.clone()).collect();
        match manager::validate_new_root(&existing, &self.cwd) {
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
    fn persist_roots(&self, cx: &mut Context<Self>) {
        let roots: Vec<PathBuf> = self.roots.iter().map(|slot| slot.path.clone()).collect();
        self.settings.update(cx, |store, cx| {
            store.update(cx, |settings| settings.roots = roots);
        });
    }

    fn load_dir(&mut self, path: &Path, cx: &App) {
        let settings = self.settings.read(cx).settings();
        let (sort, show_hidden) = (settings.sort, settings.show_hidden_files);
        match read_dir_sorted(path, &sort) {
            Ok(mut entries) => {
                if !show_hidden {
                    entries.retain(|entry| !entry.is_hidden);
                }
                self.cwd = path.to_path_buf();
                self.entries = entries;
                self.load_error = None;
                self.selected = None;
                // Any in-flight rename points at rows that no longer
                // exist; drop the editor.
                self.renaming = None;
                self.browse_scroll.scroll_to_item(0, ScrollStrategy::Top);
            }
            Err(err) => {
                self.load_error = Some(format!("{err:#}").into());
            }
        }
    }

    /// Schedule a thumbnail decode for a visible image row (no-op if
    /// cached or in flight). Called from the list processor, so only
    /// rows that actually render ever spawn work.
    fn request_thumbnail(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.thumbnails.contains_key(&path) {
            return;
        }
        if self.thumbnails.len() >= thumbnails::CACHE_CAP {
            self.thumbnails.clear(); // visible rows repopulate immediately
        }
        self.thumbnails.insert(path.clone(), ThumbnailState::Loading);
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

    /// The icon cell for a row: a decoded thumbnail for image files when
    /// ready, otherwise the kind glyph. May schedule a decode as a side
    /// effect — only rows the virtualized list renders get here.
    fn render_icon_cell(
        &mut self,
        name: &str,
        path: &Path,
        is_dir: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let kind = FileKind::of(name, is_dir);
        if kind == FileKind::Image && self.settings.read(cx).settings().thumbnails_enabled {
            match self.thumbnails.get(path) {
                Some(ThumbnailState::Ready(imagery)) => {
                    return ui::icon::thumbnail_icon(imagery.clone());
                }
                Some(_) => {}
                None => self.request_thumbnail(path.to_path_buf(), cx),
            }
        }
        ui::icon::glyph_icon(kind, is_dir)
    }

    /// Length of whichever list selection currently applies to.
    fn active_list_len(&self) -> usize {
        if self.query.is_empty() { self.entries.len() } else { self.results.len() }
    }

    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let len = self.active_list_len();
        if len == 0 {
            self.selected = None;
            return;
        }
        let next = match self.selected {
            Some(ix) => ix.saturating_add_signed(delta).min(len - 1),
            None if delta > 0 => 0,
            None => len - 1,
        };
        self.selected = Some(next);
        let handle = if self.query.is_empty() { &self.browse_scroll } else { &self.results_scroll };
        handle.scroll_to_item(next, ScrollStrategy::Center);
        cx.notify();
    }

    /// Enter / double-click: directories navigate, files open with the
    /// platform's default application.
    fn activate(&mut self, ix: usize, cx: &mut Context<Self>) {
        let (path, is_dir, from_search) = if self.query.is_empty() {
            let Some(entry) = self.entries.get(ix) else { return };
            (entry.path.clone(), entry.is_dir, false)
        } else {
            let Some(row) = self.results.get(ix) else { return };
            (row.target.clone(), row.is_dir, true)
        };
        if is_dir {
            self.navigate(path, cx);
            if from_search {
                self.clear_search(cx);
            }
        } else {
            if let Err(err) = open_with_default_app(&path) {
                self.notice = Some(format!("couldn't open {}: {err}", path.display()).into());
            }
            if from_search {
                self.clear_search(cx);
            }
            cx.notify();
        }
    }

    fn activate_selected(&mut self, cx: &mut Context<Self>) {
        if let Some(ix) = self.selected {
            self.activate(ix, cx);
        }
    }

    fn select(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.selected = Some(ix);
        cx.notify();
    }

    fn navigate(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.load_dir(&path, cx);
        cx.notify();
    }

    fn go_up(&mut self, cx: &mut Context<Self>) {
        if let Some(parent) = self.cwd.parent().map(Path::to_path_buf) {
            self.navigate(parent, cx);
        }
    }

    fn clear_search(&mut self, cx: &mut Context<Self>) {
        // The input owns the text; its Changed event clears our mirror
        // and re-runs the (now empty) search.
        self.search_input.update(cx, |input, cx| {
            if !input.is_empty() {
                input.set_text("", cx);
            }
        });
    }

    /// Begin renaming the selected browse entry (F2): the row's name
    /// cell becomes an input prefilled with the current name, content
    /// preselected so typing replaces it.
    fn start_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.query.is_empty() || self.settings_open {
            return;
        }
        let Some(ix) = self.selected else { return };
        let Some(entry) = self.entries.get(ix) else { return };
        let name = entry.name.clone();
        let input = cx.new(SearchInput::new);
        input.update(cx, |input, cx| {
            input.set_placeholder("new name", cx);
            input.set_text(name, cx);
            input.select_all_text(cx);
        });
        let subscription =
            cx.subscribe_in(&input, window, |this, _input, event, window, cx| {
                if matches!(event, SearchInputEvent::Dismissed) {
                    this.cancel_rename(window, cx);
                }
            });
        window.focus(&input.focus_handle(cx));
        self.renaming = Some(RenameState { ix, input, _subscription: subscription });
        cx.notify();
    }

    fn cancel_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.renaming.take().is_some() {
            window.focus(&self.search_input.focus_handle(cx));
            cx.notify();
        }
    }

    fn commit_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(RenameState { ix, input, .. }) = self.renaming.take() else {
            return;
        };
        window.focus(&self.search_input.focus_handle(cx));
        cx.notify();
        let Some(entry) = self.entries.get(ix) else { return };
        let new_name = input.read(cx).text().trim().to_string();
        if new_name.is_empty() || new_name == entry.name {
            return; // nothing to do — treated as cancel
        }
        self.run_op(FileOp::Rename { path: entry.path.clone(), new_name }, cx);
    }

    /// Execute a file operation on the background executor; success
    /// lands in the undo journal and refreshes the listing. Ops never
    /// touch the index — the watchers pick the change up as deltas.
    fn run_op(&mut self, op: FileOp, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { ops::apply(&op) }).await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(applied) => {
                        this.notice = Some(applied.describe().into());
                        this.journal.record(applied);
                    }
                    Err(err) => this.notice = Some(format!("{err:#}").into()),
                }
                let cwd = this.cwd.clone();
                this.load_dir(&cwd, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Undo the most recent file operation (cmd-z / ctrl-z). The disk
    /// work runs off-thread; a failed undo goes back on the journal so
    /// the user can fix the cause and retry.
    fn undo_last(&mut self, cx: &mut Context<Self>) {
        let Some(applied) = self.journal.pop() else {
            self.notice = Some("nothing to undo".into());
            cx.notify();
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn({
                    let applied = applied.clone();
                    async move { ops::undo(&applied) }
                })
                .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(()) => this.notice = Some(format!("undid: {}", applied.describe()).into()),
                    Err(err) => {
                        this.notice = Some(format!("undo failed: {err:#}").into());
                        this.journal.restore(applied);
                    }
                }
                let cwd = this.cwd.clone();
                this.load_dir(&cwd, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// List-navigation keys. Text editing lives in the SearchInput (which
    /// is focused); unhandled keys bubble up here.
    fn handle_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        if keystroke.modifiers.platform || keystroke.modifiers.control {
            return; // shortcuts are handled by actions
        }
        if self.renaming.is_some() {
            // Escape is consumed by the input itself (Dismissed event);
            // enter lands here and commits.
            if keystroke.key.as_str() == "enter" {
                self.commit_rename(window, cx);
            }
            return;
        }
        match keystroke.key.as_str() {
            "up" => self.move_selection(-1, cx),
            "down" => self.move_selection(1, cx),
            "enter" => self.activate_selected(cx),
            _ => {}
        }
    }

    /// Kick off a merged query across every ready root on the background
    /// executor. Stale completions are dropped by generation check.
    fn update_search(&mut self, cx: &mut Context<Self>) {
        self.search_generation += 1;
        let generation = self.search_generation;
        cx.notify();

        if self.query.is_empty() {
            self.results.clear();
            self.selected = None;
            return;
        }

        // Service mode: the query goes over IPC; a failed roundtrip falls
        // back to local indexing and re-runs.
        #[cfg(target_os = "windows")]
        if let Some(client) = self.service.clone() {
            let query = self.query.clone();
            cx.spawn(async move |this, cx| {
                let result = cx
                    .background_executor()
                    .spawn(async move { client.search(&query, SEARCH_RESULT_LIMIT as u32) })
                    .await;
                this.update(cx, |this, cx| {
                    if this.search_generation != generation {
                        return;
                    }
                    match result {
                        Ok(hits) => {
                            this.results = hits
                                .into_iter()
                                .map(|hit| SearchRow {
                                    name: hit.name.into(),
                                    path_label: hit.path.display().to_string().into(),
                                    is_dir: hit.is_dir,
                                    target: hit.path,
                                })
                                .collect();
                            this.select_first_result();
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

        let indexes: Vec<SharedIndex> =
            self.roots.iter().filter_map(RootSlot::ready_index).collect();
        if indexes.is_empty() {
            return; // still building; root readiness re-runs the query
        }
        let query = self.query.clone();

        cx.spawn(async move |this, cx| {
            let rows = cx
                .background_executor()
                .spawn(async move {
                    manager::search_all(&indexes, &query, SEARCH_RESULT_LIMIT)
                        .into_iter()
                        .map(|hit| SearchRow {
                            name: hit.name.into(),
                            path_label: hit.path.display().to_string().into(),
                            is_dir: hit.is_dir,
                            target: hit.path,
                        })
                        .collect::<Vec<_>>()
                })
                .await;
            this.update(cx, |this, cx| {
                if this.search_generation == generation {
                    this.results = rows;
                    this.select_first_result();
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// New results select the first hit (Spotlight-style), so Enter
    /// immediately opens the top match.
    fn select_first_result(&mut self) {
        self.selected = (!self.results.is_empty()).then_some(0);
        self.results_scroll.scroll_to_item(0, ScrollStrategy::Top);
    }

    fn any_root_ready(&self) -> bool {
        if self.service_mode() {
            return true;
        }
        self.roots
            .iter()
            .any(|slot| matches!(slot.state, RootState::Ready { .. }))
    }

    fn index_status_text(&self) -> SharedString {
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

    /// Clickable path segments. Deep paths elide the middle ("…"),
    /// keeping the root and the last few segments — the tail is what
    /// the user actually navigates with.
    fn render_breadcrumbs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        const MAX_SEGMENTS: usize = 6;
        const TAIL_SEGMENTS: usize = 4;
        let segments = path_segments(&self.cwd);
        let elide = segments.len() > MAX_SEGMENTS;
        let tail_start = if elide { segments.len() - TAIL_SEGMENTS } else { usize::MAX };

        let mut children: Vec<gpui::AnyElement> = Vec::new();
        for (ix, (label, target)) in segments.into_iter().enumerate() {
            if elide && ix > 0 && ix < tail_start {
                if ix == 1 {
                    children.push(ui::top_bar::breadcrumb_separator("›").into_any_element());
                    children.push(ui::top_bar::breadcrumb_separator("…").into_any_element());
                }
                continue;
            }
            if ix > 0 {
                children.push(ui::top_bar::breadcrumb_separator("›").into_any_element());
            }
            children.push(
                ui::top_bar::breadcrumb_segment(("crumb", ix), label)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                        this.navigate(target.clone(), cx);
                    }))
                    .into_any_element(),
            );
        }
        ui::top_bar::breadcrumbs().children(children)
    }

    fn toggle_settings(&mut self, cx: &mut Context<Self>) {
        self.settings_open = !self.settings_open;
        cx.notify();
    }

    fn render_top_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        ui::top_bar::top_bar()
            .child(ui::top_bar::toolbar_button("up", "↑").on_click(cx.listener(
                |this, _: &ClickEvent, _window, cx| {
                    this.go_up(cx);
                },
            )))
            .child(self.render_breadcrumbs(cx))
            .child(ui::top_bar::search_box(!self.query.is_empty()).child(self.search_input.clone()))
            .child(
                ui::top_bar::toolbar_button("settings", "⚙")
                    .when(self.settings_open, |s| s.text_color(rgb(ACCENT)))
                    .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                        this.toggle_settings(cx);
                    })),
            )
    }

    fn render_settings_pane(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let settings = self.settings.read(cx).settings().clone();
        let file_note: SharedString = match filex::settings::default_settings_file() {
            Some(path) => format!("saved to {}", path.display()).into(),
            None => "no config directory found — settings won't persist".into(),
        };
        ui::settings_pane::settings_pane()
            .child(ui::settings_pane::pane_title("Settings"))
            .child(
                ui::settings_pane::toggle_row(
                    "show-hidden",
                    "Show hidden files",
                    "Dotfiles and OS-hidden entries in the browse list",
                    settings.show_hidden_files,
                )
                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                    this.settings.update(cx, |store, cx| {
                        store.update(cx, |s| s.show_hidden_files = !s.show_hidden_files);
                    });
                })),
            )
            .child(
                ui::settings_pane::toggle_row(
                    "dirs-first",
                    "Directories first",
                    "Group folders above files whatever the sort order",
                    settings.sort.directories_first,
                )
                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                    this.settings.update(cx, |store, cx| {
                        store.update(cx, |s| {
                            s.sort.directories_first = !s.sort.directories_first;
                        });
                    });
                })),
            )
            .child(
                ui::settings_pane::toggle_row(
                    "thumbnails",
                    "Image thumbnails",
                    "Decode small previews for image files in the list",
                    settings.thumbnails_enabled,
                )
                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                    this.settings.update(cx, |store, cx| {
                        store.update(cx, |s| s.thumbnails_enabled = !s.thumbnails_enabled);
                    });
                })),
            )
            .child(ui::settings_pane::footnote(file_note))
            .into_any_element()
    }

    /// A row for one service-managed root (service mode has no local
    /// slots; state comes from the status poll).
    #[cfg(target_os = "windows")]
    fn render_service_root_row(
        &self,
        ix: usize,
        root: &filex::index::ipc::RootStatus,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let path = PathBuf::from(&root.path);
        let label: SharedString = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| root.path.clone())
            .into();
        ui::sidebar::sidebar_row(("service-root", ix))
            .child(ui::sidebar::root_marker("◆", ACCENT))
            .child(div().flex_1().overflow_hidden().child(label))
            .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                this.navigate(path.clone(), cx);
            }))
            .into_any_element()
    }

    // `use<>`: the built element owns its data; without opting out of
    // lifetime capture it couldn't be collected across loop iterations.
    fn render_root_row(&self, ix: usize, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let slot = &self.roots[ix];
        let path = slot.path.clone();
        // Clicking a healthy root navigates to it; clicking a failed one
        // surfaces why it failed in the status bar.
        let failure: Option<SharedString> = match &slot.state {
            RootState::Failed(err) => Some(err.clone()),
            _ => None,
        };
        let (marker, marker_color) = match &slot.state {
            RootState::Building => ("…", TEXT_DIM),
            RootState::Ready { .. } => ("●", ACCENT),
            RootState::Failed(_) => ("✕", WARN),
        };
        ui::sidebar::sidebar_row(("root", ix))
            .child(ui::sidebar::root_marker(marker, marker_color))
            .child(div().flex_1().overflow_hidden().child(slot.label.clone()))
            .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                match &failure {
                    Some(err) => {
                        this.notice = Some(err.clone());
                        cx.notify();
                    }
                    None => this.navigate(path.clone(), cx),
                }
            }))
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let places: Vec<(&str, PathBuf)> = [
            ("Home", std::env::home_dir()),
            ("Root", Some(PathBuf::from("/"))),
        ]
        .into_iter()
        .filter_map(|(label, path)| Some((label, path?)))
        .collect();

        let root_rows: Vec<gpui::AnyElement> = {
            #[cfg(target_os = "windows")]
            if self.service_mode() {
                self.service_status
                    .iter()
                    .enumerate()
                    .map(|(ix, root)| self.render_service_root_row(ix, root, cx))
                    .collect()
            } else {
                (0..self.roots.len())
                    .map(|ix| self.render_root_row(ix, cx).into_any_element())
                    .collect()
            }
            #[cfg(not(target_os = "windows"))]
            (0..self.roots.len())
                .map(|ix| self.render_root_row(ix, cx).into_any_element())
                .collect()
        };

        let sidebar = ui::sidebar::sidebar_panel()
            .child(ui::sidebar::section_header("PLACES"))
            .children(places.into_iter().enumerate().map(|(ix, (label, path))| {
                ui::sidebar::sidebar_row(("place", ix)).child(label).on_click(cx.listener(
                    move |this, _: &ClickEvent, _window, cx| {
                        this.navigate(path.clone(), cx);
                    },
                ))
            }))
            .child(ui::sidebar::section_header("INDEXED").pt_3())
            .children(root_rows)
            .child(
                ui::sidebar::sidebar_row("add-root")
                    .text_color(rgb(TEXT_DIM))
                    .child("+ index current folder")
                    .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                        this.add_current_folder(cx);
                    })),
            );

        // Shadowing rebind (not `mut`): the binding is only extended on
        // macOS, and an unconditional `mut` breaks -D warnings elsewhere.
        #[cfg(target_os = "macos")]
        let sidebar = if self.fda_missing {
            sidebar.child(div().flex_1()).child(
                ui::sidebar::sidebar_row("fda-banner")
                    .text_xs()
                    .text_color(rgb(WARN))
                    .child("⚠ Grant Full Disk Access")
                    .on_click(|_: &ClickEvent, _window, _cx| {
                        filex::index::macos::open_full_disk_access_settings();
                    }),
            )
        } else {
            sidebar
        };

        sidebar
    }

    /// Header click: select the column, or flip the direction when it's
    /// already the sort key. Goes through the settings store, so it
    /// persists and the Changed event re-sorts the listing.
    fn set_sort(&mut self, by: SortBy, cx: &mut Context<Self>) {
        self.settings.update(cx, |store, cx| {
            store.update(cx, |settings| {
                if settings.sort.by == by {
                    settings.sort.ascending = !settings.sort.ascending;
                } else {
                    settings.sort.by = by;
                    settings.sort.ascending = true;
                }
            });
        });
    }

    fn render_column_headers(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let sort = self.settings.read(cx).settings().sort;
        let active = |by: SortBy| (sort.by == by).then_some(sort.ascending);
        let on_sort = |by: SortBy| {
            cx.listener(move |this: &mut Self, _: &ClickEvent, _window, cx| {
                this.set_sort(by, cx);
            })
        };
        ui::list_row::header_row(ui::icon::ICON_SIZE)
            .child(
                ui::list_row::header_cell("sort-name", "Name", active(SortBy::Name))
                    .flex_1()
                    .on_click(on_sort(SortBy::Name)),
            )
            .child(
                ui::list_row::header_cell("sort-modified", "Modified", active(SortBy::Modified))
                    .w(px(ui::list_row::MODIFIED_COL_WIDTH))
                    .flex_none()
                    .text_right()
                    .on_click(on_sort(SortBy::Modified)),
            )
            .child(
                ui::list_row::header_cell("sort-size", "Size", active(SortBy::Size))
                    .w(px(ui::list_row::SIZE_COL_WIDTH))
                    .flex_none()
                    .text_right()
                    .on_click(on_sort(SortBy::Size)),
            )
    }

    fn render_browse_pane(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(self.render_column_headers(cx))
            .child(self.render_file_list(cx))
            .into_any_element()
    }

    fn render_file_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        uniform_list(
            "entries",
            self.entries.len(),
            cx.processor(|this, range: Range<usize>, _window, cx| {
                range
                    .filter_map(|ix| {
        // Copy row data out first: the icon cell needs &mut self (it may
        // schedule a thumbnail decode) while `entry` borrows self.
                        let entry = this.entries.get(ix)?;
                        let is_dir = entry.is_dir;
                        let size = entry.size;
                        let modified = entry.modified;
                        let is_selected = this.selected == Some(ix);
                        let (name, path) = (entry.name.clone(), entry.path.clone());
                        let icon = this.render_icon_cell(&name, &path, is_dir, cx);
                        let rename_input = this
                            .renaming
                            .as_ref()
                            .filter(|rename| rename.ix == ix)
                            .map(|rename| rename.input.clone());
                        let name_cell = match rename_input {
                            Some(input) => div()
                                .flex_1()
                                .px_1()
                                .rounded_sm()
                                .border_1()
                                .border_color(rgb(ACCENT))
                                .text_sm()
                                .child(input)
                                .into_any_element(),
                            None => div().flex_1().text_sm().child(name).into_any_element(),
                        };
                        Some(
                            ui::list_row::list_row(ix, is_selected)
                                .child(icon)
                                .child(name_cell)
                                .child(ui::list_row::detail_cell(
                                    ui::list_row::MODIFIED_COL_WIDTH,
                                    format_modified(modified, std::time::SystemTime::now()),
                                ))
                                .child(ui::list_row::detail_cell(
                                    ui::list_row::SIZE_COL_WIDTH,
                                    if is_dir { "—".to_string() } else { format_size(size) },
                                ))
                                .on_click(cx.listener(move |this, event: &ClickEvent, _window, cx| {
                                    if event.click_count() >= 2 {
                                        this.activate(ix, cx);
                                    } else {
                                        this.select(ix, cx);
                                    }
                                })),
                        )
                    })
                    .collect()
            }),
        )
        .track_scroll(self.browse_scroll.clone())
        .flex_1()
    }

    fn render_search_results(&self, cx: &mut Context<Self>) -> impl IntoElement {
        uniform_list(
            "results",
            self.results.len(),
            cx.processor(|this, range: Range<usize>, _window, cx| {
                range
                    .filter_map(|ix| {
                        let row = this.results.get(ix)?;
                        let is_dir = row.is_dir;
                        let is_selected = this.selected == Some(ix);
                        let (name, path) = (row.name.clone(), row.target.clone());
                        let path_label = row.path_label.clone();
                        let icon = this.render_icon_cell(&name, &path, is_dir, cx);
                        Some(
                            ui::list_row::list_row(ix, is_selected)
                                .child(icon)
                                .child(div().text_sm().child(name))
                                .child(
                                    div()
                                        .flex_1()
                                        .text_xs()
                                        .text_color(rgb(TEXT_DIM))
                                        .overflow_hidden()
                                        .child(path_label),
                                )
                                .on_click(cx.listener(move |this, event: &ClickEvent, _window, cx| {
                                    if event.click_count() >= 2 {
                                        this.activate(ix, cx);
                                    } else {
                                        this.select(ix, cx);
                                    }
                                })),
                        )
                    })
                    .collect()
            }),
        )
        .track_scroll(self.results_scroll.clone())
        .flex_1()
    }

    fn render_search_pane(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        if !self.any_root_ready() {
            return ui::pane::empty_state("still indexing — results will appear when ready…");
        }
        if self.results.is_empty() {
            return ui::pane::empty_state(format!("no matches for “{}”", self.query));
        }
        self.render_search_results(cx).into_any_element()
    }

    fn render_status_bar(&self) -> impl IntoElement {
        let left: SharedString = if let Some(notice) = &self.notice {
            notice.clone()
        } else if let Some(err) = &self.load_error {
            format!("error — {err}").into()
        } else if self.query.is_empty() {
            format!("{} items", self.entries.len()).into()
        } else {
            format!(
                "{} result{}",
                self.results.len(),
                if self.results.len() == 1 { "" } else { "s" }
            )
            .into()
        };
        ui::status_bar::status_bar(left, self.index_status_text())
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let searching = !self.query.is_empty();
        div()
            .track_focus(&self.focus_handle)
            .on_action(|_: &CloseWindow, window, _| window.remove_window())
            .on_action(cx.listener(|this, _: &GoUp, _window, cx| this.go_up(cx)))
            .on_action(cx.listener(|this, _: &ToggleSettings, _window, cx| {
                this.toggle_settings(cx);
            }))
            .on_action(cx.listener(|this, _: &RenameSelected, window, cx| {
                this.start_rename(window, cx);
            }))
            .on_action(cx.listener(|this, _: &Undo, _window, cx| {
                this.undo_last(cx);
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.handle_key(event, window, cx);
            }))
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
            .child(self.render_top_bar(cx))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_sidebar(cx))
                    .child(if searching {
                        self.render_search_pane(cx)
                    } else if self.settings_open {
                        self.render_settings_pane(cx)
                    } else {
                        self.render_browse_pane(cx)
                    }),
            )
            .child(self.render_status_bar())
    }
}

fn main() {
    let _logging_guard = filex::logging::init("filex");
    Application::new().run(|cx: &mut App| {
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.bind_keys([
            #[cfg(target_os = "macos")]
            KeyBinding::new("cmd-q", Quit, None),
            #[cfg(target_os = "macos")]
            KeyBinding::new("cmd-w", CloseWindow, None),
            #[cfg(target_os = "macos")]
            KeyBinding::new("cmd-up", GoUp, None),
            #[cfg(target_os = "macos")]
            KeyBinding::new("cmd-,", ToggleSettings, None),
            #[cfg(target_os = "macos")]
            KeyBinding::new("cmd-z", Undo, None),
            #[cfg(not(target_os = "macos"))]
            KeyBinding::new("ctrl-q", Quit, None),
            #[cfg(not(target_os = "macos"))]
            KeyBinding::new("ctrl-w", CloseWindow, None),
            #[cfg(not(target_os = "macos"))]
            KeyBinding::new("alt-up", GoUp, None),
            #[cfg(not(target_os = "macos"))]
            KeyBinding::new("ctrl-,", ToggleSettings, None),
            #[cfg(not(target_os = "macos"))]
            KeyBinding::new("ctrl-z", Undo, None),
            KeyBinding::new("f2", RenameSelected, None),
        ]);
        search_input::bind_keys(cx);
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        let bounds = Bounds::centered(None, size(px(1000.), px(700.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("filex".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |window, cx| {
                cx.activate(true);
                cx.new(|cx| {
                    let workspace = Workspace::new(cx);
                    // Focus the input so typing searches immediately;
                    // navigation keys bubble up to the workspace.
                    window.focus(&workspace.search_input.focus_handle(cx));
                    workspace
                })
            },
        )
        .expect("failed to open the main window");
    });
}

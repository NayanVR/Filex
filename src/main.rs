use std::ops::Range;
use std::path::{Path, PathBuf};

use futures::StreamExt as _;
use gpui::{
    App, Application, Bounds, ClickEvent, Context, FocusHandle, KeyBinding, KeyDownEvent,
    SharedString, TitlebarOptions, Window, WindowBounds, WindowOptions, actions, div, prelude::*,
    px, rgb, size, uniform_list,
};

use filex::index::watcher::SharedIndex;
use filex::index::{LiveIndex, VolumeIndex, manager, start_live_index};
use filex::listing::{Entry, format_size, read_dir_sorted};

actions!(filex, [Quit, CloseWindow, GoUp]);

// Palette (dark). Placeholder until a real theme system lands.
const BG: u32 = 0x1e2227;
const BG_PANEL: u32 = 0x23272e;
const BG_HOVER: u32 = 0x2f343c;
const BORDER: u32 = 0x363c45;
const TEXT: u32 = 0xd7dae0;
const TEXT_DIM: u32 = 0x8b929e;
const ACCENT: u32 = 0x5ac8fa;
const WARN: u32 = 0xe5c07b;

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

struct Workspace {
    focus_handle: FocusHandle,
    cwd: PathBuf,
    entries: Vec<Entry>,
    load_error: Option<SharedString>,
    roots: Vec<RootSlot>,
    roots_file: Option<PathBuf>,
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
    query: String,
    results: Vec<SearchRow>,
    search_generation: u64,
}

impl Workspace {
    fn configured_roots() -> Vec<PathBuf> {
        let mut configured = manager::default_roots_file()
            .as_deref()
            .map(manager::load_roots)
            .unwrap_or_default();
        if configured.is_empty()
            && let Some(home) = std::env::home_dir()
        {
            configured.push(home);
        }
        configured
    }

    fn new(cx: &mut Context<Self>) -> Self {
        let cwd = std::env::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            cwd: cwd.clone(),
            entries: Vec::new(),
            load_error: None,
            roots: Vec::new(),
            roots_file: manager::default_roots_file(),
            notice: None,
            #[cfg(target_os = "macos")]
            fda_missing: false,
            #[cfg(target_os = "windows")]
            service: None,
            #[cfg(target_os = "windows")]
            service_status: Vec::new(),
            query: String::new(),
            results: Vec::new(),
            search_generation: 0,
        };
        this.load_dir(&cwd);
        // Windows probes for the elevated index service first and only
        // falls back to in-process indexing if it's absent; elsewhere
        // indexing is always in-process.
        #[cfg(target_os = "windows")]
        this.spawn_service_probe(cx);
        #[cfg(not(target_os = "windows"))]
        for path in Self::configured_roots() {
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
                        for path in Self::configured_roots() {
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
                        eprintln!("filex: index service lost ({err:#}); indexing locally");
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
            for path in Self::configured_roots() {
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

    fn persist_roots(&self, cx: &mut Context<Self>) {
        let Some(file) = self.roots_file.clone() else {
            return;
        };
        let roots: Vec<PathBuf> = self.roots.iter().map(|slot| slot.path.clone()).collect();
        cx.background_executor()
            .spawn(async move {
                if let Err(err) = manager::save_roots(&file, &roots) {
                    eprintln!("filex: failed to save root list: {err:#}");
                }
            })
            .detach();
    }

    fn load_dir(&mut self, path: &Path) {
        match read_dir_sorted(path) {
            Ok(entries) => {
                self.cwd = path.to_path_buf();
                self.entries = entries;
                self.load_error = None;
            }
            Err(err) => {
                self.load_error = Some(format!("{err:#}").into());
            }
        }
    }

    fn navigate(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.load_dir(&path);
        cx.notify();
    }

    fn go_up(&mut self, cx: &mut Context<Self>) {
        if let Some(parent) = self.cwd.parent().map(Path::to_path_buf) {
            self.navigate(parent, cx);
        }
    }

    fn clear_search(&mut self, cx: &mut Context<Self>) {
        self.query.clear();
        self.update_search(cx);
    }

    /// Interim search input: characters are captured from raw key events.
    /// Replace with a real text input view (gpui-component's Input or an
    /// EntityInputHandler) once the shell grows edit affordances.
    fn handle_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        if keystroke.modifiers.platform || keystroke.modifiers.control {
            return; // shortcuts are handled by actions
        }
        match keystroke.key.as_str() {
            "backspace" => {
                if self.query.is_empty() {
                    self.go_up(cx);
                } else {
                    self.query.pop();
                    self.update_search(cx);
                }
            }
            "escape" => self.clear_search(cx),
            _ => {
                if let Some(text) = &keystroke.key_char
                    && !text.chars().any(char::is_control)
                {
                    self.query.push_str(text);
                    self.update_search(cx);
                }
            }
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
                            cx.notify();
                        }
                        Err(err) => {
                            eprintln!("filex: service search failed ({err:#})");
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
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn open_search_result(&mut self, row_target: PathBuf, is_dir: bool, cx: &mut Context<Self>) {
        let destination = if is_dir {
            Some(row_target)
        } else {
            row_target.parent().map(Path::to_path_buf)
        };
        if let Some(destination) = destination {
            self.navigate(destination, cx);
        }
        self.clear_search(cx);
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
            let roots = self.service_status.len();
            return format!(
                "service · {files} files · {roots} root{}",
                if roots == 1 { "" } else { "s" }
            )
            .into();
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
        let mut text = if ready == total {
            format!("{files} files · {total} root{}", if total == 1 { "" } else { "s" })
        } else {
            format!("indexing {ready}/{total} roots · {files} files")
        };
        if failed > 0 {
            text.push_str(&format!(" · {failed} failed"));
        }
        text.into()
    }

    fn render_top_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let searching = !self.query.is_empty();
        div()
            .flex()
            .items_center()
            .gap_2()
            .h(px(40.))
            .px_3()
            .border_b_1()
            .border_color(rgb(BORDER))
            .bg(rgb(BG_PANEL))
            .child(
                div()
                    .id("up")
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(BG_HOVER)))
                    .text_color(rgb(TEXT_DIM))
                    .child("↑")
                    .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                        this.go_up(cx);
                    })),
            )
            .child(
                div()
                    .flex_1()
                    .text_sm()
                    .text_color(rgb(TEXT_DIM))
                    .overflow_hidden()
                    .child(self.cwd.display().to_string()),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(if searching { ACCENT } else { BORDER }))
                    .text_sm()
                    .child(if searching {
                        div().text_color(rgb(TEXT)).child(self.query.clone())
                    } else {
                        div().text_color(rgb(TEXT_DIM)).child("type to search")
                    }),
            )
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
        div()
            .id(("service-root", ix))
            .flex()
            .items_center()
            .gap_2()
            .mx_2()
            .px_2()
            .py_1()
            .rounded_md()
            .cursor_pointer()
            .text_sm()
            .hover(|s| s.bg(rgb(BG_HOVER)))
            .child(div().text_xs().text_color(rgb(ACCENT)).child("◆"))
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
        div()
            .id(("root", ix))
            .flex()
            .items_center()
            .gap_2()
            .mx_2()
            .px_2()
            .py_1()
            .rounded_md()
            .cursor_pointer()
            .text_sm()
            .hover(|s| s.bg(rgb(BG_HOVER)))
            .child(div().text_xs().text_color(rgb(marker_color)).child(marker))
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

        let sidebar = div()
            .flex()
            .flex_col()
            .w(px(200.))
            .h_full()
            .py_2()
            .border_r_1()
            .border_color(rgb(BORDER))
            .bg(rgb(BG_PANEL))
            .child(
                div()
                    .px_3()
                    .pb_1()
                    .text_xs()
                    .text_color(rgb(TEXT_DIM))
                    .child("PLACES"),
            )
            .children(places.into_iter().enumerate().map(|(ix, (label, path))| {
                div()
                    .id(("place", ix))
                    .mx_2()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .text_sm()
                    .hover(|s| s.bg(rgb(BG_HOVER)))
                    .child(label)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                        this.navigate(path.clone(), cx);
                    }))
            }))
            .child(
                div()
                    .px_3()
                    .pt_3()
                    .pb_1()
                    .text_xs()
                    .text_color(rgb(TEXT_DIM))
                    .child("INDEXED"),
            )
            .children(root_rows)
            .child(
                div()
                    .id("add-root")
                    .mx_2()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .text_sm()
                    .text_color(rgb(TEXT_DIM))
                    .hover(|s| s.bg(rgb(BG_HOVER)))
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
                div()
                    .id("fda-banner")
                    .mx_2()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .text_xs()
                    .text_color(rgb(WARN))
                    .hover(|s| s.bg(rgb(BG_HOVER)))
                    .child("⚠ Grant Full Disk Access for complete indexing")
                    .on_click(|_: &ClickEvent, _window, _cx| {
                        filex::index::macos::open_full_disk_access_settings();
                    }),
            )
        } else {
            sidebar
        };

        sidebar
    }

    fn render_file_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        uniform_list(
            "entries",
            self.entries.len(),
            cx.processor(|this, range: Range<usize>, _window, cx| {
                range
                    .filter_map(|ix| {
                        let entry = this.entries.get(ix)?;
                        let path = entry.path.clone();
                        let is_dir = entry.is_dir;
                        Some(
                            div()
                                .id(ix)
                                .flex()
                                .items_center()
                                .gap_2()
                                .h(px(28.))
                                .px_3()
                                .cursor_pointer()
                                .hover(|s| s.bg(rgb(BG_HOVER)))
                                .child(
                                    div()
                                        .w(px(16.))
                                        .text_color(rgb(if is_dir { ACCENT } else { TEXT_DIM }))
                                        .child(if is_dir { "▸" } else { "·" }),
                                )
                                .child(div().flex_1().text_sm().child(entry.name.clone()))
                                .child(div().text_xs().text_color(rgb(TEXT_DIM)).child(
                                    if is_dir {
                                        "—".to_string()
                                    } else {
                                        format_size(entry.size)
                                    },
                                ))
                                .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                                    if is_dir {
                                        this.navigate(path.clone(), cx);
                                    }
                                })),
                        )
                    })
                    .collect()
            }),
        )
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
                        let target = row.target.clone();
                        let is_dir = row.is_dir;
                        Some(
                            div()
                                .id(ix)
                                .flex()
                                .items_center()
                                .gap_2()
                                .h(px(28.))
                                .px_3()
                                .cursor_pointer()
                                .hover(|s| s.bg(rgb(BG_HOVER)))
                                .child(
                                    div()
                                        .w(px(16.))
                                        .text_color(rgb(if is_dir { ACCENT } else { TEXT_DIM }))
                                        .child(if is_dir { "▸" } else { "·" }),
                                )
                                .child(div().text_sm().child(row.name.clone()))
                                .child(
                                    div()
                                        .flex_1()
                                        .text_xs()
                                        .text_color(rgb(TEXT_DIM))
                                        .overflow_hidden()
                                        .child(row.path_label.clone()),
                                )
                                .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                                    this.open_search_result(target.clone(), is_dir, cx);
                                })),
                        )
                    })
                    .collect()
            }),
        )
        .flex_1()
    }

    fn render_search_pane(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        if !self.any_root_ready() {
            return div()
                .flex_1()
                .p_4()
                .text_sm()
                .text_color(rgb(TEXT_DIM))
                .child("still indexing — results will appear when ready…")
                .into_any_element();
        }
        if self.results.is_empty() {
            return div()
                .flex_1()
                .p_4()
                .text_sm()
                .text_color(rgb(TEXT_DIM))
                .child(format!("no matches for “{}”", self.query))
                .into_any_element();
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
        div()
            .flex()
            .items_center()
            .justify_between()
            .h(px(26.))
            .px_3()
            .border_t_1()
            .border_color(rgb(BORDER))
            .bg(rgb(BG_PANEL))
            .text_xs()
            .text_color(rgb(TEXT_DIM))
            .child(left)
            .child(self.index_status_text())
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let searching = !self.query.is_empty();
        div()
            .track_focus(&self.focus_handle)
            .on_action(|_: &CloseWindow, window, _| window.remove_window())
            .on_action(cx.listener(|this, _: &GoUp, _window, cx| this.go_up(cx)))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _window, cx| {
                this.handle_key(event, cx);
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
                    } else {
                        self.render_file_list(cx).into_any_element()
                    }),
            )
            .child(self.render_status_bar())
    }
}

fn main() {
    Application::new().run(|cx: &mut App| {
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.bind_keys([
            #[cfg(target_os = "macos")]
            KeyBinding::new("cmd-q", Quit, None),
            #[cfg(target_os = "macos")]
            KeyBinding::new("cmd-w", CloseWindow, None),
            #[cfg(target_os = "macos")]
            KeyBinding::new("cmd-up", GoUp, None),
            #[cfg(not(target_os = "macos"))]
            KeyBinding::new("ctrl-q", Quit, None),
            #[cfg(not(target_os = "macos"))]
            KeyBinding::new("ctrl-w", CloseWindow, None),
            #[cfg(not(target_os = "macos"))]
            KeyBinding::new("alt-up", GoUp, None),
        ]);
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
                    workspace.focus_handle.focus(window);
                    workspace
                })
            },
        )
        .expect("failed to open the main window");
    });
}

//! The Filex application shell: the [`Workspace`] root view, its state
//! types, and the `run` entry point. The `impl Workspace` surface is split
//! across the sibling `workspace::*` modules by concern; each is a plain
//! `impl Workspace` block reaching shared imports via `use super::*`.

use std::ops::Range;
use std::path::{Path, PathBuf};

use gpui::{
    App, Bounds, ClickEvent, Context, ExternalPaths, FocusHandle, Focusable as _, KeyBinding,
    KeyDownEvent, MouseButton, MouseDownEvent, Pixels, Point, ScrollStrategy, SharedString,
    UniformListScrollHandle, Window, WindowAppearance, WindowBounds, WindowOptions, actions, div,
    prelude::*, px, size, uniform_list,
};

use filex::drives::Drive;
use filex::listing::{Entry, format_modified, format_size, read_dir_sorted};
use filex::ops::{self, FileOp};
use filex::recents::Recents;
use filex::search::filter::Filter;
use filex::selection::Selection;
use filex::settings::{AccentColor, Density, FolderIcon, SortBy, ThemeMode, ViewMode};
use filex::tags::{PlatformTags, Tag, TagColor, TagStore as _};

use crate::settings_store::{SettingsEvent, SettingsStore};
use crate::thumbnails::{self, ThumbnailState};
use crate::ui;
use crate::ui::search_input::{self, SearchInput, SearchInputEvent};
use crate::ui::theme::{ActiveTheme as _, Theme};
pub use app::run;
use filex::listing::FileKind;
use platform::{open_with_default_app, open_with_dialog, open_with_supported};
use state::*;

actions!(
    filex,
    [
        Quit,
        GoUp,
        GoBack,
        GoForward,
        Refresh,
        ToggleSettings,
        TogglePreview,
        QuickLook,
        NewTab,
        CloseTab,
        NextTab,
        PrevTab,
        RenameSelected,
        DeleteSelected,
        Undo,
        FocusSearch,
        FocusPath,
        SubmitPath,
        CancelPath,
        SelectPrevious,
        SelectNext,
        ExtendPrevious,
        ExtendNext,
        OpenSelected,
        ToggleShortcuts,
        ToggleView
    ]
);

/// The payload of an in-app file drag. Both the value a drop target
/// matches by type in `on_drop` and, via [`Render`], the pill that follows
/// the cursor. `position` is the cursor offset gpui hands the drag
/// constructor; the pill offsets by it to sit under the mouse.
#[derive(Clone)]
struct DragItems {
    paths: Vec<PathBuf>,
    label: SharedString,
    theme: Theme,
    position: Point<Pixels>,
}

impl DragItems {
    fn at(mut self, position: Point<Pixels>) -> Self {
        self.position = position;
        self
    }
}

impl gpui::Render for DragItems {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        div()
            .pl(self.position.x + px(12.))
            .pt(self.position.y + px(8.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .px_2()
                    .py_0p5()
                    .rounded_md()
                    .bg(theme.accent)
                    .text_color(theme.on_accent)
                    .text_xs()
                    .shadow_md()
                    .child(self.label.clone()),
            )
    }
}

/// Metadata for the details panel, fetched lazily off-thread per
/// selection (created time + image dimensions cost a stat / header read
/// that browse doesn't already do).
struct PreviewMeta {
    /// The item this describes — guards against a stale async result
    /// landing after the selection moved on.
    path: PathBuf,
    size: u64,
    modified: Option<std::time::SystemTime>,
    created: Option<std::time::SystemTime>,
    /// Pixel dimensions for images; `None` for everything else.
    dimensions: Option<(u32, u32)>,
}

/// Stat `path` and, for images, read its pixel dimensions from the
/// header. Blocking — run on the background executor.
fn fetch_preview_meta(path: &Path) -> PreviewMeta {
    let (mut size, mut modified, mut created) = (0, None, None);
    if let Ok(md) = std::fs::metadata(path) {
        size = md.len();
        modified = md.modified().ok();
        created = md.created().ok();
    }
    // `image_dimensions` reads only the header and errors on non-images.
    let dimensions = image::image_dimensions(path).ok();
    PreviewMeta {
        path: path.to_path_buf(),
        size,
        modified,
        created,
        dimensions,
    }
}

const SEARCH_RESULT_LIMIT: usize = 100;

/// How long the query must hold still before a scan starts. Sized against
/// typing, not the scan: ~50 ms falls below a fast typist's inter-key gap
/// only at the tail of a burst, so the scan fires once per word rather
/// than per character, and stays imperceptible on the final keystroke —
/// the one the user is waiting on.
const SEARCH_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(50);

/// Latency at or above which an operation logs at `warn` rather than
/// `debug`. A shipped `.app` launched by double-click has no `RUST_LOG`,
/// so `debug` never writes — but a 10-second search needs to leave a
/// trace. Set well above a healthy scan so normal use stays silent.
const SLOW_OP_MS: u64 = 500;

/// Singular or plural item label for plans and jobs.
fn plural_items(n: usize) -> &'static str {
    if n == 1 { "item" } else { "items" }
}

/// One Magic-plan op as a review row: the three cells (source name, the
/// folder it lives in, where it's going) plus the full
/// `source → destination` string for the hover tooltip. The target is
/// `None` for a delete — inventing "→ Trash" would imply a path the op
/// does not have.
struct PlanRow {
    name: SharedString,
    location: SharedString,
    dest: Option<SharedString>,
    tooltip: SharedString,
}

fn describe_op(op: &FileOp) -> PlanRow {
    let file_name = |path: &Path| -> String {
        path.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    };
    let parent = |path: &Path| -> String {
        path.parent()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    };
    match op {
        FileOp::Delete { path } => PlanRow {
            name: file_name(path).into(),
            location: parent(path).into(),
            dest: None,
            tooltip: format!("{} → Trash", path.display()).into(),
        },
        FileOp::Move { from, to } | FileOp::Copy { from, to } => {
            // Full destination path when the plan retargeted to dodge a
            // collision (`ROADMAP.md` → `ROADMAP 2.md`) so the rename is
            // visible; otherwise just the folder, since the name is
            // already in the source cell. No "→ " prefix — the arrow is a
            // fixed-width gutter column in `op_row`, which is what aligns
            // every row's destination.
            let renamed = to.file_name() != from.file_name();
            let dest = if renamed {
                to.display().to_string()
            } else {
                to.parent().unwrap_or(to.as_path()).display().to_string()
            };
            PlanRow {
                name: file_name(from).into(),
                location: parent(from).into(),
                dest: Some(dest.into()),
                tooltip: format!("{} → {}", from.display(), to.display()).into(),
            }
        }
        FileOp::Rename { path, new_name } => PlanRow {
            name: file_name(path).into(),
            location: parent(path).into(),
            // Bare, for the same reason as Move/Copy above.
            dest: Some(SharedString::from(new_name.clone())),
            tooltip: format!("{} → {new_name}", path.display()).into(),
        },
    }
}

/// Whether dragging `src` into directory `dest` is a meaningful move.
/// Rejects the no-op (`src` already lives in `dest`) and the recursion
/// (`dest` is `src` or nested inside it). Path-only, no filesystem.
fn is_valid_drop(dest: &Path, src: &Path) -> bool {
    src.parent() != Some(dest) && !dest.starts_with(src)
}

fn describe_items(items: &[(PathBuf, String)]) -> Option<String> {
    match items {
        [] => None,
        [(_, name)] => Some(format!("“{name}”")),
        _ => Some(format!("{} items", items.len())),
    }
}

/// Migrate the sidecar tag index so tags follow a just-completed file op.
/// Logs rather than fails — a tag mishap must never derail the file
/// operation. Runs on the background executor (it persists).
fn migrate_tags(tags: &PlatformTags, applied: &mut ops::AppliedOp) {
    filex::daemon::ipc::notify_applied(applied);
    if let Err(err) = tags.apply_applied(applied) {
        tracing::error!("failed to migrate tags: {err:#}");
    }
}

struct Workspace {
    focus_handle: FocusHandle,
    /// Where the tab is (or is navigating to); set as a load starts.
    cwd: PathBuf,
    /// The directory `entries` were read from. Lags `cwd` while a load
    /// is in flight, so the previous listing stays on screen.
    listed_dir: PathBuf,
    entries: Vec<Entry>,
    load_error: Option<SharedString>,
    roots: Vec<IndexedRoot>,
    settings: gpui::Entity<SettingsStore>,
    _settings_subscription: gpui::Subscription,
    /// Last-known OS window appearance, tracked so the `system` theme
    /// mode can be re-resolved when settings change without a window in
    /// hand. Updated at startup and by the appearance observer.
    appearance: WindowAppearance,
    /// Transient user-facing message (e.g. why a root couldn't be added).
    notice: Option<SharedString>,
    /// The auto-updater's UI state, a slim banner above the status bar.
    /// Advanced by a background manifest check on launch (macOS/Linux);
    /// the Windows service path would report via IPC (pending).
    update_status: filex::update::UpdateStatus,
    #[cfg(target_os = "macos")]
    fda_missing: bool,
    /// Connection to the per-user filex-indexd daemon.
    /// Searches go over IPC and no local indexing happens.
    service: Option<std::sync::Arc<filex::daemon::ipc::Client>>,
    daemon_status: filex::daemon::ipc::Status,
    search_hits: std::collections::HashMap<PathBuf, filex::daemon::ipc::Hit>,
    search_more: bool,
    index_system_files: bool,
    search_page_query: Option<filex::daemon::ipc::Query>,
    search_paging: bool,
    search_client_id: u64,
    /// Mirror of the search input's content (the input entity owns it).
    query: String,
    search_input: gpui::Entity<SearchInput>,
    path_input: gpui::Entity<SearchInput>,
    _path_subscription: gpui::Subscription,
    path_error: Option<SharedString>,
    path_request: u64,
    path_loading: bool,
    _search_input_subscription: gpui::Subscription,
    /// The accent hex field in Settings (its own text input, parsed into a
    /// `Custom` accent on change).
    accent_hex: gpui::Entity<SearchInput>,
    _accent_hex_subscription: gpui::Subscription,
    results: Vec<SearchRow>,
    search_generation: u64,
    /// The Magic card's state when the query parses as a command.
    magic: Option<MagicState>,
    /// How the search bar decides between normal search and magic mode;
    /// see [`MagicMode`].
    magic_mode: MagicMode,
    /// Whether queries look everywhere or only under `cwd` — the search
    /// bar's scope dropdown. Applies to both normal and magic searches.
    search_scope: SearchScope,
    /// Anchor for the open scope dropdown (window coords from the click),
    /// or `None` when closed. Same overlay pattern as `context_menu`.
    scope_menu: Option<Point<Pixels>>,
    /// The user's well-known folders, for resolving "… to Documents".
    /// Read once at startup — `from_os` does environment lookups and has
    /// no business running per keystroke.
    user_dirs: filex::magic::UserDirs,
    /// Multi-selection over the browse list (directory entries). Belongs
    /// to the active tab (block 6); survives a search and is restored
    /// when the query clears.
    selection: Selection,
    /// Multi-selection over the global search results. Separate from
    /// `selection` so a search doesn't disturb the browse selection —
    /// search is a Spotlight-style overlay, not part of a tab.
    search_selection: Selection,
    /// The settings pane replaces the browse list while open (search
    /// still takes precedence, Spotlight-style).
    settings_open: bool,
    /// Whether the keyboard-shortcuts overlay is up (toggled by `?`).
    settings_section: preferences::SettingsSection,
    settings_focus: FocusHandle,
    recording_shortcut: Option<&'static str>,
    shortcut_error: Option<SharedString>,
    /// In-flight rename; `None` when no row is being edited.
    renaming: Option<RenameState>,
    /// Undo stack of completed file operations.
    journal: ops::Journal,
    /// Two-press delete confirmation: the set of paths armed by the
    /// first press; a second press on the same set deletes.
    pending_delete: Option<Vec<PathBuf>>,
    /// Internal file clipboard (cmd-c / cmd-x). Holds every path from
    /// the selection at copy/cut time.
    clipboard: Option<(Vec<PathBuf>, ClipMode)>,
    /// Open conflict dialog, if any.
    conflict: Option<ConflictState>,
    /// In-flight copy/move jobs (renames and deletes are instant and
    /// never appear here).
    jobs: Vec<Job>,
    next_job_id: u64,
    /// Open context menu, if any.
    context_menu: Option<ContextMenu>,
    browse_scroll: UniformListScrollHandle,
    /// Entries per `browse_scroll` row as of the last render: 1 in list
    /// view, the column count in grid view (whose rows are card strips).
    browse_cols: usize,
    results_scroll: UniformListScrollHandle,
    /// Scroll handle for the virtualized Magic plan list.
    magic_scroll: UniformListScrollHandle,
    /// Persistent drag/hover state for each list's Mac-style scrollbar.
    browse_scrollbar: ui::scrollbar::ScrollbarState,
    results_scrollbar: ui::scrollbar::ScrollbarState,
    magic_scrollbar: ui::scrollbar::ScrollbarState,
    thumbnails: thumbnails::Cache,
    /// Lazily-fetched metadata for the details panel's current item.
    preview_meta: Option<PreviewMeta>,
    /// Lazily attached native full-file viewer (separate from details/thumbnails).
    quick_look: Option<crate::quick_look::Viewer>,
    /// The lead item's tags, cached so rendering never reads the store
    /// (which on macOS is an xattr syscall). Refreshed off-thread when the
    /// selection changes or an edit lands.
    preview_tags: Vec<Tag>,
    /// Open tag editor in the details panel, if any.
    tag_editor: Option<TagEditor>,
    /// Distinct tags across the store, for the sidebar TAGS section.
    /// Cached (rendering never scans the store) and refreshed off-thread
    /// whenever tags change.
    sidebar_tags: Vec<Tag>,
    /// All browse tabs. `tabs[active_tab]` is stale — that tab's live
    /// state is the fields above; the others are real snapshots.
    tabs: Vec<TabSnapshot>,
    active_tab: usize,
    /// Tab indices most-recently-used first (`tab_mru[0]` is `active_tab`
    /// once a switch settles). Ctrl-Tab walks this order, not the strip's
    /// positional order.
    tab_mru: Vec<usize>,
    /// Position within `tab_mru` during a Ctrl-Tab cycle, so repeated
    /// presses walk the frozen recency order instead of reshuffling each
    /// hop. Cleared on open/close/click (see [`Workspace::commit_mru`]).
    tab_cycle: Option<usize>,
    /// Back/forward navigation history for the active tab.
    history_back: Vec<PathBuf>,
    history_forward: Vec<PathBuf>,
    /// Recently-opened folders/files (local-only usage log).
    recents: Recents,
    /// Sidecar tag index: enumeration source for the sidebar TAGS section
    /// and the `tag:` filter, and the store whose path keys our file ops
    /// migrate. Shared into background closures, which persist it.
    tags: std::sync::Arc<PlatformTags>,
    /// Mounted volumes with capacity, refreshed on a slow timer.
    drives: Vec<Drive>,
    /// Pending debounced scan from [`Workspace::update_search`]. Held so
    /// the next keystroke cancels it by dropping it — a `gpui::Task`
    /// cancels on drop.
    search_debounce: Option<gpui::Task<()>>,
    /// Cancel flag for the *in-flight* scan. Dropping the debounce task
    /// only stops a scan that hasn't started; this stops one that has. A
    /// new search sets the old flag and installs a fresh one.
    search_cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

mod app;
mod file_ops;
mod input;
mod location;
mod navigation;
mod platform;
mod preferences;
mod quick_look;
mod render;
mod render_lists;
mod render_menus;
mod roots;
mod search;
mod services;
mod shortcuts;
mod sidebar;
mod state;
mod tabs;
mod tags;

impl Workspace {
    /// Resolve the color theme from the `theme` setting + the OS
    /// appearance and install it as the global `cx.theme()` reads. Called
    /// at startup, on settings change, and when the appearance flips.
    pub(super) fn apply_theme(&self, cx: &mut Context<Self>) {
        let settings = self.settings.read(cx).settings();
        let (mode, accent, density) = (settings.theme, settings.accent, settings.density);
        let theme = Theme::resolve(mode, self.appearance, accent).with_density(density);
        cx.set_global(theme);
        cx.notify();
    }

    pub(super) fn new(cx: &mut Context<Self>) -> Self {
        let cwd = std::env::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        let search_input = cx.new(SearchInput::new);
        search_input.update(cx, |input, _| input.set_propagate_empty(false));
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
        let path_input = cx.new(SearchInput::new);
        path_input.update(cx, |input, cx| {
            input.set_propagate_empty(false);
            input.set_placeholder("Enter a folder path", cx);
            input.set_text(cwd.to_string_lossy().into_owned(), cx);
        });
        let path_subscription = cx.subscribe(&path_input, |this, _, event, cx| {
            if matches!(event, SearchInputEvent::Changed(_)) {
                if this.path_loading {
                    this.path_request = this.path_request.wrapping_add(1);
                    this.path_loading = false;
                }
                this.path_error = None;
                cx.notify();
            }
        });
        let accent_hex = cx.new(SearchInput::new);
        accent_hex.update(cx, |input, _| input.set_propagate_empty(false));
        accent_hex.update(cx, |input, cx| input.set_placeholder("#RRGGBB", cx));
        let accent_hex_subscription = cx.subscribe(&accent_hex, |this, _input, event, cx| {
            if let SearchInputEvent::Changed(text) = event
                && let Some(hex) = ui::theme::parse_hex(text)
            {
                this.settings.update(cx, |store, cx| {
                    store.update(cx, |s| s.accent = filex::settings::AccentColor::Custom(hex));
                });
            }
        });
        let settings = cx.new(SettingsStore::new);
        // Settings changes re-derive everything visible that depends on
        // them (the hidden-file filter on the browse list, and the
        // active color theme).
        let settings_subscription =
            cx.subscribe(&settings, |this, _store, event, cx| match event {
                SettingsEvent::Changed(previous) => {
                    let include_system = this.settings.read(cx).settings().index_system_files;
                    if this.index_system_files != include_system {
                        this.index_system_files = include_system;
                        if let Some(client) = this.service.clone() {
                            cx.background_executor()
                                .spawn(async move {
                                    let _ = client.call(filex::daemon::ipc::Command::Reconcile);
                                })
                                .detach();
                        }
                    }
                    let current = this.settings.read(cx).settings();
                    let reload = previous.sort != current.sort
                        || previous.show_hidden_files != current.show_hidden_files;
                    let remap = previous.keyboard_shortcuts != current.keyboard_shortcuts;
                    if remap {
                        let overrides = current.keyboard_shortcuts.clone();
                        shortcuts::install(&overrides, cx);
                    }
                    if reload {
                        let cwd = this.cwd.clone();
                        this.load_dir(&cwd, cx);
                    }
                    this.apply_theme(cx);
                }
            });
        let recents = filex::recents::default_recents_file()
            .map(|file| Recents::load(&file))
            .unwrap_or_default();
        shortcuts::install(&settings.read(cx).settings().keyboard_shortcuts.clone(), cx);
        let index_system_files = settings.read(cx).settings().index_system_files;
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            cwd: cwd.clone(),
            listed_dir: PathBuf::new(),
            entries: Vec::new(),
            load_error: None,
            roots: Vec::new(),
            settings,
            _settings_subscription: settings_subscription,
            // Corrected the moment a window exists (main's open-window
            // closure) and kept current by the appearance observer.
            appearance: WindowAppearance::Dark,
            notice: None,
            #[cfg(target_os = "macos")]
            fda_missing: false,
            service: None,
            daemon_status: filex::daemon::ipc::Status::default(),
            search_hits: Default::default(),
            search_more: false,
            index_system_files,
            search_page_query: None,
            search_paging: false,
            search_client_id: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos() as u64)
                ^ u64::from(std::process::id()),
            query: String::new(),
            search_input,
            path_input,
            _path_subscription: path_subscription,
            path_error: None,
            path_request: 0,
            path_loading: false,
            _search_input_subscription: subscription,
            accent_hex,
            _accent_hex_subscription: accent_hex_subscription,
            results: Vec::new(),
            search_generation: 0,
            magic: None,
            magic_mode: MagicMode::Auto,
            search_scope: SearchScope::Anywhere,
            scope_menu: None,
            settings_section: preferences::SettingsSection::Appearance,
            settings_focus: cx.focus_handle(),
            recording_shortcut: None,
            shortcut_error: None,
            user_dirs: filex::magic::UserDirs::from_os(),
            selection: Selection::default(),
            search_selection: Selection::default(),
            settings_open: false,
            renaming: None,
            journal: ops::Journal::default(),
            pending_delete: None,
            clipboard: None,
            conflict: None,
            jobs: Vec::new(),
            next_job_id: 0,
            context_menu: None,
            browse_scroll: UniformListScrollHandle::new(),
            browse_cols: 1,
            results_scroll: UniformListScrollHandle::new(),
            magic_scroll: UniformListScrollHandle::new(),
            browse_scrollbar: ui::scrollbar::ScrollbarState::new(),
            results_scrollbar: ui::scrollbar::ScrollbarState::new(),
            magic_scrollbar: ui::scrollbar::ScrollbarState::new(),
            thumbnails: thumbnails::Cache::default(),
            preview_meta: None,
            quick_look: None,
            preview_tags: Vec::new(),
            tag_editor: None,
            sidebar_tags: Vec::new(),
            tabs: vec![TabSnapshot::placeholder()],
            active_tab: 0,
            tab_mru: vec![0],
            tab_cycle: None,
            history_back: Vec::new(),
            history_forward: Vec::new(),
            recents,
            tags: std::sync::Arc::new(PlatformTags::load(
                filex::tags::default_tags_file()
                    .unwrap_or_else(|| std::env::temp_dir().join("filex").join("tags.json")),
            )),
            drives: Vec::new(),
            search_debounce: None,
            search_cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            update_status: filex::update::UpdateStatus::default(),
        };
        this.load_dir(&cwd, cx);
        this.spawn_tag_prune(cx);
        this.refresh_sidebar_tags(cx);
        this.spawn_crash_upload(cx);
        this.spawn_service_probe(cx);
        this.spawn_fda_check(cx);
        this.spawn_drive_refresh(cx);
        #[cfg(feature = "observability")]
        this.spawn_resource_sampling(cx);
        // Update discovery belongs to the UI on every platform.
        #[cfg(feature = "updater")]
        this.spawn_update_check(cx);
        this
    }
}

#[cfg(test)]
mod magic_ui_tests {
    use super::*;
    use filex::magic::{Plan, Verb};

    /// Regression for the real case in dogfooding: `rename gravloc to …`
    /// returned 148 rows, of which 4 actually contained "gravloc". The
    /// rest were subsequence matches on unrelated files, and every one of
    /// them would have been renamed on confirm.
    fn state(outcome: Result<Plan, filex::magic::PlanError>, checked: Vec<bool>) -> MagicState {
        MagicState {
            source_query: "delete screenshots older than 30 days".into(),
            command: filex::magic::parse("delete screenshots older than 30 days", 1_785_067_200)
                .expect("fixture should parse"),
            outcome: Some(outcome),
            loading: false,
            error: None,
            progress: Default::default(),
            checked,
        }
    }

    fn plan(ops: Vec<FileOp>) -> Plan {
        Plan {
            verb: Verb::Delete,
            skipped: 0,
            ops,
        }
    }

    fn delete(path: &str) -> FileOp {
        FileOp::Delete {
            path: PathBuf::from(path),
        }
    }

    #[test]
    fn magic_status_counts_checkboxes_and_blocks_loading_or_failed_plans() {
        let mut state = state(
            Ok(plan(vec![delete("/a"), delete("/b")])),
            vec![false, true],
        );
        assert_eq!(state.status(), "1 of 2 selected");
        state.checked.fill(false);
        assert_eq!(state.status(), "0 of 2 selected");
        state.loading = true;
        assert!(state.selected_ops().is_empty());
        state
            .progress
            .scanned
            .store(4096, std::sync::atomic::Ordering::Relaxed);
        assert!(state.status().contains("4096 entries checked"));
        state.error = Some("Disconnected".into());
        assert!(state.status().contains("Disconnected"));
        assert!(!state.status().contains("Finding"));
    }

    #[test]
    fn refreshed_plan_preserves_exclusions_when_rows_change_order() {
        let mut state = state(
            Ok(plan(vec![delete("/a"), delete("/b")])),
            vec![false, true],
        );
        state.install_plan(Ok(plan(vec![delete("/b"), delete("/c"), delete("/a")])));
        assert_eq!(state.checked, vec![true, false, false]);
        assert_eq!(state.selected_ops(), vec![delete("/b")]);
    }

    #[test]
    fn only_checked_ops_are_executed() {
        // The whole point of the review step: unchecking a row must keep
        // that file out of the batch that runs.
        let state = state(
            Ok(plan(vec![
                delete("/a.png"),
                delete("/b.png"),
                delete("/c.png"),
            ])),
            vec![true, false, true],
        );
        assert_eq!(
            state.selected_ops(),
            vec![delete("/a.png"), delete("/c.png")]
        );
    }

    #[test]
    fn nothing_runs_from_an_unresolved_or_failed_plan() {
        // A plan that never resolved, or resolved to an error, must not
        // produce ops even if `checked` is somehow non-empty.
        let mut unresolved = state(Ok(plan(vec![delete("/a.png")])), vec![true]);
        unresolved.outcome = None;
        assert!(unresolved.selected_ops().is_empty());

        let failed = state(Err(filex::magic::PlanError::NoMatches), vec![true]);
        assert!(failed.selected_ops().is_empty());
    }

    #[test]
    fn all_unchecked_yields_no_ops() {
        let state = state(Ok(plan(vec![delete("/a.png")])), vec![false]);
        assert!(state.selected_ops().is_empty());
    }

    #[test]
    fn a_checked_flag_without_a_matching_op_cannot_invent_one() {
        // `checked` is parallel to `ops`; zip must not run past the ops.
        let state = state(Ok(plan(vec![delete("/a.png")])), vec![true, true, true]);
        assert_eq!(state.selected_ops(), vec![delete("/a.png")]);
    }

    #[test]
    fn delete_rows_show_the_source_folder_and_no_destination() {
        let row = describe_op(&delete("/photos/shot.png"));
        assert_eq!(row.name, SharedString::from("shot.png"));
        // The folder the file lives in is shown so identical names in
        // different folders are distinguishable.
        assert_eq!(
            row.location.to_string(),
            Path::new("/photos").display().to_string()
        );
        assert_eq!(row.dest, None);
    }

    #[test]
    fn transfer_rows_show_source_folder_and_destination_folder() {
        let op = FileOp::Move {
            from: "/a/shot.png".into(),
            to: "/b/Archive/shot.png".into(),
        };
        let row = describe_op(&op);
        assert_eq!(row.name, SharedString::from("shot.png"));
        assert_eq!(
            row.location.to_string(),
            Path::new("/a").display().to_string()
        );
        // Same name at the destination → show the folder, not the file.
        // Bare: the "→" is a gutter column in the row, not part of the text.
        assert_eq!(
            row.dest.unwrap().to_string(),
            Path::new("/b/Archive").display().to_string()
        );
    }

    #[test]
    fn a_retargeted_transfer_row_shows_the_full_new_path() {
        // Collision keep-both: the destination name differs from the
        // source, so the whole retargeted path is shown, not just the
        // folder — otherwise the rename would be invisible.
        let op = FileOp::Move {
            from: "/a/shot.png".into(),
            to: "/b/shot 2.png".into(),
        };
        let row = describe_op(&op);
        assert_eq!(
            row.dest.unwrap().to_string(),
            Path::new("/b/shot 2.png").display().to_string()
        );
    }

    #[test]
    fn rename_rows_show_the_new_name() {
        let op = FileOp::Rename {
            path: "/a/img01.png".into(),
            new_name: "shot-1.png".into(),
        };
        let row = describe_op(&op);
        assert_eq!(row.name, SharedString::from("img01.png"));
        assert_eq!(row.dest.unwrap().to_string(), "shot-1.png");
    }

    /// Regression: the destination text used to carry a literal "→ ".
    /// `op_row` now renders the arrow as its own fixed-width gutter so the
    /// destination column starts at the same x on every row, so an arrow
    /// left in the text would both double it on screen and re-ragged the
    /// column it was introduced to align.
    #[test]
    fn destination_text_carries_no_arrow_glyph() {
        let ops = [
            FileOp::Move {
                from: "/a/shot.png".into(),
                to: "/b/Archive/shot.png".into(),
            },
            FileOp::Copy {
                from: "/a/shot.png".into(),
                to: "/b/shot 2.png".into(),
            },
            FileOp::Rename {
                path: "/a/img01.png".into(),
                new_name: "shot-1.png".into(),
            },
        ];
        for op in &ops {
            let row = describe_op(op);
            let dest = row.dest.expect("these verbs all have a destination");
            assert!(
                !dest.contains('→'),
                "destination text should be bare, got {dest:?}"
            );
        }
    }

    #[test]
    fn item_counts_read_naturally() {
        assert_eq!(plural_items(1), "item");
        assert_eq!(plural_items(0), "items");
        assert_eq!(plural_items(2), "items");
    }
}

#[cfg(test)]
mod drop_target_tests {
    use super::is_valid_drop;
    use std::path::Path;

    #[test]
    fn accepts_a_move_into_a_sibling_folder() {
        assert!(is_valid_drop(
            Path::new("/home/user/Archive"),
            Path::new("/home/user/report.pdf")
        ));
    }

    #[test]
    fn rejects_an_item_already_in_the_destination() {
        // The file is already directly inside the target — a move would
        // be a no-op, so the drop must be filtered out.
        assert!(!is_valid_drop(
            Path::new("/home/user"),
            Path::new("/home/user/report.pdf")
        ));
    }

    #[test]
    fn rejects_dropping_a_folder_onto_itself() {
        assert!(!is_valid_drop(
            Path::new("/home/user/docs"),
            Path::new("/home/user/docs")
        ));
    }

    #[test]
    fn rejects_dropping_a_folder_into_its_own_descendant() {
        // Moving /a into /a/b/c would try to relocate a directory inside
        // its own subtree — nonsensical, and must be refused.
        assert!(!is_valid_drop(Path::new("/a/b/c"), Path::new("/a")));
    }

    #[test]
    fn allows_moving_a_child_out_to_a_deeper_unrelated_path() {
        // The destination is nested, but not under the source, so it's a
        // legitimate move.
        assert!(is_valid_drop(
            Path::new("/a/b/c"),
            Path::new("/other/file.txt")
        ));
    }
}

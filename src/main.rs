use std::ops::Range;
use std::path::{Path, PathBuf};

use futures::StreamExt as _;
use gpui::{
    App, Application, Bounds, ClickEvent, Context, ExternalPaths, FocusHandle, Focusable as _,
    KeyBinding, KeyDownEvent, MouseButton, MouseDownEvent, Pixels, Point, ScrollStrategy,
    SharedString, TitlebarOptions, UniformListScrollHandle, Window, WindowAppearance, WindowBounds,
    WindowOptions, actions, div, prelude::*, px, size, uniform_list,
};

use filex::drives::Drive;
use filex::index::watcher::SharedIndex;
use filex::index::{LiveIndex, MatchKind, VolumeIndex, manager, start_live_index};
use filex::listing::{Entry, format_modified, format_size, path_segments, read_dir_sorted};
use filex::ops::{self, FileOp};
use filex::recents::Recents;
use filex::search_filter::Filter;
use filex::selection::Selection;
use filex::settings::{SortBy, ThemeMode, ViewMode};
use filex::tags::{PlatformTags, Tag, TagColor, TagStore as _};

mod settings_store;
mod thumbnails;
mod ui;
use filex::listing::FileKind;
use settings_store::{SettingsEvent, SettingsStore};
use thumbnails::ThumbnailState;
use ui::search_input::{self, SearchInput, SearchInputEvent};
use ui::theme::{ActiveTheme as _, Theme};

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
        NewTab,
        CloseTab,
        NextTab,
        PrevTab,
        RenameSelected,
        DeleteSelected,
        Undo
    ]
);

/// The payload of an in-app file drag: the paths being moved. It is both
/// the value handed to a drop target (matched by type in `on_drop`) and,
/// via its [`Render`] impl, the little pill that follows the cursor while
/// dragging. `position` is the cursor offset gpui hands the drag
/// constructor; the pill offsets itself by it so it sits under the mouse.
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

/// The update affordance for this platform's UI banner. macOS copies a
/// `brew` command; Linux opens the releases page to re-download the
/// tarball. (Windows applies via the service, so it isn't handled here.)
#[cfg(all(not(target_os = "windows"), feature = "updater"))]
fn platform_affordance() -> filex::update::UpdateAffordance {
    #[cfg(target_os = "macos")]
    {
        filex::update::UpdateAffordance::RunCommand("brew upgrade filex".to_string())
    }
    #[cfg(target_os = "linux")]
    {
        // TODO(block 5): point at the real releases URL once the repo is
        // published.
        filex::update::UpdateAffordance::OpenUrl(
            "https://github.com/NayanVR/filex/releases/latest".to_string(),
        )
    }
}

const SEARCH_RESULT_LIMIT: usize = 500;

/// How long the query must hold still before a scan starts.
///
/// Sized against typing, not against the scan: ~50 ms is below a fast
/// typist's inter-key interval only at the tail of a burst, so the scan
/// fires once the word is finished rather than once per character. Short
/// enough to stay imperceptible on the final keystroke — the one the user
/// is actually waiting on — and the scan it guards costs several times
/// this on a large index, so trading it away is strictly a win.
const SEARCH_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(50);

/// Latency at or above which an operation logs itself at `warn` rather
/// than `debug`. The point is a shipped `.app` launched by double-click
/// has no `RUST_LOG`, so `debug` never writes — but a user staring at a
/// 10-second search needs *something* in the log. Set well above a
/// healthy scan (tens of ms) so normal use stays silent and only genuine
/// stalls speak up.
const SLOW_OP_MS: u64 = 500;

/// Minimum gap between refreshes triggered by *filesystem* events.
///
/// Deliberately far longer than [`SEARCH_DEBOUNCE`], because the two are
/// answering different questions. A keystroke is the user waiting on us;
/// an FSEvent is a browser cache or a `node_modules` write, and nobody is
/// waiting on it. Refreshing per burst meant a full parallel scan of the
/// arena *plus* an O(n) live-entry count many times a second on an active
/// home directory — the app pinning cores while sitting idle.
///
/// A trailing throttle rather than a debounce: under continuous churn a
/// debounce would keep pushing its deadline back and never refresh at
/// all.
const FS_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// A short label for a set of files: the single name quoted, or a count.
/// `None` for an empty set (callers use it to bail early).
/// "item" / "items" — Magic card headings and job labels count files.
fn plural_items(n: usize) -> &'static str {
    if n == 1 { "item" } else { "items" }
}

/// One Magic-plan op as a review row: what it acts on, and where that
/// ends up. The target is `None` for a delete — there is no destination,
/// and inventing "→ Trash" would imply a path the op does not have.
/// The three cells of a plan row — source name, the folder it lives in,
/// and where it is going (`None` for a delete) — plus the full
/// `source → destination` string for the row's hover tooltip.
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
            // Show the full destination path when the plan retargeted this
            // file to dodge a collision (`ROADMAP.md` → `ROADMAP 2.md`),
            // so the rename is visible; otherwise the destination folder,
            // since the name is already in the source cell.
            let renamed = to.file_name() != from.file_name();
            let dest = if renamed {
                format!("→ {}", to.display())
            } else {
                format!("→ {}", to.parent().unwrap_or(to.as_path()).display())
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
            dest: Some(format!("→ {new_name}").into()),
            tooltip: format!("{} → {new_name}", path.display()).into(),
        },
    }
}

/// Whether dragging `src` into directory `dest` is a meaningful move.
/// Rejects the two degenerate cases: `src` already lives directly in
/// `dest` (the move would be a no-op), and `dest` is `src` itself or a
/// folder nested inside it (which would try to move a directory into its
/// own subtree). Path-only — it does not touch the filesystem.
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

/// Migrate the sidecar tag index for a just-completed file op (moving,
/// copying, or dropping the file's tags to follow it), logging rather
/// than failing — a tag mishap must never derail the file operation
/// itself. Runs on the background executor (it persists).
fn migrate_tags(tags: &PlatformTags, applied: &mut ops::AppliedOp) {
    if let Err(err) = tags.apply_applied(applied) {
        tracing::error!("failed to migrate tags: {err:#}");
    }
}

/// Build search rows for a set of tagged paths (a `tag:`-only query,
/// where there's no filename text to rank). Paths that no longer exist
/// are skipped — a light visual prune — and the list is capped at
/// `limit`. Blocking (it stats each path) — call off the UI thread.
fn rows_from_tagged_paths(paths: Vec<PathBuf>, limit: usize) -> Vec<SearchRow> {
    paths
        .into_iter()
        .filter_map(|path| {
            let meta = std::fs::symlink_metadata(&path).ok()?;
            let name = path.file_name()?.to_string_lossy().into_owned();
            Some(SearchRow {
                name: name.into(),
                path_label: path.display().to_string().into(),
                is_dir: meta.is_dir(),
                target: path,
            })
        })
        .take(limit)
        .collect()
}

/// Keep only the rows whose path carries every one of `required` tags
/// (the filename-search ∩ `tag:` intersection). A no-op when `required`
/// is empty. Blocking (scans the sidecar) — call off the UI thread.
/// May a hit feed a Magic plan?
///
/// Fuzzy (subsequence) hits are excluded from command queries, because a
/// command query's rows *are* its plan — every row becomes a file the
/// batch renames, moves or deletes. Subsequence matching is far too loose
/// to carry that weight: `gravloc` is a subsequence of
/// `xstate-graph.development.cjs.js`, which is a fine thing to surface
/// when someone is looking for a file and an unacceptable thing to rename
/// on their behalf. Ordinary searches keep every fuzzy hit.
fn usable_in_plan(kind: MatchKind, command_query: bool) -> bool {
    !command_query || kind != MatchKind::Fuzzy
}

fn filter_rows_by_tags(
    rows: Vec<SearchRow>,
    store: &PlatformTags,
    required: &[String],
) -> Vec<SearchRow> {
    if required.is_empty() {
        return rows;
    }
    let tagged: std::collections::HashSet<PathBuf> =
        store.paths_with_all_tags(required).into_iter().collect();
    rows.into_iter()
        .filter(|row| tagged.contains(&row.target))
        .collect()
}

/// A lock-free snapshot of the index (optimization B). Derefs (through the
/// `Arc`) to `VolumeIndex`; hold it only as long as the read.
fn read_index(index: &SharedIndex) -> arc_swap::Guard<std::sync::Arc<VolumeIndex>> {
    index.load()
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
        Self {
            path,
            label,
            state: RootState::Building,
        }
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

/// What a copy/cut put on the internal file clipboard. This is app
/// state, not the OS clipboard — pasting files copied in other apps is
/// out of scope for now.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ClipMode {
    Copy,
    Cut,
}

/// An operation blocked on an occupied destination, awaiting the
/// user's choice in the conflict dialog.
struct ConflictState {
    op: FileOp,
    dest: PathBuf,
}

/// What an open context menu is about.
enum MenuTarget {
    /// A file row — browse (`from_search: false`) or a search result.
    Entry {
        ix: usize,
        path: PathBuf,
        name: String,
        is_dir: bool,
        from_search: bool,
    },
    /// An indexed root in the sidebar.
    Root { path: PathBuf },
    /// A pinned folder in the sidebar's Favorites section.
    Favorite { path: PathBuf },
}

struct ContextMenu {
    position: Point<Pixels>,
    target: MenuTarget,
}

/// One background file-operation job with live progress; shown in the
/// jobs bar while running.
struct Job {
    id: u64,
    label: SharedString,
    progress: std::sync::Arc<ops::OpProgress>,
}

/// How the search bar chooses between normal search and magic mode
/// (`docs/design-magic-mode.md` v2). Three states, not a bool, because
/// auto-switch and an explicit toggle both exist and can disagree: the
/// toggle has to be able to force magic mode *off* on a query that
/// auto-switch would otherwise light up, and *on* for a query that hasn't
/// parsed into a command yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MagicMode {
    /// The default. A command-shaped query with structured evidence flips
    /// into magic view on its own; anything else is a normal search. The
    /// query clearing resets to this.
    Auto,
    /// The user toggled magic on. The delete gate is dropped and the plan
    /// view is shown even before a command parses (it prompts for one).
    On,
    /// The user toggled magic off while it was showing. Stays a normal
    /// search even for a command-shaped query, until the toggle or a
    /// cleared query returns it to [`Auto`](Self::Auto).
    Off,
}

/// Where a query (normal or magic) looks: across every indexed root, or
/// only within the folder currently on screen. Chosen from the search
/// bar's scope dropdown; defaults to [`Anywhere`](Self::Anywhere).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchScope {
    /// Every indexed root — the index's whole reach.
    Anywhere,
    /// Only `cwd` and its subtree. Resolved per-scan against the index so
    /// it costs nothing when off, and a folder the index hasn't caught up
    /// to yet simply returns nothing.
    CurrentDir,
}

impl SearchScope {
    fn label(self) -> &'static str {
        match self {
            Self::Anywhere => "Anywhere",
            Self::CurrentDir => "Current Dir",
        }
    }
}

/// A parsed Magic command and the plan it resolved to, for the review
/// card (`docs/design-magic-mode.md` §3).
///
/// Held separately from `results` even though the two are computed from
/// the same search: `results` is what the user is *looking at*, while
/// `ops`/`checked` are what would actually run. Keeping them apart is
/// what lets a row be unchecked without disturbing the result list.
struct MagicState {
    command: filex::magic::Command,
    /// The resolved plan, or why there isn't one. `None` until the
    /// search backing it has landed — distinguishing "still looking"
    /// from "nothing matched" matters, because a root that is still
    /// indexing would otherwise sit there claiming the command matched
    /// no files.
    ///
    /// An error still shows a card: saying "no folder called Archive" is
    /// far more useful than silently showing nothing after the user
    /// typed a real command.
    outcome: Option<Result<filex::magic::Plan, filex::magic::PlanError>>,
    /// One flag per op in the plan, parallel to `Plan::ops`. Everything
    /// starts checked; the review step is about *removing* what you
    /// didn't mean, not opting in one file at a time.
    checked: Vec<bool>,
}

impl MagicState {
    /// The ops the user has left checked.
    fn selected_ops(&self) -> Vec<FileOp> {
        let Some(Ok(plan)) = &self.outcome else {
            return Vec::new();
        };
        plan.ops
            .iter()
            .zip(&self.checked)
            .filter(|(_, checked)| **checked)
            .map(|(op, _)| op.clone())
            .collect()
    }
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

/// An in-progress tag edit in the details panel: which file it targets,
/// the input owning the typed tag name, the currently-chosen color, and
/// (when editing an existing chip rather than adding) that tag's original
/// name so commit replaces it in place.
struct TagEditor {
    path: PathBuf,
    input: gpui::Entity<SearchInput>,
    color: Option<TagColor>,
    /// `Some(original_name)` when recoloring/renaming an existing tag;
    /// `None` when adding a new one.
    existing: Option<String>,
    /// Watches for Dismissed (escape) to cancel.
    _subscription: gpui::Subscription,
}

/// One browse tab's saved state (block 6). The *active* tab's state
/// lives directly on the [`Workspace`] (so the existing browse code is
/// untouched); this holds the inactive tabs, and the active slot is
/// refreshed from the live fields on every switch. Search is global and
/// not part of a tab. Transient per-tab UI (an in-progress rename, an
/// armed delete) is intentionally dropped on switch rather than saved.
struct TabSnapshot {
    cwd: PathBuf,
    entries: Vec<Entry>,
    load_error: Option<SharedString>,
    selection: Selection,
    scroll: UniformListScrollHandle,
    history_back: Vec<PathBuf>,
    history_forward: Vec<PathBuf>,
}

impl TabSnapshot {
    /// A blank slot; the active tab's slot always holds one of these
    /// until the next switch refreshes it from the live fields.
    fn placeholder() -> Self {
        Self {
            cwd: PathBuf::new(),
            entries: Vec::new(),
            load_error: None,
            selection: Selection::default(),
            scroll: UniformListScrollHandle::new(),
            history_back: Vec::new(),
            history_forward: Vec::new(),
        }
    }
}

struct Workspace {
    focus_handle: FocusHandle,
    cwd: PathBuf,
    entries: Vec<Entry>,
    load_error: Option<SharedString>,
    roots: Vec<RootSlot>,
    settings: gpui::Entity<SettingsStore>,
    _settings_subscription: gpui::Subscription,
    /// Last-known OS window appearance, tracked so the `system` theme
    /// mode can be re-resolved when settings change without a window in
    /// hand. Updated at startup and by the appearance observer.
    appearance: WindowAppearance,
    /// Transient user-facing message (e.g. why a root couldn't be added).
    notice: Option<SharedString>,
    /// The auto-updater's UI state, rendered as a slim banner above the
    /// status bar. Advanced by a background manifest check on launch
    /// (macOS/Linux); the Windows service path would report via IPC
    /// (pending — the banner stays `Idle` there for now).
    update_status: filex::update::UpdateStatus,
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
    /// The Magic card's state when the query parses as a command.
    magic: Option<MagicState>,
    /// How the search bar decides between normal search and magic mode.
    /// See [`MagicMode`] — the three states exist so the toggle can both
    /// force magic *on* and force it *off* against what auto-switch would
    /// otherwise do.
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
    results_scroll: UniformListScrollHandle,
    /// Scroll handle for the virtualized Magic plan list.
    magic_scroll: UniformListScrollHandle,
    thumbnails: std::collections::HashMap<PathBuf, ThumbnailState>,
    /// Lazily-fetched metadata for the details panel's current item.
    preview_meta: Option<PreviewMeta>,
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
    /// Back/forward navigation history for the active tab.
    history_back: Vec<PathBuf>,
    history_forward: Vec<PathBuf>,
    /// Recently-opened folders/files (local-only usage log).
    recents: Recents,
    /// Cached path → frecency score, derived from `recents` and handed to
    /// background search tasks for stage-B re-ranking. Rebuilt whenever
    /// `recents` changes rather than per keystroke — the scores decay on
    /// a 30-day half-life, so within a session it never goes stale.
    frecency: std::sync::Arc<std::collections::HashMap<PathBuf, f32>>,
    /// Sidecar tag index: the enumeration source for the sidebar TAGS
    /// section and the `tag:` filter, and (on every platform) the store
    /// whose path keys are migrated by our own file ops. Shared into
    /// background closures, which persist it off the UI thread.
    tags: std::sync::Arc<PlatformTags>,
    /// Mounted volumes with capacity, refreshed on a slow timer.
    drives: Vec<Drive>,
    /// Pending debounced scan from [`Workspace::update_search`]. Held so
    /// the next keystroke cancels it by dropping it — a `gpui::Task`
    /// cancels on drop.
    search_debounce: Option<gpui::Task<()>>,
    /// Cancel flag for the *in-flight* scan (the one already past the
    /// debounce and running on the background pool). Dropping the debounce
    /// task only stops a scan that hasn't started; this stops one that
    /// has. A new search sets the old flag and installs a fresh one — see
    /// [`Workspace::update_search`].
    search_cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// True while a filesystem-driven refresh is already scheduled. This
    /// is a *throttle*, not a debounce: further events during the window
    /// are dropped rather than pushing the deadline back, so continuous
    /// churn still refreshes on a fixed cadence instead of starving.
    fs_refresh_pending: bool,
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

    /// Resolve the color theme from the `theme` setting + the current OS
    /// appearance and install it as the global every component reads
    /// through `cx.theme()`. Called at startup, whenever settings
    /// change, and when the OS appearance flips.
    fn apply_theme(&self, cx: &mut Context<Self>) {
        let mode = self.settings.read(cx).settings().theme;
        cx.set_global(Theme::resolve(mode, self.appearance));
        cx.notify();
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
        // them (the hidden-file filter on the browse list, and the
        // active color theme).
        let settings_subscription =
            cx.subscribe(&settings, |this, _store, event, cx| match event {
                SettingsEvent::Changed => {
                    let cwd = this.cwd.clone();
                    this.load_dir(&cwd, cx);
                    this.apply_theme(cx);
                }
            });
        let recents = filex::recents::default_recents_file()
            .map(|file| Recents::load(&file))
            .unwrap_or_default();
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            cwd: cwd.clone(),
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
            #[cfg(target_os = "windows")]
            service: None,
            #[cfg(target_os = "windows")]
            service_status: Vec::new(),
            query: String::new(),
            search_input,
            _search_input_subscription: subscription,
            results: Vec::new(),
            search_generation: 0,
            magic: None,
            magic_mode: MagicMode::Auto,
            search_scope: SearchScope::Anywhere,
            scope_menu: None,
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
            results_scroll: UniformListScrollHandle::new(),
            magic_scroll: UniformListScrollHandle::new(),
            thumbnails: std::collections::HashMap::new(),
            preview_meta: None,
            preview_tags: Vec::new(),
            tag_editor: None,
            sidebar_tags: Vec::new(),
            tabs: vec![TabSnapshot::placeholder()],
            active_tab: 0,
            history_back: Vec::new(),
            history_forward: Vec::new(),
            frecency: std::sync::Arc::new(recents.score_table(filex::frecency::now_secs())),
            recents,
            tags: std::sync::Arc::new(PlatformTags::load(
                filex::tags::default_tags_file()
                    .unwrap_or_else(|| std::env::temp_dir().join("filex").join("tags.json")),
            )),
            drives: Vec::new(),
            search_debounce: None,
            search_cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            fs_refresh_pending: false,
            update_status: filex::update::UpdateStatus::default(),
        };
        this.load_dir(&cwd, cx);
        this.spawn_tag_prune(cx);
        this.refresh_sidebar_tags(cx);
        this.spawn_crash_upload(cx);
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
        this.spawn_drive_refresh(cx);
        // macOS/Linux check the manifest themselves (no service); Windows
        // learns of updates from filex-indexd instead.
        #[cfg(all(not(target_os = "windows"), feature = "updater"))]
        this.spawn_update_check(cx);
        this
    }

    /// Refresh the mounted-volume list on a slow timer (drives change
    /// rarely; enumerating them hits the disk, so never per-frame). The
    /// blocking enumeration runs on the background executor.
    fn spawn_drive_refresh(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                let drives = cx
                    .background_executor()
                    .spawn(async { filex::drives::list_drives() })
                    .await;
                let updated = this
                    .update(cx, |this, cx| {
                        if this.drives != drives {
                            this.drives = drives;
                            cx.notify();
                        }
                    })
                    .is_ok();
                if !updated {
                    break; // workspace dropped
                }
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(30))
                    .await;
            }
        })
        .detach();
    }

    /// Manifest URL for the UI-side "is there a newer version?" check.
    /// Per-OS, since each platform publishes its own manifest; the
    /// `latest/download/…` path always resolves to the newest release.
    #[cfg(all(target_os = "macos", feature = "updater"))]
    const UPDATE_MANIFEST_URL: &'static str =
        "https://github.com/NayanVR/filex/releases/latest/download/filex-macos.json";
    #[cfg(all(target_os = "linux", feature = "updater"))]
    const UPDATE_MANIFEST_URL: &'static str =
        "https://github.com/NayanVR/filex/releases/latest/download/filex-linux.json";

    /// Check the manifest once on launch (distribution decision 7: no
    /// timer) and surface the banner if a newer version exists. Notice-
    /// only — macOS/Linux install via their package manager, so this never
    /// downloads or verifies an artifact. Off-thread; a failed check is
    /// silent (retried next launch).
    #[cfg(all(not(target_os = "windows"), feature = "updater"))]
    fn spawn_update_check(&self, cx: &mut Context<Self>) {
        let url = Self::UPDATE_MANIFEST_URL;
        if url.is_empty() {
            return;
        }
        cx.spawn(async move |this, cx| {
            let found = cx
                .background_executor()
                .spawn(async move {
                    let cancel = filex::update::CancelFlag::new();
                    filex::update::check_for_newer_version(
                        filex::update::http_fetch,
                        url,
                        filex::update::CURRENT_VERSION,
                        &cancel,
                    )
                })
                .await;
            if let Ok(Some(version)) = found {
                this.update(cx, |this, cx| {
                    this.update_status = filex::update::UpdateStatus::Available {
                        version,
                        affordance: platform_affordance(),
                    };
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    /// Act on the update banner's primary button per the current
    /// affordance: copy the command, open the releases page, or (Windows,
    /// unused here) restart.
    fn apply_update_action(&mut self, cx: &mut Context<Self>) {
        let filex::update::UpdateStatus::Available { affordance, .. } = &self.update_status else {
            return;
        };
        match affordance.clone() {
            filex::update::UpdateAffordance::RunCommand(cmd) => {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(cmd));
                self.notice = Some("Update command copied — run it in your terminal".into());
            }
            filex::update::UpdateAffordance::OpenUrl(url) => {
                let _ = open_with_default_app(std::path::Path::new(&url));
            }
            filex::update::UpdateAffordance::Restart => {}
        }
        cx.notify();
    }

    /// Hide the update banner (the ✕). The check runs again next launch.
    fn dismiss_update(&mut self, cx: &mut Context<Self>) {
        self.update_status = filex::update::UpdateStatus::Idle;
        cx.notify();
    }

    /// The slim update banner above the status bar, or nothing when there's
    /// no update to show. Text/labels come from `filex::update`'s tested
    /// pure functions; this only renders and wires the buttons.
    fn render_update_banner(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let content = filex::update::banner_content(&self.update_status)?;
        let theme = *cx.theme();
        let mut bar = ui::update_banner::update_banner(&theme)
            .child(ui::update_banner::message(&theme, content.message));
        if let Some(label) = content.action_label {
            bar = bar.child(
                ui::update_banner::action_button(&theme, "update-action", label).on_click(
                    cx.listener(|this, _: &ClickEvent, _window, cx| {
                        this.apply_update_action(cx);
                    }),
                ),
            );
        }
        bar = bar.child(
            ui::update_banner::dismiss_button(&theme, "update-dismiss").on_click(cx.listener(
                |this, _: &ClickEvent, _window, cx| {
                    this.dismiss_update(cx);
                },
            )),
        );
        Some(bar.into_any_element())
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
    fn refresh_after_fs_change(&mut self, path: PathBuf, cx: &mut Context<Self>) {
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
        let cwd = self.cwd.clone();
        self.add_root(cwd, cx);
    }

    /// Validate and start indexing a new root.
    fn add_root(&mut self, candidate: PathBuf, cx: &mut Context<Self>) {
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
    fn persist_roots(&self, cx: &mut Context<Self>) {
        let roots: Vec<PathBuf> = self.roots.iter().map(|slot| slot.path.clone()).collect();
        self.settings.update(cx, |store, cx| {
            store.update(cx, |settings| settings.roots = roots);
        });
    }

    fn load_dir(&mut self, path: &Path, cx: &mut Context<Self>) {
        let settings = self.settings.read(cx).settings();
        let (sort, show_hidden) = (settings.sort, settings.show_hidden_files);
        match read_dir_sorted(path, &sort) {
            Ok(mut entries) => {
                if !show_hidden {
                    entries.retain(|entry| !entry.is_hidden);
                }
                let changed = self.cwd != path;
                self.cwd = path.to_path_buf();
                self.entries = entries;
                self.load_error = None;
                self.selection.clear();
                // Any in-flight rename or armed delete points at rows
                // that no longer exist; drop them.
                self.renaming = None;
                self.pending_delete = None;
                self.browse_scroll.scroll_to_item(0, ScrollStrategy::Top);
                // A "Current Dir" search follows the folder you're in, so
                // navigating with a scoped query live must re-scope it to
                // the new directory. "Anywhere" is cwd-independent, so it
                // is left untouched.
                if changed && self.search_scope == SearchScope::CurrentDir && !self.query.is_empty()
                {
                    self.update_search(cx);
                }
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
        self.thumbnails
            .insert(path.clone(), ThumbnailState::Loading);
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
        edge: f32,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let kind = FileKind::of(name, is_dir);
        if kind == FileKind::Image && self.settings.read(cx).settings().thumbnails_enabled {
            match self.thumbnails.get(path) {
                Some(ThumbnailState::Ready(imagery)) => {
                    return ui::icon::thumbnail_icon(imagery.clone(), edge);
                }
                Some(_) => {}
                None => self.request_thumbnail(path.to_path_buf(), cx),
            }
        }
        ui::icon::file_icon(cx.theme(), kind, edge)
    }

    /// Length of whichever list selection currently applies to.
    fn active_list_len(&self) -> usize {
        if self.query.is_empty() {
            self.entries.len()
        } else {
            self.results.len()
        }
    }

    /// The selection for the active list: the global search selection
    /// while a query is live, otherwise the (per-tab) browse selection.
    fn active_selection(&self) -> &Selection {
        if self.query.is_empty() {
            &self.selection
        } else {
            &self.search_selection
        }
    }

    fn active_selection_mut(&mut self) -> &mut Selection {
        if self.query.is_empty() {
            &mut self.selection
        } else {
            &mut self.search_selection
        }
    }

    /// Arrow-key navigation. `extend` (shift held) grows the range from
    /// the anchor instead of moving a single selection.
    fn move_selection(&mut self, delta: isize, extend: bool, cx: &mut Context<Self>) {
        let len = self.active_list_len();
        let next = if extend {
            self.active_selection_mut().extend_lead(delta, len)
        } else {
            self.active_selection_mut().move_lead(delta, len)
        };
        if let Some(next) = next {
            let handle = if self.query.is_empty() {
                &self.browse_scroll
            } else {
                &self.results_scroll
            };
            handle.scroll_to_item(next, ScrollStrategy::Center);
        }
        self.refresh_preview(cx);
        cx.notify();
    }

    /// Enter / double-click: directories navigate, files open with the
    /// platform's default application.
    fn activate(&mut self, ix: usize, cx: &mut Context<Self>) {
        let (path, is_dir, from_search) = if self.query.is_empty() {
            let Some(entry) = self.entries.get(ix) else {
                return;
            };
            (entry.path.clone(), entry.is_dir, false)
        } else {
            let Some(row) = self.results.get(ix) else {
                return;
            };
            (row.target.clone(), row.is_dir, true)
        };
        self.open_target(path, is_dir, from_search, cx);
    }

    fn open_target(
        &mut self,
        path: PathBuf,
        is_dir: bool,
        from_search: bool,
        cx: &mut Context<Self>,
    ) {
        if is_dir {
            self.navigate(path, cx);
            if from_search {
                self.clear_search(cx);
            }
        } else {
            if let Err(err) = open_with_default_app(&path) {
                self.notice = Some(format!("couldn't open {}: {err}", path.display()).into());
            }
            self.record_recent(path, cx);
            if from_search {
                self.clear_search(cx);
            }
            cx.notify();
        }
    }

    /// Jump to a file's parent directory and select it there (context
    /// menu "Reveal in Folder" on search results).
    fn reveal(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let Some(parent) = path.parent().map(Path::to_path_buf) else {
            return;
        };
        self.clear_search(cx);
        self.navigate(parent, cx);
        if let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned())
            && let Some(ix) = self.entries.iter().position(|entry| entry.name == name)
        {
            self.selection.select_one(ix);
            self.browse_scroll
                .scroll_to_item(ix, ScrollStrategy::Center);
        }
        self.refresh_preview(cx);
        cx.notify();
    }

    /// Stop indexing a root. Dropping the slot drops its LiveIndex —
    /// the watcher stops and a final snapshot saves on drop.
    fn remove_root(&mut self, path: &Path, cx: &mut Context<Self>) {
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

    /// Write every selected item's path to the OS clipboard (one per
    /// line), for pasting into a terminal or another app.
    fn copy_selected_paths(&mut self, cx: &mut Context<Self>) {
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
    fn trash_selected(&mut self, cx: &mut Context<Self>) {
        let paths = self
            .selected_paths()
            .into_iter()
            .map(|(path, _)| path)
            .collect();
        self.delete_paths(paths, cx);
    }

    fn activate_selected(&mut self, cx: &mut Context<Self>) {
        if let Some(ix) = self.active_selection().lead() {
            self.activate(ix, cx);
        }
    }

    /// A left click on row `ix`, dispatched by its keyboard modifiers:
    /// cmd/ctrl toggles, shift ranges from the anchor, plain click
    /// selects only it.
    fn select_click(&mut self, ix: usize, modifiers: gpui::Modifiers, cx: &mut Context<Self>) {
        if modifiers.secondary() {
            self.active_selection_mut().toggle(ix);
        } else if modifiers.shift {
            self.active_selection_mut().range_to(ix);
        } else {
            self.active_selection_mut().select_one(ix);
        }
        self.refresh_preview(cx);
        cx.notify();
    }

    fn select_all(&mut self, cx: &mut Context<Self>) {
        if self.renaming.is_some() || self.settings_open {
            return;
        }
        let len = self.active_list_len();
        self.active_selection_mut().select_all(len);
        self.refresh_preview(cx);
        cx.notify();
    }

    fn navigate(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if path == self.cwd {
            return;
        }
        // A new destination severs the forward history (the tab owns
        // both stacks).
        self.history_back.push(self.cwd.clone());
        self.history_forward.clear();
        self.load_dir(&path, cx);
        self.record_recent(path, cx);
        cx.notify();
    }

    fn go_up(&mut self, cx: &mut Context<Self>) {
        if let Some(parent) = self.cwd.parent().map(Path::to_path_buf) {
            self.navigate(parent, cx);
        }
    }

    /// Back/forward through the active tab's history.
    fn go_back(&mut self, cx: &mut Context<Self>) {
        if let Some(prev) = self.history_back.pop() {
            self.history_forward.push(self.cwd.clone());
            self.load_dir(&prev, cx);
            cx.notify();
        }
    }

    fn go_forward(&mut self, cx: &mut Context<Self>) {
        if let Some(next) = self.history_forward.pop() {
            self.history_back.push(self.cwd.clone());
            self.load_dir(&next, cx);
            cx.notify();
        }
    }

    /// Save the live browse state into the active tab's slot.
    fn snapshot_active(&mut self) -> TabSnapshot {
        TabSnapshot {
            cwd: std::mem::take(&mut self.cwd),
            entries: std::mem::take(&mut self.entries),
            load_error: self.load_error.take(),
            selection: std::mem::take(&mut self.selection),
            scroll: std::mem::replace(&mut self.browse_scroll, UniformListScrollHandle::new()),
            history_back: std::mem::take(&mut self.history_back),
            history_forward: std::mem::take(&mut self.history_forward),
        }
    }

    /// Load a saved tab into the live browse fields. Transient per-tab UI
    /// (rename, armed delete) does not survive the switch.
    fn restore_tab(&mut self, snap: TabSnapshot) {
        self.cwd = snap.cwd;
        self.entries = snap.entries;
        self.load_error = snap.load_error;
        self.selection = snap.selection;
        self.browse_scroll = snap.scroll;
        self.history_back = snap.history_back;
        self.history_forward = snap.history_forward;
        self.renaming = None;
        self.pending_delete = None;
    }

    /// Switch to tab `i` (no-op if it's already active or out of range).
    fn activate_tab(&mut self, i: usize, cx: &mut Context<Self>) {
        if i == self.active_tab || i >= self.tabs.len() {
            return;
        }
        self.tabs[self.active_tab] = self.snapshot_active();
        let target = std::mem::replace(&mut self.tabs[i], TabSnapshot::placeholder());
        self.restore_tab(target);
        self.active_tab = i;
        self.refresh_preview(cx);
        cx.notify();
    }

    /// Open a new tab at `path` and make it active.
    fn open_tab(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.tabs[self.active_tab] = self.snapshot_active();
        self.tabs.push(TabSnapshot::placeholder());
        self.active_tab = self.tabs.len() - 1;
        self.restore_tab(TabSnapshot::placeholder());
        self.load_dir(&path, cx);
        self.refresh_preview(cx);
        cx.notify();
    }

    /// Close tab `i`, activating a neighbor if it was the active one.
    /// Closing the last tab is handled by the caller (it closes the
    /// window) — this assumes more than one tab remains.
    fn close_tab(&mut self, i: usize, cx: &mut Context<Self>) {
        if self.tabs.len() <= 1 || i >= self.tabs.len() {
            return;
        }
        if i == self.active_tab {
            self.tabs.remove(i);
            let new_active = i.min(self.tabs.len() - 1);
            let target = std::mem::replace(&mut self.tabs[new_active], TabSnapshot::placeholder());
            self.restore_tab(target);
            self.active_tab = new_active;
            self.refresh_preview(cx);
        } else {
            self.tabs.remove(i);
            if i < self.active_tab {
                self.active_tab -= 1;
            }
        }
        cx.notify();
    }

    /// The folder name shown on tab `i` (the drive/root shows as "/").
    fn tab_title(&self, i: usize) -> SharedString {
        let cwd = if i == self.active_tab {
            &self.cwd
        } else {
            &self.tabs[i].cwd
        };
        cwd.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| cwd.display().to_string())
            .into()
    }

    fn is_favorite(&self, path: &Path, cx: &App) -> bool {
        self.settings
            .read(cx)
            .settings()
            .favorites
            .iter()
            .any(|p| p == path)
    }

    /// Pin a folder to the sidebar's Favorites (idempotent).
    fn pin_favorite(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.settings.update(cx, |store, cx| {
            store.update(cx, |settings| {
                if !settings.favorites.iter().any(|p| p == &path) {
                    settings.favorites.push(path.clone());
                }
            });
        });
    }

    fn unpin_favorite(&mut self, path: &Path, cx: &mut Context<Self>) {
        let path = path.to_path_buf();
        self.settings.update(cx, |store, cx| {
            store.update(cx, |settings| settings.favorites.retain(|p| p != &path));
        });
    }

    /// Move a favorite up (`delta < 0`) or down in the list.
    fn move_favorite(&mut self, path: &Path, delta: isize, cx: &mut Context<Self>) {
        let path = path.to_path_buf();
        self.settings.update(cx, |store, cx| {
            store.update(cx, |settings| {
                let favorites = &mut settings.favorites;
                if let Some(i) = favorites.iter().position(|p| p == &path) {
                    let j = (i as isize + delta).clamp(0, favorites.len() as isize - 1) as usize;
                    if i != j {
                        let item = favorites.remove(i);
                        favorites.insert(j, item);
                    }
                }
            });
        });
    }

    fn is_section_collapsed(&self, id: &str, cx: &App) -> bool {
        self.settings
            .read(cx)
            .settings()
            .collapsed_sections
            .iter()
            .any(|s| s == id)
    }

    fn toggle_section(&mut self, id: &'static str, cx: &mut Context<Self>) {
        self.settings.update(cx, |store, cx| {
            store.update(cx, |settings| {
                let sections = &mut settings.collapsed_sections;
                if let Some(i) = sections.iter().position(|s| s == id) {
                    sections.remove(i);
                } else {
                    sections.push(id.to_string());
                }
            });
        });
    }

    /// Note `path` as recently opened and persist the log off-thread.
    fn record_recent(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.recents.record(path);
        self.refresh_frecency();
        self.persist_recents(cx);
    }

    /// Rebuild the cached frecency table after `recents` changed.
    fn refresh_frecency(&mut self) {
        self.frecency = std::sync::Arc::new(self.recents.score_table(filex::frecency::now_secs()));
    }

    fn persist_recents(&self, cx: &Context<Self>) {
        let Some(file) = filex::recents::default_recents_file() else {
            return;
        };
        let recents = self.recents.clone();
        cx.background_executor()
            .spawn(async move {
                if let Err(err) = recents.save(&file) {
                    tracing::error!("failed to save recents: {err:#}");
                }
            })
            .detach();
    }

    fn clear_recents(&mut self, cx: &mut Context<Self>) {
        self.recents.clear();
        self.refresh_frecency();
        self.persist_recents(cx);
        cx.notify();
    }

    /// Upload any queued crash reports at launch — only with the user's
    /// consent (`crash_reports == Some(true)`) and a configured endpoint.
    /// Runs off-thread; a scrubbed report is POSTed and deleted on success,
    /// failures stay queued for next launch (Phase 2c).
    fn spawn_crash_upload(&self, cx: &mut Context<Self>) {
        if !self.settings.read(cx).settings().crash_reports {
            return;
        }
        let (Some(url), Some(dir)) = (
            filex::telemetry::endpoint(),
            filex::telemetry::default_queue_dir(),
        ) else {
            return; // no endpoint configured, or no data dir
        };
        cx.background_executor()
            .spawn(async move {
                let sent = filex::telemetry::drain(&dir, |report| {
                    let Ok(body) = serde_json::to_string(report) else {
                        return false;
                    };
                    ureq::post(&url)
                        .set("Content-Type", "application/json")
                        .send_string(&body)
                        .is_ok()
                });
                if sent > 0 {
                    tracing::info!("uploaded {sent} crash report(s)");
                }
            })
            .detach();
    }

    /// Drop sidecar tag keys whose file no longer exists — lazy cleanup
    /// (design-tags.md) for files moved/deleted outside filex, where we
    /// never saw the `from→to` pairing. Runs once at startup, off-thread.
    fn spawn_tag_prune(&self, cx: &mut Context<Self>) {
        let tags = self.tags.clone();
        cx.background_executor()
            .spawn(async move {
                match tags.prune(|path| path.exists()) {
                    Ok(n) if n > 0 => tracing::debug!("pruned {n} stale tag entries"),
                    Ok(_) => {}
                    Err(err) => tracing::error!("failed to prune tags: {err:#}"),
                }
            })
            .detach();
    }

    /// Recompute the sidebar's distinct-tag list off-thread (the store
    /// scan/clone must not run on the UI thread) and cache it. Called at
    /// startup and after any change to the store.
    fn refresh_sidebar_tags(&self, cx: &mut Context<Self>) {
        let store = self.tags.clone();
        cx.spawn(async move |this, cx| {
            let distinct = cx
                .background_executor()
                .spawn(async move {
                    let all = store.all();
                    filex::tags::distinct_tags(all.iter().flat_map(|(_, tags)| tags.iter()))
                })
                .await;
            this.update(cx, |this, cx| {
                this.sidebar_tags = distinct;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Run a `tag:NAME` search (clicking a sidebar tag), focusing the
    /// search field so it can be refined.
    fn search_tag(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.search_input.focus_handle(cx));
        self.search_input.update(cx, |input, cx| {
            input.set_text(format!("tag:{name}"), cx);
        });
    }

    /// Remove one recognized filter token from the query (clicking its
    /// chip) by rewriting the search input's text.
    fn remove_filter_token(&mut self, token: &str, cx: &mut Context<Self>) {
        let rewritten = filex::search_filter::without_token(&self.query, token);
        self.search_input
            .update(cx, |input, cx| input.set_text(rewritten, cx));
    }

    /// Remove an inferred natural-language phrase (clicking its chip) by
    /// stripping the words that produced it.
    fn remove_phrase(&mut self, source: &str, cx: &mut Context<Self>) {
        let rewritten = filex::phrases::without_phrase(&self.query, source);
        self.search_input
            .update(cx, |input, cx| input.set_text(rewritten, cx));
    }

    /// The removable filter chips shown under the top bar — one pill per
    /// recognized `key:value` token in the query (`tag:` pills carry the
    /// tag's color dot). `None` when the query has no filter tokens.
    fn render_filter_chips(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if self.query.is_empty() {
            return None;
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        // Chips must describe the search that actually ran. For a command
        // query that is *not* the raw text: `magic::parse` reads only the
        // command's target ("pdfs from downloads", not the verb or the
        // destination) and expands it with `expand_as_description`, whose
        // lone-word rule differs from `expand`. Deriving chips from the
        // whole query instead showed filters the plan does not apply —
        // "move all pdfs from downloads to documents" grew a `kind:document`
        // chip off the destination word, while the command carried only
        // `ext:pdf`.
        let source = match &self.magic {
            Some(state) => state.command.selection.source.clone(),
            None => self.query.clone(),
        };
        let tokens = filex::search_filter::filter_tokens(&source, now);
        let residual = filex::search_filter::parse_query(&source, now).text;
        let phrases = match &self.magic {
            Some(_) => filex::phrases::expand_as_description(&residual, now).phrases,
            None => filex::phrases::expand(&residual, now).phrases,
        };
        if tokens.is_empty() && phrases.is_empty() {
            return None;
        }
        let theme = *cx.theme();
        let mut strip = ui::top_bar::filter_chip_strip(&theme);
        for (i, (token, filter)) in tokens.into_iter().enumerate() {
            // `tag:` pills show the tag name with its color dot; the rest
            // show the raw token (`kind:image`, `size:>2mb`, …).
            let (label, dot) = match &filter {
                Filter::Tag(name) => {
                    let color = self
                        .sidebar_tags
                        .iter()
                        .find(|t| t.name.eq_ignore_ascii_case(name))
                        .and_then(|t| t.color);
                    (
                        name.clone(),
                        Some(ui::details::tag_dot_color(&theme, color)),
                    )
                }
                _ => (token.clone(), None),
            };
            strip = strip.child(
                ui::top_bar::filter_chip(&theme, ("filter-chip", i), label, dot).on_click(
                    cx.listener(move |this, _: &ClickEvent, _window, cx| {
                        this.remove_filter_token(&token, cx);
                    }),
                ),
            );
        }
        // Inferred phrase chips, after the explicit ones. Each is labelled
        // with what it became (`kind:image`) or, for sizes and dates, the
        // words the user typed — and clicking removes those words.
        for (i, phrase) in phrases.into_iter().enumerate() {
            for (j, filter) in phrase.filters.iter().enumerate() {
                let label = filex::phrases::label_for(filter, &phrase.source);
                let source = phrase.source.clone();
                strip = strip.child(
                    ui::top_bar::filter_chip(&theme, ("phrase-chip", i * 8 + j), label, None)
                        .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                            this.remove_phrase(&source, cx);
                        })),
                );
            }
        }
        Some(strip.into_any_element())
    }

    /// The full-pane magic view (`docs/design-magic-mode.md` v2): the plan
    /// *replaces* the results list rather than floating above it. Shown
    /// whenever [`in_magic_view`](Self::in_magic_view) holds.
    ///
    /// Three shapes: a hint (magic mode on, nothing typed yet), a resolved
    /// plan (the reviewable op list with confirm/cancel), or a refusal
    /// (the command parsed but couldn't build a plan — say why).
    fn render_magic_pane(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = *cx.theme();
        let Some(state) = self.magic.as_ref() else {
            // Explicit magic mode, no command yet.
            return ui::magic_card::pane()
                .child(ui::magic_card::pane_hint(
                    &theme,
                    "Type a command",
                    &[
                        "move all pdfs to Documents",
                        "delete screenshots older than 30 days",
                        "rename * to invoice-{n}.{ext}",
                    ],
                ))
                .into_any_element();
        };

        let verb = state.command.verb;
        let destructive = verb == filex::magic::Verb::Delete;
        let echo = format!("matching “{}”", state.command.selection.source);

        let mut header = ui::magic_card::pane_header(&theme);
        let mut body: Option<gpui::AnyElement> = None;
        let mut action_bar: Option<gpui::Div> = None;

        match &state.outcome {
            None => {
                header = header
                    .child(ui::magic_card::heading(&theme, verb.label()))
                    .child(ui::magic_card::subtitle(
                        &theme,
                        if self.any_root_ready() {
                            "finding matches…"
                        } else {
                            "still indexing — matches will appear when ready…"
                        },
                    ));
                header = header.child(ui::magic_card::subtitle(&theme, echo));
            }
            Some(Ok(plan)) => {
                let count = state.checked.iter().filter(|c| **c).count();
                header = header
                    .child(ui::magic_card::heading(
                        &theme,
                        format!("{} {} {}", verb.label(), count, plural_items(count)),
                    ))
                    .child(ui::magic_card::subtitle(&theme, echo));
                if plan.skipped > 0 {
                    header = header.child(ui::magic_card::subtitle(
                        &theme,
                        format!(
                            "{} already {} — nothing to do for {}",
                            plan.skipped,
                            if verb == filex::magic::Verb::Rename {
                                "named that"
                            } else {
                                "there"
                            },
                            if plan.skipped == 1 { "it" } else { "them" },
                        ),
                    ));
                }

                // Virtualized: a 342-op plan renders only the visible rows.
                // Building every row as a child (the old approach) is what
                // made the plan view sluggish to scroll on a large batch.
                let list = uniform_list(
                    "magic-plan",
                    plan.ops.len(),
                    cx.processor(|this, range: Range<usize>, _window, cx| {
                        let theme = *cx.theme();
                        let Some(state) = this.magic.as_ref() else {
                            return Vec::new();
                        };
                        let Some(Ok(plan)) = state.outcome.as_ref() else {
                            return Vec::new();
                        };
                        range
                            .filter_map(|ix| {
                                let op = plan.ops.get(ix)?;
                                let checked = state.checked.get(ix).copied().unwrap_or(false);
                                let PlanRow {
                                    name,
                                    location,
                                    dest,
                                    tooltip,
                                } = describe_op(op);
                                Some(
                                    ui::magic_card::op_row(
                                        &theme,
                                        ("magic-op", ix),
                                        checked,
                                        name,
                                        location,
                                        dest,
                                    )
                                    .tooltip(ui::tooltip::text_tooltip(tooltip, theme))
                                    .on_click(cx.listener(
                                        move |this, _: &ClickEvent, _window, cx| {
                                            this.toggle_magic_op(ix, cx);
                                        },
                                    )),
                                )
                            })
                            .collect()
                    }),
                )
                .track_scroll(self.magic_scroll.clone())
                .flex_1();
                body = Some(list.into_any_element());

                action_bar = Some(
                    ui::magic_card::pane_actions(&theme)
                        .child(
                            ui::magic_card::secondary_button(&theme, "magic-cancel", "Cancel")
                                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                                    this.clear_search(cx);
                                })),
                        )
                        .child(
                            ui::magic_card::confirm_button(
                                &theme,
                                "magic-confirm",
                                verb.label(),
                                count > 0,
                                destructive,
                            )
                            .on_click(cx.listener(
                                |this, _: &ClickEvent, _window, cx| {
                                    this.confirm_magic(cx);
                                },
                            )),
                        ),
                );
            }
            Some(Err(err)) => {
                header = header
                    .child(ui::magic_card::heading(&theme, verb.label()))
                    .child(ui::magic_card::subtitle(&theme, echo))
                    .child(ui::magic_card::refusal(&theme, format!("{err}")));
            }
        }

        ui::magic_card::pane()
            .child(header)
            .children(body)
            .children(action_bar)
            .into_any_element()
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
    fn confirm_magic(&mut self, cx: &mut Context<Self>) {
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
                            verb.label().to_lowercase(),
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

    fn clear_search(&mut self, cx: &mut Context<Self>) {
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

    /// Begin renaming the selected browse entry (F2): the row's name
    /// cell becomes an input prefilled with the current name, content
    /// preselected so typing replaces it.
    fn start_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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

    /// The (path, name) at a list index in whichever list is active
    /// (browse entries, or search results while a query is live).
    fn path_at(&self, ix: usize) -> Option<(PathBuf, String)> {
        if self.query.is_empty() {
            let entry = self.entries.get(ix)?;
            Some((entry.path.clone(), entry.name.clone()))
        } else {
            let row = self.results.get(ix)?;
            Some((row.target.clone(), row.name.to_string()))
        }
    }

    /// Every selected item's (path, name), in list order.
    fn selected_paths(&self) -> Vec<(PathBuf, String)> {
        self.active_selection()
            .iter()
            .filter_map(|ix| self.path_at(ix))
            .collect()
    }

    /// The lead item (path, name, is_dir) — the details panel's subject
    /// and the natural single target.
    fn lead_item(&self) -> Option<(PathBuf, String, bool)> {
        let ix = self.active_selection().lead()?;
        if self.query.is_empty() {
            let entry = self.entries.get(ix)?;
            Some((entry.path.clone(), entry.name.clone(), entry.is_dir))
        } else {
            let row = self.results.get(ix)?;
            Some((row.target.clone(), row.name.to_string(), row.is_dir))
        }
    }

    /// Toggle the details panel (top-bar button / cmd-i). Opening it
    /// kicks off the metadata fetch for the current item.
    fn toggle_preview(&mut self, cx: &mut Context<Self>) {
        self.settings.update(cx, |store, cx| {
            store.update(cx, |settings| {
                settings.preview_open = !settings.preview_open
            });
        });
        self.refresh_preview(cx);
    }

    /// Ensure `preview_meta` describes the current lead item, fetching it
    /// off-thread when the selection moved (or clearing it when the panel
    /// is closed / nothing is selected). Called after selection changes.
    fn refresh_preview(&mut self, cx: &mut Context<Self>) {
        if !self.settings.read(cx).settings().preview_open {
            self.preview_meta = None;
            self.clear_tag_state();
            return;
        }
        let Some((path, _, _)) = self.lead_item() else {
            self.preview_meta = None;
            self.clear_tag_state();
            return;
        };
        if self.preview_meta.as_ref().map(|m| m.path.as_path()) == Some(path.as_path()) {
            return; // already have this item's metadata (and tags)
        }
        // A new item is selected: drop stale tag state and reload its tags.
        self.clear_tag_state();
        self.spawn_refresh_tags(path.clone(), cx);
        self.preview_meta = None; // drop stale while the fetch is in flight
        cx.spawn(async move |this, cx| {
            let meta = cx
                .background_executor()
                .spawn({
                    let path = path.clone();
                    async move { fetch_preview_meta(&path) }
                })
                .await;
            this.update(cx, |this, cx| {
                // Apply only if the lead still points at the same item.
                if this.lead_item().map(|(p, _, _)| p).as_deref() == Some(path.as_path()) {
                    this.preview_meta = Some(meta);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Drop the details panel's cached tags and any open tag editor
    /// (selection moved, or the panel closed).
    fn clear_tag_state(&mut self) {
        self.preview_tags.clear();
        self.tag_editor = None;
    }

    /// Read the lead item's tags into `preview_tags` off-thread — the
    /// store read is an xattr syscall on macOS, so it never runs on the UI
    /// thread. Applied only if the selection hasn't moved on.
    fn spawn_refresh_tags(&self, path: PathBuf, cx: &mut Context<Self>) {
        let store = self.tags.clone();
        cx.spawn(async move |this, cx| {
            let fetched = cx
                .background_executor()
                .spawn({
                    let store = store.clone();
                    let path = path.clone();
                    async move { store.tags(&path) }
                })
                .await;
            this.update(cx, |this, cx| {
                if this.lead_item().map(|(p, _, _)| p).as_deref() == Some(path.as_path()) {
                    this.preview_tags = fetched;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Persist `tags` for `path` through the store off-thread (xattr +
    /// sidecar), then mirror them into `preview_tags` if the item is still
    /// selected. A failure surfaces as a notice, never a crash.
    fn spawn_set_tags(&mut self, path: PathBuf, tags: Vec<Tag>, cx: &mut Context<Self>) {
        let store = self.tags.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn({
                    let store = store.clone();
                    let path = path.clone();
                    let tags = tags.clone();
                    async move { store.set_tags(&path, &tags) }
                })
                .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        if this.lead_item().map(|(p, _, _)| p).as_deref() == Some(path.as_path()) {
                            this.preview_tags = tags;
                        }
                        this.refresh_sidebar_tags(cx);
                    }
                    Err(err) => this.notice = Some(format!("{err:#}").into()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Open the details-panel tag editor. `existing` prefills to
    /// recolor/rename an existing chip; `None` starts a fresh tag.
    fn open_tag_editor(
        &mut self,
        existing: Option<Tag>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((path, _, _)) = self.lead_item() else {
            return;
        };
        let input = cx.new(SearchInput::new);
        let (seed, color, original) = match existing {
            Some(tag) => (tag.name.clone(), tag.color, Some(tag.name)),
            None => (String::new(), None, None),
        };
        input.update(cx, |input, cx| {
            input.set_placeholder("tag name", cx);
            if !seed.is_empty() {
                input.set_text(seed, cx);
                input.select_all_text(cx);
            }
        });
        let subscription = cx.subscribe_in(&input, window, |this, _input, event, window, cx| {
            if matches!(event, SearchInputEvent::Dismissed) {
                this.cancel_tag_editor(window, cx);
            }
        });
        window.focus(&input.focus_handle(cx));
        self.tag_editor = Some(TagEditor {
            path,
            input,
            color,
            existing: original,
            _subscription: subscription,
        });
        cx.notify();
    }

    fn cancel_tag_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.tag_editor.take().is_some() {
            window.focus(&self.search_input.focus_handle(cx));
            cx.notify();
        }
    }

    /// Set the pending color in the open editor (clicking a swatch).
    fn set_editor_color(&mut self, color: Option<TagColor>, cx: &mut Context<Self>) {
        if let Some(editor) = self.tag_editor.as_mut() {
            editor.color = color;
            cx.notify();
        }
    }

    /// Commit the tag editor: fold the typed name + chosen color into the
    /// item's tag set (adding, or replacing the edited tag in place) and
    /// persist it. An empty name is treated as cancel.
    fn commit_tag_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.tag_editor.take() else {
            return;
        };
        window.focus(&self.search_input.focus_handle(cx));
        let name = editor.input.read(cx).text().trim().to_string();
        if name.is_empty() {
            cx.notify();
            return;
        }
        let new_tag = Tag {
            name,
            color: editor.color,
        };
        let tags = filex::tags::upsert_tag(&self.preview_tags, editor.existing.as_deref(), new_tag);
        self.spawn_set_tags(editor.path, tags, cx);
        cx.notify();
    }

    /// Remove the tag currently being edited (the editor's Remove button).
    fn remove_editing_tag(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.tag_editor.take() else {
            return;
        };
        window.focus(&self.search_input.focus_handle(cx));
        if let Some(original) = editor.existing {
            let mut tags = self.preview_tags.clone();
            tags.retain(|t| t.name != original);
            self.spawn_set_tags(editor.path, tags, cx);
        }
        cx.notify();
    }

    /// cmd-c / cmd-x (reaches us only while the search input is empty —
    /// otherwise the keys edit query text). Captures every selected item.
    fn clip_selected(&mut self, mode: ClipMode, cx: &mut Context<Self>) {
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
    fn paste_clipboard(&mut self, cx: &mut Context<Self>) {
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
    fn drag_source_paths(&self, ix: usize) -> Vec<PathBuf> {
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
    fn drag_items(&self, ix: usize, theme: Theme) -> Option<DragItems> {
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
    fn drop_onto(
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
    fn folder_drop_target(
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
    fn reload_dir(&mut self, cx: &mut Context<Self>) {
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
    fn spawn_transfer_batch(
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
            label: format!("{verb} {} items", sources.len()).into(),
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
                    this.notice = Some(format!("{verb} {} items", applied.len()).into());
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
    fn delete_selected(&mut self, cx: &mut Context<Self>) {
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
    fn delete_paths(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
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
    fn run_op(&mut self, op: FileOp, cx: &mut Context<Self>) {
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
    fn resolve_conflict(&mut self, keep_both: bool, cx: &mut Context<Self>) {
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
    fn spawn_apply(&mut self, op: FileOp, cx: &mut Context<Self>) {
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
    fn spawn_job_ticker(&self, id: u64, cx: &mut Context<Self>) {
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

    fn cancel_job(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some(job) = self.jobs.iter().find(|job| job.id == id) {
            job.progress.request_cancel();
            self.notice = Some("canceling…".into());
            cx.notify();
        }
    }

    fn render_jobs(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if self.jobs.is_empty() {
            return None;
        }
        let theme = *cx.theme();
        let mut bar = ui::job::jobs_bar(&theme);
        for job in &self.jobs {
            let id = job.id;
            bar = bar.child(
                ui::job::job_row(
                    &theme,
                    ("job", id as usize),
                    job.label.clone(),
                    job.progress.fraction(),
                )
                .child(
                    ui::job::cancel_button(&theme, ("job-cancel", id as usize)).on_click(
                        cx.listener(move |this, _: &ClickEvent, _window, cx| {
                            this.cancel_job(id, cx);
                        }),
                    ),
                ),
            );
        }
        Some(bar.into_any_element())
    }

    /// Undo the most recent file operation (cmd-z / ctrl-z). The disk
    /// work runs off-thread; a failed undo goes back on the journal so
    /// the user can fix the cause and retry.
    fn undo_last(&mut self, cx: &mut Context<Self>) {
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

    /// List-navigation keys. Text editing lives in the SearchInput (which
    /// is focused); unhandled keys bubble up here.
    fn handle_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        if keystroke.modifiers.platform || keystroke.modifiers.control {
            return; // shortcuts are handled by actions
        }
        if self.conflict.is_some() {
            // Enter = the primary (keep both); escape arrives as the
            // input's ClearInput action, handled in render().
            if keystroke.key.as_str() == "enter" {
                self.resolve_conflict(true, cx);
            }
            return;
        }
        if self.renaming.is_some() {
            // Escape is consumed by the input itself (Dismissed event);
            // enter lands here and commits.
            if keystroke.key.as_str() == "enter" {
                self.commit_rename(window, cx);
            }
            return;
        }
        if self.tag_editor.is_some() {
            // Same as rename: the input handles escape (Dismissed → cancel);
            // enter commits the tag.
            if keystroke.key.as_str() == "enter" {
                self.commit_tag_editor(window, cx);
            }
            return;
        }
        let extend = keystroke.modifiers.shift;
        match keystroke.key.as_str() {
            "up" => self.move_selection(-1, extend, cx),
            "down" => self.move_selection(1, extend, cx),
            "enter" => self.activate_selected(cx),
            _ => {}
        }
    }

    /// The search-bar toggle. Clicking it means "give me the other mode
    /// than what I'm seeing": from any magic view (forced or auto) to a
    /// forced-off normal search, and from any normal search to forced-on
    /// magic. Without the forced-off state, clicking the toggle on an
    /// auto-switched command would set `On` and look like a no-op, and
    /// there'd be no way back to plain search without editing the query.
    fn toggle_magic_mode(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
    fn open_scope_menu(
        &mut self,
        position: Point<Pixels>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        self.scope_menu = Some(Self::clamped_menu_position(position, 88., window));
        cx.notify();
    }

    fn close_scope_menu(&mut self, cx: &mut Context<Self>) {
        if self.scope_menu.take().is_some() {
            cx.notify();
        }
    }

    /// Pick a search scope and re-run the query under it. No-op (beyond
    /// closing the menu) when the scope is unchanged.
    fn set_scope(&mut self, scope: SearchScope, cx: &mut Context<Self>) {
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
    fn in_magic_view(&self) -> bool {
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
    fn update_search(&mut self, cx: &mut Context<Self>) {
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
    fn run_search(&mut self, cx: &mut Context<Self>) {
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
    fn rebuild_magic_plan(&mut self) {
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
    fn toggle_magic_op(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(state) = self.magic.as_mut()
            && let Some(checked) = state.checked.get_mut(ix)
        {
            *checked = !*checked;
            cx.notify();
        }
    }

    /// New results select the first hit (Spotlight-style), so Enter
    /// immediately opens the top match.
    fn select_first_result(&mut self) {
        if self.results.is_empty() {
            self.search_selection.clear();
        } else {
            self.search_selection.select_one(0);
        }
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
        let theme = *cx.theme();
        let segments = path_segments(&self.cwd);
        let elide = segments.len() > MAX_SEGMENTS;
        let tail_start = if elide {
            segments.len() - TAIL_SEGMENTS
        } else {
            usize::MAX
        };

        let mut children: Vec<gpui::AnyElement> = Vec::new();
        for (ix, (label, target)) in segments.into_iter().enumerate() {
            if elide && ix > 0 && ix < tail_start {
                if ix == 1 {
                    children.push(ui::top_bar::breadcrumb_chevron(&theme).into_any_element());
                    children.push(ui::top_bar::breadcrumb_ellipsis(&theme).into_any_element());
                }
                continue;
            }
            if ix > 0 {
                children.push(ui::top_bar::breadcrumb_chevron(&theme).into_any_element());
            }
            children.push(
                ui::top_bar::breadcrumb_segment(&theme, ("crumb", ix), label)
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

    /// Flip the browse layout between list and grid (persisted).
    fn toggle_view(&mut self, cx: &mut Context<Self>) {
        self.settings.update(cx, |store, cx| {
            store.update(cx, |settings| {
                settings.view = match settings.view {
                    ViewMode::List => ViewMode::Grid,
                    ViewMode::Grid => ViewMode::List,
                };
            });
        });
    }

    /// Step the grid card size, clamped to the valid range (persisted).
    fn zoom_grid(&mut self, delta: i8, cx: &mut Context<Self>) {
        self.settings.update(cx, |store, cx| {
            store.update(cx, |settings| {
                let max = ui::grid::max_zoom() as i16;
                settings.grid_zoom =
                    ((settings.grid_zoom as i16 + delta as i16).clamp(0, max)) as u8;
            });
        });
    }

    /// The navigation bar (second row): refresh/back/forward/up, the path
    /// breadcrumbs, then the search box pinned to the right.
    fn render_top_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *cx.theme();
        let (can_back, can_forward) = (
            !self.history_back.is_empty(),
            !self.history_forward.is_empty(),
        );
        let dim = |enabled: bool| {
            if enabled {
                theme.text_dim
            } else {
                theme.border
            }
        };
        // Left: the navigation controls followed by the path, taking the
        // flexible share so a long breadcrumb trail eats into the gap
        // before the search box (which keeps its fixed width on the right).
        let left = div()
            .flex_1()
            .min_w_0()
            .flex()
            .items_center()
            .gap_1()
            .overflow_hidden()
            .child(
                ui::top_bar::toolbar_button(
                    &theme,
                    "refresh",
                    "icons/refresh-cw.svg",
                    theme.text_dim,
                )
                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| this.reload_dir(cx))),
            )
            .child(
                ui::top_bar::toolbar_button(
                    &theme,
                    "back",
                    "icons/chevron-left.svg",
                    dim(can_back),
                )
                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| this.go_back(cx))),
            )
            .child(
                ui::top_bar::toolbar_button(
                    &theme,
                    "forward",
                    "icons/chevron-right.svg",
                    dim(can_forward),
                )
                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| this.go_forward(cx))),
            )
            .child(
                ui::top_bar::toolbar_button(&theme, "up", "icons/arrow-up.svg", theme.text_dim)
                    .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                        this.go_up(cx);
                    })),
            )
            .child(self.render_breadcrumbs(cx));

        // The magic toggle lights for the *effective* mode, not just the
        // explicit flag: a command typed in normal mode auto-switches the
        // app into magic view, and the icon must say so — otherwise the
        // results list vanishes with no visible reason. `in_magic_view`
        // is the same predicate the render path branches on.
        // A definite width so the search box's `w_full` resolves (a
        // `flex_none` parent would otherwise shrink-wrap to nothing); the
        // box's own `max_w` still caps it. The nav controls to the left
        // take the remaining space and shrink first on a narrow window.
        let search = div().flex_none().w(px(340.)).flex().justify_end().child(
            ui::top_bar::search_box(&theme, !self.query.is_empty() || self.in_magic_view())
                .child(
                    ui::top_bar::scope_selector(
                        &theme,
                        self.search_scope.label(),
                        self.scope_menu.is_some(),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseDownEvent, window, cx| {
                            this.open_scope_menu(event.position, window, cx);
                        }),
                    ),
                )
                .child(div().flex_1().min_w_0().child(self.search_input.clone()))
                .child(
                    ui::top_bar::magic_toggle(&theme, self.in_magic_view()).on_click(cx.listener(
                        |this, _: &ClickEvent, window, cx| {
                            this.toggle_magic_mode(window, cx);
                        },
                    )),
                ),
        );

        ui::top_bar::top_bar(&theme).child(left).child(search)
    }

    /// Settings live in a centred modal over the browse view (rather
    /// than replacing it): a dimmed backdrop closes on an outside click,
    /// Escape closes it too (see the ClearInput handler). `None` while
    /// closed.
    fn render_settings_modal(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if !self.settings_open {
            return None;
        }
        let theme = *cx.theme();
        let settings = self.settings.read(cx).settings().clone();
        let file_note: SharedString = match filex::settings::default_settings_file() {
            Some(path) => format!("saved to {}", path.display()).into(),
            None => "no config directory found — settings won't persist".into(),
        };
        let close =
            ui::top_bar::toolbar_button(&theme, "settings-close", "icons/x.svg", theme.text_dim)
                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| this.toggle_settings(cx)))
                .into_any_element();
        let card = ui::settings_pane::settings_card(&theme, "settings-panel")
            .on_click(|_, _, cx| cx.stop_propagation())
            .child(ui::settings_pane::card_header(&theme, "Settings", close))
            .child(ui::settings_pane::choice_row(
                &theme,
                "Appearance",
                "Match the system, or force a light or dark palette",
                self.render_theme_selector(&theme, settings.theme, cx),
            ))
            .child(
                ui::settings_pane::toggle_row(
                    &theme,
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
                    &theme,
                    "confirm-delete",
                    "Confirm before deleting",
                    "First press arms; a second press moves the file to the trash",
                    settings.confirm_delete,
                )
                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                    this.settings.update(cx, |store, cx| {
                        store.update(cx, |s| s.confirm_delete = !s.confirm_delete);
                    });
                })),
            )
            .child(
                ui::settings_pane::toggle_row(
                    &theme,
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
                    &theme,
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
            .child(
                ui::settings_pane::toggle_row(
                    &theme,
                    "crash-reports",
                    "Send crash reports",
                    "Scrubbed crash details only — never file names, paths, or queries",
                    settings.crash_reports,
                )
                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                    this.settings.update(cx, |store, cx| {
                        store.update(cx, |s| s.crash_reports = !s.crash_reports);
                    });
                    // Turning it on drains anything already queued.
                    this.spawn_crash_upload(cx);
                })),
            )
            .child(ui::settings_pane::footnote(&theme, file_note));
        Some(
            ui::modal::backdrop("settings-backdrop")
                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| this.toggle_settings(cx)))
                .child(card)
                .into_any_element(),
        )
    }

    /// The three-way light/dark/system segmented control for the
    /// Appearance setting. Each segment writes the chosen mode straight
    /// to the store; the Changed event re-resolves and reinstalls the
    /// theme, restyling the whole app live.
    fn render_theme_selector(
        &self,
        theme: &Theme,
        current: ThemeMode,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let segment = |id: &'static str, label: &'static str, mode: ThemeMode| {
            ui::settings_pane::segment(theme, id, label, current == mode).on_click(cx.listener(
                move |this, _: &ClickEvent, _window, cx| {
                    this.settings.update(cx, |store, cx| {
                        store.update(cx, |s| s.theme = mode);
                    });
                },
            ))
        };
        ui::settings_pane::segmented(theme)
            .child(segment("theme-system", "System", ThemeMode::System))
            .child(segment("theme-light", "Light", ThemeMode::Light))
            .child(segment("theme-dark", "Dark", ThemeMode::Dark))
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
        let theme = *cx.theme();
        ui::sidebar::sidebar_row(&theme, ("service-root", ix))
            .child(ui::icon::ui_icon("icons/dot.svg", theme.accent).size(px(14.)))
            .child(div().flex_1().overflow_hidden().child(label))
            .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                this.navigate(path.clone(), cx);
            }))
            .into_any_element()
    }

    // `use<>`: the built element owns its data; without opting out of
    // lifetime capture it couldn't be collected across loop iterations.
    fn render_root_row(&self, ix: usize, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = *cx.theme();
        let slot = &self.roots[ix];
        let path = slot.path.clone();
        // Clicking a healthy root navigates to it; clicking a failed one
        // surfaces why it failed in the status bar.
        let failure: Option<SharedString> = match &slot.state {
            RootState::Failed(err) => Some(err.clone()),
            _ => None,
        };
        // Building spins (indexing is live); ready/failed are static.
        // Sidebar rows aren't virtualized, so animating here is fine.
        let marker = match &slot.state {
            RootState::Building => ui::icon::spinner(
                "icons/loader-circle.svg",
                theme.text_dim,
                14.,
                ("root-spin", ix),
            ),
            RootState::Ready { .. } => ui::icon::ui_icon("icons/dot.svg", theme.accent)
                .size(px(14.))
                .into_any_element(),
            RootState::Failed(_) => ui::icon::ui_icon("icons/triangle-alert.svg", theme.warn)
                .size(px(14.))
                .into_any_element(),
        };
        let menu_path = slot.path.clone();
        let tip = slot.path.display().to_string();
        ui::sidebar::sidebar_row(&theme, ("root", ix))
            .tooltip(ui::tooltip::text_tooltip(tip, theme))
            .child(marker)
            .child(ui::sidebar::sidebar_label(slot.label.clone()))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.open_root_menu(menu_path.clone(), event.position, window, cx);
                }),
            )
            .on_click(
                cx.listener(move |this, _: &ClickEvent, _window, cx| match &failure {
                    Some(err) => {
                        this.notice = Some(err.clone());
                        cx.notify();
                    }
                    None => this.navigate(path.clone(), cx),
                }),
            )
    }

    /// Navigate a recent folder, or open a recent file with its default
    /// app (a stat on click decides which).
    fn open_recent(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if path.is_dir() {
            self.navigate(path, cx);
        } else {
            self.open_target(path, false, false, cx);
        }
    }

    /// A clickable disclosure header; toggling it persists into settings
    /// and the Changed event re-renders the sidebar.
    fn section_header(
        &self,
        theme: &Theme,
        id: &'static str,
        label: &'static str,
        cx: &mut Context<Self>,
    ) -> (gpui::Stateful<gpui::Div>, bool) {
        let collapsed = self.is_section_collapsed(id, cx);
        let header = ui::sidebar::collapsible_header(theme, id, label, collapsed).on_click(
            cx.listener(move |this, _: &ClickEvent, _window, cx| {
                this.toggle_section(id, cx);
            }),
        );
        (header, collapsed)
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *cx.theme();
        let favorites = self.settings.read(cx).settings().favorites.clone();

        let mut content = div()
            .id("sidebar-scroll")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll();

        // PLACES.
        let (header, collapsed) = self.section_header(&theme, "places", "PLACES", cx);
        content = content.child(header);
        if !collapsed {
            let places: Vec<(&'static str, &str, PathBuf)> = [
                ("icons/house.svg", "Home", std::env::home_dir()),
                ("icons/hard-drive.svg", "Root", Some(PathBuf::from("/"))),
            ]
            .into_iter()
            .filter_map(|(icon, label, path)| Some((icon, label, path?)))
            .collect();
            content = content.children(places.into_iter().enumerate().map(
                |(ix, (icon, label, path))| {
                    ui::sidebar::sidebar_row(&theme, ("place", ix))
                        .child(ui::icon::ui_icon(icon, theme.text_dim).size(px(16.)))
                        .child(ui::sidebar::sidebar_label(label))
                        .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                            this.navigate(path.clone(), cx);
                        }))
                },
            ));
        }

        // FAVORITES (only shown once something is pinned).
        if !favorites.is_empty() {
            let (header, collapsed) = self.section_header(&theme, "favorites", "FAVORITES", cx);
            content = content.child(header);
            if !collapsed {
                content = content.children(favorites.into_iter().enumerate().map(|(ix, path)| {
                    let label = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.display().to_string());
                    let (nav, menu) = (path.clone(), path.clone());
                    let tip = path.display().to_string();
                    ui::sidebar::sidebar_row(&theme, ("favorite", ix))
                        .tooltip(ui::tooltip::text_tooltip(tip, theme))
                        .child(ui::icon::ui_icon("icons/star.svg", theme.accent).size(px(14.)))
                        .child(ui::sidebar::sidebar_label(label))
                        .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                            this.navigate(nav.clone(), cx);
                        }))
                        .on_mouse_down(
                            MouseButton::Right,
                            cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                                this.open_favorite_menu(menu.clone(), event.position, window, cx);
                            }),
                        )
                }));
            }
        }

        // RECENTS (only shown once something's been opened).
        if !self.recents.is_empty() {
            let (header, collapsed) = self.section_header(&theme, "recents", "RECENTS", cx);
            content = content.child(header);
            if !collapsed {
                content =
                    content.children(self.recents.recent_paths().enumerate().map(|(ix, path)| {
                        let path = path.to_path_buf();
                        let label = path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| path.display().to_string());
                        let tip = path.display().to_string();
                        ui::sidebar::sidebar_row(&theme, ("recent", ix))
                            .tooltip(ui::tooltip::text_tooltip(tip, theme))
                            .child(
                                ui::icon::ui_icon("icons/clock.svg", theme.text_dim).size(px(14.)),
                            )
                            .child(ui::sidebar::sidebar_label(label))
                            .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                                this.open_recent(path.clone(), cx);
                            }))
                    }));
                content = content.child(
                    ui::sidebar::sidebar_row(&theme, "recents-clear")
                        .text_color(theme.text_dim)
                        .child(ui::icon::ui_icon("icons/trash-2.svg", theme.text_dim).size(px(13.)))
                        .child("Clear")
                        .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                            this.clear_recents(cx);
                        })),
                );
            }
        }

        // TAGS (only once something's been tagged). Clicking a tag runs a
        // `tag:NAME` search.
        if !self.sidebar_tags.is_empty() {
            let (header, collapsed) = self.section_header(&theme, "tags", "TAGS", cx);
            content = content.child(header);
            if !collapsed {
                content =
                    content.children(self.sidebar_tags.iter().enumerate().map(|(ix, tag)| {
                        let name = tag.name.clone();
                        let dot = ui::details::tag_dot_color(&theme, tag.color);
                        ui::sidebar::sidebar_row(&theme, ("tag", ix))
                            .tooltip(ui::tooltip::text_tooltip(name.clone(), theme))
                            .child(div().flex_none().size(px(8.)).rounded_full().bg(dot))
                            .child(ui::sidebar::sidebar_label(SharedString::from(name.clone())))
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                this.search_tag(name.clone(), window, cx);
                            }))
                    }));
            }
        }

        // INDEXED.
        let (header, collapsed) = self.section_header(&theme, "indexed", "INDEXED", cx);
        content = content.child(header);
        if !collapsed {
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
            content = content.children(root_rows).child(
                ui::sidebar::sidebar_row(&theme, "add-root")
                    .text_color(theme.text_dim)
                    .child("+ index current folder")
                    .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                        this.add_current_folder(cx);
                    })),
            );
        }

        // DRIVES (only once the background refresh has found any).
        if !self.drives.is_empty() {
            let (header, collapsed) = self.section_header(&theme, "drives", "DRIVES", cx);
            content = content.child(header);
            if !collapsed {
                content = content.children(self.drives.iter().enumerate().map(|(ix, drive)| {
                    let path = drive.path.clone();
                    let free_line: SharedString = format!(
                        "{} free of {}",
                        format_size(drive.free_bytes),
                        format_size(drive.total_bytes)
                    )
                    .into();
                    ui::sidebar::drive_row(&theme, ("drive", ix))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    ui::icon::ui_icon("icons/hard-drive.svg", theme.text_dim)
                                        .size(px(14.)),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .text_sm()
                                        .overflow_hidden()
                                        .child(SharedString::from(drive.name.clone())),
                                ),
                        )
                        .child(ui::sidebar::capacity_bar(&theme, drive.used_fraction()))
                        .child(div().text_xs().text_color(theme.text_dim).child(free_line))
                        .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                            this.navigate(path.clone(), cx);
                        }))
                }));
            }
        }

        let sidebar = ui::sidebar::sidebar_panel(&theme).child(content);

        // The FDA banner stays pinned below the scrollable sections.
        #[cfg(target_os = "macos")]
        let sidebar = if self.fda_missing {
            sidebar.child(
                ui::sidebar::sidebar_row(&theme, "fda-banner")
                    .text_xs()
                    .text_color(theme.warn)
                    .child(ui::icon::ui_icon("icons/triangle-alert.svg", theme.warn).size(px(14.)))
                    .child("Grant Full Disk Access")
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
        let theme = *cx.theme();
        let sort = self.settings.read(cx).settings().sort;
        let active = |by: SortBy| (sort.by == by).then_some(sort.ascending);
        let on_sort = |by: SortBy| {
            cx.listener(move |this: &mut Self, _: &ClickEvent, _window, cx| {
                this.set_sort(by, cx);
            })
        };
        ui::list_row::header_row(&theme, ui::icon::ICON_SIZE)
            .child(
                ui::list_row::header_cell(&theme, "sort-name", "Name", active(SortBy::Name))
                    .flex_1()
                    .on_click(on_sort(SortBy::Name)),
            )
            .child(
                ui::list_row::header_cell(
                    &theme,
                    "sort-modified",
                    "Modified",
                    active(SortBy::Modified),
                )
                .w(px(ui::list_row::MODIFIED_COL_WIDTH))
                .flex_none()
                .justify_end()
                .on_click(on_sort(SortBy::Modified)),
            )
            .child(
                ui::list_row::header_cell(&theme, "sort-size", "Size", active(SortBy::Size))
                    .w(px(ui::list_row::SIZE_COL_WIDTH))
                    .flex_none()
                    .justify_end()
                    .on_click(on_sort(SortBy::Size)),
            )
    }

    fn render_browse_pane(&mut self, window: &Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let inner = match self.settings.read(cx).settings().view {
            ViewMode::List => div()
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .child(self.render_column_headers(cx))
                .child(self.render_file_list(cx))
                .into_any_element(),
            ViewMode::Grid => self.render_grid(window, cx),
        };
        // The whole pane accepts an OS-file drop into the current folder
        // (a copy). Folder rows/cards register their own handlers and win
        // when the drop lands on them; this catches drops on empty space
        // or on non-folder rows. Internal drags are not handled here — the
        // items are already in this folder.
        let dest = self.cwd.clone();
        div()
            .id("browse-pane")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(inner)
            .on_drop(cx.listener(move |this, ext: &ExternalPaths, _window, cx| {
                this.drop_onto(dest.clone(), ext.paths().to_vec(), ClipMode::Copy, cx);
            }))
            .into_any_element()
    }

    /// The card grid: a `uniform_list` whose rows are strips of N cards,
    /// N derived from the pane width so it reflows on resize. Each row
    /// builds only its own cards, so the grid is as virtualized as the
    /// list.
    fn render_grid(&self, window: &Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let settings = self.settings.read(cx).settings();
        let size = ui::grid::card_size(settings.grid_zoom);
        let cell = ui::grid::cell_width(size);
        let preview_w = if settings.preview_open {
            ui::details::clamp_width(settings.preview_width)
        } else {
            0.
        };
        // Approximate content width: the window minus the sidebar, the
        // open details panel, and the grid's own inset. Off-by-one on the
        // odd frame just reflows.
        let content_w = (f32::from(window.viewport_size().width)
            - ui::sidebar::SIDEBAR_WIDTH
            - preview_w
            - ui::grid::CARD_GAP * 2.)
            .max(cell);
        let cols = ui::grid::columns_for(content_w, cell);
        let rows = self.entries.len().div_ceil(cols);
        uniform_list(
            "grid",
            rows,
            cx.processor(move |this, range: Range<usize>, _window, cx| {
                let theme = *cx.theme();
                range
                    .map(|row| {
                        let start = row * cols;
                        let end = (start + cols).min(this.entries.len());
                        let mut strip = ui::grid::grid_row(size);
                        for ix in start..end {
                            strip = strip.child(this.render_card(ix, size, &theme, cx));
                        }
                        strip
                    })
                    .collect()
            }),
        )
        .track_scroll(self.browse_scroll.clone())
        .flex_1()
        .into_any_element()
    }

    /// One browse card for entry `ix`. Copies the row data out before
    /// asking for the icon (which needs `&mut self`), mirroring the list
    /// processor.
    fn render_card(
        &mut self,
        ix: usize,
        size: f32,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(entry) = self.entries.get(ix) else {
            return div().into_any_element();
        };
        let is_dir = entry.is_dir;
        let is_selected = self.active_selection().contains(ix);
        let (name, path) = (entry.name.clone(), entry.path.clone());
        let detail: SharedString = if is_dir {
            format_modified(entry.modified, std::time::SystemTime::now()).into()
        } else {
            format_size(entry.size).into()
        };
        let name_tip: SharedString = name.clone().into();
        let icon = self.render_icon_cell(&name, &path, is_dir, size, cx);
        let drag = self.drag_items(ix, *theme);
        let card = ui::grid::card(theme, ("card", ix), size, is_selected)
            .tooltip(ui::tooltip::text_tooltip(name_tip, *theme))
            .child(ui::grid::card_icon_area(size).child(icon))
            .child(ui::grid::card_name(theme, size).child(name))
            .child(
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(theme.text_dim)
                    .child(detail),
            )
            .on_click(cx.listener(move |this, event: &ClickEvent, _window, cx| {
                if event.click_count() >= 2 {
                    this.activate(ix, cx);
                } else {
                    this.select_click(ix, event.modifiers(), cx);
                }
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.open_entry_menu(ix, false, event.position, window, cx);
                }),
            );
        let card = card.when_some(drag, |card, items| {
            card.on_drag(items, |items, position, _window, cx| {
                cx.new(|_| items.clone().at(position))
            })
        });
        let card = if is_dir {
            self.folder_drop_target(card, path, *theme, cx)
        } else {
            card
        };
        card.into_any_element()
    }

    fn render_file_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        uniform_list(
            "entries",
            self.entries.len(),
            cx.processor(|this, range: Range<usize>, _window, cx| {
                let theme = *cx.theme();
                range
                    .filter_map(|ix| {
                        // Copy row data out first: the icon cell needs &mut self (it may
                        // schedule a thumbnail decode) while `entry` borrows self.
                        let entry = this.entries.get(ix)?;
                        let is_dir = entry.is_dir;
                        let size = entry.size;
                        let modified = entry.modified;
                        let is_selected = this.active_selection().contains(ix);
                        let (name, path) = (entry.name.clone(), entry.path.clone());
                        let name_tip: SharedString = name.clone().into();
                        let icon =
                            this.render_icon_cell(&name, &path, is_dir, ui::icon::ICON_SIZE, cx);
                        let rename_input = this
                            .renaming
                            .as_ref()
                            .filter(|rename| rename.ix == ix)
                            .map(|rename| rename.input.clone());
                        let is_renaming = rename_input.is_some();
                        let name_cell = match rename_input {
                            Some(input) => div()
                                .flex_1()
                                .px_1()
                                .rounded_sm()
                                .border_1()
                                .border_color(theme.accent)
                                .text_sm()
                                .child(input)
                                .into_any_element(),
                            None => div()
                                .flex_1()
                                .min_w_0()
                                .child(div().w_full().text_sm().truncate().child(name))
                                .into_any_element(),
                        };
                        let drag = (!is_renaming).then(|| this.drag_items(ix, theme)).flatten();
                        let row = ui::list_row::list_row(&theme, ix, is_selected)
                            // Full name on hover (helps when it's truncated);
                            // suppressed mid-rename so it doesn't cover the field.
                            .when(!is_renaming, |row| {
                                row.tooltip(ui::tooltip::text_tooltip(name_tip, theme))
                            })
                            .child(icon)
                            .child(name_cell)
                            .child(ui::list_row::detail_cell(
                                &theme,
                                ui::list_row::MODIFIED_COL_WIDTH,
                                format_modified(modified, std::time::SystemTime::now()),
                            ))
                            .child(ui::list_row::detail_cell(
                                &theme,
                                ui::list_row::SIZE_COL_WIDTH,
                                if is_dir {
                                    "—".to_string()
                                } else {
                                    format_size(size)
                                },
                            ))
                            .on_click(cx.listener(move |this, event: &ClickEvent, _window, cx| {
                                if event.click_count() >= 2 {
                                    this.activate(ix, cx);
                                } else {
                                    this.select_click(ix, event.modifiers(), cx);
                                }
                            }))
                            .on_mouse_down(
                                MouseButton::Right,
                                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                                    this.open_entry_menu(ix, false, event.position, window, cx);
                                }),
                            );
                        // Any row can be a drag source; only folders accept a drop.
                        let row = row.when_some(drag, |row, items| {
                            row.on_drag(items, |items, position, _window, cx| {
                                cx.new(|_| items.clone().at(position))
                            })
                        });
                        let row = row.when(is_dir, |row| {
                            this.folder_drop_target(row, path.clone(), theme, cx)
                        });
                        Some(row)
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
                let theme = *cx.theme();
                range
                    .filter_map(|ix| {
                        let row = this.results.get(ix)?;
                        let is_dir = row.is_dir;
                        let is_selected = this.active_selection().contains(ix);
                        let (name, path) = (row.name.clone(), row.target.clone());
                        let path_label = row.path_label.clone();
                        let icon =
                            this.render_icon_cell(&name, &path, is_dir, ui::icon::ICON_SIZE, cx);
                        Some(
                            ui::list_row::list_row(&theme, ix, is_selected)
                                // Full path on hover — the row truncates it below.
                                .tooltip(ui::tooltip::text_tooltip(path_label.clone(), theme))
                                .child(icon)
                                .child(div().flex_none().text_sm().whitespace_nowrap().child(name))
                                // flex_1 + min_w_0 lets the cell shrink; the inner
                                // w_full gives the text a *definite* width, which
                                // gpui needs to actually paint the … ellipsis
                                // (a bare flex child never truncates).
                                .child(
                                    div().flex_1().min_w_0().child(
                                        div()
                                            .w_full()
                                            .text_xs()
                                            .text_color(theme.text_dim)
                                            .truncate()
                                            .child(path_label),
                                    ),
                                )
                                .on_click(cx.listener(
                                    move |this, event: &ClickEvent, _window, cx| {
                                        if event.click_count() >= 2 {
                                            this.activate(ix, cx);
                                        } else {
                                            this.select_click(ix, event.modifiers(), cx);
                                        }
                                    },
                                ))
                                .on_mouse_down(
                                    MouseButton::Right,
                                    cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                                        this.open_entry_menu(ix, true, event.position, window, cx);
                                    }),
                                ),
                        )
                    })
                    .collect()
            }),
        )
        .track_scroll(self.results_scroll.clone())
        .flex_1()
    }

    fn render_search_pane(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = *cx.theme();
        if !self.any_root_ready() {
            return ui::pane::empty_state(
                &theme,
                "still indexing — results will appear when ready…",
            );
        }
        if self.results.is_empty() {
            return ui::pane::empty_state(&theme, format!("no matches for “{}”", self.query));
        }
        self.render_search_results(cx).into_any_element()
    }

    /// Keep the menu on screen: pull the anchor back from the right and
    /// bottom edges by the menu's size (estimated height — items are
    /// fixed-height so the estimate is close).
    fn clamped_menu_position(
        raw: Point<Pixels>,
        est_height: f32,
        window: &Window,
    ) -> Point<Pixels> {
        let viewport = window.viewport_size();
        let max_x = viewport.width - px(ui::menu::MENU_WIDTH + 8.);
        let max_y = viewport.height - px(est_height);
        gpui::point(raw.x.min(max_x).max(px(0.)), raw.y.min(max_y).max(px(0.)))
    }

    fn open_entry_menu(
        &mut self,
        ix: usize,
        from_search: bool,
        position: Point<Pixels>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if self.renaming.is_some() || self.conflict.is_some() {
            return;
        }
        let target = if from_search {
            let Some(row) = self.results.get(ix) else {
                return;
            };
            MenuTarget::Entry {
                ix,
                path: row.target.clone(),
                name: row.name.to_string(),
                is_dir: row.is_dir,
                from_search,
            }
        } else {
            let Some(entry) = self.entries.get(ix) else {
                return;
            };
            MenuTarget::Entry {
                ix,
                path: entry.path.clone(),
                name: entry.name.clone(),
                is_dir: entry.is_dir,
                from_search,
            }
        };
        // Right-clicking a row outside the current selection selects
        // just it; right-clicking one already in a multi-selection keeps
        // the whole set, so the menu can act on all of it.
        if !self.active_selection().contains(ix) {
            self.active_selection_mut().select_one(ix);
        }
        self.context_menu = Some(ContextMenu {
            position: Self::clamped_menu_position(position, 280., window),
            target,
        });
        cx.notify();
    }

    fn open_root_menu(
        &mut self,
        path: PathBuf,
        position: Point<Pixels>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        self.context_menu = Some(ContextMenu {
            position: Self::clamped_menu_position(position, 90., window),
            target: MenuTarget::Root { path },
        });
        cx.notify();
    }

    fn open_favorite_menu(
        &mut self,
        path: PathBuf,
        position: Point<Pixels>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        self.context_menu = Some(ContextMenu {
            position: Self::clamped_menu_position(position, 130., window),
            target: MenuTarget::Favorite { path },
        });
        cx.notify();
    }

    fn close_menu(&mut self, cx: &mut Context<Self>) {
        if self.context_menu.take().is_some() {
            cx.notify();
        }
    }

    /// The search-scope dropdown (Anywhere / Current Dir), when open.
    fn render_scope_menu(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let position = *self.scope_menu.as_ref()?;
        let theme = *cx.theme();
        let current = self.search_scope;
        let items = [SearchScope::Anywhere, SearchScope::CurrentDir]
            .into_iter()
            .enumerate()
            .map(|(i, scope)| {
                let label = if scope == current {
                    format!("✓ {}", scope.label())
                } else {
                    format!("   {}", scope.label())
                };
                ui::menu::item(&theme, ("scope-item", i), label, false)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                        this.set_scope(scope, cx);
                    }))
                    .into_any_element()
            })
            .collect();
        Some(
            ui::menu::overlay("scope-menu-overlay")
                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                    this.close_scope_menu(cx);
                }))
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(|this, _: &MouseDownEvent, _window, cx| {
                        this.close_scope_menu(cx);
                    }),
                )
                .child(ui::menu::panel(&theme, position, items))
                .into_any_element(),
        )
    }

    fn render_context_menu(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let menu = self.context_menu.as_ref()?;
        let theme = *cx.theme();
        let mut items: Vec<gpui::AnyElement> = Vec::new();
        match &menu.target {
            MenuTarget::Entry {
                ix,
                path,
                name,
                is_dir,
                from_search,
            } => {
                let (ix, is_dir, from_search) = (*ix, *is_dir, *from_search);
                // The whole selection is the target; single-item-only
                // actions (open, rename, reveal, index) drop out when
                // more than one row is selected.
                let count = self.active_selection().len();
                let heading =
                    describe_items(&self.selected_paths()).unwrap_or_else(|| name.clone());
                items.push(ui::menu::heading(&theme, heading).into_any_element());

                if count <= 1 {
                    let p = path.clone();
                    items.push(
                        ui::menu::item(&theme, "menu-open", "Open", false)
                            .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                                this.close_menu(cx);
                                this.open_target(p.clone(), is_dir, from_search, cx);
                            }))
                            .into_any_element(),
                    );
                    if from_search {
                        let p = path.clone();
                        items.push(
                            ui::menu::item(&theme, "menu-reveal", "Reveal in Folder", false)
                                .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                                    this.close_menu(cx);
                                    this.reveal(p.clone(), cx);
                                }))
                                .into_any_element(),
                        );
                    } else {
                        items.push(
                            ui::menu::item(&theme, "menu-rename", "Rename", false)
                                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                    this.close_menu(cx);
                                    this.active_selection_mut().select_one(ix);
                                    this.start_rename(window, cx);
                                }))
                                .into_any_element(),
                        );
                    }
                    items.push(ui::menu::separator(&theme).into_any_element());
                }

                items.push(
                    ui::menu::item(&theme, "menu-copy", "Copy", false)
                        .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                            this.close_menu(cx);
                            this.clip_selected(ClipMode::Copy, cx);
                        }))
                        .into_any_element(),
                );
                items.push(
                    ui::menu::item(&theme, "menu-cut", "Cut", false)
                        .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                            this.close_menu(cx);
                            this.clip_selected(ClipMode::Cut, cx);
                        }))
                        .into_any_element(),
                );
                let copy_path_label = if count > 1 { "Copy Paths" } else { "Copy Path" };
                items.push(
                    ui::menu::item(&theme, "menu-copy-path", copy_path_label, false)
                        .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                            this.close_menu(cx);
                            this.copy_selected_paths(cx);
                        }))
                        .into_any_element(),
                );
                if count <= 1 && is_dir && !self.service_mode() {
                    let p = path.clone();
                    items.push(
                        ui::menu::item(&theme, "menu-index", "Index This Folder", false)
                            .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                                this.close_menu(cx);
                                this.add_root(p.clone(), cx);
                            }))
                            .into_any_element(),
                    );
                }
                if count <= 1 && is_dir {
                    let pinned = self.is_favorite(path, cx);
                    let p = path.clone();
                    let label = if pinned {
                        "Unpin from Sidebar"
                    } else {
                        "Pin to Sidebar"
                    };
                    items.push(
                        ui::menu::item(&theme, "menu-pin", label, false)
                            .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                                this.close_menu(cx);
                                if pinned {
                                    this.unpin_favorite(&p, cx);
                                } else {
                                    this.pin_favorite(p.clone(), cx);
                                }
                            }))
                            .into_any_element(),
                    );
                }
                items.push(ui::menu::separator(&theme).into_any_element());
                items.push(
                    ui::menu::item(&theme, "menu-trash", "Move to Trash", true)
                        .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                            this.close_menu(cx);
                            this.trash_selected(cx);
                        }))
                        .into_any_element(),
                );
            }
            MenuTarget::Root { path } => {
                let label = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                items.push(ui::menu::heading(&theme, label).into_any_element());
                let p = path.clone();
                items.push(
                    ui::menu::item(&theme, "menu-remove-root", "Remove from Index", true)
                        .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                            this.close_menu(cx);
                            this.remove_root(&p.clone(), cx);
                        }))
                        .into_any_element(),
                );
            }
            MenuTarget::Favorite { path } => {
                let label = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                items.push(ui::menu::heading(&theme, label).into_any_element());
                let p = path.clone();
                items.push(
                    ui::menu::item(&theme, "fav-up", "Move Up", false)
                        .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                            this.close_menu(cx);
                            this.move_favorite(&p, -1, cx);
                        }))
                        .into_any_element(),
                );
                let p = path.clone();
                items.push(
                    ui::menu::item(&theme, "fav-down", "Move Down", false)
                        .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                            this.close_menu(cx);
                            this.move_favorite(&p, 1, cx);
                        }))
                        .into_any_element(),
                );
                items.push(ui::menu::separator(&theme).into_any_element());
                let p = path.clone();
                items.push(
                    ui::menu::item(&theme, "fav-unpin", "Unpin from Sidebar", true)
                        .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                            this.close_menu(cx);
                            this.unpin_favorite(&p, cx);
                        }))
                        .into_any_element(),
                );
            }
        }
        Some(
            ui::menu::overlay("context-menu-overlay")
                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                    this.close_menu(cx);
                }))
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(|this, _: &MouseDownEvent, _window, cx| {
                        this.close_menu(cx);
                    }),
                )
                .child(ui::menu::panel(&theme, menu.position, items))
                .into_any_element(),
        )
    }

    fn render_conflict_modal(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let conflict = self.conflict.as_ref()?;
        let theme = *cx.theme();
        let name = conflict
            .dest
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        Some(
            ui::modal::backdrop("conflict-backdrop")
                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                    this.resolve_conflict(false, cx);
                }))
                .child(
                    ui::modal::panel(&theme, "conflict-panel")
                        .on_click(|_, _, cx| cx.stop_propagation())
                        .child(ui::modal::title(
                            &theme,
                            format!("“{name}” already exists here"),
                        ))
                        .child(ui::modal::message(
                            &theme,
                            "Nothing is overwritten: keep both renames the new one to a \
                             free “name 2” variant.",
                        ))
                        .child(
                            ui::modal::buttons()
                                .child(
                                    ui::modal::button(&theme, "conflict-cancel", "Cancel", false)
                                        .on_click(cx.listener(
                                            |this, _: &ClickEvent, _window, cx| {
                                                this.resolve_conflict(false, cx);
                                            },
                                        )),
                                )
                                .child(
                                    ui::modal::button(&theme, "conflict-keep", "Keep Both", true)
                                        .on_click(cx.listener(
                                            |this, _: &ClickEvent, _window, cx| {
                                                this.resolve_conflict(true, cx);
                                            },
                                        )),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }

    fn render_status_bar(&self, theme: &Theme) -> impl IntoElement {
        let left: SharedString = if let Some(notice) = &self.notice {
            notice.clone()
        } else if let Some(err) = &self.load_error {
            format!("error — {err}").into()
        } else if !self.active_selection().is_empty() {
            let (n, total) = (self.active_selection().len(), self.active_list_len());
            // Combined size only in browse — search rows carry no size.
            if self.query.is_empty() {
                let bytes: u64 = self
                    .selection
                    .iter()
                    .filter_map(|ix| self.entries.get(ix))
                    .filter(|entry| !entry.is_dir)
                    .map(|entry| entry.size)
                    .sum();
                format!("{n} of {total} selected · {}", format_size(bytes)).into()
            } else {
                format!("{n} of {total} selected").into()
            }
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
        ui::status_bar::status_bar(theme, left, self.index_status_text())
    }

    /// The tab strip, shown only with more than one tab open. The "+"
    /// opens a tab at the active tab's folder.
    /// The topmost bar: the tab strip on the left and the window/view
    /// icon group (zoom, list⇄grid, preview, settings) on the right.
    /// Always shown — a single tab still gets a chip, and the icon group
    /// needs a stable home now that it has left the nav bar.
    fn render_tab_bar(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = *cx.theme();
        let settings = self.settings.read(cx).settings();
        let is_grid = settings.view == ViewMode::Grid;
        let preview_open = settings.preview_open;

        // Only offer a close control when closing is meaningful (more than
        // one tab); a lone tab reads as the window's title, not a chip to
        // dismiss.
        let closable = self.tabs.len() > 1;
        let mut tabs = ui::tabs::tab_group();
        for i in 0..self.tabs.len() {
            let mut chip = ui::tabs::tab(&theme, ("tab", i), i == self.active_tab)
                .child(ui::tabs::tab_label(self.tab_title(i)));
            if closable {
                chip = chip.child(ui::tabs::tab_close(&theme, ("tab-close", i)).on_click(
                    cx.listener(move |this, _: &ClickEvent, _window, cx| {
                        cx.stop_propagation();
                        this.close_tab(i, cx);
                    }),
                ));
            }
            tabs = tabs.child(
                chip.on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                    this.activate_tab(i, cx);
                }))
                .on_mouse_down(
                    MouseButton::Middle,
                    cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                        this.close_tab(i, cx);
                    }),
                ),
            );
        }
        let cwd = self.cwd.clone();
        tabs = tabs.child(
            ui::tabs::new_tab_button(&theme, "new-tab").on_click(cx.listener(
                move |this, _: &ClickEvent, _window, cx| {
                    this.open_tab(cwd.clone(), cx);
                },
            )),
        );

        let icons = div()
            .flex()
            .flex_none()
            .items_center()
            .gap_1()
            // Grid-only zoom stepper.
            .children(is_grid.then(|| {
                ui::top_bar::toolbar_button(&theme, "zoom-out", "icons/minus.svg", theme.text_dim)
                    .on_click(
                        cx.listener(|this, _: &ClickEvent, _window, cx| this.zoom_grid(-1, cx)),
                    )
            }))
            .children(is_grid.then(|| {
                ui::top_bar::toolbar_button(&theme, "zoom-in", "icons/plus.svg", theme.text_dim)
                    .on_click(
                        cx.listener(|this, _: &ClickEvent, _window, cx| this.zoom_grid(1, cx)),
                    )
            }))
            // List ⇄ grid toggle: shows the layout it switches to.
            .child(
                ui::top_bar::toolbar_button(
                    &theme,
                    "view-toggle",
                    if is_grid {
                        "icons/list.svg"
                    } else {
                        "icons/layout-grid.svg"
                    },
                    theme.text_dim,
                )
                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| this.toggle_view(cx))),
            )
            // Details/preview panel toggle.
            .child(
                ui::top_bar::toolbar_button(
                    &theme,
                    "preview-toggle",
                    "icons/panel-right.svg",
                    if preview_open {
                        theme.accent
                    } else {
                        theme.text_dim
                    },
                )
                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| this.toggle_preview(cx))),
            )
            .child(
                ui::top_bar::toolbar_button(
                    &theme,
                    "settings",
                    "icons/settings.svg",
                    if self.settings_open {
                        theme.accent
                    } else {
                        theme.text_dim
                    },
                )
                .on_click(
                    cx.listener(|this, _: &ClickEvent, _window, cx| this.toggle_settings(cx)),
                ),
            );

        ui::tabs::tab_bar(&theme)
            .child(tabs)
            .child(icons)
            .into_any_element()
    }

    /// The right-hand details/preview panel for the lead item. `None`
    /// when the panel is closed. (Settings now float in a modal over the
    /// browse view rather than replacing it, so the panel stays put
    /// underneath.) Uses `&mut self` because the preview image may
    /// schedule a thumbnail decode, exactly like a list row.
    fn render_details_panel(&mut self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let settings = self.settings.read(cx).settings();
        if !settings.preview_open {
            return None;
        }
        let width = settings.preview_width;
        let theme = *cx.theme();
        let Some((path, name, is_dir)) = self.lead_item() else {
            return Some(
                ui::details::panel(&theme, width)
                    .child(ui::details::empty(&theme, "No selection"))
                    .into_any_element(),
            );
        };
        let kind = FileKind::of(&name, is_dir);
        let preview = self.render_icon_cell(&name, &path, is_dir, 160., cx);
        let meta = self.preview_meta.as_ref().filter(|m| m.path == path);
        let now = std::time::SystemTime::now();

        let size_value: SharedString = if is_dir {
            "—".into()
        } else {
            match meta {
                Some(m) => format_size(m.size).into(),
                None => "…".into(),
            }
        };
        let modified_value: SharedString = match meta {
            Some(m) => format_modified(m.modified, now).into(),
            None => "…".into(),
        };
        let created_value: SharedString = match meta {
            Some(m) => format_modified(m.created, now).into(),
            None => "…".into(),
        };
        let where_value: SharedString = path
            .parent()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
            .into();

        let mut panel = ui::details::panel(&theme, width)
            .child(ui::details::preview_box(&theme).child(preview))
            .child(ui::details::title(&theme, name))
            .child(ui::details::divider(&theme))
            .child(ui::details::meta_row(&theme, "Kind", kind.label()))
            .child(ui::details::meta_row(&theme, "Size", size_value))
            .child(ui::details::meta_row(&theme, "Modified", modified_value))
            .child(ui::details::meta_row(&theme, "Created", created_value));
        if let Some((w, h)) = meta.and_then(|m| m.dimensions) {
            panel = panel.child(ui::details::meta_row(
                &theme,
                "Dimensions",
                format!("{w} × {h}"),
            ));
        }
        let panel = panel
            .child(ui::details::divider(&theme))
            .child(ui::details::meta_row(&theme, "Where", where_value))
            .child(ui::details::divider(&theme))
            .child(self.render_tags_section(&path, &theme, cx));
        Some(panel.into_any_element())
    }

    /// The details-panel "Tags" section: the item's chips (each opens the
    /// editor), an add affordance, and the inline editor when open.
    fn render_tags_section(
        &self,
        path: &Path,
        theme: &ui::theme::Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let mut chips = ui::details::wrap_row();
        for (i, tag) in self.preview_tags.iter().enumerate() {
            let for_edit = tag.clone();
            chips = chips.child(
                ui::details::tag_chip(theme, ("tag-chip", i), tag.name.clone(), tag.color)
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.open_tag_editor(Some(for_edit.clone()), window, cx);
                    })),
            );
        }
        chips = chips.child(ui::details::add_tag_chip(theme, "tag-add").on_click(
            cx.listener(|this, _: &ClickEvent, window, cx| this.open_tag_editor(None, window, cx)),
        ));

        let mut section = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(ui::details::section_label(theme, "Tags"))
            .child(chips);

        if let Some(editor) = self.tag_editor.as_ref().filter(|e| e.path == path) {
            section = section.child(self.render_tag_editor(editor, theme, cx));
        }
        section.into_any_element()
    }

    /// The inline tag editor card: name input, color swatches, and the
    /// Save / Remove / Cancel buttons.
    fn render_tag_editor(
        &self,
        editor: &TagEditor,
        theme: &ui::theme::Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let selected = editor.color;
        let is_existing = editor.existing.is_some();

        let input_box = div()
            .px_2()
            .py_1()
            .rounded_md()
            .border_1()
            .border_color(theme.accent)
            .text_sm()
            .child(editor.input.clone());

        let mut swatches = ui::details::wrap_row().child(
            ui::details::tag_swatch(theme, "tag-sw-none", None, selected.is_none()).on_click(
                cx.listener(|this, _: &ClickEvent, _window, cx| this.set_editor_color(None, cx)),
            ),
        );
        for color in TagColor::all() {
            swatches = swatches.child(
                ui::details::tag_swatch(
                    theme,
                    ("tag-sw", color.finder_index() as usize),
                    Some(color),
                    selected == Some(color),
                )
                .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                    this.set_editor_color(Some(color), cx);
                })),
            );
        }

        let mut footer = div().flex().items_center().gap_2().child(
            ui::details::tag_button(
                theme,
                "tag-save",
                if is_existing { "Save" } else { "Add" },
                false,
            )
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.commit_tag_editor(window, cx);
            })),
        );
        if is_existing {
            footer = footer.child(
                ui::details::tag_button(theme, "tag-remove", "Remove", true).on_click(cx.listener(
                    |this, _: &ClickEvent, window, cx| {
                        this.remove_editing_tag(window, cx);
                    },
                )),
            );
        }
        footer = footer.child(
            ui::details::tag_button(theme, "tag-cancel", "Cancel", false).on_click(
                cx.listener(|this, _: &ClickEvent, window, cx| this.cancel_tag_editor(window, cx)),
            ),
        );

        ui::details::tag_editor_box(theme)
            .child(input_box)
            .child(swatches)
            .child(footer)
            .into_any_element()
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let searching = !self.query.is_empty();
        let theme = *cx.theme();
        div()
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &GoUp, _window, cx| this.go_up(cx)))
            .on_action(cx.listener(|this, _: &GoBack, _window, cx| this.go_back(cx)))
            .on_action(cx.listener(|this, _: &GoForward, _window, cx| this.go_forward(cx)))
            .on_action(cx.listener(|this, _: &Refresh, _window, cx| this.reload_dir(cx)))
            .on_action(cx.listener(|this, _: &NewTab, _window, cx| {
                let cwd = this.cwd.clone();
                this.open_tab(cwd, cx);
            }))
            // cmd-w closes the active tab; the last tab closes the window.
            .on_action(cx.listener(|this, _: &CloseTab, window, cx| {
                if this.tabs.len() <= 1 {
                    window.remove_window();
                } else {
                    this.close_tab(this.active_tab, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &NextTab, _window, cx| {
                let next = (this.active_tab + 1) % this.tabs.len();
                this.activate_tab(next, cx);
            }))
            .on_action(cx.listener(|this, _: &PrevTab, _window, cx| {
                let prev = (this.active_tab + this.tabs.len() - 1) % this.tabs.len();
                this.activate_tab(prev, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleSettings, _window, cx| {
                this.toggle_settings(cx);
            }))
            .on_action(cx.listener(|this, _: &TogglePreview, _window, cx| {
                this.toggle_preview(cx);
            }))
            .on_action(cx.listener(|this, _: &RenameSelected, window, cx| {
                this.start_rename(window, cx);
            }))
            .on_action(cx.listener(|this, _: &DeleteSelected, _window, cx| {
                this.delete_selected(cx);
            }))
            // Bubbled up from the (empty) search input: file-level
            // clipboard operations on the selected row.
            .on_action(cx.listener(|this, _: &search_input::Copy, _window, cx| {
                this.clip_selected(ClipMode::Copy, cx);
            }))
            .on_action(cx.listener(|this, _: &search_input::Cut, _window, cx| {
                this.clip_selected(ClipMode::Cut, cx);
            }))
            .on_action(cx.listener(|this, _: &search_input::Paste, _window, cx| {
                this.paste_clipboard(cx);
            }))
            // cmd-a while the input is empty selects every row.
            .on_action(
                cx.listener(|this, _: &search_input::SelectAll, _window, cx| {
                    this.select_all(cx);
                }),
            )
            // Escape, bubbled by the empty input: close whatever is
            // topmost — conflict dialog first, then the settings pane.
            .on_action(
                cx.listener(|this, _: &search_input::ClearInput, _window, cx| {
                    if this.context_menu.is_some() {
                        this.close_menu(cx);
                    } else if this.conflict.is_some() {
                        this.resolve_conflict(false, cx);
                    } else if this.settings_open {
                        this.toggle_settings(cx);
                    }
                }),
            )
            .on_action(cx.listener(|this, _: &Undo, _window, cx| {
                this.undo_last(cx);
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.handle_key(event, window, cx);
            }))
            .flex()
            .flex_col()
            .size_full()
            .font_family(ui::fonts::UI_FONT_FAMILY)
            .bg(theme.bg)
            .text_color(theme.text)
            .child(self.render_tab_bar(cx))
            .child(self.render_top_bar(cx))
            .children(self.render_filter_chips(cx))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_sidebar(cx))
                    // Magic view replaces the results list outright — the
                    // plan is the content, not a card above unrelated rows
                    // (docs/design-magic-mode.md v2). Details panel is
                    // suppressed alongside it: it describes a browse/search
                    // selection that magic mode isn't showing.
                    .child(if self.in_magic_view() {
                        self.render_magic_pane(cx)
                    } else if searching {
                        self.render_search_pane(cx)
                    } else {
                        self.render_browse_pane(window, cx)
                    })
                    .children(
                        (!self.in_magic_view())
                            .then(|| self.render_details_panel(cx))
                            .flatten(),
                    ),
            )
            .children(self.render_jobs(cx))
            .children(self.render_update_banner(cx))
            .child(self.render_status_bar(&theme))
            .children(self.render_scope_menu(cx))
            .children(self.render_context_menu(cx))
            .children(self.render_settings_modal(cx))
            .children(self.render_conflict_modal(cx))
    }
}

fn main() {
    let _logging_guard = filex::logging::init("filex");
    filex::telemetry::install_panic_hook("filex");
    // A startup line at the default level, so the log file is never empty:
    // "logs are blank" then means "not writing", not "nothing happened".
    // Names the log directory in the log itself, and the slow-op warnings
    // land here without any RUST_LOG needed.
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        "filex starting — slow operations (>{SLOW_OP_MS}ms) log at warn; \
         set RUST_LOG=filex=debug for per-scan timing"
    );
    Application::new()
        .with_assets(ui::assets::Assets)
        .run(|cx: &mut App| {
            // Register the bundled UI font before anything renders.
            ui::fonts::register(cx);
            // A default theme so `cx.theme()` is valid from the first frame;
            // the workspace refines it against the real window appearance as
            // soon as a window exists (see the open-window closure below).
            cx.set_global(Theme::dark());
            cx.on_action(|_: &Quit, cx| cx.quit());
            cx.bind_keys([
                #[cfg(target_os = "macos")]
                KeyBinding::new("cmd-q", Quit, None),
                #[cfg(target_os = "macos")]
                KeyBinding::new("cmd-w", CloseTab, None),
                #[cfg(target_os = "macos")]
                KeyBinding::new("cmd-t", NewTab, None),
                #[cfg(target_os = "macos")]
                KeyBinding::new("cmd-up", GoUp, None),
                #[cfg(target_os = "macos")]
                KeyBinding::new("cmd-[", GoBack, None),
                #[cfg(target_os = "macos")]
                KeyBinding::new("cmd-]", GoForward, None),
                #[cfg(target_os = "macos")]
                KeyBinding::new("cmd-r", Refresh, None),
                #[cfg(target_os = "macos")]
                KeyBinding::new("cmd-,", ToggleSettings, None),
                #[cfg(target_os = "macos")]
                KeyBinding::new("cmd-i", TogglePreview, None),
                #[cfg(target_os = "macos")]
                KeyBinding::new("cmd-z", Undo, None),
                // Finder's delete shortcut. Plain Delete can't work here:
                // the always-focused search input consumes it as text
                // editing.
                #[cfg(target_os = "macos")]
                KeyBinding::new("cmd-backspace", DeleteSelected, None),
                #[cfg(not(target_os = "macos"))]
                KeyBinding::new("ctrl-q", Quit, None),
                #[cfg(not(target_os = "macos"))]
                KeyBinding::new("ctrl-w", CloseTab, None),
                #[cfg(not(target_os = "macos"))]
                KeyBinding::new("ctrl-t", NewTab, None),
                #[cfg(not(target_os = "macos"))]
                KeyBinding::new("alt-up", GoUp, None),
                #[cfg(not(target_os = "macos"))]
                KeyBinding::new("alt-left", GoBack, None),
                #[cfg(not(target_os = "macos"))]
                KeyBinding::new("alt-right", GoForward, None),
                #[cfg(not(target_os = "macos"))]
                KeyBinding::new("ctrl-r", Refresh, None),
                // F5 refreshes on every platform (Explorer convention).
                KeyBinding::new("f5", Refresh, None),
                #[cfg(not(target_os = "macos"))]
                KeyBinding::new("ctrl-,", ToggleSettings, None),
                #[cfg(not(target_os = "macos"))]
                KeyBinding::new("ctrl-i", TogglePreview, None),
                #[cfg(not(target_os = "macos"))]
                KeyBinding::new("ctrl-z", Undo, None),
                // Not plain Delete: the always-focused search input
                // consumes that for text editing.
                #[cfg(not(target_os = "macos"))]
                KeyBinding::new("ctrl-delete", DeleteSelected, None),
                // Tab cycling is ctrl-tab on every platform.
                KeyBinding::new("ctrl-tab", NextTab, None),
                KeyBinding::new("ctrl-shift-tab", PrevTab, None),
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
            // macOS: unified-titlebar look — the system titlebar goes
            // transparent and the traffic lights sit inset in our top bar
            // (which pads left to clear them). Elsewhere the native
            // titlebar stays.
            #[cfg(target_os = "macos")]
            let titlebar = TitlebarOptions {
                title: None,
                appears_transparent: true,
                // The tab bar is now the topmost bar, so the inset traffic
                // lights are centered against its height, not the nav bar's.
                traffic_light_position: Some(gpui::point(
                    px(12.),
                    px((ui::tabs::TAB_BAR_HEIGHT - 12.) / 2.),
                )),
            };
            #[cfg(not(target_os = "macos"))]
            let titlebar = TitlebarOptions {
                title: Some("filex".into()),
                ..Default::default()
            };
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(titlebar),
                    ..Default::default()
                },
                |window, cx| {
                    cx.activate(true);
                    let workspace = cx.new(|cx| {
                        let workspace = Workspace::new(cx);
                        // Focus the input so typing searches immediately;
                        // navigation keys bubble up to the workspace.
                        window.focus(&workspace.search_input.focus_handle(cx));
                        workspace
                    });
                    // A window now exists, so its OS appearance is known:
                    // resolve the theme against it, and keep it in sync as
                    // the user flips the OS between light and dark.
                    workspace.update(cx, |ws, cx| {
                        ws.appearance = window.appearance();
                        ws.apply_theme(cx);
                    });
                    window
                        .observe_window_appearance({
                            let workspace = workspace.downgrade();
                            move |window, cx| {
                                workspace
                                    .update(cx, |ws, cx| {
                                        ws.appearance = window.appearance();
                                        ws.apply_theme(cx);
                                    })
                                    .ok();
                            }
                        })
                        .detach();
                    workspace
                },
            )
            .expect("failed to open the main window");
        });
}

#[cfg(test)]
mod magic_ui_tests {
    use super::*;
    use filex::magic::{Plan, Verb};

    /// Regression for the real case in dogfooding: `rename gravloc to …`
    /// returned 148 rows, of which 4 actually contained "gravloc". The
    /// rest were subsequence matches on unrelated files, and every one of
    /// them would have been renamed on confirm.
    #[test]
    fn a_command_plan_never_includes_fuzzy_matches() {
        use filex::index::{ROOT, VolumeIndex};

        let mut index = VolumeIndex::new("/vol");
        for name in ["Gravloc", "Gravloc.pdf", "Gravloc Logo.af"] {
            index.insert(ROOT, name, false).unwrap();
        }
        // Subsequence-matches "gravloc" (g-r-a-v-l-o-c) but shares no
        // substring with it — exactly the shape that polluted the plan.
        for name in [
            "xstate-graph.development.cjs.js",
            "generate_umath_validation.cpp",
        ] {
            index.insert(ROOT, name, false).unwrap();
        }

        let hits = index.search("gravloc", 500);
        assert!(
            hits.iter().any(|h| h.score.kind == MatchKind::Fuzzy),
            "fixture must actually produce fuzzy hits, or this proves nothing"
        );

        let planned = hits
            .iter()
            .filter(|h| usable_in_plan(h.score.kind, true))
            .count();
        let searched = hits
            .iter()
            .filter(|h| usable_in_plan(h.score.kind, false))
            .count();

        assert_eq!(
            planned, 3,
            "only the literal Gravloc matches may be planned"
        );
        assert_eq!(
            searched,
            hits.len(),
            "ordinary search still shows everything"
        );
    }

    fn state(outcome: Result<Plan, filex::magic::PlanError>, checked: Vec<bool>) -> MagicState {
        MagicState {
            command: filex::magic::parse("delete screenshots older than 30 days", 1_785_067_200)
                .expect("fixture should parse"),
            outcome: Some(outcome),
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
        assert_eq!(
            row.dest.unwrap().to_string(),
            format!("→ {}", Path::new("/b/Archive").display())
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
            format!("→ {}", Path::new("/b/shot 2.png").display())
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
        assert_eq!(row.dest.unwrap().to_string(), "→ shot-1.png");
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

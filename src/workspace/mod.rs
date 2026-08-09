//! The Filex application shell: the [`Workspace`] root view, its state
//! types, and the `run` entry point. The `impl Workspace` surface is large
//! enough that it is split across the sibling `workspace::*` modules by
//! concern (rendering, navigation, search, file ops, tags, input); each is
//! a plain `impl Workspace` block that reaches shared imports through
//! `use super::*`.

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

use crate::settings_store::{SettingsEvent, SettingsStore};
use crate::thumbnails::{self, ThumbnailState};
use crate::ui;
use crate::ui::search_input::{self, SearchInput, SearchInputEvent};
use crate::ui::theme::{ActiveTheme as _, Theme};
use filex::listing::FileKind;

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
        use std::os::windows::process::CommandExt as _;
        // `start` is a cmd builtin; the empty string is the window title
        // slot so paths with spaces aren't misparsed as a title.
        // CREATE_NO_WINDOW keeps this transient cmd from flashing a
        // console every time the user opens a file.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let mut command = std::process::Command::new("cmd");
        command
            .args(["/C", "start", ""])
            .arg(path)
            .creation_flags(CREATE_NO_WINDOW);
        command
    };
    command.spawn().map(drop)
}

/// Whether this platform can show an OS "Open with…" application chooser.
/// macOS has no CLI entry point for the picker (it needs a LaunchServices
/// call — a future item), so the menu entry is hidden there rather than
/// offering an action that can't work.
fn open_with_supported() -> bool {
    !cfg!(target_os = "macos")
}

/// Show the platform's native "Open with…" application chooser for
/// `path`, so the user can pick a program other than the default.
/// Detached — the chosen app owns its own lifetime.
fn open_with_dialog(path: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt as _;
        // The Shell's classic "How do you want to open this file?" dialog.
        // CREATE_NO_WINDOW keeps rundll32 from flashing a console.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        std::process::Command::new("rundll32.exe")
            .arg("shell32.dll,OpenAs_RunDLL")
            .arg(path)
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map(drop)
    }
    #[cfg(target_os = "linux")]
    {
        // `mimeopen -d` (perl-file-mimeinfo) prompts for the application;
        // plain `xdg-open` would silently use the default instead.
        std::process::Command::new("mimeopen")
            .arg("-d")
            .arg(path)
            .spawn()
            .map(drop)
    }
    #[cfg(target_os = "macos")]
    {
        let _ = path;
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Open with… is not yet available on macOS",
        ))
    }
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
    /// Persistent drag/hover state for each list's Mac-style scrollbar.
    browse_scrollbar: ui::scrollbar::ScrollbarState,
    results_scrollbar: ui::scrollbar::ScrollbarState,
    magic_scrollbar: ui::scrollbar::ScrollbarState,
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

mod file_ops;
mod input;
mod navigation;
mod render;
mod render_lists;
mod render_menus;
mod roots;
mod search;
mod services;
mod sidebar;
mod tabs;
mod tags;

impl Workspace {
    /// Resolve the color theme from the `theme` setting + the current OS
    /// appearance and install it as the global every component reads
    /// through `cx.theme()`. Called at startup, whenever settings
    /// change, and when the OS appearance flips.
    pub(super) fn apply_theme(&self, cx: &mut Context<Self>) {
        let mode = self.settings.read(cx).settings().theme;
        cx.set_global(Theme::resolve(mode, self.appearance));
        cx.notify();
    }

    pub(super) fn new(cx: &mut Context<Self>) -> Self {
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
            browse_scrollbar: ui::scrollbar::ScrollbarState::new(),
            results_scrollbar: ui::scrollbar::ScrollbarState::new(),
            magic_scrollbar: ui::scrollbar::ScrollbarState::new(),
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
        #[cfg(feature = "observability")]
        this.spawn_resource_sampling(cx);
        // macOS/Linux check the manifest themselves (no service); Windows
        // learns of updates from filex-indexd instead.
        #[cfg(all(not(target_os = "windows"), feature = "updater"))]
        this.spawn_update_check(cx);
        this
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
}

pub fn run() {
    let _logging_guard = filex::logging::init("filex");
    filex::telemetry::install_panic_hook("filex");
    // Sentry (UI process only; on-by-default, opt-out). The `crash_reports`
    // setting is consent — default true, cleared from the Settings pane —
    // and gates the whole integration, so read it straight from disk before
    // the app exists; the returned guard flushes pending events (and closes
    // the release-health session) on process exit and must live for the
    // whole run. Builds without the `observability` feature never link the
    // SDK (the elevated `filex-indexd` service is built that way).
    #[cfg(feature = "observability")]
    let _sentry_guard = {
        let consent = filex::settings::default_settings_file()
            .and_then(|file| {
                let legacy = filex::index::manager::default_roots_file();
                filex::settings::Settings::load(&file, legacy.as_deref()).ok()
            })
            .is_some_and(|settings| settings.crash_reports);
        filex::observability::init("filex", env!("CARGO_PKG_VERSION"), consent)
    };
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

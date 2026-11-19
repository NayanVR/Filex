//! The Filex application shell: the [`Workspace`] root view, its state
//! types, and the `run` entry point. The `impl Workspace` surface is split
//! across the sibling `workspace::*` modules by concern; each is a plain
//! `impl Workspace` block reaching shared imports via `use super::*`.

use std::ops::Range;
use std::path::{Path, PathBuf};

use gpui::{
    AnimationExt as _, App, Bounds, ClickEvent, Context, ExternalPaths, FocusHandle,
    Focusable as _, KeyBinding, KeyDownEvent, MouseButton, MouseDownEvent, Pixels, Point,
    ScrollStrategy, ScrollWheelEvent, SharedString, UniformListScrollHandle, Window,
    WindowAppearance, WindowBounds, WindowOptions, actions, div, prelude::*, px, size,
    uniform_list,
};

use filex::drives::{Drive, DriveKind};
use filex::listing::{
    Entry, Listing, backfill, display_path, format_modified, format_size, format_timestamp,
    read_dir_names, read_dir_sorted, sort_needs_metadata,
};
use filex::ops::{self, FileOp};
use filex::recents::Recents;
use filex::search::filter::Filter;
use filex::selection::Selection;
use filex::settings::{AccentColor, Density, SortBy, ThemeMode, ViewMode};
use filex::tags::{PlatformTags, Tag, TagColor, TagStore as _};

use crate::settings_store::{SettingsEvent, SettingsStore};
use crate::thumbnails::{self, ThumbnailState};
use crate::ui;
use crate::ui::search_input::{self, SearchInput, SearchInputEvent};
use crate::ui::theme::{ActiveTheme as _, Theme};
pub use app::run;
use filex::context_menu as menu_model;
use filex::listing::FileKind;
pub use picker::run_picker;
pub(crate) use platform::open_with_default_app;
use platform::{
    AppChoice, OPEN_WITH_OTHER, open_terminal, open_with_app, open_with_choices, open_with_dialog,
};
pub use startup::{hand_to_system_explorer, run_shell_command};
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
        DeletePermanently,
        Undo,
        FocusSearch,
        FocusPath,
        SubmitPath,
        CancelPath,
        SelectPrevious,
        SelectNext,
        ExtendPrevious,
        ExtendNext,
        PageUp,
        PageDown,
        OpenSelected,
        ToggleShortcuts,
        ToggleView,
        ZoomIn,
        ZoomOut,
        NewFolderHere,
        ToggleMagic,
        ApplyPlan,
        CopyAsPath,
        ShowProperties,
        MenuPrev,
        MenuNext,
        MenuLeft,
        MenuRight,
        MenuConfirm,
        MenuDismiss
    ]
);

/// The payload of an in-app file drag. Both the value a drop target
/// matches by type in `on_drop` and, via [`Render`], the pill that follows
/// the cursor. `position` is the cursor offset gpui hands the drag
/// constructor; the pill offsets by it to sit under the mouse.
#[derive(Clone)]
struct DragItems {
    paths: Vec<PathBuf>,
    /// Whether each of `paths` is a directory (known from the listing, so
    /// building the drag never stats the disk).
    dirs: Vec<bool>,
    label: SharedString,
    /// Whether the lead item is a folder, and its name, for the preview's
    /// icon.
    is_dir: bool,
    name: String,
    theme: Theme,
    position: Point<Pixels>,
}

impl DragItems {
    fn at(mut self, position: Point<Pixels>) -> Self {
        self.position = position;
        self
    }

    /// The payload for a drag that leaves the window, handed to the OS
    /// as a real file drag where gpui-ce supports one (macOS today).
    fn external_payload(&self) -> Option<gpui::ExternalDragPayload> {
        Some(gpui::ExternalDragPayload::Files(gpui::FileDragPaths::new(
            self.paths.iter().cloned().zip(self.dirs.iter().copied()),
        )))
    }
}

/// Size of the drag preview card; the stacked cards behind it for a
/// multi-item drag share it.
const DRAG_CARD: (f32, f32) = (220., 44.);

impl gpui::Render for DragItems {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let count = self.paths.len();
        let (w, h) = DRAG_CARD;
        let card = || {
            div()
                .w(px(w))
                .h(px(h))
                .rounded_lg()
                .border_1()
                .border_color(theme.border)
                .bg(theme.elevated)
        };
        // Up to two offset cards behind the lead one: a stack, so a
        // multi-item drag reads as "these" before the badge is read.
        let stack = (1..count.min(3)).rev().map(move |depth| {
            let offset = px(5. * depth as f32);
            card().absolute().top(offset).left(offset).shadow_sm()
        });
        let lead = card()
            .relative()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .shadow_lg()
            .text_sm()
            .text_color(theme.text)
            .child(if self.is_dir {
                ui::icon::folder_icon(&theme, filex::settings::FolderIcon::Plain, 22.)
            } else {
                ui::icon::file_icon(&theme, &self.name, 22.)
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(self.label.clone()),
            )
            .when(count > 1, |lead| {
                lead.child(
                    div()
                        .flex_none()
                        .min_w(px(20.))
                        .h(px(20.))
                        .px_1p5()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_full()
                        .bg(theme.accent)
                        .text_color(theme.on_accent)
                        .text_xs()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child(count.to_string()),
                )
            });
        div()
            .pl(self.position.x + px(14.))
            .pt(self.position.y + px(10.))
            .child(
                div()
                    .relative()
                    .children(stack)
                    .child(lead)
                    // Lifts off the row as the drag starts.
                    .with_animation(
                        "drag-preview-in",
                        gpui::Animation::new(ui::motion::FADE).with_easing(gpui::ease_out_quint()),
                        |card, t| card.opacity(0.4 + 0.55 * t).mt(px(6. * (1. - t))),
                    ),
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
    /// How many entries a folder holds; `None` for files or unreadable.
    items: Option<usize>,
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
    // ponytail: counts every entry of a huge folder; cap the walk if a
    // 100k-entry folder makes the details pane visibly late.
    let items = path
        .is_dir()
        .then(|| std::fs::read_dir(path).ok().map(Iterator::count))
        .flatten();
    PreviewMeta {
        path: path.to_path_buf(),
        size,
        modified,
        created,
        dimensions,
        items,
    }
}

const SEARCH_RESULT_LIMIT: usize = 100;
/// How close to the end of the loaded results the visible rows get before
/// the next page is fetched — about a screen's worth.
const RESULTS_PREFETCH_ROWS: usize = 30;

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

/// One Magic-plan row for review: the file's name, where it ends up, and
/// the full `source → destination` string for the hover tooltip. The
/// folder it comes from is the group header, not a cell. `dest` is `None`
/// where there is no per-file target — inventing "→ Trash" would imply a
/// path the op does not have.
struct PlanRow {
    name: SharedString,
    dest: Option<SharedString>,
    tooltip: SharedString,
}

fn leaf(path: &Path) -> String {
    path.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

/// Row `ix` of `plan`: one of its ops, or (compress, tag) one of the
/// files it acts on.
fn describe_row(plan: &filex::magic::Plan, ix: usize) -> Option<PlanRow> {
    if let Some(op) = plan.ops.get(ix) {
        return Some(describe_op(op));
    }
    let path = plan.source(ix)?;
    Some(PlanRow {
        name: leaf(path).into(),
        dest: None,
        tooltip: display_path(path).into(),
    })
}

/// A folder for a narrow column: a long path keeps its last two names
/// (`…/scratch/Docs`), since truncation would otherwise eat the end —
/// the part that says where. The row's tooltip has it whole.
fn short_path(folder: &Path) -> String {
    let full = display_path(folder);
    let mut tail: Vec<_> = folder.components().rev().take(2).collect();
    if full.chars().count() <= 36 || tail.len() < 2 {
        return full;
    }
    tail.reverse();
    let tail: PathBuf = tail.into_iter().collect();
    format!("…{}{}", std::path::MAIN_SEPARATOR, tail.display())
}

/// Why there is no plan, as the pane's message: the reason, then what
/// to do about it.
fn plan_error_advice(err: &filex::magic::PlanError) -> (String, &'static str) {
    use filex::magic::PlanError;
    match err {
        PlanError::NoMatches => (
            "Nothing matched".into(),
            "Try a broader name, or widen the scope beside the search box.",
        ),
        PlanError::NothingSelected => (
            "Nothing is selected".into(),
            "Select files in the list first, then ask again.",
        ),
        PlanError::TooMany(_) => (
            format!("More than {} files matched", filex::magic::MAX_PLAN_OPS),
            "Narrow it down — add a folder (“from Downloads”) or an age — so the plan can be reviewed.",
        ),
        PlanError::NothingToDo => (
            "Nothing to do".into(),
            "Every match is already where it should be.",
        ),
        PlanError::UnknownDestination(name) => (
            format!("No folder called “{name}”"),
            "Check the spelling, or create it here.",
        ),
        other => {
            let mut text = other.to_string();
            if let Some(first) = text.get_mut(..1) {
                first.make_ascii_uppercase();
            }
            (text, "")
        }
    }
}

/// `path` with the home folder written `~`, as paths are everywhere a
/// Unix user reads them; unchanged on Windows, where nobody writes that.
fn tilde(path: String, home: Option<&Path>) -> String {
    let Some(home) = home.filter(|_| !cfg!(windows)).map(display_path) else {
        return path;
    };
    match path.strip_prefix(&home) {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("~{rest}"),
        _ => path,
    }
}

fn describe_op(op: &FileOp) -> PlanRow {
    match op {
        FileOp::Delete { path } => PlanRow {
            name: leaf(path).into(),
            dest: None,
            tooltip: format!("{} → Trash", display_path(path)).into(),
        },
        FileOp::Move { from, to } | FileOp::Copy { from, to } => {
            // The destination folder — relative (`Images/`) when it is
            // beside the file, as organize's buckets are. When the plan
            // gave the file a new name to dodge a collision
            // (`ROADMAP.md` → `ROADMAP 2.md`) that name is appended so
            // the rename is visible. No "→ " prefix: the arrow is a
            // fixed-width gutter column in `op_row`.
            let folder = to.parent().unwrap_or(to.as_path());
            let mut dest = if folder.parent().is_some() && folder.parent() == from.parent() {
                format!("{}{}", leaf(folder), std::path::MAIN_SEPARATOR)
            } else {
                short_path(folder)
            };
            if to.file_name() != from.file_name() {
                if !dest.ends_with(std::path::MAIN_SEPARATOR) {
                    dest.push(std::path::MAIN_SEPARATOR);
                }
                dest.push_str(&leaf(to));
            }
            PlanRow {
                name: leaf(from).into(),
                dest: Some(dest.into()),
                tooltip: format!("{} → {}", display_path(from), display_path(to)).into(),
            }
        }
        FileOp::Rename { path, new_name } => PlanRow {
            name: leaf(path).into(),
            // Bare, for the same reason as Move/Copy above.
            dest: Some(SharedString::from(new_name.clone())),
            tooltip: format!("{} → {new_name}", display_path(path)).into(),
        },
        // Plans list the folders they make as a note, not as rows;
        // described for completeness.
        FileOp::CreateFolder { path } => PlanRow {
            name: leaf(path).into(),
            dest: None,
            tooltip: format!("new folder {}", display_path(path)).into(),
        },
    }
}

/// What a plan does beyond its rows, as (icon, sentence) notes for the
/// review header: folders it makes, where an archive lands, the tag,
/// and what was left out.
fn plan_notes(
    command: &filex::magic::Command,
    plan: &filex::magic::Plan,
) -> Vec<(&'static str, String)> {
    use filex::magic::{Action, Verb};
    let mut notes = Vec::new();
    match plan.creates.as_slice() {
        [] => {}
        [only] => notes.push((
            "icons/plus.svg",
            format!("Creates the folder “{}”", leaf(only)),
        )),
        many => {
            let names: Vec<String> = many.iter().take(6).map(|path| leaf(path)).collect();
            notes.push((
                "icons/plus.svg",
                format!(
                    "Creates {} folders: {}{}",
                    many.len(),
                    names.join(", "),
                    if many.len() > names.len() {
                        ", …"
                    } else {
                        ""
                    }
                ),
            ));
        }
    }
    if let Some(archive) = &plan.archive {
        notes.push((
            "icons/archive.svg",
            format!("Into {}", display_path(archive)),
        ));
    }
    match &command.action {
        Action::Tag(tag) => notes.push(("icons/dot.svg", format!("Adds the tag “{}”", tag.name))),
        Action::Untag(Some(name)) => {
            notes.push(("icons/dot.svg", format!("Removes the tag “{name}”")));
        }
        Action::Untag(None) => notes.push(("icons/dot.svg", "Removes every tag".to_owned())),
        _ => {}
    }
    if plan.skipped > 0 {
        let them = if plan.skipped == 1 { "it" } else { "them" };
        notes.push((
            "icons/info.svg",
            match command.verb {
                Verb::Rename => format!(
                    "{} already named that — nothing to do for {them}",
                    plan.skipped
                ),
                Verb::Compress => {
                    format!("{} left out — a ZIP can hold each name once", plan.skipped)
                }
                Verb::Organize => format!("{} folders and hidden files stay put", plan.skipped),
                _ => match &command.action {
                    Action::Move(dest) | Action::Copy(dest) => {
                        format!(
                            "{} already in {} · left where they are",
                            plan.skipped, dest.name
                        )
                    }
                    _ => format!("{} already there — skipped", plan.skipped),
                },
            },
        ));
    }
    notes
}

/// `query` with its destination changed to `new folder <name>` — what the
/// "Create it here" button on an unknown destination types for the user.
/// `None` when the query has no `to`/`into` to rewrite after.
fn with_new_folder(query: &str, name: &str) -> Option<String> {
    let lower = query.to_ascii_lowercase();
    let (at, separator) = [" to ", " into "]
        .iter()
        .filter_map(|sep| lower.rfind(sep).map(|at| (at, *sep)))
        .max()?;
    Some(format!(
        "{}new folder {name}",
        &query[..at + separator.len()]
    ))
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
    /// `entries` are a names-first listing whose size, mtime and hidden
    /// flags are still being backfilled (FIL-25); rows leave those blank.
    meta_pending: bool,
    /// Set to stop the in-flight backfill once its folder is left.
    backfill_cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    load_error: Option<SharedString>,
    roots: Vec<IndexedRoot>,
    settings: gpui::Entity<SettingsStore>,
    _settings_subscription: gpui::Subscription,
    /// Last-known OS window appearance, tracked so the `system` theme
    /// mode can be re-resolved when settings change without a window in
    /// hand. Updated at startup and by the appearance observer.
    appearance: WindowAppearance,
    /// The details panel's slide in/out, and its openness this frame
    /// (0..=1) — read by the panel and by the grid's column math so both
    /// move together.
    preview_motion: ui::motion::Toggle,
    preview_frac: f32,
    /// The settings page's eased wheel scrolling.
    settings_scroll: ui::motion::SmoothScroll,
    /// The files of the filex drag in progress (refreshed on each drag
    /// move), so losing focus mid-drag can still hand them to the OS.
    drag_in_flight: Option<Vec<PathBuf>>,
    /// The drive tile last clicked in This PC (a click selects, a double
    /// click opens).
    this_pc_selected: Option<usize>,
    /// Transient user-facing message (e.g. why a root couldn't be added).
    notice: Option<SharedString>,
    /// The auto-updater's UI state, a slim banner above the status bar.
    /// Advanced by a background manifest check on launch (macOS/Linux) or
    /// by the `filex-updater` service over its pipe (Windows).
    update_status: filex::update::UpdateStatus,
    /// Set once an install has started and the app is shutting its daemon
    /// down to quit: the service probe must not restart the daemon.
    exiting_for_update: bool,
    #[cfg(target_os = "macos")]
    fda_missing: bool,
    /// Connection to the per-user filex-indexd daemon.
    /// Searches go over IPC and no local indexing happens.
    service: Option<std::sync::Arc<filex::daemon::ipc::Client>>,
    daemon_status: filex::daemon::ipc::Status,
    search_hits: std::collections::HashMap<PathBuf, filex::daemon::ipc::Hit>,
    /// Size and modified time of search hits, read off-thread after each
    /// page lands; a row shows blanks until its entry arrives.
    search_meta: std::collections::HashMap<PathBuf, (u64, Option<std::time::SystemTime>)>,
    search_more: bool,
    /// The query whose results came from its plain-filename reading
    /// because names matched it, so its phrases show as suggestions.
    phrases_literal: Option<String>,
    /// The query the user told to use its phrases as filters anyway.
    phrases_forced: Option<String>,
    /// What `results` currently answers; see [`ResultsKey`].
    results_key: Option<ResultsKey>,
    search_page_query: Option<filex::daemon::ipc::Query>,
    search_paging: bool,
    search_client_id: u64,
    /// Mirror of the search input's content (the input entity owns it).
    query: String,
    /// Set in picker mode (`filex --pick`): another app's Open/Save request.
    picker: Option<picker::PickerState>,
    /// What OS integration is registered; `None` until first read.
    integration_status: Option<filex::shell_integration::Status>,
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
    /// The "add a folder name" field of the index exclude list (FIL-28).
    excluded_folder_input: gpui::Entity<SearchInput>,
    /// The search field on the Keyboard settings page; filters the list as
    /// you type.
    shortcut_search: gpui::Entity<SearchInput>,
    _shortcut_search_subscription: gpui::Subscription,
    results: Vec<SearchRow>,
    /// Request-to-results time of the first search for the query it names
    /// (debounce excluded), shown by onboarding's live search demo. Live
    /// index refreshes re-run the same query and must not overwrite it, or
    /// the number would change while the user isn't typing.
    search_latency: Option<(String, std::time::Duration)>,
    search_generation: u64,
    /// The Magic card's state when the query parses as a command.
    magic: Option<MagicState>,
    /// How the search bar decides between normal search and magic mode;
    /// see [`MagicMode`].
    magic_mode: MagicMode,
    /// The current Magic query read as something other than a file plan.
    magic_instant: Option<MagicInstant>,
    /// The query in the grammar's own words, when Magic read a paraphrase
    /// or fixed a typo — shown so that is never done invisibly.
    magic_read_as: Option<String>,
    /// A typo-corrected query to offer when this one found nothing.
    magic_did_you_mean: Option<String>,
    /// What an unfinished Magic query could become; the idle pane's list.
    magic_suggestions: Vec<filex::magic::Suggestion>,
    /// The highlighted suggestion (up/down, Enter).
    magic_cursor: usize,
    /// What plans do about a taken name in the destination.
    magic_conflicts: filex::magic::ConflictPolicy,
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
    /// The first-run welcome flow; while `Some`, it renders instead of the
    /// whole app.
    onboarding: Option<onboarding::Onboarding>,
    /// The onboarding-to-app transition, while it plays.
    reveal: Option<onboarding::Reveal>,
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
    /// The open delete confirmation (Recycle Bin / Trash or permanent).
    delete_prompt: Option<delete_prompt::DeletePrompt>,
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
    /// Bumped per opened menu; hover timers carry it so one from a closed
    /// menu can never act on the next.
    menu_serial: u64,
    /// After a menu closes: its focus handle and the one to return focus
    /// to. Applied at the next render (closing has no `Window` in hand),
    /// and only if the menu still held focus — an action that moved focus
    /// itself (rename) wins.
    menu_focus_restore: Option<(FocusHandle, FocusHandle)>,
    /// A folder just made by New › Folder: start renaming it once the
    /// reload lists it.
    pending_rename: Option<String>,
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
    /// Open feedback dialog (sidebar footer button), if any.
    feedback: Option<FeedbackDraft>,
    /// The "Connect to Server" dialog (FIL-59), while open.
    connect: Option<connect::ConnectDialog>,
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
    /// Watches the active tab's folder for changes made by other apps;
    /// `None` when the platform watcher couldn't start.
    folder_watch: Option<filex::listing::watch::FolderWatch>,
    /// The folder `folder_watch` was last pointed at.
    watched_dir: Option<PathBuf>,
    /// The watched folder changed while a search, settings or an edit was
    /// in the way; reload once it's gone.
    folder_stale: bool,
    /// When the latest folder load began. A load that began after a
    /// change was seen already shows it, so the watcher skips its own.
    last_load_start: std::time::Instant,
}

mod app;
mod connect;
mod delete_prompt;
mod file_ops;
mod folder_watch;
mod input;
mod integration;
mod location;
mod magic;
mod menus;
mod navigation;
mod onboarding;
#[cfg(target_os = "linux")]
mod open_with_linux;
#[cfg(target_os = "windows")]
mod open_with_windows;
mod picker;
mod platform;
mod preferences;
mod quick_look;
mod render;
mod render_lists;
mod render_menus;
mod roots;
mod search;
mod services;
#[cfg(target_os = "windows")]
mod shell_windows;
mod shortcuts;
mod sidebar;
mod startup;
mod state;
mod tabs;
mod tags;
mod this_pc;

impl Workspace {
    /// Resolve the color theme from the `theme` setting + the OS
    /// appearance and install it as the global `cx.theme()` reads. Called
    /// at startup, on settings change, and when the appearance flips.
    pub(super) fn apply_theme(&self, cx: &mut Context<Self>) {
        let settings = self.settings.read(cx).settings();
        let (mode, accent, density) = (settings.theme, settings.accent, settings.density);
        let theme = Theme::resolve(mode, self.appearance, accent)
            .with_density(density)
            .with_transparency(settings.transparency_effects);
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
                    // `>` asks for a command, as in an editor's palette —
                    // no filename starts with it.
                    if text.starts_with('>') && this.magic_mode != MagicMode::On {
                        this.magic_mode = MagicMode::On;
                        this.sync_magic_placeholder(cx);
                    }
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
            input.set_text(display_path(&cwd), cx);
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
        let excluded_folder_input = cx.new(SearchInput::new);
        excluded_folder_input.update(cx, |input, cx| {
            input.set_propagate_empty(false);
            input.set_placeholder("Folder name, e.g. build", cx);
        });
        let shortcut_search = cx.new(SearchInput::new);
        shortcut_search.update(cx, |input, cx| {
            input.set_propagate_empty(false);
            input.set_placeholder("Search shortcuts", cx);
        });
        let shortcut_search_subscription = cx.subscribe(&shortcut_search, |_, _, event, cx| {
            if let SearchInputEvent::Changed(_) = event {
                cx.notify();
            }
        });
        let settings = cx.new(SettingsStore::new);
        // Settings changes re-derive everything visible that depends on
        // them (the hidden-file filter on the browse list, and the
        // active color theme).
        let settings_subscription =
            cx.subscribe(&settings, |this, _store, event, cx| match event {
                SettingsEvent::Changed(previous) => {
                    let current = this.settings.read(cx).settings();
                    let filter_changed = previous.index_system_files != current.index_system_files
                        || previous.exclude_dev_folders != current.exclude_dev_folders
                        || (current.exclude_dev_folders
                            && previous.excluded_folder_names != current.excluded_folder_names);
                    if filter_changed && let Some(client) = this.service.clone() {
                        cx.background_executor()
                            .spawn(async move {
                                let _ = client.call(filex::daemon::ipc::Command::Reconcile);
                            })
                            .detach();
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
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            cwd: cwd.clone(),
            listed_dir: PathBuf::new(),
            entries: Vec::new(),
            meta_pending: false,
            backfill_cancel: Default::default(),
            load_error: None,
            roots: Vec::new(),
            settings,
            _settings_subscription: settings_subscription,
            // Corrected the moment a window exists (main's open-window
            // closure) and kept current by the appearance observer.
            appearance: WindowAppearance::Dark,
            preview_motion: Default::default(),
            preview_frac: 0.,
            settings_scroll: Default::default(),
            this_pc_selected: None,
            drag_in_flight: None,
            notice: None,
            #[cfg(target_os = "macos")]
            fda_missing: false,
            service: None,
            daemon_status: filex::daemon::ipc::Status::default(),
            search_hits: Default::default(),
            search_meta: Default::default(),
            search_more: false,
            phrases_literal: None,
            phrases_forced: None,
            results_key: None,
            search_page_query: None,
            search_paging: false,
            search_client_id: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos() as u64)
                ^ u64::from(std::process::id()),
            query: String::new(),
            picker: None,
            integration_status: None,
            search_input,
            path_input,
            _path_subscription: path_subscription,
            path_error: None,
            path_request: 0,
            path_loading: false,
            _search_input_subscription: subscription,
            accent_hex,
            _accent_hex_subscription: accent_hex_subscription,
            excluded_folder_input,
            shortcut_search,
            _shortcut_search_subscription: shortcut_search_subscription,
            results: Vec::new(),
            search_latency: None,
            search_generation: 0,
            magic: None,
            magic_mode: MagicMode::Auto,
            magic_instant: None,
            magic_read_as: None,
            magic_did_you_mean: None,
            magic_suggestions: Vec::new(),
            magic_cursor: 0,
            magic_conflicts: Default::default(),
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
            onboarding: None,
            reveal: None,
            renaming: None,
            journal: ops::Journal::default(),
            delete_prompt: None,
            clipboard: None,
            conflict: None,
            jobs: Vec::new(),
            next_job_id: 0,
            context_menu: None,
            menu_serial: 0,
            menu_focus_restore: None,
            pending_rename: None,
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
            feedback: None,
            connect: None,
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
            folder_watch: None,
            watched_dir: None,
            folder_stale: false,
            last_load_start: std::time::Instant::now(),
            update_status: filex::update::UpdateStatus::default(),
            exiting_for_update: false,
        };
        // A picker process (`filex --pick`) is a short errand for another
        // app: it browses and searches, but runs none of the app-lifetime
        // chores — no welcome tour, no network (update check, crash
        // upload), no housekeeping. It skips `load_dir` too: the picker
        // lands on the requested folder itself.
        let picker_process = cx.has_global::<picker::PickerProcess>();
        // Before the first load, so its folder gets watched. The picker
        // too: a file another app saves should appear in the dialog.
        this.spawn_folder_watch(cx);
        if !picker_process {
            this.load_dir(&cwd, cx);
        }
        if !picker_process && this.settings.read(cx).settings().onboarding_pending {
            this.start_onboarding(cx);
        }
        this.refresh_sidebar_tags(cx);
        this.spawn_service_probe(cx);
        this.spawn_fda_check(cx);
        this.spawn_drive_refresh(cx);
        if picker_process {
            return this;
        }
        this.spawn_tag_prune(cx);
        this.spawn_crash_upload(cx);
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
        let query = "delete screenshots older than 30 days";
        let command = filex::magic::parse(query, 1_785_067_200).expect("fixture should parse");
        MagicState {
            outcome: Some(outcome),
            loading: false,
            checked,
            ..MagicState::new(query, command)
        }
    }

    fn plan(ops: Vec<FileOp>) -> Plan {
        Plan {
            verb: Verb::Delete,
            skipped: 0,
            ops,
            creates: Vec::new(),
            conflicts: 0,
            targets: Vec::new(),
            archive: None,
        }
    }

    fn delete(path: &str) -> FileOp {
        FileOp::Delete {
            path: PathBuf::from(path),
        }
    }

    #[test]
    fn the_layout_knows_a_shared_destination_and_what_is_hidden() {
        let mv = |from: &str, to: &str| FileOp::Move {
            from: PathBuf::from(from),
            to: PathBuf::from(to),
        };
        let mut state = state(Err(filex::magic::PlanError::NoMatches), Vec::new());
        // Everything to one folder, one name changed to fit.
        state.install_plan(Ok(plan(vec![
            mv("/a/x.pdf", "/dest/x.pdf"),
            mv("/b/.cache/y.pdf", "/dest/y 2.pdf"),
        ])));
        assert_eq!(state.layout.dest.as_deref(), Some(Path::new("/dest")));
        assert!(state.layout.renames);
        assert_eq!(state.layout.hidden, [1]);
        // App data starts unticked: nobody pointed the search at it.
        assert_eq!(state.checked, [true, false]);
        state.set_hidden(true);
        assert_eq!(state.checked, [true, true]);
        // Two destinations (organize): no single one to put in a title.
        state.install_plan(Ok(plan(vec![
            mv("/a/x.pdf", "/a/Docs/x.pdf"),
            mv("/a/y.png", "/a/Images/y.png"),
        ])));
        assert_eq!(state.layout.dest, None);
        assert!(!state.layout.renames);
        assert!(state.layout.hidden.is_empty());
        // A delete goes nowhere.
        state.install_plan(Ok(plan(vec![delete("/a/.git/x")])));
        assert_eq!(
            (state.layout.dest.clone(), state.layout.hidden.len()),
            (None, 1)
        );
    }

    #[test]
    fn a_refusal_leads_with_its_reason() {
        use filex::magic::PlanError;
        let (title, advice) = plan_error_advice(&PlanError::UnknownDestination("Nowhere".into()));
        assert_eq!(title, "No folder called “Nowhere”");
        assert!(!advice.is_empty());
        // Reasons without their own wording still read as a sentence.
        let (title, _) = plan_error_advice(&PlanError::PatternNotUnique);
        assert!(title.starts_with("The new name"));
    }

    #[test]
    fn magic_status_counts_checkboxes_and_blocks_loading_or_failed_plans() {
        let mut state = state(
            Ok(plan(vec![delete("/a"), delete("/b")])),
            vec![false, true],
        );
        assert_eq!(state.status(), "1 checked · 1 left out");
        state.checked.fill(true);
        assert_eq!(state.status(), "2 checked");
        state.checked.fill(false);
        assert_eq!(state.status(), "0 checked · 2 left out");
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
    fn delete_rows_have_no_destination() {
        let row = describe_op(&delete("/photos/shot.png"));
        assert_eq!(row.name, SharedString::from("shot.png"));
        assert_eq!(row.dest, None);
    }

    #[test]
    fn rows_are_grouped_under_the_folder_they_come_from() {
        // Identical names in different folders are told apart by the
        // group header above them, which also folds the group away.
        let mut state = state(
            Ok(plan(vec![
                delete("/a/shot.png"),
                delete("/b/shot.png"),
                delete("/a/other.png"),
            ])),
            Vec::new(),
        );
        state.install_plan(Ok(plan(vec![
            delete("/a/shot.png"),
            delete("/b/shot.png"),
            delete("/a/other.png"),
        ])));
        use PlanLine::{Group, Op};
        assert_eq!(
            state.layout.lines,
            [Group(0), Op(0), Op(2), Group(1), Op(1)]
        );
        assert_eq!(state.layout.groups[0].0, PathBuf::from("/a"));

        // The header's checkbox takes its whole folder, and only it.
        state.checked = vec![true, true, false];
        assert_eq!(state.group_count(0), (1, 2));
        state.toggle_group(0);
        assert_eq!(state.checked, vec![true, true, true]);
        state.toggle_group(0);
        assert_eq!(state.checked, vec![false, true, false]);

        // Folding hides the rows without unchecking them.
        state.layout.toggle_collapsed(1);
        assert_eq!(state.layout.lines, [Group(0), Op(0), Op(2), Group(1)]);
        assert_eq!(state.selected_ops(), vec![delete("/b/shot.png")]);
    }

    #[test]
    fn a_plan_from_one_folder_has_no_group_headers() {
        let mut state = state(Ok(plan(Vec::new())), Vec::new());
        state.install_plan(Ok(plan(vec![delete("/a/x"), delete("/a/y")])));
        assert_eq!(state.layout.lines, [PlanLine::Op(0), PlanLine::Op(1)]);
    }

    #[test]
    fn a_plan_without_ops_reviews_and_selects_its_targets() {
        // Compress and tag plans list files, not operations.
        let mut state = state(Ok(plan(Vec::new())), Vec::new());
        let mut targets = plan(Vec::new());
        targets.targets = vec!["/a/x.pdf".into(), "/a/y.pdf".into()];
        state.install_plan(Ok(targets));
        assert_eq!(state.status(), "2 checked");
        state.checked[0] = false;
        assert_eq!(state.selected_targets(), vec![PathBuf::from("/a/y.pdf")]);
        assert!(state.selected_ops().is_empty());
    }

    #[test]
    fn a_bucket_beside_the_file_is_shown_relative() {
        // Organize: "Images/" says more than the full path repeated on
        // every row.
        let row = describe_op(&FileOp::Move {
            from: "/dl/shot.png".into(),
            to: "/dl/Images/shot 2.png".into(),
        });
        let sep = std::path::MAIN_SEPARATOR;
        assert_eq!(
            row.dest.unwrap().to_string(),
            format!("Images{sep}shot 2.png")
        );
    }

    #[test]
    fn an_unknown_destination_can_be_rewritten_to_create_it() {
        assert_eq!(
            with_new_folder("move pdfs to Old Stuff", "Old Stuff").as_deref(),
            Some("move pdfs to new folder Old Stuff")
        );
        // The last separator is the destination's, as in the grammar.
        assert_eq!(
            with_new_folder("copy notes to self INTO archive", "archive").as_deref(),
            Some("copy notes to self INTO new folder archive")
        );
        assert_eq!(with_new_folder("delete old logs", "x"), None);
    }

    #[test]
    fn notes_say_what_a_plan_does_beyond_its_rows() {
        let command = filex::magic::parse_with_gate("zip invoices", 0, false).unwrap();
        let mut packed = plan(Vec::new());
        packed.targets = vec!["/a/x.pdf".into()];
        packed.archive = Some("/a/x.zip".into());
        packed.skipped = 2;
        let notes: Vec<String> = plan_notes(&command, &packed)
            .into_iter()
            .map(|(_, note)| note)
            .collect();
        assert!(notes[0].ends_with("x.zip"), "{notes:?}");
        assert!(notes[1].starts_with("2 left out"), "{notes:?}");

        let command = filex::magic::parse_with_gate("tag invoices as Work", 0, false).unwrap();
        let mut made = plan(Vec::new());
        made.creates = vec!["/a/One".into(), "/a/Two".into()];
        let notes = plan_notes(&command, &made);
        assert_eq!(notes[0].1, "Creates 2 folders: One, Two");
        assert_eq!(notes[1].1, "Adds the tag “Work”");
    }

    #[test]
    fn transfer_rows_show_source_folder_and_destination_folder() {
        let op = FileOp::Move {
            from: "/a/shot.png".into(),
            to: "/b/Archive/shot.png".into(),
        };
        let row = describe_op(&op);
        assert_eq!(row.name, SharedString::from("shot.png"));
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
            format!(
                "{}{}shot 2.png",
                Path::new("/b").display(),
                std::path::MAIN_SEPARATOR
            )
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
    #[test]
    #[cfg(not(windows))]
    fn the_home_folder_is_written_as_a_tilde() {
        let home = Some(Path::new("/Users/me"));
        let tilde = |path: &str| super::tilde(path.to_owned(), home);
        assert_eq!(tilde("/Users/me/Downloads"), "~/Downloads");
        assert_eq!(tilde("/Users/me"), "~");
        // A sibling that merely starts the same is not inside it.
        assert_eq!(tilde("/Users/melissa/x"), "/Users/melissa/x");
        assert_eq!(super::tilde("/srv/x".to_owned(), None), "/srv/x");
    }

    #[test]
    fn a_long_destination_keeps_the_end_that_says_where() {
        assert_eq!(super::short_path(Path::new("/srv/Docs")), "/srv/Docs");
        let long = Path::new("/a-very-long-folder-name/another-long-one/scratch/Docs");
        let sep = std::path::MAIN_SEPARATOR;
        assert_eq!(super::short_path(long), format!("…{sep}scratch{sep}Docs"));
    }
}

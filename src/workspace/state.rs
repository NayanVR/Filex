//! View-state types owned by [`Workspace`](super::Workspace): indexed
//! roots, search rows, menus, jobs, Magic, rename/tag editors, and tab
//! snapshots.

use super::*;

pub(super) enum IndexState {
    Building,
    Ready,
    Failed(SharedString),
}
pub(super) struct IndexedRoot {
    pub(super) path: PathBuf,
    pub(super) label: SharedString,
    pub(super) state: IndexState,
}
impl IndexedRoot {
    pub(super) fn from_status(root: &filex::daemon::ipc::RootStatus) -> Self {
        let label = filex::listing::location_label(&root.path);
        let state = if root.state == "Building index" {
            IndexState::Building
        } else if root.state == "Ready" {
            IndexState::Ready
        } else {
            IndexState::Failed(root.state.clone().into())
        };
        Self {
            path: root.path.clone(),
            label: label.into(),
            state,
        }
    }
}

/// A search hit prepared for display (paths pre-materialized off-thread).
pub(super) struct SearchRow {
    pub(super) name: SharedString,
    pub(super) path_label: SharedString,
    pub(super) target: PathBuf,
    pub(super) is_dir: bool,
}

/// What a copy/cut put on the internal file clipboard. This is app
/// state, not the OS clipboard — pasting files copied in other apps is
/// out of scope for now.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ClipMode {
    Copy,
    Cut,
}

/// An operation blocked on an occupied destination, awaiting the
/// user's choice in the conflict dialog.
pub(super) struct ConflictState {
    pub(super) op: FileOp,
    pub(super) dest: PathBuf,
}

/// What an open context menu is about.
pub(super) enum MenuTarget {
    /// A file row — browse (`from_search: false`) or a search result.
    Entry {
        ix: usize,
        path: PathBuf,
        is_dir: bool,
        from_search: bool,
    },
    /// Empty space in the folder on screen (`Workspace::cwd`).
    Background,
    /// A pinned folder in the sidebar's Favorites section.
    Favorite { path: PathBuf },
    /// Icon choices for one folder, opened from its context menu.
    FolderIcon { path: PathBuf },
    /// A network drive in the sidebar or on This PC (FIL-60).
    NetworkDrive { path: PathBuf },
}

/// Which panel of an open menu a row belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MenuPanel {
    Main,
    Sub,
}

pub(super) struct ContextMenu {
    pub(super) position: Point<Pixels>,
    pub(super) target: MenuTarget,
    /// The main panel's rows (`filex::context_menu`).
    pub(super) rows: Vec<menu_model::Row>,
    /// Highlighted row of the main panel (pointer or keyboard).
    pub(super) focus: Option<menu_model::Focus>,
    /// Pointer-rest timing for opening/closing submenus on hover.
    pub(super) hover: menu_model::HoverIntent,
    pub(super) submenu: Option<OpenSubmenu>,
    /// The Open With app list, looked up off the UI thread as a file's
    /// menu opens, so hovering "Open With" shows it without waiting.
    pub(super) apps: AppsLookup,
    /// Main-panel row whose submenu should open once `apps` lands.
    pub(super) wants_apps: Option<(usize, bool)>,
    /// Keyboard focus while the menu is up (its arrow-key context).
    pub(super) focus_handle: FocusHandle,
    /// Where keyboard focus returns when the menu closes.
    pub(super) restore_focus: Option<FocusHandle>,
}

pub(super) enum AppsLookup {
    /// Not a single file: there is no Open With.
    Unused,
    Pending,
    /// `None` inside: the platform couldn't list apps for the type, so the
    /// submenu offers only the OS chooser.
    Ready(Option<Vec<AppChoice>>),
}

/// A submenu shown beside the main panel.
pub(super) struct OpenSubmenu {
    /// The main-panel row it belongs to.
    pub(super) parent_row: usize,
    pub(super) rows: Vec<menu_model::Row>,
    pub(super) position: Point<Pixels>,
    /// Capped to the window; the list scrolls past it.
    pub(super) max_height: Pixels,
    /// Highlighted row; `Some` also means the keyboard is in the submenu.
    pub(super) focus: Option<menu_model::Focus>,
}

/// One background file-operation job with live progress; shown in the
/// jobs bar while running.
pub(super) struct Job {
    pub(super) id: u64,
    pub(super) label: SharedString,
    pub(super) progress: std::sync::Arc<ops::OpProgress>,
    /// What the job reads from or writes to, so a share it's using isn't
    /// disconnected under it (FIL-60).
    pub(super) paths: Vec<PathBuf>,
}

/// Whether any of `jobs` reads from or writes to something under `mount`.
pub(super) fn jobs_touch(jobs: &[Job], mount: &Path) -> bool {
    jobs.iter()
        .flat_map(|job| &job.paths)
        .any(|path| path.starts_with(mount))
}

/// How the search bar chooses between normal search and magic mode. Three
/// states, not a bool, because auto-switch and the explicit toggle can
/// disagree: the toggle must force magic *off* on a query auto-switch
/// would light up, and *on* for one that hasn't parsed yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MagicMode {
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

/// What the rows on screen are the answer to. A search with the same key
/// is a refresh of those rows rather than a new search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ResultsKey {
    pub query: String,
    pub scope: Option<PathBuf>,
    pub phrases_forced: bool,
}

/// Where a query (normal or magic) looks: across every indexed root, or
/// only within the folder currently on screen. Chosen from the search
/// bar's scope dropdown; defaults to [`Anywhere`](Self::Anywhere).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SearchScope {
    /// Every indexed root — the index's whole reach.
    Anywhere,
    /// Only `cwd` and its subtree. Resolved per-scan against the index so
    /// it costs nothing when off, and a folder the index hasn't caught up
    /// to yet simply returns nothing.
    CurrentDir,
}

impl SearchScope {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Anywhere => "Anywhere",
            Self::CurrentDir => "This Folder",
        }
    }
}

/// A parsed Magic command and the plan it resolved to, for the review
/// card. Held separately from `results` though both come from one search:
/// `results` is what the user is looking at, `ops`/`checked` is what would
/// run — which is what lets a row be unchecked without disturbing the
/// result list.
#[derive(Default)]
pub(super) struct MagicProgress {
    pub(super) scanned: std::sync::atomic::AtomicU64,
    pub(super) matched: std::sync::atomic::AtomicU64,
}

pub(super) struct MagicState {
    pub(super) source_query: String,
    pub(super) command: filex::magic::Command,
    pub(super) loading: bool,
    pub(super) error: Option<String>,
    pub(super) progress: std::sync::Arc<MagicProgress>,
    /// The resolved plan, or why there isn't one. `None` until the search
    /// lands, so a still-indexing root doesn't claim the command matched
    /// nothing. An error still shows a card — "no folder called Archive"
    /// beats silence after the user typed a real command.
    pub(super) outcome: Option<Result<filex::magic::Plan, filex::magic::PlanError>>,
    /// One flag per row in the plan, parallel to its ops (or targets).
    /// Everything starts checked; the review step is about *removing*
    /// what you didn't mean, not opting in one file at a time.
    pub(super) checked: Vec<bool>,
    /// Built from the selection or a folder listing, not a search: there
    /// are no index hits to check the files against before applying.
    pub(super) local: bool,
    pub(super) layout: PlanLayout,
    /// Size and modified time of each searched file, read off-thread
    /// with the plan, so the review shows what an age or size rule
    /// matched on. Empty for plans built from the selection.
    pub(super) meta: std::collections::HashMap<PathBuf, (u64, Option<std::time::SystemTime>)>,
}

/// One line of the review list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PlanLine {
    /// A source-folder header, by index into [`PlanLayout::groups`].
    Group(usize),
    /// A plan row, by index into the plan.
    Op(usize),
}

/// The review list's shape: plan rows grouped under the folder they come
/// from, so a 700-file plan reads as a dozen folders that fold away
/// rather than one undifferentiated scroll. A plan from a single folder
/// has no headers at all.
#[derive(Default)]
pub(super) struct PlanLayout {
    /// Each source folder and the plan rows in it, in path order.
    pub(super) groups: Vec<(PathBuf, Vec<usize>)>,
    pub(super) collapsed: std::collections::HashSet<PathBuf>,
    /// What the virtualized list draws, top to bottom.
    pub(super) lines: Vec<PlanLine>,
    /// Rows whose file sits inside a hidden (dot) folder or an app
    /// bundle — caches and app data a broad search reaches without the
    /// user meaning it.
    pub(super) hidden: Vec<usize>,
    /// The one folder every row lands in, when there is one: it is said
    /// once, in the title, instead of on every row.
    pub(super) dest: Option<PathBuf>,
    /// Whether any row also changes the file's name (a dodged collision),
    /// which keeps the per-row destination worth showing.
    pub(super) renames: bool,
}

/// Whether `folder` is somewhere files live for a program's sake, not
/// the user's: under a dot folder, or inside a macOS app bundle.
pub(super) fn is_app_data(folder: &Path) -> bool {
    folder.components().any(|part| {
        matches!(part, std::path::Component::Normal(name) if {
            let name = name.to_string_lossy();
            name.starts_with('.') || name.ends_with(".app")
        })
    })
}

impl PlanLayout {
    fn rebuild(&mut self, plan: &filex::magic::Plan) {
        let mut groups: std::collections::BTreeMap<PathBuf, Vec<usize>> = Default::default();
        for ix in 0..plan.len() {
            let folder = plan
                .source(ix)
                .and_then(Path::parent)
                .unwrap_or(Path::new(""));
            groups.entry(folder.to_path_buf()).or_default().push(ix);
        }
        self.hidden = groups
            .iter()
            .filter(|(folder, _)| is_app_data(folder))
            .flat_map(|(_, rows)| rows.iter().copied())
            .collect();
        let moves: Vec<(&PathBuf, &PathBuf)> = plan
            .ops
            .iter()
            .filter_map(|op| match op {
                FileOp::Move { from, to } | FileOp::Copy { from, to } => Some((from, to)),
                _ => None,
            })
            .collect();
        self.dest = match moves.first() {
            Some((_, first))
                if moves.len() == plan.ops.len()
                    && moves.iter().all(|(_, to)| to.parent() == first.parent()) =>
            {
                first.parent().map(Path::to_path_buf)
            }
            _ => None,
        };
        self.renames = moves
            .iter()
            .any(|(from, to)| from.file_name() != to.file_name());
        self.groups = groups.into_iter().collect();
        // What needs a look comes first: rows that had to take a new
        // name lead their folder, and folders with such rows lead the
        // list. App data goes last. (Stable sorts, so everything else
        // stays in path order.)
        let renamed = |ix: &usize| {
            matches!(plan.ops.get(*ix), Some(
                FileOp::Move { from, to } | FileOp::Copy { from, to },
            ) if from.file_name() != to.file_name())
        };
        for (_, rows) in &mut self.groups {
            rows.sort_by_key(|ix| !renamed(ix));
        }
        self.groups
            .sort_by_key(|(folder, rows)| (is_app_data(folder), !rows.iter().any(renamed)));
        self.collapsed
            .retain(|folder| self.groups.iter().any(|(f, _)| f == folder));
        self.relayout();
    }

    fn relayout(&mut self) {
        self.lines.clear();
        let grouped = self.groups.len() > 1;
        for (gix, (folder, rows)) in self.groups.iter().enumerate() {
            if grouped {
                self.lines.push(PlanLine::Group(gix));
                if self.collapsed.contains(folder) {
                    continue;
                }
            }
            self.lines.extend(rows.iter().map(|&ix| PlanLine::Op(ix)));
        }
    }

    pub(super) fn toggle_collapsed(&mut self, gix: usize) {
        if let Some((folder, _)) = self.groups.get(gix) {
            if !self.collapsed.remove(folder) {
                self.collapsed.insert(folder.clone());
            }
            self.relayout();
        }
    }
}

impl MagicState {
    pub(super) fn new(query: &str, command: filex::magic::Command) -> Self {
        Self {
            source_query: query.to_owned(),
            command,
            outcome: None,
            checked: Vec::new(),
            loading: true,
            error: None,
            progress: Default::default(),
            local: false,
            layout: PlanLayout::default(),
            meta: Default::default(),
        }
    }

    pub(super) fn selection_count(&self) -> usize {
        match &self.outcome {
            Some(Ok(plan)) => self
                .checked
                .iter()
                .take(plan.len())
                .filter(|checked| **checked)
                .count(),
            _ => 0,
        }
    }

    /// How many of group `gix`'s rows are checked, and how many it has.
    pub(super) fn group_count(&self, gix: usize) -> (usize, usize) {
        let Some((_, rows)) = self.layout.groups.get(gix) else {
            return (0, 0);
        };
        let checked = rows
            .iter()
            .filter(|&&ix| self.checked.get(ix).copied().unwrap_or(false))
            .count();
        (checked, rows.len())
    }

    /// The group header's checkbox: check all of its rows, or — when they
    /// all already are — uncheck them.
    pub(super) fn toggle_group(&mut self, gix: usize) {
        let (checked, total) = self.group_count(gix);
        let Some((_, rows)) = self.layout.groups.get(gix) else {
            return;
        };
        for &ix in rows {
            if let Some(flag) = self.checked.get_mut(ix) {
                *flag = checked < total;
            }
        }
    }

    pub(super) fn status(&self) -> String {
        if let Some(error) = &self.error {
            return format!("Couldn’t prepare preview: {error}");
        }
        if self.loading {
            let scanned = self
                .progress
                .scanned
                .load(std::sync::atomic::Ordering::Relaxed);
            let matched = self
                .progress
                .matched
                .load(std::sync::atomic::Ordering::Relaxed);
            return if scanned == 0 {
                "Finding matching files…".into()
            } else {
                format!("{matched} matches found · {scanned} entries checked…")
            };
        }
        match &self.outcome {
            // What will happen and what won't, not a fraction to subtract.
            Some(Ok(plan)) => {
                let ticked = self.selection_count();
                match plan.len() - ticked {
                    0 => format!("{ticked} checked"),
                    left => format!("{ticked} checked · {left} left out"),
                }
            }
            Some(Err(error)) => super::plan_error_advice(error).0,
            None => "Preparing preview…".into(),
        }
    }

    /// Tick or untick the app-data rows together (the warning's action).
    pub(super) fn set_hidden(&mut self, checked: bool) {
        for &ix in &self.layout.hidden {
            if let Some(slot) = self.checked.get_mut(ix) {
                *slot = checked;
            }
        }
    }

    pub(super) fn install_plan(
        &mut self,
        outcome: Result<filex::magic::Plan, filex::magic::PlanError>,
    ) {
        // Keyed by source file. A row keeps its choice only if it would
        // still do the same thing (same op; a target row has only its
        // path, so the same path is the same row).
        let previous = match &self.outcome {
            Some(Ok(plan)) => (0..plan.len())
                .zip(&self.checked)
                .filter_map(|(ix, checked)| Some((plan.source(ix)?, (plan.ops.get(ix), *checked))))
                .collect::<std::collections::HashMap<_, _>>(),
            _ => Default::default(),
        };
        // A search nobody pointed at a folder starts with app data left
        // out: "delete old screenshots" must not arrive one keypress from
        // trashing an emulator's snapshots. Files the user selected, or
        // in a folder they named, are theirs to mean.
        let guarded = !self.local && self.command.selection.within.is_none();
        self.checked = match &outcome {
            Ok(plan) => (0..plan.len())
                .map(|ix| {
                    let source = plan.source(ix);
                    let wanted =
                        !(guarded && source.and_then(Path::parent).is_some_and(is_app_data));
                    source
                        .and_then(|source| previous.get(source))
                        .filter(|(old, _)| *old == plan.ops.get(ix))
                        .map_or(previous.is_empty() && wanted, |(_, checked)| *checked)
                })
                .collect(),
            Err(_) => Vec::new(),
        };
        match &outcome {
            Ok(plan) => self.layout.rebuild(plan),
            Err(_) => self.layout = PlanLayout::default(),
        }
        self.outcome = Some(outcome);
        self.loading = false;
        self.error = None;
    }

    fn ready_plan(&self) -> Option<&filex::magic::Plan> {
        if self.loading || self.error.is_some() {
            return None;
        }
        self.outcome.as_ref()?.as_ref().ok()
    }

    /// The ops the user has left checked.
    pub(super) fn selected_ops(&self) -> Vec<FileOp> {
        let Some(plan) = self.ready_plan() else {
            return Vec::new();
        };
        plan.ops
            .iter()
            .zip(&self.checked)
            .filter(|(_, checked)| **checked)
            .map(|(op, _)| op.clone())
            .collect()
    }

    /// The checked files of a compress or tag plan, which has no ops.
    pub(super) fn selected_targets(&self) -> Vec<PathBuf> {
        let Some(plan) = self.ready_plan() else {
            return Vec::new();
        };
        plan.targets
            .iter()
            .zip(&self.checked)
            .filter(|(_, checked)| **checked)
            .map(|(path, _)| path.clone())
            .collect()
    }
}

/// A Magic query that isn't a file plan: it runs on Enter (or answers
/// itself) instead of going through review.
pub(super) enum MagicInstant {
    /// `target` is `None` while the place is being looked up; then the
    /// path and whether it is a folder (to go into) or a file (to open).
    Go {
        go: filex::magic::Go,
        target: Option<Result<(PathBuf, bool), String>>,
    },
    App(filex::magic::AppCommand),
    /// `find invoices`: Enter hands the words to the ordinary search.
    Find(String),
    Ask {
        answer: Option<Result<MagicAnswer, String>>,
        progress: std::sync::Arc<MagicProgress>,
    },
}

/// A question's answer: the number, what it counts, and the files behind
/// it (largest first, sizes where they were measured).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct MagicAnswer {
    pub(super) headline: String,
    pub(super) detail: String,
    pub(super) rows: Vec<(PathBuf, Option<u64>)>,
}

/// A rename-in-place in progress: which browse row is being edited and
/// the input that owns the edited text (the SearchInput element reused
/// as a transient editor, per docs/roadmap.md).
pub(super) struct RenameState {
    pub(super) ix: usize,
    pub(super) input: gpui::Entity<SearchInput>,
    /// Watches for Dismissed (escape) to cancel.
    pub(super) _subscription: gpui::Subscription,
}

/// An in-progress tag edit in the details panel: target file, the input
/// owning the typed name, the chosen color, and (when editing rather than
/// adding) the original name so commit replaces it in place.
pub(super) struct TagEditor {
    pub(super) path: PathBuf,
    pub(super) input: gpui::Entity<SearchInput>,
    pub(super) color: Option<TagColor>,
    /// `Some(original_name)` when recoloring/renaming an existing tag;
    /// `None` when adding a new one.
    pub(super) existing: Option<String>,
    /// Watches for Dismissed (escape) to cancel.
    pub(super) _subscription: gpui::Subscription,
}

/// The open feedback dialog: the message and (optional) reply-email
/// inputs.
pub(super) struct FeedbackDraft {
    pub(super) message: gpui::Entity<SearchInput>,
    pub(super) email: gpui::Entity<SearchInput>,
    /// Watch both inputs for Dismissed (escape) to cancel.
    pub(super) _subscriptions: [gpui::Subscription; 2],
}

/// One browse tab's saved state. The *active* tab lives directly on the
/// [`Workspace`]; this holds the inactive ones, refreshed from the live
/// fields on each switch. Search is global, not per-tab. Transient UI (an
/// in-progress rename, an armed delete) is dropped on switch.
pub(super) struct TabSnapshot {
    pub(super) cwd: PathBuf,
    pub(super) listed_dir: PathBuf,
    pub(super) entries: Vec<Entry>,
    pub(super) meta_pending: bool,
    pub(super) load_error: Option<SharedString>,
    pub(super) selection: Selection,
    pub(super) scroll: UniformListScrollHandle,
    pub(super) history_back: Vec<PathBuf>,
    pub(super) history_forward: Vec<PathBuf>,
}

impl TabSnapshot {
    /// A blank slot; the active tab's slot always holds one of these
    /// until the next switch refreshes it from the live fields.
    pub(super) fn placeholder() -> Self {
        Self {
            cwd: PathBuf::new(),
            listed_dir: PathBuf::new(),
            entries: Vec::new(),
            meta_pending: false,
            load_error: None,
            selection: Selection::default(),
            scroll: UniformListScrollHandle::new(),
            history_back: Vec::new(),
            history_forward: Vec::new(),
        }
    }
}

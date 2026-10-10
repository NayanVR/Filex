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
        let label = root
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| root.path.display().to_string());
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
        name: String,
        is_dir: bool,
        from_search: bool,
    },
    /// An indexed root in the sidebar.
    /// A pinned folder in the sidebar's Favorites section.
    Favorite { path: PathBuf },
    /// Icon choices for one folder, opened from its context menu.
    FolderIcon { path: PathBuf },
}

pub(super) struct ContextMenu {
    pub(super) position: Point<Pixels>,
    pub(super) target: MenuTarget,
}

/// One background file-operation job with live progress; shown in the
/// jobs bar while running.
pub(super) struct Job {
    pub(super) id: u64,
    pub(super) label: SharedString,
    pub(super) progress: std::sync::Arc<ops::OpProgress>,
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
            Self::CurrentDir => "Current Dir",
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
    /// One flag per op in the plan, parallel to `Plan::ops`. Everything
    /// starts checked; the review step is about *removing* what you
    /// didn't mean, not opting in one file at a time.
    pub(super) checked: Vec<bool>,
}

impl MagicState {
    pub(super) fn selection_count(&self) -> usize {
        match &self.outcome {
            Some(Ok(plan)) => plan
                .ops
                .iter()
                .zip(&self.checked)
                .filter(|(_, checked)| **checked)
                .count(),
            _ => 0,
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
            Some(Ok(plan)) => format!("{} of {} selected", self.selection_count(), plan.ops.len()),
            Some(Err(error)) => error.to_string(),
            None => "Preparing preview…".into(),
        }
    }

    pub(super) fn install_plan(
        &mut self,
        outcome: Result<filex::magic::Plan, filex::magic::PlanError>,
    ) {
        let previous = match &self.outcome {
            Some(Ok(plan)) => plan
                .ops
                .iter()
                .zip(&self.checked)
                .map(|(op, checked)| (op.source(), (op, *checked)))
                .collect::<std::collections::HashMap<_, _>>(),
            _ => Default::default(),
        };
        self.checked = match &outcome {
            Ok(plan) => plan
                .ops
                .iter()
                .map(|op| {
                    previous
                        .get(op.source())
                        .filter(|(old, _)| *old == op)
                        .map_or(previous.is_empty(), |(_, checked)| *checked)
                })
                .collect(),
            Err(_) => Vec::new(),
        };
        self.outcome = Some(outcome);
        self.loading = false;
        self.error = None;
    }

    /// The ops the user has left checked.
    pub(super) fn selected_ops(&self) -> Vec<FileOp> {
        if self.loading || self.error.is_some() {
            return Vec::new();
        }
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

/// One browse tab's saved state. The *active* tab lives directly on the
/// [`Workspace`]; this holds the inactive ones, refreshed from the live
/// fields on each switch. Search is global, not per-tab. Transient UI (an
/// in-progress rename, an armed delete) is dropped on switch.
pub(super) struct TabSnapshot {
    pub(super) cwd: PathBuf,
    pub(super) listed_dir: PathBuf,
    pub(super) entries: Vec<Entry>,
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
            load_error: None,
            selection: Selection::default(),
            scroll: UniformListScrollHandle::new(),
            history_back: Vec::new(),
            history_forward: Vec::new(),
        }
    }
}

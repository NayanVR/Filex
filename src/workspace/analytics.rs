//! UI-side analytics: the bits that need workspace state. Events are the
//! typed structs in `filex::diagnostics::analytics::catalog` (imported as
//! `ev`), tracked with `fa::track`. Simple ones are tracked inline at their
//! call site. This module holds the launch event, shortcut tallies, the
//! search-session accumulator, and helpers shared by several sites.
//!
//! Everything here is a channel send or a no-op when analytics is off.
//! Nothing touches I/O on the UI thread.

use super::*;
use filex::launch::Browse;

/// How the process was asked to start.
pub(super) fn launch_kind(browse: &Browse) -> ev::LaunchKind {
    match browse {
        Browse::Default => ev::LaunchKind::Default,
        Browse::Open(_) => ev::LaunchKind::OpenFolder,
        Browse::Select(_) => ev::LaunchKind::SelectItem,
        Browse::ThisPc => ev::LaunchKind::ThisPc,
        Browse::Foreign(_) => ev::LaunchKind::Foreign,
    }
}

/// `app_launched`, plus `settings_snapshot` when settings loaded.
pub(super) fn track_launch(
    launch: ev::LaunchKind,
    started: fa::Started,
    settings: Option<&filex::settings::Settings>,
    startup: std::time::Duration,
) {
    fa::track(ev::AppLaunched {
        launch,
        new_install: started.new_install,
        startup_ms: startup,
        onboarding_pending: settings.is_some_and(|s| s.onboarding_pending),
    });
    if let Some(settings) = settings {
        fa::track(fa::snapshot::snapshot(settings));
    }
}

/// Shortcut usage, aggregated: each keystroke that resolved to an app
/// action bumps a counter named after the action (a compile-time name like
/// `filex::GoUp`). Text-editing keys in the search box are typing, not
/// shortcuts, and are skipped. Counters ship in `usage_summary`.
pub(super) fn observe_shortcuts(cx: &mut Context<Workspace>) {
    cx.observe_keystrokes(|_, event, _, _| {
        if !fa::is_enabled() {
            return;
        }
        if let Some(action) = &event.action {
            let name = action.name();
            if is_shortcut(name) {
                fa::count(name);
            }
        }
    })
    .detach();
}

/// Whether an action name counts as a shortcut (vs. text editing).
fn is_shortcut(name: &str) -> bool {
    const EDITING: &[&str] = &[
        "Backspace",
        "Delete",
        "DeleteToPreviousWord",
        "DeleteToNextWord",
        "DeleteToBeginningOfLine",
        "Left",
        "Right",
        "WordLeft",
        "WordRight",
        "SelectLeft",
        "SelectRight",
        "SelectWordLeft",
        "SelectWordRight",
        "Home",
        "End",
        "ShowCharacterPalette",
    ];
    match name.rsplit_once("::") {
        Some((ns, action)) if ns.ends_with("search_input") => !EDITING.contains(&action),
        _ => true,
    }
}

/// `folder_opened` / `folder_load_failed` for a folder that landed.
pub(super) fn folder_opened(
    path: &Path,
    via: NavVia,
    outcome: Result<usize, &anyhow::Error>,
    elapsed: std::time::Duration,
) {
    let location = ev::Location::of(path);
    match outcome {
        Ok(entries) => fa::track(ev::FolderOpened {
            via,
            location,
            load_ms: elapsed,
            entries,
            entries_bucket: ev::CountBucket::of(entries as u64),
        }),
        Err(err) => fa::track(ev::FolderLoadFailed {
            via,
            location,
            load_ms: elapsed,
            error: ev::IoError::of(err),
        }),
    }
}

/// `file_op` for a batch that reached the undo journal.
pub(super) fn ops_applied(
    applied: &[filex::ops::AppliedOp],
    source: ev::OpSource,
    requested: usize,
    elapsed: Option<std::time::Duration>,
) {
    fa::track(ev::FileOp {
        op: ev::OpKind::of_batch(applied),
        source,
        ok: !applied.is_empty(),
        items: applied.len(),
        requested,
        kinds: applied.iter().map(ev::OpKind::of_applied).collect(),
        duration_ms: elapsed,
        error: None,
    });
}

/// `file_op` for a single op that failed or was canceled.
pub(super) fn op_failed(op: ev::OpKind, source: ev::OpSource, error: ev::IoError) {
    fa::track(ev::FileOp {
        op,
        source,
        ok: false,
        items: 0,
        requested: 1,
        kinds: Default::default(),
        duration_ms: None,
        error: Some(error),
    });
}

/// `index_status` on the first daemon answer of a run and whenever a build
/// finishes. Root paths are never sent.
pub(super) fn index_transition(
    seen_before: bool,
    previous: &filex::daemon::ipc::Status,
    current: &filex::daemon::ipc::Status,
) {
    let finished_build = previous.building && !current.building;
    if !fa::is_enabled() || (seen_before && !finished_build) {
        return;
    }
    let files: u64 = current.roots.iter().map(|r| r.files).sum();
    fa::track(ev::IndexStatus {
        trigger: if seen_before {
            ev::IndexTrigger::BuildFinished
        } else {
            ev::IndexTrigger::Connected
        },
        building: current.building,
        roots: current.roots.len(),
        files,
        files_bucket: ev::CountBucket::of(files),
        skipped: current.skipped,
        error: current.error.is_some(),
    });
}

/// One search, from the first keystroke to opening a result or clearing
/// the box, summarized into a single `search_session` event. Only sizes,
/// timings and filter kinds are kept: the query text never is.
#[derive(Debug)]
pub(super) struct SearchSession {
    started: std::time::Instant,
    edits: u32,
    max_len: usize,
    final_len: usize,
    responses: u32,
    first_ms: Option<u64>,
    total_ms: u64,
    max_ms: u64,
    results: usize,
    zero_result_responses: u32,
    errors: u32,
    pages: u32,
    filters: Vec<ev::SearchFilter>,
    phrases: bool,
    magic: bool,
    scope: ev::Scope,
    partial: bool,
    literal_won: bool,
    plan_shown: bool,
}

impl SearchSession {
    pub(super) fn new() -> Self {
        Self {
            started: std::time::Instant::now(),
            edits: 0,
            max_len: 0,
            final_len: 0,
            responses: 0,
            first_ms: None,
            total_ms: 0,
            max_ms: 0,
            results: 0,
            zero_result_responses: 0,
            errors: 0,
            pages: 0,
            filters: Vec::new(),
            phrases: false,
            magic: false,
            scope: ev::Scope::Anywhere,
            partial: false,
            literal_won: false,
            plan_shown: false,
        }
    }

    /// True the first time a Magic plan lands in this session, so a plan
    /// re-run by file-system events is reported once.
    pub(super) fn first_plan(&mut self) -> bool {
        !std::mem::replace(&mut self.plan_shown, true)
    }

    /// The query changed to one `len` chars long.
    pub(super) fn edit(&mut self, len: usize) {
        self.edits += 1;
        self.max_len = self.max_len.max(len);
        self.final_len = len;
    }

    /// A search was issued with these properties.
    pub(super) fn issued(
        &mut self,
        filters: impl IntoIterator<Item = ev::SearchFilter>,
        phrases: bool,
        magic: bool,
        scope: ev::Scope,
    ) {
        for filter in filters {
            if !self.filters.contains(&filter) {
                self.filters.push(filter);
            }
        }
        self.phrases |= phrases;
        self.magic |= magic;
        self.scope = scope;
    }

    /// A response landed.
    pub(super) fn answered(
        &mut self,
        took: std::time::Duration,
        results: usize,
        partial: bool,
        literal_won: bool,
    ) {
        let ms = took.as_millis() as u64;
        self.responses += 1;
        self.first_ms.get_or_insert(ms);
        self.total_ms += ms;
        self.max_ms = self.max_ms.max(ms);
        self.results = results;
        if results == 0 {
            self.zero_result_responses += 1;
        }
        self.partial |= partial;
        self.literal_won = literal_won;
    }

    pub(super) fn failed(&mut self) {
        self.errors += 1;
    }

    pub(super) fn paged(&mut self) {
        self.pages += 1;
    }

    /// The summary event.
    pub(super) fn finish(
        self,
        outcome: ev::SearchOutcome,
        rank: Option<usize>,
    ) -> ev::SearchSession {
        ev::SearchSession {
            outcome,
            rank,
            duration_ms: self.started.elapsed(),
            edits: self.edits,
            max_query_len: self.max_len,
            final_query_len: self.final_len,
            responses: self.responses,
            first_response_ms: self.first_ms,
            mean_response_ms: self.total_ms.checked_div(u64::from(self.responses)),
            max_response_ms: self.max_ms,
            results: self.results,
            results_bucket: ev::CountBucket::of(self.results as u64),
            zero_result_responses: self.zero_result_responses,
            errors: self.errors,
            pages_loaded: self.pages,
            filters: self.filters,
            phrases: self.phrases,
            magic: self.magic,
            scope: self.scope,
            hit_limit: self.partial,
            literal_won: self.literal_won,
        }
    }
}

impl Workspace {
    /// Count a query edit, starting a session on the first keystroke and
    /// ending it (as `cleared`) when the box empties.
    pub(super) fn search_session_edit(&mut self, len: usize) {
        if len == 0 {
            self.end_search_session(ev::SearchOutcome::Cleared, None);
        } else if fa::is_enabled() {
            self.search_session
                .get_or_insert_with(SearchSession::new)
                .edit(len);
        }
    }

    /// Report and drop the live search session, if any.
    pub(super) fn end_search_session(&mut self, outcome: ev::SearchOutcome, rank: Option<usize>) {
        if let Some(session) = self.search_session.take() {
            fa::track(session.finish(outcome, rank));
        }
    }

    /// `folder_opened` for a load that just landed (see `load_listing`).
    pub(super) fn track_folder_opened(
        &self,
        path: &Path,
        via: NavVia,
        read: &anyhow::Result<Vec<filex::listing::Entry>>,
    ) {
        let outcome = read.as_ref().map(Vec::len);
        folder_opened(path, via, outcome, self.last_load_start.elapsed());
    }

    /// React to a settings change: apply a consent flip first (turning
    /// sharing off stops collection before anything else is recorded), then
    /// report every other changed setting as `setting_changed`.
    pub(super) fn analytics_settings_changed(
        &mut self,
        previous: &filex::settings::Settings,
        cx: &mut Context<Self>,
    ) {
        let current = self.settings.read(cx).settings();
        if previous.share_diagnostics != current.share_diagnostics {
            if current.share_diagnostics {
                self.enable_analytics(cx);
            } else {
                // Joins the worker and deletes two small files; bounded
                // and only on this explicit toggle.
                fa::disable();
                self.search_session = None;
            }
            return;
        }
        for event in fa::snapshot::changes(previous, current) {
            fa::track(event);
        }
    }

    /// Sharing turned on mid-session: start analytics off the UI thread
    /// (it reads or creates the install-ID file), then drain queued crashes
    /// and start resource sampling.
    fn enable_analytics(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let started = cx
                .background_executor()
                .spawn(async { fa::init("filex", env!("CARGO_PKG_VERSION"), true) })
                .await;
            if started.is_some() {
                fa::track(ev::FeatureUsed {
                    feature: ev::Feature::AnalyticsEnabled,
                    items: None,
                });
                this.update(cx, |this, cx| {
                    this.spawn_crash_upload(cx);
                    this.spawn_resource_sampling(cx);
                })
                .ok();
            }
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use filex::diagnostics::analytics::testing::Capture;

    #[test]
    fn text_editing_is_not_a_shortcut() {
        assert!(!is_shortcut("search_input::Backspace"));
        assert!(!is_shortcut("search_input::WordLeft"));
        assert!(is_shortcut("search_input::Paste"));
        assert!(is_shortcut("filex::GoUp"));
        assert!(is_shortcut("filex::NewTab"));
    }

    #[test]
    fn search_session_summarizes_without_the_query() {
        let capture = Capture::start();
        let mut session = SearchSession::new();
        session.edit(1);
        session.edit(4);
        session.edit(3);
        session.issued(
            [ev::SearchFilter::Kind, ev::SearchFilter::Modified],
            true,
            false,
            ev::Scope::CurrentDir,
        );
        session.issued(
            [ev::SearchFilter::Kind],
            false,
            false,
            ev::Scope::CurrentDir,
        );
        session.answered(std::time::Duration::from_millis(12), 0, false, false);
        session.answered(std::time::Duration::from_millis(8), 40, false, true);
        fa::track(session.finish(ev::SearchOutcome::Opened, Some(2)));
        let (name, json) = capture.events().remove(0);
        assert_eq!(name, "search_session");
        assert_eq!(json["outcome"], "opened");
        assert_eq!(json["rank"], 2);
        assert_eq!(json["edits"], 3);
        assert_eq!(json["max_query_len"], 4);
        assert_eq!(json["final_query_len"], 3);
        assert_eq!(json["first_response_ms"], 12);
        assert_eq!(json["mean_response_ms"], 10);
        assert_eq!(json["zero_result_responses"], 1);
        assert_eq!(json["filters"], serde_json::json!(["kind", "modified"]));
        assert_eq!(json["scope"], "current_dir");
        assert_eq!(json["literal_won"], true);
    }

    #[test]
    fn ops_report_batch_kind_and_tally() {
        let capture = Capture::start();
        let applied = vec![
            filex::ops::AppliedOp::Created { path: "/a".into() },
            filex::ops::AppliedOp::Moved {
                from: "/b".into(),
                to: "/c".into(),
            },
        ];
        ops_applied(&applied, ev::OpSource::Magic, 3, None);
        let (_, json) = capture.events().remove(0);
        assert_eq!(json["op"], "mixed");
        assert_eq!(json["source"], "magic");
        assert_eq!(json["requested"], 3);
        assert_eq!(json["kinds"], serde_json::json!({"create": 1, "move": 1}));
        let text = serde_json::Value::Object(json).to_string();
        assert!(!text.contains("/a") && !text.contains("/c"), "{text}");
    }

    #[test]
    fn launch_kinds_never_carry_the_path() {
        let kind = launch_kind(&Browse::Open("/Users/ann/Secret".into()));
        assert_eq!(kind, ev::LaunchKind::OpenFolder);
    }
}

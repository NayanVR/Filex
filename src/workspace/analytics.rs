//! UI-side product analytics helpers: the launch event, keyboard-shortcut
//! tallies, and small label mappings shared by the instrumented call
//! sites. Events themselves are built with `filex::diagnostics::analytics`
//! (typed props only, no PII; see that module and
//! `docs/design-telemetry.md` for the catalog).
//!
//! Everything here is a channel send or a no-op when analytics is off.
//! Nothing touches I/O on the UI thread.

use super::*;
use filex::diagnostics::analytics::{self as fa, Event, names};
use filex::launch::Browse;

/// How the process was asked to start, as a fixed label.
pub(super) fn launch_kind(browse: &Browse) -> &'static str {
    match browse {
        Browse::Default => "default",
        Browse::Open(_) => "open_folder",
        Browse::Select(_) => "select_item",
        Browse::ThisPc => "this_pc",
        Browse::Foreign(_) => "foreign",
    }
}

/// `app_launched`: launch kind, whether this is a new install, time to the
/// first window, and the settings snapshot.
pub(super) fn track_launch(
    launch: &'static str,
    started: fa::Started,
    settings: Option<&filex::settings::Settings>,
    startup: std::time::Duration,
) {
    let mut event = Event::new(names::APP_LAUNCHED)
        .with("launch", launch)
        .with("new_install", started.new_install)
        .with("startup_ms", startup);
    if let Some(settings) = settings {
        event = fa::snapshot::with_settings(event, settings)
            .with("onboarding_pending", settings.onboarding_pending);
    }
    fa::track(event);
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

/// `folder_opened` (or `folder_load_failed`): how the folder was reached,
/// its coarse location kind, time to first paint and entry count. Never
/// the path or any name in it.
pub(super) fn folder_opened(
    path: &std::path::Path,
    via: &'static str,
    outcome: Result<usize, &anyhow::Error>,
    elapsed: std::time::Duration,
) {
    if !fa::is_enabled() {
        return;
    }
    let event = match outcome {
        Ok(entries) => Event::new(names::FOLDER_OPENED)
            .with("entries", entries)
            .with("entries_bucket", fa::event::bucket(entries as u64)),
        Err(err) => Event::new(names::FOLDER_LOAD_FAILED).with("error", io_error_kind(err)),
    };
    fa::track(
        event
            .with("via", via)
            .with("location", location_kind(path))
            .with("load_ms", elapsed),
    );
}

/// The `io::ErrorKind` behind an error, as a fixed label. The message is
/// never sent (it usually names the path).
pub(super) fn io_error_kind(err: &anyhow::Error) -> &'static str {
    use std::io::ErrorKind as K;
    let Some(io) = err.chain().find_map(|e| e.downcast_ref::<std::io::Error>()) else {
        return "other";
    };
    match io.kind() {
        K::NotFound => "not_found",
        K::PermissionDenied => "permission_denied",
        K::AlreadyExists => "already_exists",
        K::TimedOut => "timed_out",
        K::Interrupted => "interrupted",
        K::InvalidInput | K::InvalidData | K::InvalidFilename => "invalid",
        K::StorageFull | K::QuotaExceeded => "storage_full",
        K::ReadOnlyFilesystem => "read_only",
        K::DirectoryNotEmpty => "directory_not_empty",
        K::NotADirectory => "not_a_directory",
        K::IsADirectory => "is_a_directory",
        K::CrossesDevices => "crosses_devices",
        K::ResourceBusy => "busy",
        K::Unsupported => "unsupported",
        K::NetworkUnreachable
        | K::HostUnreachable
        | K::ConnectionRefused
        | K::ConnectionReset
        | K::ConnectionAborted
        | K::NotConnected
        | K::NetworkDown => "network",
        _ => "io_other",
    }
}

/// The coarse kind of a file, as a fixed label (from the extension's
/// family, never the name or the extension itself).
pub(super) fn file_kind(name: &str, is_dir: bool) -> &'static str {
    use filex::listing::FileKind as K;
    match K::of(name, is_dir) {
        K::Directory => "directory",
        K::Image => "image",
        K::Video => "video",
        K::Audio => "audio",
        K::Archive => "archive",
        K::Code => "code",
        K::Document => "document",
        K::Other => "other",
    }
}

/// The label for a search filter's *kind* (never its value).
pub(super) fn filter_label(filter: &filex::search::filter::Filter) -> &'static str {
    use filex::search::filter::Filter as F;
    match filter {
        F::Tag(_) => "tag",
        F::Kind(_) => "kind",
        F::Ext(_) => "ext",
        F::Size(_) => "size",
        F::Modified(_) => "modified",
    }
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
    filters: Vec<&'static str>,
    phrases: bool,
    magic: bool,
    scope: &'static str,
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
            scope: "anywhere",
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
        filters: impl IntoIterator<Item = &'static str>,
        phrases: bool,
        magic: bool,
        scope: &'static str,
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

    /// The summary event. `outcome` is how the search ended (`opened`,
    /// `revealed`, `cleared`, `applied`…), `rank` the opened row.
    pub(super) fn finish(self, outcome: &'static str, rank: Option<usize>) -> Event {
        let mean_ms = self.total_ms.checked_div(u64::from(self.responses));
        Event::new(names::SEARCH_SESSION)
            .with("outcome", outcome)
            .with_opt("rank", rank)
            .with("duration_ms", self.started.elapsed())
            .with("edits", self.edits)
            .with("max_query_len", self.max_len)
            .with("final_query_len", self.final_len)
            .with("responses", self.responses)
            .with_opt("first_response_ms", self.first_ms)
            .with_opt("mean_response_ms", mean_ms)
            .with("max_response_ms", self.max_ms)
            .with("results", self.results)
            .with("results_bucket", fa::event::bucket(self.results as u64))
            .with("zero_result_responses", self.zero_result_responses)
            .with("errors", self.errors)
            .with("pages_loaded", self.pages)
            .with("filters", self.filters)
            .with("phrases", self.phrases)
            .with("magic", self.magic)
            .with("scope", self.scope)
            .with("hit_limit", self.partial)
            .with("literal_won", self.literal_won)
    }
}

/// The kind of a requested op, as a fixed label.
pub(super) fn op_kind(op: &filex::ops::FileOp) -> &'static str {
    use filex::ops::FileOp as F;
    match op {
        F::Move { .. } => "move",
        F::Copy { .. } => "copy",
        F::Rename { .. } => "rename",
        F::Delete { .. } => "trash",
        F::CreateFolder { .. } => "create",
    }
}

/// The kind of a completed op, matching [`op_kind`]'s labels.
fn applied_kind(op: &filex::ops::AppliedOp) -> &'static str {
    use filex::ops::AppliedOp as A;
    match op {
        A::Moved { .. } => "move",
        A::Copied { .. } => "copy",
        A::Renamed { .. } => "rename",
        A::Deleted { .. } => "trash",
        A::Created { .. } => "create",
    }
}

/// Per-kind counts of a batch, e.g. `{move: 3, create: 1}`.
fn kind_tally(applied: &[filex::ops::AppliedOp]) -> fa::Prop {
    let mut tally: Vec<(&'static str, u64)> = Vec::new();
    for op in applied {
        let kind = applied_kind(op);
        match tally.iter_mut().find(|(k, _)| *k == kind) {
            Some((_, n)) => *n += 1,
            None => tally.push((kind, 1)),
        }
    }
    fa::Prop::Tally(tally)
}

/// `file_op`: a batch that reached the undo journal. `source` is where it
/// came from (paste, drop, delete, magic, single, new_folder, compress);
/// `requested` vs `items` shows how many were skipped or failed.
pub(super) fn ops_applied(
    applied: &[filex::ops::AppliedOp],
    source: &'static str,
    requested: usize,
    elapsed: std::time::Duration,
) {
    if !fa::is_enabled() {
        return;
    }
    let op = match applied {
        [] => "none",
        [first, rest @ ..] if rest.iter().all(|o| applied_kind(o) == applied_kind(first)) => {
            applied_kind(first)
        }
        _ => "mixed",
    };
    fa::track(
        Event::new(names::FILE_OP)
            .with("op", op)
            .with("source", source)
            .with("ok", !applied.is_empty())
            .with("items", applied.len())
            .with("requested", requested)
            .with("kinds", kind_tally(applied))
            .with("duration_ms", elapsed),
    );
}

/// `file_op` for a single op that failed or was canceled.
pub(super) fn op_failed(op: &'static str, source: &'static str, error: &'static str) {
    fa::track(
        Event::new(names::FILE_OP)
            .with("op", op)
            .with("source", source)
            .with("ok", false)
            .with("items", 0u32)
            .with("requested", 1u32)
            .with("error", error),
    );
}

/// `undo`: what kinds of op were undone, and whether it worked.
pub(super) fn undo(batch: &[filex::ops::AppliedOp], error: Option<&anyhow::Error>) {
    fa::track(
        Event::new(names::UNDO)
            .with("items", batch.len())
            .with("kinds", kind_tally(batch))
            .with("ok", error.is_none())
            .with_opt("error", error.map(io_error_kind)),
    );
}

/// `update`: a step of the update flow (available, the banner action
/// taken, dismissed, failed). The version is in `$app_version`; the
/// offered version isn't sent.
pub(super) fn update_stage(stage: &'static str) {
    fa::track(Event::new(names::UPDATE).with("stage", stage));
}

/// `index_status` on the first daemon answer of a run and whenever a build
/// finishes: how many roots, how many files, how many unreadable entries,
/// and whether the daemon reports an error. Root paths are never sent.
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
    fa::track(
        Event::new(names::INDEX_STATUS)
            .with(
                "trigger",
                if seen_before {
                    "build_finished"
                } else {
                    "connected"
                },
            )
            .with("building", current.building)
            .with("roots", current.roots.len())
            .with("files", files)
            .with("files_bucket", fa::event::bucket(files))
            .with("skipped", current.skipped)
            .with("error", current.error.is_some()),
    );
}

impl Workspace {
    /// Count a query edit, starting a session on the first keystroke and
    /// ending it (as `cleared`) when the box empties.
    pub(super) fn search_session_edit(&mut self, len: usize) {
        if len == 0 {
            self.end_search_session("cleared", None);
        } else if fa::is_enabled() {
            self.search_session
                .get_or_insert_with(SearchSession::new)
                .edit(len);
        }
    }

    /// Report and drop the live search session, if any.
    pub(super) fn end_search_session(&mut self, outcome: &'static str, rank: Option<usize>) {
        if let Some(session) = self.search_session.take() {
            fa::track(session.finish(outcome, rank));
        }
    }

    /// `folder_opened` for a load that just landed (see `load_listing`).
    pub(super) fn track_folder_opened(
        &self,
        path: &std::path::Path,
        via: &'static str,
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
                fa::disable();
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
                feature("analytics_enabled");
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

/// Track a `feature_used` event: one generic event for the long tail of
/// UI affordances, keyed by a fixed `feature` label.
pub(super) fn feature(feature: &'static str) {
    fa::track(Event::new(names::FEATURE_USED).with("feature", feature));
}

/// `feature_used` with one extra typed property.
pub(super) fn feature_with(feature: &'static str, key: &'static str, value: impl Into<fa::Prop>) {
    fa::track(
        Event::new(names::FEATURE_USED)
            .with("feature", feature)
            .with(key, value),
    );
}

/// The coarse kind of a location, for navigation events. Never the path.
/// Pure string checks, no I/O: a UNC path is `network`, anything else is
/// classified by shape.
pub(super) fn location_kind(path: &std::path::Path) -> &'static str {
    let text = path.as_os_str().to_string_lossy();
    if text.starts_with(r"\\") || text.starts_with("//") {
        "network"
    } else if path.parent().is_none() {
        "root"
    } else if dirs::home_dir().is_some_and(|home| path == home) {
        "home"
    } else {
        "local"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_editing_is_not_a_shortcut() {
        assert!(!is_shortcut("search_input::Backspace"));
        assert!(!is_shortcut("search_input::WordLeft"));
        assert!(is_shortcut("search_input::Paste"));
        assert!(is_shortcut("filex::GoUp"));
        assert!(is_shortcut("filex::NewTab"));
    }

    #[test]
    fn location_kinds() {
        assert_eq!(
            location_kind(std::path::Path::new(r"\\nas\share\x")),
            "network"
        );
        assert_eq!(location_kind(std::path::Path::new("/")), "root");
        assert_eq!(location_kind(std::path::Path::new("/srv/data")), "local");
    }

    #[test]
    fn search_session_summarizes_without_the_query() {
        let mut session = SearchSession::new();
        session.edit(1);
        session.edit(4);
        session.edit(3);
        session.issued(["kind", "modified"], true, false, "current_dir");
        session.issued(["kind"], false, false, "current_dir");
        session.answered(std::time::Duration::from_millis(12), 0, false, false);
        session.answered(std::time::Duration::from_millis(8), 40, false, true);
        let event = session.finish("opened", Some(2));
        let json = serde_json::Value::Object(event.props_json());
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
    fn file_kinds_are_coarse_labels() {
        assert_eq!(file_kind("Tax Return 2025.pdf", false), "document");
        assert_eq!(file_kind("holiday.JPG", false), "image");
        assert_eq!(file_kind("Projects", true), "directory");
    }

    #[test]
    fn launch_kinds_never_carry_the_path() {
        let kind = launch_kind(&Browse::Open("/Users/ann/Secret".into()));
        assert_eq!(kind, "open_folder");
    }
}

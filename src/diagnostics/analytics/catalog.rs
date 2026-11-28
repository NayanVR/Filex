//! **The analytics event catalog: every product event filex can send.**
//!
//! [`super::track`] only accepts types declared here, so this file is the
//! complete, reviewable list of what leaves the machine. Field types are
//! limited by [`super::schema::Field`] to numbers, bools, durations, fixed
//! ids and the closed vocabularies below. A path, name, query or other
//! user text cannot be a field.
//!
//! `docs/analytics-events.md` is generated from this file; the
//! `catalog_doc_is_current` test fails when it is stale. After changing
//! anything here, run `FILEX_BLESS=1 cargo test --lib catalog_doc_is_current`.
//!
//! Conventions: event names are `object_action` in snake_case; durations
//! are `*_ms`; a breaking change to an event's shape bumps
//! [`super::wire::SCHEMA_VERSION`].

use std::time::Duration;

use super::event::Prop;
use super::schema::{Field, FieldType, Tally, events, labels};

labels! {
    /// How the process was started.
    pub enum LaunchKind {
        Default = "default",
        OpenFolder = "open_folder",
        SelectItem = "select_item",
        ThisPc = "this_pc",
        Foreign = "foreign",
    }

    /// How a folder was reached.
    pub enum NavVia {
        Launch = "launch",
        Up = "up",
        Back = "back",
        Forward = "forward",
        /// Enter or double-click on a folder row.
        Open = "open",
        SearchResult = "search_result",
        Reveal = "reveal",
        PathBar = "path_bar",
        Breadcrumb = "breadcrumb",
        SidebarPlace = "sidebar_place",
        SidebarFavorite = "sidebar_favorite",
        SidebarDrive = "sidebar_drive",
        Recent = "recent",
        ThisPc = "this_pc",
        ConnectServer = "connect_server",
        Magic = "magic",
        Picker = "picker",
        NewTab = "new_tab",
        Refresh = "refresh",
        /// A load the code didn't attribute (a reload to a new folder).
        Other = "other",
    }

    /// The coarse kind of a location (never the path).
    pub enum Location {
        Local = "local",
        Home = "home",
        Root = "root",
        Network = "network",
    }

    /// An `io::ErrorKind`, grouped. The error message is never sent.
    pub enum IoError {
        NotFound = "not_found",
        PermissionDenied = "permission_denied",
        AlreadyExists = "already_exists",
        TimedOut = "timed_out",
        Interrupted = "interrupted",
        Invalid = "invalid",
        StorageFull = "storage_full",
        ReadOnly = "read_only",
        DirectoryNotEmpty = "directory_not_empty",
        NotADirectory = "not_a_directory",
        IsADirectory = "is_a_directory",
        CrossesDevices = "crosses_devices",
        Busy = "busy",
        Unsupported = "unsupported",
        Network = "network",
        Canceled = "canceled",
        IoOther = "io_other",
        /// Not an I/O error at all.
        Other = "other",
    }

    /// A count rounded into a bucket.
    pub enum CountBucket {
        Zero = "0",
        One = "1",
        UpTo5 = "2-5",
        UpTo20 = "6-20",
        UpTo100 = "21-100",
        UpTo1k = "101-1k",
        UpTo10k = "1k-10k",
        UpTo100k = "10k-100k",
        More = "100k+",
    }

    /// How a search ended.
    pub enum SearchOutcome {
        Opened = "opened",
        Revealed = "revealed",
        Cleared = "cleared",
        MagicApplied = "magic_applied",
        MagicCommand = "magic_command",
        MagicGo = "magic_go",
    }

    /// The kind of a search filter (never its value).
    pub enum SearchFilter {
        Tag = "tag",
        Kind = "kind",
        Ext = "ext",
        Size = "size",
        Modified = "modified",
    }

    /// Where a search looks.
    pub enum Scope {
        Anywhere = "anywhere",
        CurrentDir = "current_dir",
    }

    /// A Magic plan's verb.
    pub enum MagicVerb {
        Delete = "delete",
        Move = "move",
        Copy = "copy",
        Rename = "rename",
        Compress = "compress",
        Tag = "tag",
        Untag = "untag",
        Organize = "organize",
    }

    /// What a Magic plan does when a name is taken.
    pub enum Conflicts {
        KeepBoth = "keep_both",
        Skip = "skip",
        Replace = "replace",
    }

    /// A Magic instant command's kind.
    pub enum MagicInstantKind {
        App = "app",
        Go = "go",
    }

    /// A file operation's kind.
    pub enum OpKind {
        Move = "move",
        Copy = "copy",
        Rename = "rename",
        Trash = "trash",
        Create = "create",
        /// A batch of several kinds.
        Mixed = "mixed",
        /// Nothing was applied.
        None = "none",
    }

    /// Where a file operation came from.
    pub enum OpSource {
        Paste = "paste",
        Drop = "drop",
        Delete = "delete",
        Magic = "magic",
        /// One op (rename, single paste, conflict resolution).
        Single = "single",
        NewFolder = "new_folder",
        Compress = "compress",
    }

    /// A file's broad category, from its extension.
    pub enum FileKind {
        Directory = "directory",
        Image = "image",
        Video = "video",
        Audio = "audio",
        Archive = "archive",
        Code = "code",
        Document = "document",
        Other = "other",
    }

    /// Where a file was opened from.
    pub enum OpenSource {
        Browse = "browse",
        Search = "search",
        OpenWith = "open_with",
    }

    /// Which app opened a file.
    pub enum OpenApp {
        Default = "default",
        Chosen = "chosen",
    }

    /// What a context menu was opened on.
    pub enum MenuTarget {
        Entry = "entry",
        SearchResult = "search_result",
        Favorite = "favorite",
        FolderIcon = "folder_icon",
        NetworkDrive = "network_drive",
        Background = "background",
    }

    /// A top-bar navigation button.
    pub enum ToolbarAction {
        Back = "back",
        Forward = "forward",
        Parent = "parent",
        Refresh = "refresh",
    }

    /// A tab being opened or closed.
    pub enum TabAction {
        Opened = "opened",
        Closed = "closed",
    }

    /// Small affordances without properties of their own.
    pub enum Feature {
        QuickLook = "quick_look",
        DragOut = "drag_out",
        SettingsOpened = "settings_opened",
        SidebarTagSearch = "sidebar_tag_search",
        TagRemoved = "tag_removed",
        AnalyticsEnabled = "analytics_enabled",
    }

    /// A user setting.
    pub enum SettingKey {
        Theme = "theme",
        Accent = "accent",
        View = "view",
        Density = "density",
        SortBy = "sort_by",
        SortAscending = "sort_ascending",
        DirsFirst = "dirs_first",
        GridZoom = "grid_zoom",
        PreviewOpen = "preview_open",
        ShowHidden = "show_hidden",
        IndexSystem = "index_system",
        ExcludeDev = "exclude_dev",
        ConfirmDelete = "confirm_delete",
        DeleteToTrash = "delete_to_trash",
        Thumbnails = "thumbnails",
        Transparency = "transparency",
        Roots = "roots",
        Favorites = "favorites",
        ExcludedNames = "excluded_names",
        FolderIcons = "folder_icons",
        CustomShortcuts = "custom_shortcuts",
        CollapsedSections = "collapsed_sections",
    }

    /// An onboarding screen.
    pub enum OnboardingScreen {
        Loading = "loading",
        Welcome = "welcome",
        Theme = "theme",
        Favorites = "favorites",
        Search = "search",
        Magic = "magic",
        DefaultApp = "default_app",
        Ready = "ready",
    }

    /// A step of the update flow.
    pub enum UpdateStage {
        Available = "available",
        CopyCommand = "copy_command",
        OpenUrl = "open_url",
        Restart = "restart",
        Dismissed = "dismissed",
        Failed = "failed",
    }

    /// Why `index_status` was sent.
    pub enum IndexTrigger {
        Connected = "connected",
        BuildFinished = "build_finished",
    }
}

impl CountBucket {
    pub fn of(n: u64) -> Self {
        match n {
            0 => Self::Zero,
            1 => Self::One,
            2..=5 => Self::UpTo5,
            6..=20 => Self::UpTo20,
            21..=100 => Self::UpTo100,
            101..=1_000 => Self::UpTo1k,
            1_001..=10_000 => Self::UpTo10k,
            10_001..=100_000 => Self::UpTo100k,
            _ => Self::More,
        }
    }
}

impl IoError {
    /// The `io::ErrorKind` behind an error chain, grouped.
    pub fn of(err: &anyhow::Error) -> Self {
        use std::io::ErrorKind as K;
        let Some(io) = err.chain().find_map(|e| e.downcast_ref::<std::io::Error>()) else {
            return Self::Other;
        };
        match io.kind() {
            K::NotFound => Self::NotFound,
            K::PermissionDenied => Self::PermissionDenied,
            K::AlreadyExists => Self::AlreadyExists,
            K::TimedOut => Self::TimedOut,
            K::Interrupted => Self::Interrupted,
            K::InvalidInput | K::InvalidData | K::InvalidFilename => Self::Invalid,
            K::StorageFull | K::QuotaExceeded => Self::StorageFull,
            K::ReadOnlyFilesystem => Self::ReadOnly,
            K::DirectoryNotEmpty => Self::DirectoryNotEmpty,
            K::NotADirectory => Self::NotADirectory,
            K::IsADirectory => Self::IsADirectory,
            K::CrossesDevices => Self::CrossesDevices,
            K::ResourceBusy => Self::Busy,
            K::Unsupported => Self::Unsupported,
            K::NetworkUnreachable
            | K::HostUnreachable
            | K::ConnectionRefused
            | K::ConnectionReset
            | K::ConnectionAborted
            | K::NotConnected
            | K::NetworkDown => Self::Network,
            _ => Self::IoOther,
        }
    }
}

impl FileKind {
    /// The category of a file name (never the name or extension itself).
    pub fn of(name: &str, is_dir: bool) -> Self {
        use crate::listing::FileKind as K;
        match K::of(name, is_dir) {
            K::Directory => Self::Directory,
            K::Image => Self::Image,
            K::Video => Self::Video,
            K::Audio => Self::Audio,
            K::Archive => Self::Archive,
            K::Code => Self::Code,
            K::Document => Self::Document,
            K::Other => Self::Other,
        }
    }
}

impl MagicVerb {
    pub fn of(verb: crate::magic::Verb) -> Self {
        use crate::magic::Verb as V;
        match verb {
            V::Delete => Self::Delete,
            V::Move => Self::Move,
            V::Copy => Self::Copy,
            V::Rename => Self::Rename,
            V::Compress => Self::Compress,
            V::Tag => Self::Tag,
            V::Untag => Self::Untag,
            V::Organize => Self::Organize,
        }
    }
}

impl Conflicts {
    pub fn of(policy: crate::magic::ConflictPolicy) -> Self {
        use crate::magic::ConflictPolicy as P;
        match policy {
            P::KeepBoth => Self::KeepBoth,
            P::Skip => Self::Skip,
            P::Replace => Self::Replace,
        }
    }
}

impl SearchFilter {
    pub fn of(filter: &crate::search::filter::Filter) -> Self {
        use crate::search::filter::Filter as F;
        match filter {
            F::Tag(_) => Self::Tag,
            F::Kind(_) => Self::Kind,
            F::Ext(_) => Self::Ext,
            F::Size(_) => Self::Size,
            F::Modified(_) => Self::Modified,
        }
    }
}

impl OpKind {
    pub fn of_op(op: &crate::ops::FileOp) -> Self {
        use crate::ops::FileOp as F;
        match op {
            F::Move { .. } => Self::Move,
            F::Copy { .. } => Self::Copy,
            F::Rename { .. } => Self::Rename,
            F::Delete { .. } => Self::Trash,
            F::CreateFolder { .. } => Self::Create,
        }
    }

    pub fn of_applied(op: &crate::ops::AppliedOp) -> Self {
        use crate::ops::AppliedOp as A;
        match op {
            A::Moved { .. } => Self::Move,
            A::Copied { .. } => Self::Copy,
            A::Renamed { .. } => Self::Rename,
            A::Deleted { .. } => Self::Trash,
            A::Created { .. } => Self::Create,
        }
    }

    /// The batch's single kind, `Mixed`, or `None` when empty.
    pub fn of_batch(ops: &[crate::ops::AppliedOp]) -> Self {
        let mut kinds = ops.iter().map(Self::of_applied);
        match kinds.next() {
            None => Self::None,
            Some(first) if kinds.all(|k| k == first) => first,
            Some(_) => Self::Mixed,
        }
    }
}

impl Location {
    /// Classify by shape alone (no I/O): UNC → network, `/` or `C:\` →
    /// root, the home folder → home, else local.
    pub fn of(path: &std::path::Path) -> Self {
        let text = path.as_os_str().to_string_lossy();
        if text.starts_with(r"\\") || text.starts_with("//") {
            Self::Network
        } else if path.parent().is_none() {
            Self::Root
        } else if dirs::home_dir().is_some_and(|home| path == home) {
            Self::Home
        } else {
            Self::Local
        }
    }
}

/// A setting's new value: a label, a bool, or a list's new length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingValue {
    Label(&'static str),
    Bool(bool),
    Count(u64),
}

impl Field for SettingValue {
    fn prop(&self) -> Option<Prop> {
        Some(match *self {
            Self::Label(label) => Prop::Label(label),
            Self::Bool(value) => Prop::Bool(value),
            Self::Count(n) => Prop::Int(i64::try_from(n).unwrap_or(i64::MAX)),
        })
    }
    fn schema() -> FieldType {
        FieldType::Variant("id \\| bool \\| int (list length)")
    }
}

events! {
    // ---- Lifecycle --------------------------------------------------------

    /// The first window opened.
    AppLaunched => "app_launched" {
        launch: LaunchKind,
        /// This run created the install ID (first launch with sharing on).
        new_install: bool,
        /// Process start to first window.
        startup_ms: Duration,
        /// The welcome flow will run.
        onboarding_pending: bool,
    }

    /// Settings at launch: which defaults people keep. Lists are counts.
    SettingsSnapshot => "settings_snapshot" {
        /// system, light, dark, oled.
        theme: &'static str,
        /// A preset name, or custom (the hex value is never sent).
        accent: &'static str,
        /// list, grid.
        view: &'static str,
        /// comfortable, compact.
        density: &'static str,
        grid_zoom: u8,
        preview_open: bool,
        /// name, size, modified, kind.
        sort_by: &'static str,
        sort_ascending: bool,
        dirs_first: bool,
        show_hidden: bool,
        index_system: bool,
        exclude_dev: bool,
        confirm_delete: bool,
        delete_to_trash: bool,
        thumbnails: bool,
        transparency: bool,
        roots_count: usize,
        favorites_count: usize,
        excluded_names_count: usize,
        folder_icons_count: usize,
        custom_shortcuts_count: usize,
        collapsed_sections_count: usize,
    }

    /// The process quit cleanly. Built by the pipeline.
    AppSessionEnded => "app_session_ended" {
        duration_s: u64,
        /// How many of each event this session sent.
        events: Tally<&'static str>,
        /// Shortcut use since the last summary, by action name.
        counters: Tally<&'static str>,
        /// Events dropped by the per-session cap, by name.
        dropped: Tally<&'static str>,
    }

    /// Every 30 minutes of a running session. Built by the pipeline.
    UsageSummary => "usage_summary" {
        duration_s: u64,
        since_last_summary_s: u64,
        counters: Tally<&'static str>,
        dropped: Tally<&'static str>,
    }

    /// UI resource use, every 15 minutes.
    ResourceSample => "resource_sample" {
        rss_mb: Option<u64>,
        index_roots: usize,
        tabs: usize,
    }

    /// The index daemon's state, on its first answer and after each build.
    IndexStatus => "index_status" {
        trigger: IndexTrigger,
        building: bool,
        roots: usize,
        files: u64,
        files_bucket: CountBucket,
        /// Entries the walk couldn't read (usually permissions).
        skipped: u64,
        /// The daemon reports an error (its text is never sent).
        error: bool,
    }

    /// The connection to the index daemon was lost.
    IndexUnavailable => "index_unavailable" {
        crash_looping: bool,
    }

    // ---- Navigation -------------------------------------------------------

    /// A folder's rows landed on screen.
    FolderOpened => "folder_opened" {
        via: NavVia,
        location: Location,
        /// Navigation to first paint.
        load_ms: Duration,
        entries: usize,
        entries_bucket: CountBucket,
    }

    /// A folder failed to load.
    FolderLoadFailed => "folder_load_failed" {
        via: NavVia,
        location: Location,
        load_ms: Duration,
        error: IoError,
    }

    // ---- Search and Magic -------------------------------------------------

    /// One search, first keystroke to open, clear or apply. The query text
    /// is never sent.
    SearchSession => "search_session" {
        outcome: SearchOutcome,
        /// Row index of the opened result.
        rank: Option<usize>,
        duration_ms: Duration,
        edits: u32,
        max_query_len: usize,
        final_query_len: usize,
        responses: u32,
        first_response_ms: Option<u64>,
        mean_response_ms: Option<u64>,
        max_response_ms: u64,
        /// Rows in the last response.
        results: usize,
        results_bucket: CountBucket,
        zero_result_responses: u32,
        errors: u32,
        pages_loaded: u32,
        filters: Vec<SearchFilter>,
        /// Natural-language phrases became filters.
        phrases: bool,
        magic: bool,
        scope: Scope,
        /// The daemon's candidate budget was hit.
        hit_limit: bool,
        literal_won: bool,
    }

    /// A Magic plan was shown (once per search).
    MagicPlanShown => "magic_plan_shown" {
        verb: MagicVerb,
        matches: usize,
        ok: bool,
        /// `PlanError::id` when refused.
        refusal: Option<&'static str>,
    }

    /// A Magic plan was confirmed.
    MagicPlanApplied => "magic_plan_applied" {
        verb: MagicVerb,
        selected: usize,
        unchecked: usize,
        local: bool,
        conflicts: Conflicts,
    }

    /// A Magic instant command ran.
    MagicInstant => "magic_instant" {
        kind: MagicInstantKind,
        /// `AppCommand::id` for app commands.
        command: Option<&'static str>,
        new_tab: Option<bool>,
    }

    /// The Magic toggle in the search bar.
    MagicModeToggled => "magic_mode_toggled" {
        on: bool,
    }

    /// The search scope menu.
    SearchScopeChanged => "search_scope_changed" {
        scope: Scope,
    }

    // ---- Files ------------------------------------------------------------

    /// A file operation batch finished (or a single op failed).
    FileOp => "file_op" {
        op: OpKind,
        source: OpSource,
        ok: bool,
        items: usize,
        requested: usize,
        kinds: Tally<OpKind>,
        duration_ms: Option<Duration>,
        error: Option<IoError>,
    }

    /// An undo finished.
    Undo => "undo" {
        items: usize,
        kinds: Tally<OpKind>,
        ok: bool,
        error: Option<IoError>,
    }

    /// A file was opened in an app.
    FileOpened => "file_opened" {
        kind: FileKind,
        source: OpenSource,
        app: OpenApp,
        /// Known only for the default app (the chosen app opens off-thread).
        ok: Option<bool>,
    }

    /// A tag was saved in the tag editor (its name is never sent).
    TagSaved => "tag_saved" {
        edited_existing: bool,
    }

    // ---- UI ---------------------------------------------------------------

    /// A context menu command.
    ContextMenuUsed => "context_menu_used" {
        /// `context_menu::Command::id`.
        command: &'static str,
        target: MenuTarget,
        selected: usize,
    }

    /// A top-bar navigation button was clicked.
    ToolbarButtonClicked => "toolbar_button_clicked" {
        action: ToolbarAction,
    }

    /// A tab was opened or closed.
    TabChanged => "tab_changed" {
        action: TabAction,
        /// Tabs open afterwards.
        tabs: usize,
    }

    /// A small affordance was used.
    FeatureUsed => "feature_used" {
        feature: Feature,
        /// Items involved, where it applies.
        items: Option<usize>,
    }

    /// A setting changed.
    SettingChanged => "setting_changed" {
        setting: SettingKey,
        value: SettingValue,
    }

    // ---- Onboarding and updates -------------------------------------------

    /// A move between onboarding screens.
    OnboardingStep => "onboarding_step" {
        from: OnboardingScreen,
        to: OnboardingScreen,
        /// Time spent on the screen being left.
        on_step_ms: Duration,
    }

    /// The onboarding flow closed.
    OnboardingFinished => "onboarding_finished" {
        /// Finished rather than skipped.
        completed: bool,
        last_step: OnboardingScreen,
        imported_favorites: usize,
        offered_favorites: usize,
        default_app: bool,
        magic_live: bool,
    }

    /// A step of the update flow (the offered version isn't sent).
    UpdateStep => "update" {
        stage: UpdateStage,
    }
}

/// Events that carry scrubbed text and so can't be typed. Documented in
/// the generated catalog.
pub const RAW_EVENTS: &[(&str, &str)] = &[
    (
        "$exception",
        "a panic (drained from the crash queue) or an `error!` log, in PostHog's \
         exception format. Text passes `exception::scrub_text`. See design-telemetry.md.",
    ),
    (
        "feedback_submitted",
        "feedback the user typed and sent: `message` (scrubbed of paths, file names, \
         emails and URLs; at most 5000 chars) and `length`.",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::analytics::schema::{AnalyticsEvent, render_catalog};

    #[test]
    fn catalog_doc_is_current() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/analytics-events.md");
        let rendered = render_catalog(&catalog(), RAW_EVENTS);
        if std::env::var_os("FILEX_BLESS").is_some() {
            std::fs::write(&path, &rendered).unwrap();
            return;
        }
        let on_disk = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            on_disk == rendered,
            "docs/analytics-events.md is stale: run \
             FILEX_BLESS=1 cargo test --lib catalog_doc_is_current"
        );
    }

    #[test]
    fn names_are_unique_snake_case() {
        let mut seen = std::collections::HashSet::new();
        for event in catalog() {
            assert!(seen.insert(event.name), "duplicate event {}", event.name);
            assert!(
                event
                    .name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c == '_'),
                "{} is not snake_case",
                event.name
            );
            assert!(!event.doc.trim().is_empty(), "{} has no doc", event.name);
        }
    }

    #[test]
    fn typed_events_serialize_labels_and_omit_none() {
        let event = FileOp {
            op: OpKind::Move,
            source: OpSource::Drop,
            ok: true,
            items: 2,
            requested: 3,
            kinds: [OpKind::Move, OpKind::Move].into_iter().collect(),
            duration_ms: Some(Duration::from_millis(15)),
            error: None,
        };
        assert_eq!(FileOp::NAME, "file_op");
        let props = event.props();
        assert!(props.contains(&("op", Prop::Label("move"))));
        assert!(props.contains(&("source", Prop::Label("drop"))));
        assert!(props.contains(&("kinds", Prop::Tally(vec![("move", 2)]))));
        assert!(props.contains(&("duration_ms", Prop::Int(15))));
        assert!(!props.iter().any(|(k, _)| *k == "error"));
    }

    #[test]
    fn helpers_map_to_closed_labels() {
        assert_eq!(CountBucket::of(0), CountBucket::Zero);
        assert_eq!(CountBucket::of(1_001), CountBucket::UpTo10k);
        assert_eq!(CountBucket::of(u64::MAX), CountBucket::More);
        assert_eq!(FileKind::of("Tax Return.pdf", false), FileKind::Document);
        assert_eq!(FileKind::of("Projects", true), FileKind::Directory);
        assert_eq!(
            Location::of(std::path::Path::new(r"\\nas\share")),
            Location::Network
        );
        assert_eq!(Location::of(std::path::Path::new("/")), Location::Root);
        assert_eq!(
            Location::of(std::path::Path::new("/srv/x")),
            Location::Local
        );
        let io = anyhow::Error::from(std::io::Error::from(std::io::ErrorKind::NotFound));
        assert_eq!(IoError::of(&io.context("reading")), IoError::NotFound);
        assert_eq!(IoError::of(&anyhow::anyhow!("x")), IoError::Other);
    }
}

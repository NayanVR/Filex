//! Everything the Magic bar understands, of which a file plan is one
//! kind: going somewhere, running an app command, and answering a
//! question about files are the others.
//!
//! Only the explicit entry into magic mode reads these. `settings`,
//! `new tab` and `go to x` are all perfectly good filenames, so none of
//! them may flip a normal search on their own — see
//! [`parse_with_gate`](super::parse_with_gate) for the plan verbs that may.
//!
//! Pure string work, like the rest of [`crate::magic`]: nothing here
//! touches the disk or the app. The workspace maps an [`AppCommand`] onto
//! what it already does for the matching shortcut or menu item.

use super::{Command, Selection, parse_with_gate, selection_from};
use crate::settings::{Density, SortBy, ThemeMode, ViewMode};

/// One reading of a Magic query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    /// A reviewable batch of file operations.
    Plan(Command),
    Go(Go),
    App(AppCommand),
    Ask(Question),
    /// `find invoices`: an ordinary filename search, asked for in words.
    /// Only [`understand`](super::understand) produces it.
    Find(String),
}

/// `go to Downloads` / `open ~/code in new tab`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Go {
    /// The place as spelled; resolved against the disk by the caller.
    pub name: String,
    pub new_tab: bool,
}

/// A read-only question about a set of files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Question {
    /// `how many pdfs in Downloads`
    Count(Selection),
    /// `how big is Downloads` / `how big are my videos`. `folder`: the
    /// words may name a folder ("is"), which then wins over reading them
    /// as a description — the caller decides, since only it can look.
    /// "are" describes files: `videos` there is not the Videos folder.
    Size { of: Selection, folder: bool },
    /// `largest videos`
    /// `largest videos`, `largest 10 files`.
    Largest { of: Selection, top: Option<usize> },
}

impl Question {
    pub fn selection(&self) -> &Selection {
        match self {
            Self::Count(selection)
            | Self::Size { of: selection, .. }
            | Self::Largest { of: selection, .. } => selection,
        }
    }
}

/// Something the app already does from a shortcut, menu or setting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppCommand {
    NewTab,
    CloseTab,
    NextTab,
    PrevTab,
    Back,
    Forward,
    Up,
    Refresh,
    Undo,
    Settings,
    Shortcuts,
    Theme(ThemeMode),
    Density(Density),
    /// `None` toggles.
    HiddenFiles(Option<bool>),
    Sort(SortBy),
    SortAscending(bool),
    View(ViewMode),
    TogglePreview,
    /// Named, or Explorer's "New folder" then rename-in-place.
    NewFolder(Option<String>),
    /// The connect dialog, optionally filled in.
    Connect(Option<String>),
    /// Disconnect the network drive called this.
    Eject(String),
    Terminal,
    CopyPath,
    Properties,
    /// Pin (or unpin) the folder on screen.
    Favorite(bool),
    Feedback,
    Quit,
}

impl AppCommand {
    /// A stable id for the command alone, without any value it carries
    /// (a folder name, an address). Used as an analytics label, so it must
    /// never include user text.
    pub fn id(&self) -> &'static str {
        match self {
            Self::NewTab => "new_tab",
            Self::CloseTab => "close_tab",
            Self::NextTab => "next_tab",
            Self::PrevTab => "prev_tab",
            Self::Back => "back",
            Self::Forward => "forward",
            Self::Up => "up",
            Self::Refresh => "refresh",
            Self::Undo => "undo",
            Self::Settings => "settings",
            Self::Shortcuts => "shortcuts",
            Self::Theme(_) => "theme",
            Self::Density(_) => "density",
            Self::HiddenFiles(_) => "hidden_files",
            Self::Sort(_) => "sort",
            Self::SortAscending(_) => "sort_ascending",
            Self::View(_) => "view",
            Self::TogglePreview => "toggle_preview",
            Self::NewFolder(_) => "new_folder",
            Self::Connect(_) => "connect",
            Self::Eject(_) => "eject",
            Self::Terminal => "terminal",
            Self::CopyPath => "copy_path",
            Self::Properties => "properties",
            Self::Favorite(_) => "favorite",
            Self::Feedback => "feedback",
            Self::Quit => "quit",
        }
    }
}

/// The fixed commands: the name shown in suggestions, then everything
/// else a person might type for it.
const COMMANDS: &[(&str, &[&str], AppCommand)] = &[
    ("New tab", &["open new tab"], AppCommand::NewTab),
    ("Close tab", &[], AppCommand::CloseTab),
    ("Next tab", &[], AppCommand::NextTab),
    ("Previous tab", &["prev tab"], AppCommand::PrevTab),
    ("Go back", &["back"], AppCommand::Back),
    ("Go forward", &["forward"], AppCommand::Forward),
    (
        "Go up",
        &[
            "up",
            "parent",
            "parent folder",
            "go to parent",
            "go up a folder",
            "go up a level",
            "go up one folder",
            "go up one level",
            "up a level",
            "up one level",
        ],
        AppCommand::Up,
    ),
    ("Refresh", &["reload"], AppCommand::Refresh),
    ("Undo", &["undo that", "undo last"], AppCommand::Undo),
    (
        "Settings",
        &["preferences", "open settings", "options"],
        AppCommand::Settings,
    ),
    (
        "Keyboard shortcuts",
        &["shortcuts", "keys", "help"],
        AppCommand::Shortcuts,
    ),
    (
        // "… theme", as Settings › Appearance names them.
        "Dark theme",
        &["dark", "dark mode", "theme dark", "night mode", "night"],
        AppCommand::Theme(ThemeMode::Dark),
    ),
    (
        "Light theme",
        &["light", "light mode", "theme light", "day mode"],
        AppCommand::Theme(ThemeMode::Light),
    ),
    (
        "OLED theme",
        &["oled", "oled mode", "black theme"],
        AppCommand::Theme(ThemeMode::Oled),
    ),
    (
        "System theme",
        &["theme system", "auto theme", "follow system"],
        AppCommand::Theme(ThemeMode::System),
    ),
    (
        "Compact density",
        &["compact", "compact rows"],
        AppCommand::Density(Density::Compact),
    ),
    (
        "Comfortable density",
        &["comfortable", "comfortable rows"],
        AppCommand::Density(Density::Comfortable),
    ),
    (
        "Show hidden files",
        &["show hidden", "show dotfiles"],
        AppCommand::HiddenFiles(Some(true)),
    ),
    (
        "Hide hidden files",
        &["hide hidden", "hide dotfiles"],
        AppCommand::HiddenFiles(Some(false)),
    ),
    (
        "Toggle hidden files",
        &["hidden files", "toggle hidden"],
        AppCommand::HiddenFiles(None),
    ),
    (
        "Sort by name",
        &["sort name"],
        AppCommand::Sort(SortBy::Name),
    ),
    (
        "Sort by size",
        &["sort size"],
        AppCommand::Sort(SortBy::Size),
    ),
    (
        "Sort by date modified",
        &["sort by date", "sort by modified", "sort date"],
        AppCommand::Sort(SortBy::Modified),
    ),
    (
        "Sort by kind",
        &["sort by type", "sort kind", "sort type"],
        AppCommand::Sort(SortBy::Kind),
    ),
    (
        "Sort ascending",
        &["ascending"],
        AppCommand::SortAscending(true),
    ),
    (
        "Sort descending",
        &["descending", "reverse sort"],
        AppCommand::SortAscending(false),
    ),
    (
        "List view",
        &["list", "view as list"],
        AppCommand::View(ViewMode::List),
    ),
    (
        "Grid view",
        &["grid", "icons", "icon view", "view as grid"],
        AppCommand::View(ViewMode::Grid),
    ),
    (
        "Toggle details panel",
        &[
            "details",
            "preview",
            "toggle preview",
            "show details",
            "hide details",
        ],
        AppCommand::TogglePreview,
    ),
    (
        "New folder",
        &["create folder", "make folder", "mkdir"],
        AppCommand::NewFolder(None),
    ),
    (
        "Connect to server",
        &["connect", "connect to share", "map network drive"],
        AppCommand::Connect(None),
    ),
    (
        "Open terminal here",
        &["terminal", "open terminal", "open in terminal"],
        AppCommand::Terminal,
    ),
    ("Copy path", &["copy as path"], AppCommand::CopyPath),
    ("Properties", &["get info", "info"], AppCommand::Properties),
    (
        "Add to favorites",
        &["pin", "pin folder", "favorite", "add favorite"],
        AppCommand::Favorite(true),
    ),
    (
        "Remove from favorites",
        &["unpin", "unpin folder", "unfavorite", "remove favorite"],
        AppCommand::Favorite(false),
    ),
    ("Send feedback", &["feedback"], AppCommand::Feedback),
    ("Quit Filex", &["quit", "exit"], AppCommand::Quit),
];

/// Read `raw` as anything Magic understands, taking the user at their
/// word (they asked for magic mode). `None`: not a complete command yet —
/// [`suggest`] says what it could become.
pub fn interpret(raw: &str, now: i64) -> Option<Intent> {
    // "how big is Downloads?" — the mark is punctuation, not a name.
    let words: Vec<&str> = raw
        .split_whitespace()
        .map(|w| w.trim_end_matches(['?', '!']))
        .filter(|w| !w.is_empty())
        .collect();
    let lower: Vec<String> = words.iter().map(|w| w.to_ascii_lowercase()).collect();
    let lower: Vec<&str> = lower.iter().map(String::as_str).collect();
    let phrase = lower.join(" ");

    // App commands first: `sort by size` sorts the list, and would
    // otherwise read as "organize (nothing) by size".
    if let Some((_, command)) = command_named(&phrase) {
        return Some(Intent::App(command));
    }
    if let Some(command) = app_with_argument(&words, &lower) {
        return Some(Intent::App(command));
    }
    if let Some(question) = question(&words, &lower, now) {
        return Some(Intent::Ask(question));
    }
    if let Some(go) = go(&words, &lower) {
        return Some(Intent::Go(go));
    }
    parse_with_gate(&words.join(" "), now, false).map(Intent::Plan)
}

/// The fixed command called `phrase` (its name or an alias, lowercase),
/// with the name it is shown under.
pub(super) fn command_named(phrase: &str) -> Option<(&'static str, AppCommand)> {
    COMMANDS
        .iter()
        .find(|(label, aliases, _)| label.eq_ignore_ascii_case(phrase) || aliases.contains(&phrase))
        .map(|(label, _, command)| (*label, command.clone()))
}

/// `words` after `prefix` (matched on the lowercased copy), if any are
/// left — in the user's own casing, since they name things.
fn after<'a, 'b>(words: &'a [&'b str], lower: &[&str], prefix: &[&str]) -> Option<&'a [&'b str]> {
    (lower.len() > prefix.len() && lower.starts_with(prefix)).then(|| &words[prefix.len()..])
}

fn app_with_argument(words: &[&str], lower: &[&str]) -> Option<AppCommand> {
    for prefix in [
        &["new", "folder", "called"][..],
        &["new", "folder", "named"],
        &["new", "folder"],
        &["create", "folder"],
        &["make", "folder"],
        &["mkdir"],
    ] {
        if let Some(name) = after(words, lower, prefix) {
            return Some(AppCommand::NewFolder(Some(name.join(" "))));
        }
    }
    for prefix in [
        &["connect", "to", "server"][..],
        &["connect", "to"],
        &["connect"],
    ] {
        if let Some(address) = after(words, lower, prefix) {
            return Some(AppCommand::Connect(Some(address.join(" "))));
        }
    }
    for prefix in [&["eject"][..], &["disconnect"], &["unmount"]] {
        if let Some(name) = after(words, lower, prefix) {
            return Some(AppCommand::Eject(name.join(" ")));
        }
    }
    None
}

fn go(words: &[&str], lower: &[&str]) -> Option<Go> {
    let rest = [
        &["go", "to"][..],
        &["goto"],
        &["cd"],
        &["open"],
        &["show"],
        &["take", "me", "to"],
    ]
    .iter()
    .find_map(|prefix| after(words, lower, prefix))?;
    let new_tab =
        lower.ends_with(&["in", "new", "tab"]) || lower.ends_with(&["in", "a", "new", "tab"]);
    let rest = if new_tab {
        let cut = if lower.ends_with(&["in", "a", "new", "tab"]) {
            4
        } else {
            3
        };
        &rest[..rest.len().saturating_sub(cut)]
    } else {
        rest
    };
    // "go to the Downloads folder"
    let rest = match rest.split_first() {
        Some((first, tail)) if first.eq_ignore_ascii_case("the") && !tail.is_empty() => tail,
        _ => rest,
    };
    let rest = match rest.split_last() {
        Some((last, head)) if last.eq_ignore_ascii_case("folder") && !head.is_empty() => head,
        _ => rest,
    };
    (!rest.is_empty()).then(|| Go {
        name: rest.join(" "),
        new_tab,
    })
}

fn question(words: &[&str], lower: &[&str], now: i64) -> Option<Question> {
    let about = |prefix: &[&str]| {
        let rest = after(words, lower, prefix)?;
        // "how many pdfs are there" / "do i have"
        let rest = [
            &["are", "there"][..],
            &["do", "i", "have"],
            &["there", "are"],
        ]
        .iter()
        .find_map(|tail| {
            let n = rest.len().checked_sub(tail.len())?;
            lower[lower.len() - tail.len()..]
                .eq(*tail)
                .then_some(&rest[..n])
        })
        .unwrap_or(rest);
        let rest = match rest.split_first() {
            Some((first, tail))
                if ["my", "the", "all"].contains(&first.to_ascii_lowercase().as_str())
                    && !tail.is_empty() =>
            {
                tail
            }
            _ => rest,
        };
        selection_from(rest, now, true, true)
    };
    for prefix in [&["how", "many"][..], &["count"], &["number", "of"]] {
        if let Some(selection) = about(prefix) {
            return Some(Question::Count(selection));
        }
    }
    for (prefix, folder) in [
        (&["how", "big", "is"][..], true),
        (&["how", "large", "is"], true),
        (&["size", "of"], true),
        (&["total", "size", "of"], true),
        (&["how", "big", "are"], false),
        (&["how", "large", "are"], false),
    ] {
        if let Some(of) = about(prefix) {
            return Some(Question::Size { of, folder });
        }
    }
    for word in ["largest", "biggest"] {
        let Some(rest) = after(words, lower, &[word]) else {
            continue;
        };
        // "largest 10 videos"
        let top = rest[0].parse::<usize>().ok().filter(|n| *n > 0);
        let rest = if top.is_some() { &rest[1..] } else { rest };
        let rest = if rest.is_empty() {
            &["files"][..]
        } else {
            rest
        };
        if let Some(of) = selection_from(rest, now, true, true) {
            return Some(Question::Largest { of, top });
        }
    }
    None
}

/// What a suggestion row does when chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SuggestionAction {
    Run(AppCommand),
    /// Put this text in the bar for the user to edit: commands that need
    /// a target can't run from a click.
    Fill(&'static str),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    pub label: &'static str,
    /// What it is, for the dim second column.
    pub hint: &'static str,
    pub action: SuggestionAction,
}

/// Worked examples of each sentence shape, real syntax every one — the
/// tests parse them, so a suggestion can't teach something unsupported.
const TEMPLATES: &[(&str, &str)] = &[
    ("move pdfs from Downloads to Documents", "Move files"),
    ("copy selected to Desktop", "Copy files"),
    (
        "delete screenshots older than 30 days",
        "Move files to Trash",
    ),
    ("rename selected to photo-{n}.{ext}", "Rename in bulk"),
    ("zip selected", "Compress to ZIP"),
    ("tag invoices as Work", "Tag files"),
    ("organize Downloads by type", "Sort files into folders"),
    (
        "move selected to new folder Archive",
        "Move into a new folder",
    ),
    ("go to Documents", "Open a folder"),
    ("new folder Projects", "Create a folder"),
    ("connect to smb://server/share", "Open a network share"),
    ("how big is Downloads", "Folder size"),
    ("how many pdfs", "Count files"),
    ("largest videos", "Find what takes the space"),
];

/// What `query` could become: every command and template in which each
/// typed word starts some word. An empty query lists everything, which is
/// what teaches the vocabulary.
///
/// ponytail: linear over ~50 fixed rows of short strings per keystroke —
/// nanoseconds. Index it if the table ever grows by orders of magnitude.
pub fn suggest(query: &str) -> Vec<Suggestion> {
    let typed: Vec<String> = query
        .split_whitespace()
        .map(str::to_ascii_lowercase)
        .collect();
    let all = matching(&typed);
    // Mid-sentence ("move invoices"): the user's own words fit no row,
    // but the verb still says which shapes to show.
    if all.is_empty() && typed.len() > 1 {
        return matching(&typed[..1]);
    }
    all
}

fn matching(typed: &[String]) -> Vec<Suggestion> {
    let fits = |haystack: &mut dyn Iterator<Item = &str>| {
        let words: Vec<String> = haystack
            .flat_map(str::split_whitespace)
            .map(str::to_ascii_lowercase)
            .collect();
        typed
            .iter()
            .all(|t| words.iter().any(|word| word.starts_with(t.as_str())))
    };
    let templates = TEMPLATES
        .iter()
        .filter(|(text, hint)| fits(&mut [*text, *hint].into_iter()))
        .map(|(text, hint)| Suggestion {
            label: text,
            hint,
            action: SuggestionAction::Fill(text),
        });
    let commands = COMMANDS
        .iter()
        .filter(|(label, aliases, _)| {
            fits(&mut std::iter::once(*label).chain(aliases.iter().copied()))
        })
        .map(|(label, _, command)| Suggestion {
            label,
            hint: "Command",
            action: SuggestionAction::Run(command.clone()),
        });
    templates.chain(commands).collect()
}

#[cfg(test)]
mod app_command_id_tests {
    use super::*;

    #[test]
    fn ids_never_carry_the_command_value() {
        assert_eq!(
            AppCommand::NewFolder(Some("Secret".into())).id(),
            "new_folder"
        );
        assert_eq!(
            AppCommand::Connect(Some("smb://nas".into())).id(),
            "connect"
        );
        assert_eq!(AppCommand::Eject("Work".into()).id(), "eject");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::magic::{Action, Verb};
    use crate::search::filter::Filter;

    const NOW: i64 = 1_785_067_200;

    fn read(raw: &str) -> Option<Intent> {
        interpret(raw, NOW)
    }

    #[test]
    fn fixed_commands_match_their_name_and_aliases_in_any_case() {
        assert_eq!(read("new tab"), Some(Intent::App(AppCommand::NewTab)));
        assert_eq!(
            read("Dark Mode"),
            Some(Intent::App(AppCommand::Theme(ThemeMode::Dark)))
        );
        assert_eq!(read("preferences"), Some(Intent::App(AppCommand::Settings)));
        assert_eq!(
            read("show hidden files"),
            Some(Intent::App(AppCommand::HiddenFiles(Some(true))))
        );
    }

    #[test]
    fn sort_by_size_sorts_the_list_rather_than_organizing_files() {
        // "sort" is also the organize verb; the listing command must win.
        assert_eq!(
            read("sort by size"),
            Some(Intent::App(AppCommand::Sort(SortBy::Size)))
        );
        let Some(Intent::Plan(command)) = read("sort Downloads by type") else {
            panic!("expected an organize plan")
        };
        assert_eq!(command.verb, Verb::Organize);
    }

    #[test]
    fn commands_with_an_argument_keep_its_casing() {
        assert_eq!(
            read("new folder Tax Returns"),
            Some(Intent::App(AppCommand::NewFolder(Some(
                "Tax Returns".into()
            ))))
        );
        assert_eq!(
            read("connect to smb://NAS/Media"),
            Some(Intent::App(AppCommand::Connect(Some(
                "smb://NAS/Media".into()
            ))))
        );
        assert_eq!(
            read("eject Backup"),
            Some(Intent::App(AppCommand::Eject("Backup".into())))
        );
        // Without the argument they are the fixed commands.
        assert_eq!(
            read("new folder"),
            Some(Intent::App(AppCommand::NewFolder(None)))
        );
        assert_eq!(
            read("connect"),
            Some(Intent::App(AppCommand::Connect(None)))
        );
    }

    #[test]
    fn go_reads_a_place_and_an_optional_new_tab() {
        assert_eq!(
            read("go to Downloads"),
            Some(Intent::Go(Go {
                name: "Downloads".into(),
                new_tab: false
            }))
        );
        assert_eq!(
            read("open the Project Files folder in a new tab"),
            Some(Intent::Go(Go {
                name: "Project Files".into(),
                new_tab: true
            }))
        );
        assert_eq!(
            read("cd ~/code"),
            Some(Intent::Go(Go {
                name: "~/code".into(),
                new_tab: false
            }))
        );
        // A bare verb is not a command yet.
        assert_eq!(read("go to"), None);
    }

    #[test]
    fn go_back_is_history_not_a_folder_called_back() {
        assert_eq!(read("go back"), Some(Intent::App(AppCommand::Back)));
        assert_eq!(read("back"), Some(Intent::App(AppCommand::Back)));
    }

    #[test]
    fn questions_carry_the_same_selection_grammar_as_commands() {
        let Some(Intent::Ask(Question::Count(selection))) = read("how many pdfs in Downloads?")
        else {
            panic!("expected a count")
        };
        assert_eq!(selection.filters, vec![Filter::Ext("pdf".into())]);
        assert_eq!(selection.within.map(|d| d.name), Some("Downloads".into()));

        let Some(Intent::Ask(Question::Size { of, folder })) = read("how big is Downloads") else {
            panic!("expected a size")
        };
        assert_eq!(of.source, "Downloads");
        assert!(folder);
        // "are" describes files; "videos" is not the Videos folder.
        assert!(matches!(
            read("how big are my videos"),
            Some(Intent::Ask(Question::Size { folder: false, .. }))
        ));

        // "files" means all of them, which a question may be about.
        let Some(Intent::Ask(Question::Largest { of, top })) = read("largest files") else {
            panic!("expected largest")
        };
        assert!(of.text.is_empty() && of.filters.is_empty());
        assert_eq!(top, None);
        assert!(matches!(
            read("largest 10 videos"),
            Some(Intent::Ask(Question::Largest { top: Some(10), .. }))
        ));
        assert!(matches!(
            read("how many screenshots are there"),
            Some(Intent::Ask(Question::Count(_)))
        ));
    }

    #[test]
    fn anything_else_falls_through_to_the_plan_grammar_ungated() {
        // Explicit mode: a bare-text delete is taken at face value.
        let Some(Intent::Plan(command)) = read("delete old logs") else {
            panic!("expected a plan")
        };
        assert_eq!(command.action, Action::Delete);
        assert_eq!(read("quarterly earnings"), None);
    }

    #[test]
    fn every_suggestion_is_something_interpret_accepts() {
        // The suggestion list is the feature's documentation; a row that
        // doesn't parse teaches syntax that doesn't work.
        for suggestion in suggest("") {
            assert!(
                read(suggestion.label).is_some(),
                "{:?} is suggested but not understood",
                suggestion.label
            );
        }
    }

    #[test]
    fn suggestions_narrow_by_word_prefix_across_names_and_aliases() {
        let labels =
            |query: &str| -> Vec<&str> { suggest(query).iter().map(|s| s.label).collect() };
        assert_eq!(labels("dar"), ["Dark theme"]);
        // An alias matches even though the label doesn't contain it.
        assert!(labels("pref").contains(&"Settings"));
        // Every typed word must fit, in any order.
        assert_eq!(labels("tab new"), ["New tab"]);
        assert!(labels("mov").contains(&"move pdfs from Downloads to Documents"));
        assert!(labels("zzz").is_empty());
        // Mid-sentence: the user's own words fit nothing, the verb does.
        assert!(labels("move invoices").contains(&"move pdfs from Downloads to Documents"));
        assert!(labels("zzz invoices").is_empty());
        assert!(labels("").len() > 40);
    }
}

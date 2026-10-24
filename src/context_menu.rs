//! Right-click menu model: which rows a context menu shows, and how the
//! keyboard and the pointer move through them.
//!
//! Pure on purpose: the GPUI view (`workspace::render_menus`) draws these
//! rows and maps each [`Command`] onto a `Workspace` method, so the menu's
//! contents and its navigation are unit-tested without a window.
//!
//! The layout follows Windows 11 File Explorer's menu (an icon bar for the
//! clipboard/rename/share/delete verbs, then labelled rows, submenus marked
//! with a chevron), which also reads naturally on macOS and Linux. Items a
//! platform can't back are left out by the caller through [`Capabilities`],
//! never shown disabled.

use std::borrow::Cow;
use std::time::Duration;

use crate::settings::{FolderIcon, SortBy, SortSettings, ViewMode};

/// How long the pointer must rest on a submenu row before the submenu
/// opens, and on another row before an open one closes. Windows' default
/// `MenuShowDelay` is 400 ms; slightly shorter keeps diagonal travel into
/// the submenu forgiving without feeling sluggish.
pub const SUBMENU_HOVER_DELAY: Duration = Duration::from_millis(300);

/// What activating a menu row does. The view owns the mapping to actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Cut,
    Copy,
    Rename,
    Share,
    Delete,
    Open,
    Reveal,
    AddToFavorites,
    RemoveFromFavorites,
    CompressToZip,
    CopyAsPath,
    IndexFolder,
    FolderIcon,
    Properties,
    Paste,
    OpenInTerminal,
    NewFolder,
    SetView(ViewMode),
    SetSort(SortBy),
    SetAscending(bool),
    /// Launch the target in the `n`th app of the Open With list.
    OpenInApp(usize),
    /// The OS's own application chooser.
    ChooseOtherApp,
    /// Sidebar favorites: move one place up (`-1`) or down (`1`).
    MoveFavorite(isize),
    /// Set a folder's icon; `None` is Automatic.
    SetFolderIcon(Option<FolderIcon>),
    /// A row that only opens its submenu (see [`Item::submenu`]).
    OpenSubmenu,
}

/// The submenus a row can open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Submenu {
    /// Apps that can open the file; listed off the UI thread.
    OpenWith,
    View,
    SortBy,
    New,
}

/// One activatable row (or one button of the icon bar).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub command: Command,
    pub label: Cow<'static, str>,
    /// `icons/<name>.svg`, drawn in the leading icon column.
    pub icon: Option<&'static str>,
    /// Shortcut-catalog id whose live key binding is shown on the right.
    pub shortcut: Option<&'static str>,
    /// Radio/check mark for the current choice in View and Sort by.
    pub checked: bool,
    /// Destructive (warn colour).
    pub danger: bool,
    pub submenu: Option<Submenu>,
}

impl Item {
    fn new(command: Command, label: impl Into<Cow<'static, str>>) -> Self {
        Self {
            command,
            label: label.into(),
            icon: None,
            shortcut: None,
            checked: false,
            danger: false,
            submenu: None,
        }
    }

    fn icon(mut self, icon: &'static str) -> Self {
        self.icon = Some(icon);
        self
    }

    fn shortcut(mut self, id: &'static str) -> Self {
        self.shortcut = Some(id);
        self
    }

    fn checked(mut self, checked: bool) -> Self {
        self.checked = checked;
        self
    }

    fn danger(mut self) -> Self {
        self.danger = true;
        self
    }

    fn submenu(command_label: impl Into<Cow<'static, str>>, submenu: Submenu) -> Self {
        let mut item = Self::new(Command::OpenSubmenu, command_label);
        item.submenu = Some(submenu);
        item
    }
}

/// One line of a menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    /// Dim, non-interactive (the name the menu is about).
    Heading(String),
    Separator,
    /// A horizontal strip of icon-only buttons (Windows 11's top bar).
    Icons(Vec<Item>),
    Item(Item),
}

/// What the running platform can back. Rows for anything `false` are
/// omitted rather than shown disabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// A system share sheet (Windows 10+, macOS).
    pub share: bool,
    /// A terminal can be launched in a folder.
    pub terminal: bool,
}

impl Capabilities {
    /// The running platform's capabilities.
    pub fn current() -> Self {
        Self {
            share: cfg!(windows),
            terminal: true,
        }
    }
}

/// The trash verb in the platform's own words: Windows says Delete (to the
/// Recycle Bin), macOS and Linux desktops say Move to Trash.
pub const DELETE_LABEL: &str = if cfg!(windows) {
    "Delete"
} else {
    "Move to Trash"
};

/// Facts about a file/folder row the menu was opened on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryFacts {
    /// Heading text (the item's name, or "N items").
    pub heading: String,
    /// Selected items the menu acts on (at least 1).
    pub count: usize,
    pub is_dir: bool,
    /// A search result rather than a row of the folder on screen.
    pub from_search: bool,
    /// The folder is already pinned to the sidebar's Favorites.
    pub favorite: bool,
}

/// The menu for one or more selected files/folders.
pub fn entry_menu(facts: &EntryFacts, caps: Capabilities) -> Vec<Row> {
    let single = facts.count <= 1;
    let mut rows = vec![Row::Heading(facts.heading.clone())];

    let mut icons = vec![
        Item::new(Command::Cut, "Cut").icon("icons/scissors.svg"),
        Item::new(Command::Copy, "Copy").icon("icons/copy.svg"),
    ];
    // Renaming edits a row of the folder on screen; search results reveal
    // instead.
    if single && !facts.from_search {
        icons.push(Item::new(Command::Rename, "Rename").icon("icons/pencil.svg"));
    }
    // Share sheets take files, not folders (Explorer greys it out too).
    if caps.share && !facts.is_dir {
        icons.push(Item::new(Command::Share, "Share").icon("icons/share.svg"));
    }
    icons.push(
        Item::new(Command::Delete, DELETE_LABEL)
            .icon("icons/trash-2.svg")
            .danger(),
    );
    rows.push(Row::Icons(icons));

    if single {
        rows.push(Row::Item(
            Item::new(Command::Open, "Open")
                .icon("icons/external-link.svg")
                .shortcut("open"),
        ));
        if !facts.is_dir {
            rows.push(Row::Item(
                Item::submenu("Open With", Submenu::OpenWith).icon("icons/app-window.svg"),
            ));
        }
        if facts.from_search {
            rows.push(Row::Item(
                Item::new(Command::Reveal, "Reveal in Folder").icon("icons/folder.svg"),
            ));
        }
        rows.push(Row::Separator);
    }

    if single && facts.is_dir {
        rows.push(Row::Item(if facts.favorite {
            Item::new(Command::RemoveFromFavorites, "Remove from Favorites").icon("icons/star.svg")
        } else {
            Item::new(Command::AddToFavorites, "Add to Favorites").icon("icons/star.svg")
        }));
    }
    rows.push(Row::Item(
        Item::new(Command::CompressToZip, "Compress to ZIP File").icon("icons/archive.svg"),
    ));
    rows.push(Row::Item(
        Item::new(Command::CopyAsPath, "Copy as Path")
            .icon("icons/link.svg")
            .shortcut("copy_path"),
    ));
    if single && facts.is_dir {
        rows.push(Row::Item(
            Item::new(Command::IndexFolder, "Index This Folder").icon("icons/folder-search.svg"),
        ));
        rows.push(Row::Item(
            Item::new(Command::FolderIcon, "Folder Icon…").icon("icons/palette.svg"),
        ));
    }
    if single && facts.is_dir && caps.terminal && !facts.from_search {
        rows.push(Row::Item(
            Item::new(Command::OpenInTerminal, "Open in Terminal").icon("icons/terminal.svg"),
        ));
    }
    rows.push(Row::Separator);
    rows.push(Row::Item(
        Item::new(Command::Properties, "Properties")
            .icon("icons/info.svg")
            .shortcut("properties"),
    ));
    rows
}

/// Facts about the folder whose empty space was right-clicked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackgroundFacts {
    /// Something is on Filex's file clipboard.
    pub can_paste: bool,
}

/// The menu for a folder's empty background.
pub fn background_menu(facts: BackgroundFacts, caps: Capabilities) -> Vec<Row> {
    let mut rows = vec![
        Row::Item(Item::submenu("View", Submenu::View).icon("icons/layout-grid.svg")),
        Row::Item(Item::submenu("Sort By", Submenu::SortBy).icon("icons/arrow-up-down.svg")),
        Row::Separator,
        Row::Item(Item::submenu("New", Submenu::New).icon("icons/plus.svg")),
    ];
    if facts.can_paste {
        rows.push(Row::Item(
            Item::new(Command::Paste, "Paste")
                .icon("icons/clipboard.svg")
                .shortcut("paste"),
        ));
    }
    rows.push(Row::Separator);
    if caps.terminal {
        rows.push(Row::Item(
            Item::new(Command::OpenInTerminal, "Open in Terminal").icon("icons/terminal.svg"),
        ));
    }
    rows.push(Row::Item(
        Item::new(Command::Properties, "Properties")
            .icon("icons/info.svg")
            .shortcut("properties"),
    ));
    rows
}

/// The menu for a folder pinned in the sidebar's Favorites.
pub fn favorite_menu(heading: String) -> Vec<Row> {
    vec![
        Row::Heading(heading),
        Row::Item(Item::new(Command::MoveFavorite(-1), "Move Up")),
        Row::Item(Item::new(Command::MoveFavorite(1), "Move Down")),
        Row::Separator,
        Row::Item(Item::new(Command::FolderIcon, "Folder Icon…")),
        Row::Item(
            Item::new(Command::RemoveFromFavorites, "Remove from Favorites")
                .icon("icons/star.svg")
                .danger(),
        ),
    ]
}

/// The icon choices for one folder, the current one checked.
pub fn folder_icon_menu(current: Option<FolderIcon>) -> Vec<Row> {
    let choices = [
        (None, "Automatic"),
        (Some(FolderIcon::Plain), "Plain"),
        (Some(FolderIcon::Pictures), "Pictures"),
        (Some(FolderIcon::Music), "Music"),
        (Some(FolderIcon::Videos), "Videos"),
        (Some(FolderIcon::Documents), "Documents"),
        (Some(FolderIcon::Downloads), "Downloads"),
        (Some(FolderIcon::Code), "Code"),
        (Some(FolderIcon::Archives), "Archives"),
        (Some(FolderIcon::Desktop), "Desktop"),
    ];
    std::iter::once(Row::Heading("Folder Icon".into()))
        .chain(choices.into_iter().map(|(choice, label)| {
            Row::Item(Item::new(Command::SetFolderIcon(choice), label).checked(current == choice))
        }))
        .collect()
}

/// Rows of a submenu whose contents don't need a lookup.
/// [`Submenu::OpenWith`] is built by [`open_with_rows`] instead.
pub fn static_submenu(submenu: Submenu, view: ViewMode, sort: SortSettings) -> Vec<Row> {
    match submenu {
        Submenu::View => vec![
            Row::Item(
                Item::new(Command::SetView(ViewMode::List), "List")
                    .icon("icons/list.svg")
                    .checked(view == ViewMode::List),
            ),
            Row::Item(
                Item::new(Command::SetView(ViewMode::Grid), "Grid")
                    .icon("icons/layout-grid.svg")
                    .checked(view == ViewMode::Grid),
            ),
        ],
        Submenu::SortBy => {
            let by = |key: SortBy, label: &'static str| {
                Row::Item(Item::new(Command::SetSort(key), label).checked(sort.by == key))
            };
            vec![
                by(SortBy::Name, "Name"),
                by(SortBy::Modified, "Date Modified"),
                by(SortBy::Kind, "Type"),
                by(SortBy::Size, "Size"),
                Row::Separator,
                Row::Item(
                    Item::new(Command::SetAscending(true), "Ascending").checked(sort.ascending),
                ),
                Row::Item(
                    Item::new(Command::SetAscending(false), "Descending").checked(!sort.ascending),
                ),
            ]
        }
        Submenu::New => vec![Row::Item(
            Item::new(Command::NewFolder, "Folder").icon("icons/folder.svg"),
        )],
        Submenu::OpenWith => Vec::new(),
    }
}

/// The Open With submenu: one row per app name, then the OS chooser when
/// the platform has one (`other`). `None` apps means the lookup couldn't
/// list anything for the type, so only the chooser is offered.
pub fn open_with_rows(apps: Option<&[String]>, other: Option<&'static str>) -> Vec<Row> {
    let mut rows: Vec<Row> = apps
        .unwrap_or_default()
        .iter()
        .enumerate()
        .map(|(ix, name)| Row::Item(Item::new(Command::OpenInApp(ix), name.clone())))
        .collect();
    if let Some(label) = other {
        if !rows.is_empty() {
            rows.push(Row::Separator);
        }
        rows.push(Row::Item(Item::new(Command::ChooseOtherApp, label)));
    }
    if rows.is_empty() {
        rows.push(Row::Heading("No apps found".into()));
    }
    rows
}

/// A position in a menu: `row`, and the button within an icon bar (`col`,
/// 0 for ordinary rows).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Focus {
    pub row: usize,
    pub col: usize,
}

impl Focus {
    pub fn row(row: usize) -> Self {
        Self { row, col: 0 }
    }
}

/// The item at `focus`, if it names one.
pub fn item_at(rows: &[Row], focus: Focus) -> Option<&Item> {
    match rows.get(focus.row)? {
        Row::Item(item) if focus.col == 0 => Some(item),
        Row::Icons(items) => items.get(focus.col),
        _ => None,
    }
}

fn focusable(row: &Row) -> bool {
    match row {
        Row::Item(_) => true,
        Row::Icons(items) => !items.is_empty(),
        Row::Heading(_) | Row::Separator => false,
    }
}

/// The next focusable row after (`delta` > 0) or before `from`, wrapping
/// around like Windows menus. With nothing focused, Down starts at the
/// first row and Up at the last. Landing on an icon bar focuses its first
/// button.
pub fn step(rows: &[Row], from: Option<Focus>, delta: isize) -> Option<Focus> {
    let len = rows.len();
    if len == 0 {
        return None;
    }
    let mut row = match from {
        Some(focus) => focus.row,
        None if delta >= 0 => len - 1,
        None => 0,
    };
    for _ in 0..len {
        row = (row as isize + delta.signum()).rem_euclid(len as isize) as usize;
        if focusable(&rows[row]) {
            return Some(Focus::row(row));
        }
    }
    None
}

/// Left/Right within an icon bar; `None` at either end or off an icon bar.
pub fn step_in_row(rows: &[Row], focus: Focus, delta: isize) -> Option<Focus> {
    let Some(Row::Icons(items)) = rows.get(focus.row) else {
        return None;
    };
    let col = focus.col.checked_add_signed(delta)?;
    (col < items.len()).then_some(Focus {
        row: focus.row,
        col,
    })
}

/// Pointer-rest tracking for submenu rows: a submenu opens (or an open one
/// closes) only once the pointer has stayed on a row for
/// [`SUBMENU_HOVER_DELAY`], so sweeping across the menu doesn't flash
/// submenus open. The view schedules a timer per ticket from [`hover`] and
/// applies the result of [`fire`] only if that ticket is still current.
///
/// [`hover`]: HoverIntent::hover
/// [`fire`]: HoverIntent::fire
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct HoverIntent {
    generation: u64,
    /// Row the pointer rests on; `None` for a row without a submenu.
    target: Option<Option<usize>>,
}

impl HoverIntent {
    /// The pointer entered a main-menu row: `submenu_row` is `Some(row)`
    /// when it opens a submenu. Returns a ticket to fire after the delay,
    /// or `None` when the target didn't change (no new timer needed).
    pub fn hover(&mut self, submenu_row: Option<usize>) -> Option<u64> {
        if self.target == Some(submenu_row) {
            return None;
        }
        self.target = Some(submenu_row);
        self.generation += 1;
        Some(self.generation)
    }

    /// The pointer reached the open submenu (or the keyboard took over):
    /// drop whatever was pending, so a close scheduled while crossing other
    /// rows on the way never fires.
    pub fn settle(&mut self) {
        self.target = None;
        self.generation += 1;
    }

    /// A ticket's timer elapsed: `Some(target)` if it's still current —
    /// `Some(Some(row))` opens that row's submenu, `Some(None)` closes the
    /// open one.
    pub fn fire(&mut self, ticket: u64) -> Option<Option<usize>> {
        if ticket != self.generation {
            return None;
        }
        self.target.take()
    }
}

/// Pick the row that opens `submenu` (to place its panel beside it).
pub fn submenu_row(rows: &[Row], submenu: Submenu) -> Option<usize> {
    rows.iter()
        .position(|row| matches!(row, Row::Item(item) if item.submenu == Some(submenu)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: Capabilities = Capabilities {
        share: true,
        terminal: true,
    };

    fn file(count: usize) -> EntryFacts {
        EntryFacts {
            heading: "a.txt".into(),
            count,
            is_dir: false,
            from_search: false,
            favorite: false,
        }
    }

    /// Every entry-menu row carries a glyph, so labels line up and no
    /// row reads as an indented afterthought next to its neighbours.
    #[test]
    fn every_entry_menu_row_has_an_icon() {
        let variants = [
            file(1),
            file(3),
            EntryFacts {
                is_dir: true,
                ..file(1)
            },
            EntryFacts {
                from_search: true,
                ..file(1)
            },
        ];
        for facts in variants {
            for row in entry_menu(&facts, ALL) {
                if let Row::Item(item) = row {
                    assert!(item.icon.is_some(), "{:?} has no icon", item.label);
                }
            }
        }
    }

    fn commands(rows: &[Row]) -> Vec<Command> {
        rows.iter()
            .flat_map(|row| match row {
                Row::Item(item) => vec![item.command],
                Row::Icons(items) => items.iter().map(|i| i.command).collect(),
                _ => vec![],
            })
            .collect()
    }

    fn icon_bar(rows: &[Row]) -> Vec<Command> {
        rows.iter()
            .find_map(|row| match row {
                Row::Icons(items) => Some(items.iter().map(|i| i.command).collect()),
                _ => None,
            })
            .unwrap_or_default()
    }

    #[test]
    fn file_menu_matches_the_windows_layout() {
        let rows = entry_menu(&file(1), ALL);
        assert_eq!(
            icon_bar(&rows),
            [
                Command::Cut,
                Command::Copy,
                Command::Rename,
                Command::Share,
                Command::Delete
            ]
        );
        assert_eq!(
            commands(&rows)[5..],
            [
                Command::Open,
                Command::OpenSubmenu,
                Command::CompressToZip,
                Command::CopyAsPath,
                Command::Properties,
            ]
        );
        assert_eq!(submenu_row(&rows, Submenu::OpenWith), Some(3));
    }

    #[test]
    fn folder_menu_has_favorites_and_terminal_but_no_open_with_or_share() {
        let facts = EntryFacts {
            is_dir: true,
            ..file(1)
        };
        let cmds = commands(&entry_menu(&facts, ALL));
        assert!(cmds.contains(&Command::AddToFavorites));
        assert!(cmds.contains(&Command::OpenInTerminal));
        assert!(!cmds.contains(&Command::Share));
        assert!(!cmds.contains(&Command::OpenSubmenu));

        let pinned = EntryFacts {
            favorite: true,
            ..facts
        };
        let cmds = commands(&entry_menu(&pinned, ALL));
        assert!(cmds.contains(&Command::RemoveFromFavorites));
        assert!(!cmds.contains(&Command::AddToFavorites));
    }

    #[test]
    fn multi_selection_drops_single_item_actions() {
        let cmds = commands(&entry_menu(&file(3), ALL));
        for single in [Command::Open, Command::Rename, Command::OpenSubmenu] {
            assert!(!cmds.contains(&single), "{single:?} shown for 3 items");
        }
        for multi in [
            Command::Cut,
            Command::Copy,
            Command::Delete,
            Command::CompressToZip,
            Command::CopyAsPath,
            Command::Properties,
        ] {
            assert!(cmds.contains(&multi), "{multi:?} missing for 3 items");
        }
    }

    #[test]
    fn search_results_reveal_instead_of_rename() {
        let facts = EntryFacts {
            from_search: true,
            ..file(1)
        };
        let cmds = commands(&entry_menu(&facts, ALL));
        assert!(cmds.contains(&Command::Reveal));
        assert!(!cmds.contains(&Command::Rename));
    }

    #[test]
    fn unsupported_capabilities_are_left_out() {
        let none = Capabilities {
            share: false,
            terminal: false,
        };
        let cmds = commands(&entry_menu(&file(1), none));
        assert!(!cmds.contains(&Command::Share));
        let cmds = commands(&background_menu(BackgroundFacts { can_paste: false }, none));
        assert!(!cmds.contains(&Command::OpenInTerminal));
    }

    #[test]
    fn background_menu_has_view_sort_new_and_paste_only_with_a_clipboard() {
        let rows = background_menu(BackgroundFacts { can_paste: false }, ALL);
        assert!(submenu_row(&rows, Submenu::View).is_some());
        assert!(submenu_row(&rows, Submenu::SortBy).is_some());
        assert!(submenu_row(&rows, Submenu::New).is_some());
        assert!(!commands(&rows).contains(&Command::Paste));
        let rows = background_menu(BackgroundFacts { can_paste: true }, ALL);
        assert!(commands(&rows).contains(&Command::Paste));
        assert!(commands(&rows).contains(&Command::Properties));
    }

    #[test]
    fn folder_icon_menu_checks_only_the_current_choice() {
        let rows = folder_icon_menu(Some(FolderIcon::Music));
        let checked: Vec<_> = rows
            .iter()
            .filter_map(|row| match row {
                Row::Item(item) if item.checked => Some(item.command),
                _ => None,
            })
            .collect();
        assert_eq!(checked, [Command::SetFolderIcon(Some(FolderIcon::Music))]);
        assert_eq!(commands(&folder_icon_menu(None)).len(), 10);
    }

    #[test]
    fn view_and_sort_submenus_check_the_current_choice() {
        let sort = SortSettings {
            by: SortBy::Size,
            ascending: false,
            directories_first: true,
        };
        let checked = |rows: Vec<Row>| -> Vec<Command> {
            rows.into_iter()
                .filter_map(|row| match row {
                    Row::Item(item) if item.checked => Some(item.command),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(
            checked(static_submenu(Submenu::View, ViewMode::Grid, sort)),
            [Command::SetView(ViewMode::Grid)]
        );
        assert_eq!(
            checked(static_submenu(Submenu::SortBy, ViewMode::List, sort)),
            [Command::SetSort(SortBy::Size), Command::SetAscending(false)]
        );
    }

    #[test]
    fn open_with_rows_list_apps_then_the_chooser() {
        let apps = ["Notepad".to_string(), "Code".to_string()];
        let rows = open_with_rows(Some(&apps), Some("Choose another app…"));
        assert_eq!(
            commands(&rows),
            [
                Command::OpenInApp(0),
                Command::OpenInApp(1),
                Command::ChooseOtherApp
            ]
        );
        assert_eq!(rows[2], Row::Separator);
        // Lookup failed: just the chooser, no stray separator.
        assert_eq!(
            commands(&open_with_rows(None, Some("Choose…"))),
            [Command::ChooseOtherApp]
        );
        // Nothing at all (Linux with no handler): a dim placeholder.
        let empty = open_with_rows(Some(&[]), None);
        assert!(matches!(empty.as_slice(), [Row::Heading(_)]));
        assert_eq!(step(&empty, None, 1), None);
    }

    #[test]
    fn step_skips_headings_and_separators_and_wraps() {
        let rows = entry_menu(&file(1), ALL);
        // Heading at 0, icon bar at 1.
        let first = step(&rows, None, 1).unwrap();
        assert_eq!(first, Focus::row(1));
        let open = step(&rows, Some(first), 1).unwrap();
        assert_eq!(item_at(&rows, open).unwrap().command, Command::Open);
        let last = step(&rows, None, -1).unwrap();
        assert_eq!(item_at(&rows, last).unwrap().command, Command::Properties);
        // Wraps past the end (and over the heading) back to the icon bar.
        assert_eq!(step(&rows, Some(last), 1), Some(first));
        assert_eq!(step(&rows, Some(first), -1), Some(last));
    }

    #[test]
    fn left_right_move_within_the_icon_bar_only() {
        let rows = entry_menu(&file(1), ALL);
        let bar = Focus::row(1);
        let copy = step_in_row(&rows, bar, 1).unwrap();
        assert_eq!(item_at(&rows, copy).unwrap().command, Command::Copy);
        assert_eq!(step_in_row(&rows, bar, -1), None);
        let delete = Focus { row: 1, col: 4 };
        assert_eq!(item_at(&rows, delete).unwrap().command, Command::Delete);
        assert_eq!(step_in_row(&rows, delete, 1), None);
        // Ordinary rows don't move sideways (Right opens submenus there).
        assert_eq!(step_in_row(&rows, Focus::row(2), 1), None);
    }

    #[test]
    fn hover_intent_opens_after_rest_and_ignores_stale_timers() {
        let mut hover = HoverIntent::default();
        let open = hover.hover(Some(3)).unwrap();
        // Resting on the same row again schedules nothing new.
        assert_eq!(hover.hover(Some(3)), None);
        assert_eq!(hover.fire(open), Some(Some(3)));
        // Already fired: a duplicate timer is a no-op.
        assert_eq!(hover.fire(open), None);
    }

    #[test]
    fn sweeping_past_a_submenu_row_never_opens_it() {
        let mut hover = HoverIntent::default();
        let stale = hover.hover(Some(3)).unwrap();
        let close = hover.hover(None).unwrap();
        assert_eq!(hover.fire(stale), None);
        assert_eq!(hover.fire(close), Some(None));
    }

    #[test]
    fn reaching_the_submenu_cancels_a_pending_close() {
        let mut hover = HoverIntent::default();
        let open = hover.hover(Some(3)).unwrap();
        assert_eq!(hover.fire(open), Some(Some(3)));
        // Crossing a plain row on the way to the open submenu...
        let close = hover.hover(None).unwrap();
        // ...then arriving in it.
        hover.settle();
        assert_eq!(hover.fire(close), None);
        // Coming back to the parent row from the submenu re-arms normally.
        assert!(hover.hover(Some(3)).is_some());
    }
}

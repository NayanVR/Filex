//! Magic mode — a command-shaped query becomes a reviewable plan of file
//! operations. See `docs/design-magic-mode.md`.
//!
//! `delete screenshots older than 30 days` parses into a [`Command`]
//! (verb + [`Selection`] + [`Action`]); resolving it against the files
//! the search matched produces a [`Plan`] the UI shows for review.
//! Strictly rule-based — no model. The selection half runs through
//! [`parse_query`] and [`phrases::expand`], the same two passes the
//! search bar uses.
//!
//! Three invariants: incomplete input produces `None` rather than a
//! best-guess plan; nothing here executes (plans go through
//! [`apply_with_progress`](crate::ops::apply_with_progress), so Ctrl+Z
//! undoes them); a plan over [`MAX_PLAN_OPS`] is refused. Destination
//! conflicts are [`crate::ops`]'s job, not duplicated here.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::ops::FileOp;
use crate::search::filter::{Filter, parse_query};
use crate::search::phrases;
use crate::tags::{Tag, TagColor};

mod intent;
mod natural;
pub use intent::{
    AppCommand, Go, Intent, Question, Suggestion, SuggestionAction, interpret, suggest,
};
pub use natural::{Understood, did_you_mean, name_distance, understand};

/// The most operations one Magic plan may contain. The bound is about
/// *review* — nobody reads ten thousand rows before clicking confirm.
pub const MAX_PLAN_OPS: usize = 1_000;

/// The recognized verbs. Each maps onto an existing [`FileOp`] variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    Delete,
    Move,
    Copy,
    Rename,
    /// Pack the matches into one ZIP ([`crate::archive`]).
    Compress,
    Tag,
    Untag,
    /// Sort a folder's files into subfolders (a create-folder + move plan).
    Organize,
}

impl Verb {
    /// Present-tense label for the confirm button and card heading.
    pub fn label(self) -> &'static str {
        match self {
            Self::Delete => "Delete",
            Self::Move => "Move",
            Self::Copy => "Copy",
            Self::Rename => "Rename",
            Self::Compress => "Compress",
            Self::Tag => "Tag",
            Self::Untag => "Untag",
            Self::Organize => "Organize",
        }
    }

    /// Past-tense for the completion notice ("deleted 3 items").
    pub fn past_tense(self) -> &'static str {
        match self {
            Self::Delete => "deleted",
            Self::Move => "moved",
            Self::Copy => "copied",
            Self::Rename => "renamed",
            Self::Compress => "compressed",
            Self::Tag => "tagged",
            Self::Untag => "untagged",
            Self::Organize => "organized",
        }
    }
}

/// Which files a command targets, in the shape [`crate::search`]'s search
/// already takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    /// The target words verbatim — what the card echoes back.
    pub source: String,
    /// Residual filename text to match (may be empty).
    pub text: String,
    /// Filters from `key:value` tokens and phrases alike. All AND.
    pub filters: Vec<Filter>,
    /// `from Downloads` / `in Archive`: only files under this folder.
    /// Resolved like a destination, then used as the search scope.
    pub within: Option<Destination>,
    /// "selected" / "these": the files selected in the list. No search
    /// runs; the plan is built from the selection itself.
    pub picked: bool,
}

/// Where a move or copy is headed, as spelled. [`resolve_destination`]
/// turns it into a real directory later, keeping the parse pure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Destination {
    /// The cleaned-up folder name ("documents", "project archive").
    pub name: String,
    /// `to new folder Archive`: make it in the folder on screen when it
    /// isn't there yet, instead of refusing.
    pub create: bool,
}

/// What the command does to each matched file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Delete,
    Move(Destination),
    Copy(Destination),
    Rename(NamePattern),
    Compress,
    Tag(Tag),
    /// Remove one tag by name, or every tag.
    Untag(Option<String>),
    Organize(OrganizeBy),
}

/// What an `organize` plan names its subfolders after.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrganizeBy {
    /// `Images`, `Documents`, … ([`FileKind`]).
    Kind,
    /// `PDF`, `PNG`, …
    Extension,
    /// `2026-11`, from the modified time.
    Month,
    /// `2026`.
    Year,
}

/// What a move or copy does when the destination already holds a file of
/// that name. Two matches sharing a name always keep both, whatever this
/// says — "replace" between them would silently drop one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConflictPolicy {
    /// Retarget to the first free `name 2` variant.
    #[default]
    KeepBoth,
    /// Leave that file where it is.
    Skip,
    /// Trash the existing file first (undo brings it back).
    Replace,
}

/// A parsed natural-language command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub verb: Verb,
    pub selection: Selection,
    pub action: Action,
}

/// A resolved, reviewable batch of operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub verb: Verb,
    /// One op per file that has something to do; files already in the
    /// requested state are absent (see [`build`]).
    pub ops: Vec<FileOp>,
    /// How many matches were left out for that reason, so the card can
    /// account for the difference against what the search found.
    pub skipped: usize,
    /// Folders to make before the ops run (`to new folder X`, organize's
    /// buckets). Not ops themselves: a move can't be kept while the
    /// folder it lands in is unchecked.
    pub creates: Vec<PathBuf>,
    /// Matches whose name is already taken in the destination — what
    /// [`ConflictPolicy`] decided for. Zero hides the policy control.
    pub conflicts: usize,
    /// The files a compress or tag plan acts on. Those verbs have no
    /// per-file [`FileOp`], so `ops` is empty and this is the review list.
    pub targets: Vec<PathBuf>,
    /// Where a compress plan writes its ZIP.
    pub archive: Option<PathBuf>,
}

impl Plan {
    /// Rows in the review list.
    pub fn len(&self) -> usize {
        self.ops.len() + self.targets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The file row `ix` acts on.
    pub fn source(&self, ix: usize) -> Option<&Path> {
        match self.ops.get(ix) {
            Some(op) => Some(op.source()),
            None => self.targets.get(ix - self.ops.len()).map(PathBuf::as_path),
        }
    }
}

/// Why a parsed command could not become a plan. Each suppresses the
/// card; the variants let the UI say *why*.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    /// The selection matched no files.
    NoMatches,
    /// The command is about "selected" files and none are.
    NothingSelected,
    /// More matches than [`MAX_PLAN_OPS`].
    TooMany(usize),
    /// Every match was already in the requested state.
    NothingToDo,
    /// No folder by that name in the current directory or among the
    /// user's known folders.
    UnknownDestination(String),
    /// A pattern with no `{n}`/`{name}` renders one name for every file.
    PatternNotUnique,
    /// A pattern rendered an unusable name (empty, or a path separator).
    InvalidName(String),
    /// Two files in the plan would land on the same path.
    Collision(PathBuf),
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoMatches => write!(f, "nothing matched"),
            Self::NothingSelected => {
                write!(f, "nothing is selected — select files in the list first")
            }
            // Not "{count} matched": callers fetch exactly one past the
            // cap, so the count is always MAX_PLAN_OPS + 1.
            Self::TooMany(_) => write!(
                f,
                "more than {MAX_PLAN_OPS} files matched — narrow the search to review them"
            ),
            Self::NothingToDo => write!(f, "every match is already where it should be"),
            Self::UnknownDestination(name) => write!(f, "no folder called {name:?}"),
            Self::PatternNotUnique => {
                write!(f, "the new name needs {{n}} or {{name}} to differ per file")
            }
            Self::InvalidName(name) => write!(f, "{name:?} is not a valid file name"),
            Self::Collision(path) => {
                write!(f, "two files would both become {}", path.display())
            }
        }
    }
}

impl std::error::Error for PlanError {}

// ---------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------

/// Read `raw` as a command with the delete gate **on** — the strict
/// reading that decides whether a query typed in *normal* mode flips the
/// app into magic mode. `None` means "not a command". Once the user has
/// explicitly toggled magic mode on, use [`parse_with_gate`]`(.., false)`.
///
/// `now` (unix seconds) anchors relative dates, keeping the parse pure.
pub fn parse(raw: &str, now: i64) -> Option<Command> {
    parse_with_gate(raw, now, true)
}

/// [`parse`], with control over how strict the reading is — the only
/// difference between the two entry paths into magic mode.
///
/// `auto = true` is the reading that may flip a normal search into magic
/// view by itself, so it accepts only what was measured safe against real
/// filenames (`tests/magic_false_positives.rs`): move/copy/rename, and a
/// delete that carries a filter. `auto = false` (the user asked for magic
/// mode) takes them at their word: any delete, the newer verbs (compress,
/// tag, untag, organize) and "selected" as a target. See
/// `docs/design-magic-mode.md` §"Where the delete gate goes".
pub fn parse_with_gate(raw: &str, now: i64, auto: bool) -> Option<Command> {
    let words: Vec<&str> = raw.split_whitespace().collect();
    let (first, rest) = words.split_first()?;
    let mut verb = verb_of(&first.to_ascii_lowercase())?;
    let mut rest = strip_quantifier(rest);
    // "remove tag Work from invoices" — a delete verb, but of a tag.
    let mut tag_named = None;
    if verb == Verb::Delete
        && rest.first().is_some_and(|w| w.eq_ignore_ascii_case("tag"))
        && let Some(at) = rest.iter().position(|w| w.eq_ignore_ascii_case("from"))
        && at > 1
    {
        tag_named = Some(rest[1..at].join(" "));
        rest = strip_quantifier(&rest[at + 1..]);
        verb = Verb::Untag;
    }
    if auto && !matches!(verb, Verb::Delete | Verb::Move | Verb::Copy | Verb::Rename) {
        return None;
    }
    if rest.is_empty() && verb != Verb::Organize {
        return None;
    }

    let (target, action) = match verb {
        Verb::Delete => (rest, Action::Delete),
        Verb::Compress => (rest, Action::Compress),
        Verb::Untag => (rest, Action::Untag(tag_named)),
        Verb::Organize => match rest.iter().rposition(|w| w.eq_ignore_ascii_case("by")) {
            Some(at) => (&rest[..at], Action::Organize(organize_by(&rest[at + 1..])?)),
            None => (rest, Action::Organize(OrganizeBy::Kind)),
        },
        Verb::Move | Verb::Copy | Verb::Rename | Verb::Tag => {
            // Last separator wins, so a folder named "to" in the target
            // half can't steal the split.
            let at = rest.iter().rposition(|word| is_separator(word, verb))?;
            let (target, tail) = (&rest[..at], &rest[at + 1..]);
            if target.is_empty() || tail.is_empty() {
                return None;
            }
            let action = match verb {
                Verb::Move => Action::Move(parse_destination(tail)?),
                Verb::Copy => Action::Copy(parse_destination(tail)?),
                Verb::Rename => Action::Rename(NamePattern::parse(&tail.join(" "))?),
                _ => Action::Tag(tag_from(tail)),
            };
            (target, action)
        }
    };

    let selection = if verb == Verb::Organize {
        organize_target(target)
    } else {
        selection_from(target, now, !auto, false)?
    };
    if auto && !clears_delete_gate(verb, &selection) {
        return None;
    }
    Some(Command {
        verb,
        selection,
        action,
    })
}

/// The extra bar a **delete** command must clear: at least one structured
/// filter, not just free text. Move/copy/rename need a `to`/`into`
/// separator *and* a resolvable destination, which is already an
/// effectively perfect gate; delete has no separator, so
/// `delete <anything>` parses. Filter vocabulary is how a person
/// describes a **set**, where bare text names a **thing**.
///
/// The cost is deliberate: `delete old logs` shows no card. Measurements
/// and the corpus live in `docs/design-magic-mode.md` §"False positives"
/// and `tests/magic_false_positives.rs`.
fn clears_delete_gate(verb: Verb, selection: &Selection) -> bool {
    verb != Verb::Delete || !selection.filters.is_empty()
}

fn verb_of(word: &str) -> Option<Verb> {
    Some(match word {
        "delete" | "remove" | "trash" => Verb::Delete,
        "move" => Verb::Move,
        "copy" => Verb::Copy,
        "rename" => Verb::Rename,
        "compress" | "zip" | "archive" => Verb::Compress,
        "tag" | "label" => Verb::Tag,
        "untag" => Verb::Untag,
        "organize" | "organise" | "tidy" | "sort" => Verb::Organize,
        _ => return None,
    })
}

/// The word separating target from destination. `into` reads naturally
/// for move/copy but not rename, where only `to` is English.
fn is_separator(word: &str, verb: Verb) -> bool {
    let word = word.to_ascii_lowercase();
    match verb {
        Verb::Move | Verb::Copy => word == "to" || word == "into",
        Verb::Rename => word == "to",
        Verb::Tag => word == "as" || word == "with",
        _ => false,
    }
}

/// Leading quantifiers to drop: "delete **all** screenshots". Here rather
/// than in [`crate::search::phrases`]'s filler because they are command grammar —
/// a plain search for `all hands notes` must keep its "all".
const QUANTIFIERS: &[&str] = &["all", "every", "any", "my", "the"];

fn strip_quantifier<'a, 'b>(words: &'a [&'b str]) -> &'a [&'b str] {
    match words.split_first() {
        Some((first, rest)) if QUANTIFIERS.contains(&first.to_ascii_lowercase().as_str()) => rest,
        _ => words,
    }
}

/// Targets that mean "what I have selected" rather than describing files.
const PICKED: &[&str] = &[
    "selected",
    "selection",
    "these",
    "them",
    "it",
    "this",
    "those",
    "that",
    "this file",
    "these ones",
    "selected ones",
    "what i selected",
    "everything selected",
    "selected files",
    "selected items",
    "these files",
    "these items",
];

/// Targets that mean "whatever is there": no name to match on.
const EVERYTHING: &[&str] = &["everything", "anything", "files", "items", "stuff", "*"];

/// Run the target words through the ordinary search parse, so a command's
/// target supports exactly the search bar's vocabulary. `None` when
/// nothing survives — the "verb with no target" case. `explicit` allows
/// "selected"; `allow_all` lets a question be about every file.
pub(crate) fn selection_from(
    words: &[&str],
    now: i64,
    explicit: bool,
    allow_all: bool,
) -> Option<Selection> {
    let source = words.join(" ");
    if explicit && PICKED.contains(&source.to_ascii_lowercase().as_str()) {
        return Some(Selection {
            source,
            text: String::new(),
            filters: Vec::new(),
            within: None,
            picked: true,
        });
    }
    let (words, within) = split_within(words, now);
    let parsed = parse_query(&words.join(" "), now);
    // `expand_as_description`, not `expand`: the verb already established
    // these words describe a set, so the single-word filename rule must
    // not apply — else `delete screenshots` means files *named* that.
    let expansion = phrases::expand_as_description(&parsed.text, now);

    let mut filters: Vec<Filter> = Vec::new();
    for filter in parsed.filters.into_iter().chain(expansion.filters()) {
        if !filters.contains(&filter) {
            filters.push(filter);
        }
    }
    let mut text = expansion.text;
    let everything = EVERYTHING.contains(&text.to_ascii_lowercase().as_str());
    if everything && (within.is_some() || allow_all || !filters.is_empty()) {
        text.clear();
    }
    if text.is_empty() && filters.is_empty() && within.is_none() && !(allow_all && everything) {
        return None;
    }
    Some(Selection {
        source,
        text,
        filters,
        within,
        picked: false,
    })
}

/// Split a trailing `from <folder>` / `in <folder>` off the target words.
/// "photos from last week" keeps its words: a tail that reads as a date is
/// a date, not a place.
fn split_within<'a, 'b>(words: &'a [&'b str], now: i64) -> (&'a [&'b str], Option<Destination>) {
    let Some(at) = words
        .iter()
        .rposition(|word| matches!(word.to_ascii_lowercase().as_str(), "from" | "in" | "inside"))
    else {
        return (words, None);
    };
    let tail = &words[at + 1..];
    if tail.iter().any(|word| word.contains(':')) {
        return (words, None);
    }
    let dated = phrases::expand_as_description(&tail.join(" "), now)
        .filters()
        .iter()
        .any(|filter| matches!(filter, Filter::Modified(_)));
    match parse_destination(tail) {
        Some(dest) if !dated => (
            &words[..at],
            Some(Destination {
                create: false,
                ..dest
            }),
        ),
        _ => (words, None),
    }
}

/// What an `organize` command sorts: the selection, a named folder, or
/// (nothing / "here" / "this folder") the folder on screen.
fn organize_target(words: &[&str]) -> Selection {
    let source = words.join(" ");
    // "everything here", "all the files in Downloads": the folder is
    // what's left once the words for "its contents" are gone.
    let mut words = words;
    while let Some((first, rest)) = words.split_first()
        && matches!(
            first.to_ascii_lowercase().as_str(),
            "everything" | "all" | "the" | "my" | "files" | "items" | "stuff" | "in" | "inside"
        )
    {
        words = rest;
    }
    let lower = words.join(" ").to_ascii_lowercase();
    let here = [
        "",
        "here",
        "this",
        "this folder",
        "this directory",
        "current folder",
    ];
    Selection {
        text: String::new(),
        filters: Vec::new(),
        picked: PICKED.contains(&lower.as_str()),
        within: if here.contains(&lower.as_str()) || PICKED.contains(&lower.as_str()) {
            None
        } else {
            parse_destination(words)
        },
        source,
    }
}

fn organize_by(words: &[&str]) -> Option<OrganizeBy> {
    Some(match words.join(" ").to_ascii_lowercase().as_str() {
        "type" | "kind" | "file type" => OrganizeBy::Kind,
        "extension" | "ext" => OrganizeBy::Extension,
        "month" | "date" => OrganizeBy::Month,
        "year" => OrganizeBy::Year,
        _ => return None,
    })
}

/// A tag from the words after `as`: `red` is Finder's red "Red" tag,
/// `red urgent` a red tag named "urgent", anything else a plain name.
fn tag_from(words: &[&str]) -> Tag {
    let color = |word: &str| {
        Some(match word.to_ascii_lowercase().as_str() {
            "grey" | "gray" => TagColor::Grey,
            "green" => TagColor::Green,
            "purple" => TagColor::Purple,
            "blue" => TagColor::Blue,
            "yellow" => TagColor::Yellow,
            "red" => TagColor::Red,
            "orange" => TagColor::Orange,
            _ => return None,
        })
    };
    match (color(words[0]), words) {
        (Some(color), [only]) => {
            let mut name = only.to_ascii_lowercase();
            name[..1].make_ascii_uppercase();
            Tag::colored(name, color)
        }
        (Some(color), [_, rest @ ..]) => Tag::colored(rest.join(" "), color),
        _ => Tag::new(words.join(" ")),
    }
}

/// Clean a destination phrase to a folder name: drop leading filler and a
/// trailing "folder"/"directory". `new folder X` (optionally `a new
/// folder called X`) asks for it to be made. `None` if nothing is left.
fn parse_destination(words: &[&str]) -> Option<Destination> {
    let is = |word: &&str, options: &[&str]| options.contains(&word.to_ascii_lowercase().as_str());
    let mut words = strip_quantifier(words);
    if words.first().is_some_and(|w| is(w, &["a"])) && words.len() > 1 {
        words = &words[1..];
    }
    if let [new, folder, name @ ..] = words
        && is(new, &["new"])
        && is(folder, &["folder", "directory"])
    {
        let name = match name.split_first() {
            Some((first, rest)) if is(first, &["called", "named"]) => rest,
            _ => name,
        };
        // No name left: a folder literally called "new", handled below.
        if !name.is_empty() {
            return Some(Destination {
                name: name.join(" "),
                create: true,
            });
        }
    }
    // "a folder called Trip": that folder, made if it isn't there.
    if let [folder, called, name @ ..] = words
        && is(folder, &["folder", "directory"])
        && is(called, &["called", "named"])
        && !name.is_empty()
    {
        return Some(Destination {
            name: name.join(" "),
            create: true,
        });
    }
    while let Some((last, rest)) = words.split_last() {
        if is(last, &["folder", "directory"]) {
            words = rest;
        } else {
            break;
        }
    }
    if words.is_empty() {
        return None;
    }
    Some(Destination {
        name: words.join(" "),
        create: false,
    })
}

// ---------------------------------------------------------------------
// Rename patterns
// ---------------------------------------------------------------------

/// A batch rename's new-name template: literal text interleaved with
/// `{n}` (position), `{name}` (original stem) and `{ext}` (extension, no
/// dot). `{n}` is zero-padded to the batch size's width (5 files →
/// `1..5`, 120 files → `001..120`) so results sort in numbered order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamePattern {
    segments: Vec<Segment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    Literal(String),
    Index,
    Stem,
    Ext,
}

impl NamePattern {
    /// Parse a pattern. `None` on an unclosed brace or an unrecognized
    /// placeholder — an unknown `{whatever}` is far likelier a typo than a
    /// request for literal braces, and guessing wrong renames a hundred
    /// files to `shot-{nmae}.png`.
    pub fn parse(text: &str) -> Option<Self> {
        let mut segments = Vec::new();
        let mut literal = String::new();
        let mut rest = text;

        while let Some(open) = rest.find('{') {
            let close = open + rest[open..].find('}')?;
            literal.push_str(&rest[..open]);
            let segment = match rest[open + 1..close].trim().to_ascii_lowercase().as_str() {
                "n" => Segment::Index,
                "name" => Segment::Stem,
                "ext" => Segment::Ext,
                _ => return None,
            };
            if !literal.is_empty() {
                segments.push(Segment::Literal(std::mem::take(&mut literal)));
            }
            segments.push(segment);
            rest = &rest[close + 1..];
        }
        literal.push_str(rest);
        // A closing brace with no opener is as malformed as the reverse.
        if literal.contains('}') {
            return None;
        }
        if !literal.is_empty() {
            segments.push(Segment::Literal(literal));
        }
        (!segments.is_empty()).then_some(Self { segments })
    }

    /// Does this render a different name per file? Pure literals collapse
    /// every match onto one name, which only works for a single file.
    pub fn varies_per_file(&self) -> bool {
        self.segments
            .iter()
            .any(|s| matches!(s, Segment::Index | Segment::Stem))
    }

    /// The name `original` takes as entry `index` (1-based) of a batch
    /// whose counter is `width` digits wide.
    fn render(&self, original: &Path, index: usize, width: usize) -> String {
        let mut out = String::new();
        for segment in &self.segments {
            match segment {
                Segment::Literal(text) => out.push_str(text),
                Segment::Index => out.push_str(&format!("{index:0width$}")),
                Segment::Stem => {
                    out.push_str(&original.file_stem().unwrap_or_default().to_string_lossy());
                }
                Segment::Ext => {
                    out.push_str(&original.extension().unwrap_or_default().to_string_lossy());
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------------
// Destination resolution
// ---------------------------------------------------------------------

/// The user's well-known folders, by the names a person calls them.
/// Injected rather than read at use time so plan building stays
/// deterministic and testable against a tempdir.
#[derive(Debug, Clone, Default)]
pub struct UserDirs {
    by_name: HashMap<String, PathBuf>,
}

impl UserDirs {
    /// Read the real ones from the OS. `dirs` resolves these per platform
    /// (XDG, `NSSearchPathForDirectory`, Known Folder API), so nothing
    /// here is hard-coded to one layout. Missing folders are absent, so a
    /// destination naming one reports
    /// [`PlanError::UnknownDestination`] rather than inventing a path.
    ///
    /// Does environment lookups; call at startup, not per keystroke.
    pub fn from_os() -> Self {
        let sources: [(&[&str], Option<PathBuf>); 7] = [
            (&["documents", "docs"], dirs::document_dir()),
            (&["downloads", "download"], dirs::download_dir()),
            (&["desktop"], dirs::desktop_dir()),
            (&["pictures", "photos"], dirs::picture_dir()),
            (&["music"], dirs::audio_dir()),
            (&["videos", "movies"], dirs::video_dir()),
            (&["home"], dirs::home_dir()),
        ];
        let mut by_name = HashMap::new();
        for (names, dir) in sources {
            if let Some(dir) = dir {
                for name in names {
                    by_name.insert((*name).to_string(), dir.clone());
                }
            }
        }
        Self { by_name }
    }

    /// Register a folder under `name` (lowercased).
    pub fn insert(&mut self, name: impl Into<String>, path: impl Into<PathBuf>) {
        self.by_name
            .insert(name.into().to_ascii_lowercase(), path.into());
    }

    /// Look up a cached well-known folder without querying the OS again.
    pub fn get(&self, name: &str) -> Option<&Path> {
        self.by_name
            .get(&name.to_ascii_lowercase())
            .map(PathBuf::as_path)
    }
}

/// What a plan needs from the world beyond the command itself.
#[derive(Debug, Clone, Copy)]
pub struct PlanContext<'a> {
    /// The folder being browsed; destinations resolve here first.
    pub cwd: &'a Path,
    pub dirs: &'a UserDirs,
    pub conflicts: ConflictPolicy,
}

/// Turn a spelled destination into a real directory. A subfolder of the
/// folder on screen wins over a same-named known folder ("move these to
/// Archive" means the one you're looking at); known folders are the
/// fallback, which is what makes "to Documents" work from anywhere. A
/// typed path (`~/Desktop/old`, `/tmp`) names itself.
///
/// The current-folder lookup scans the directory preferring an exact-case
/// hit, rather than probing `cwd.join(name)`. A join probe succeeds on
/// case-insensitive Windows/macOS and hands back the user's spelling
/// while resolving nothing on Linux — one command, a different
/// destination per OS. Scanning yields the true on-disk name everywhere.
///
/// A `create` destination that isn't there yet resolves to where it will
/// be made — inside the folder on screen — so the result may not exist.
pub fn resolve_destination(dest: &Destination, ctx: &PlanContext) -> Result<PathBuf, PlanError> {
    let unknown = || PlanError::UnknownDestination(dest.name.clone());
    if let Some(path) = typed_path(&dest.name, ctx.dirs) {
        return path.is_dir().then_some(path).ok_or_else(unknown);
    }
    if let Some(dir) = destination_in(ctx.cwd, &dest.name) {
        return Ok(dir);
    }
    // "from here", "in this folder": the folder on screen — unless a
    // folder really called that is in it (above).
    if !dest.create
        && ctx.cwd.is_absolute()
        && [
            "here",
            "this",
            "current",
            "this folder",
            "current folder",
            "the current folder",
            "this directory",
        ]
        .contains(&dest.name.to_ascii_lowercase().as_str())
    {
        return Ok(ctx.cwd.to_path_buf());
    }
    if dest.create {
        if !ctx.cwd.is_absolute() {
            return Err(unknown());
        }
        if !is_valid_file_name(&dest.name) {
            return Err(PlanError::InvalidName(dest.name.clone()));
        }
        return Ok(ctx.cwd.join(&dest.name));
    }
    if let Some(path) = ctx.dirs.get(&dest.name).filter(|path| path.is_dir()) {
        return Ok(path.to_path_buf());
    }
    // A near miss ("documnets", "Donwloads"): one folder here or among
    // the well-known ones is a single slip away. The plan shows the real
    // path, so the fix is visible before anything moves.
    let here = if ctx.cwd.is_absolute() {
        names_in(ctx.cwd)
    } else {
        HashSet::new()
    };
    let candidates = here
        .iter()
        .map(String::as_str)
        .chain(ctx.dirs.by_name.keys().map(String::as_str));
    let near = natural::nearest(&dest.name, candidates).ok_or_else(unknown)?;
    destination_in(ctx.cwd, near)
        .or_else(|| {
            ctx.dirs
                .get(near)
                .filter(|path| path.is_dir())
                .map(Path::to_path_buf)
        })
        .ok_or_else(unknown)
}

/// `name` as a path, when it is spelled like one: absolute, or under `~`.
pub fn typed_path(name: &str, dirs: &UserDirs) -> Option<PathBuf> {
    if name == "~" {
        return dirs.get("home").map(Path::to_path_buf);
    }
    if let Some(rest) = name.strip_prefix("~/").or_else(|| name.strip_prefix("~\\")) {
        return dirs.get("home").map(|home| home.join(rest));
    }
    let path = Path::new(name);
    // `has_root`, not `is_absolute`: on Windows `\Users` and `/x` have a
    // root but no drive, and are still a typed path, not a name to search.
    path.has_root().then(|| path.to_path_buf())
}

/// The folder `name` inside `cwd`, exact case first. A relative `cwd`
/// (the This PC page, which isn't a folder) has nothing inside it: a
/// lookup would resolve against the process's working directory.
fn destination_in(cwd: &Path, name: &str) -> Option<PathBuf> {
    if !cwd.is_absolute() {
        return None;
    }
    let mut case_insensitive: Option<PathBuf> = None;
    if let Ok(entries) = std::fs::read_dir(cwd) {
        for entry in entries.flatten() {
            let Some(entry_name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            // Compare names before stat'ing — no metadata call per child.
            let exact = entry_name == name;
            if (!exact && !entry_name.eq_ignore_ascii_case(name)) || !entry.path().is_dir() {
                continue;
            }
            if exact {
                return Some(entry.path());
            }
            case_insensitive.get_or_insert_with(|| entry.path());
        }
    }
    if case_insensitive.is_some() {
        return case_insensitive;
    }
    // A multi-segment destination isn't one directory entry.
    let nested = cwd.join(name);
    nested.is_dir().then_some(nested)
}

// ---------------------------------------------------------------------
// Plan building
// ---------------------------------------------------------------------

/// Resolve `command` against the files it targets — what the search
/// matched, the selection, or (organize) a folder's own files.
///
/// `matches` is sorted by path first, so a plan — and a rename's `{n}`
/// numbering in particular — depends only on *which* files matched, not
/// the order search returned them. Files with nothing to do are dropped
/// rather than becoming ops that fail at apply time; [`Plan::skipped`]
/// counts them.
pub fn build(command: &Command, matches: &[PathBuf], ctx: &PlanContext) -> Result<Plan, PlanError> {
    if matches.is_empty() {
        return Err(PlanError::NoMatches);
    }
    if matches.len() > MAX_PLAN_OPS {
        return Err(PlanError::TooMany(matches.len()));
    }
    let mut matches = matches.to_vec();
    matches.sort();

    let mut plan = Plan {
        verb: command.verb,
        ops: Vec::new(),
        skipped: 0,
        creates: Vec::new(),
        conflicts: 0,
        targets: Vec::new(),
        archive: None,
    };
    match &command.action {
        Action::Delete => {
            plan.ops = matches
                .iter()
                .map(|path| FileOp::Delete { path: path.clone() })
                .collect();
        }
        Action::Move(dest) => transfer_ops(&matches, dest, ctx, true, &mut plan)?,
        Action::Copy(dest) => transfer_ops(&matches, dest, ctx, false, &mut plan)?,
        Action::Rename(pattern) => {
            plan.ops = rename_ops(&matches, pattern)?;
            plan.skipped = matches.len() - plan.ops.len();
        }
        Action::Compress => {
            // A ZIP stores each item under its own name at the root, so
            // two matches sharing a name can't both go in.
            let mut names = HashSet::new();
            plan.targets = matches
                .iter()
                .filter(|path| path.file_name().is_some_and(|name| names.insert(name)))
                .cloned()
                .collect();
            plan.skipped = matches.len() - plan.targets.len();
            plan.archive = archive_dest(&plan.targets, ctx);
        }
        Action::Tag(_) | Action::Untag(_) => plan.targets = matches.clone(),
        Action::Organize(by) => organize_ops(&matches, *by, &mut plan)?,
    }

    if plan.is_empty() {
        return Err(PlanError::NothingToDo);
    }
    check_collisions(&plan.ops)?;
    Ok(plan)
}

/// Where a compress plan writes: beside the files when they share a
/// folder, else the folder on screen. Named after a lone item, `Archive`
/// otherwise. Execution retargets an occupied name.
fn archive_dest(targets: &[PathBuf], ctx: &PlanContext) -> Option<PathBuf> {
    let first = targets.first()?;
    let parent = first.parent()?;
    let together = targets.iter().all(|path| path.parent() == Some(parent));
    let dir = if together || !ctx.cwd.is_absolute() {
        parent
    } else {
        ctx.cwd
    };
    let stem = match targets {
        [only] => only.file_stem()?.to_string_lossy().into_owned(),
        _ => "Archive".to_owned(),
    };
    Some(dir.join(format!("{stem}.zip")))
}

/// The names already in `dir` — one `read_dir`, so name clashes are
/// answered in memory instead of with a stat per file. Empty when `dir`
/// doesn't exist yet.
fn names_in(dir: &Path) -> HashSet<String> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
        .collect()
}

/// Move/copy operations into a resolved destination directory.
///
/// Skipped rather than planned: a file already in the destination, and a
/// directory that *contains* the destination (moving a folder inside
/// itself) — so one degenerate match doesn't void a fine plan.
///
/// **Collisions are resolved, not refused**, per [`ConflictPolicy`]: by
/// default a taken name retargets to the first free `name 2` variant, as
/// the paste and drag paths already do, so the preview shows the real
/// outcome.
fn transfer_ops(
    matches: &[PathBuf],
    dest: &Destination,
    ctx: &PlanContext,
    moving: bool,
    plan: &mut Plan,
) -> Result<(), PlanError> {
    let dir = resolve_destination(dest, ctx)?;
    if !dir.is_dir() {
        plan.creates.push(dir.clone());
    }
    let on_disk = names_in(&dir);
    // Names earlier ops in this plan claimed.
    let mut claimed: HashSet<String> = HashSet::new();
    for from in matches {
        // Anywhere under the destination is already there: "move pdfs to
        // Downloads" must not pull Downloads/Taxes/*.pdf up a level.
        if from.starts_with(&dir) || dir.starts_with(from) {
            plan.skipped += 1;
            continue;
        }
        let Some(name) = from.file_name().and_then(|n| n.to_str()) else {
            return Err(PlanError::InvalidName(from.display().to_string()));
        };
        let clash = on_disk.contains(name);
        plan.conflicts += usize::from(clash);
        let free = match ctx.conflicts {
            ConflictPolicy::Skip if clash => continue,
            ConflictPolicy::Replace if clash && !claimed.contains(name) => name.to_owned(),
            _ => free_variant(name, |n| on_disk.contains(n) || claimed.contains(n)),
        };
        claimed.insert(free.clone());
        let to = dir.join(&free);
        plan.ops.push(if moving {
            FileOp::Move {
                from: from.clone(),
                to,
            }
        } else {
            FileOp::Copy {
                from: from.clone(),
                to,
            }
        });
    }
    Ok(())
}

/// Sort each file into a subfolder of the folder it is in, named by
/// `by`. Folders and dotfiles stay put: a folder isn't a "kind", and
/// moving `.DS_Store`-style files breaks whatever owns them.
///
/// Assumes `matches` are the files to sort, not a recursive listing —
/// the caller reads one folder's direct children.
fn organize_ops(matches: &[PathBuf], by: OrganizeBy, plan: &mut Plan) -> Result<(), PlanError> {
    // Bucket folder → names taken in it (on disk, plus earlier ops).
    let mut buckets: HashMap<PathBuf, HashSet<String>> = HashMap::new();
    for from in matches {
        let (Some(base), Some(name)) = (from.parent(), from.file_name().and_then(|n| n.to_str()))
        else {
            return Err(PlanError::InvalidName(from.display().to_string()));
        };
        if name.starts_with('.') || from.is_dir() {
            plan.skipped += 1;
            continue;
        }
        let bucket = base.join(bucket_name(from, name, by));
        let taken = buckets.entry(bucket.clone()).or_insert_with(|| {
            if !bucket.is_dir() {
                plan.creates.push(bucket.clone());
            }
            names_in(&bucket)
        });
        let free = free_variant(name, |n| taken.contains(n));
        taken.insert(free.clone());
        plan.ops.push(FileOp::Move {
            from: from.clone(),
            to: bucket.join(free),
        });
    }
    Ok(())
}

/// The subfolder `organize` files `name` under.
fn bucket_name(path: &Path, name: &str, by: OrganizeBy) -> String {
    use crate::listing::FileKind;
    let ext = Path::new(name).extension().and_then(|e| e.to_str());
    match by {
        OrganizeBy::Kind => match FileKind::of(name, false) {
            FileKind::Image => "Images",
            FileKind::Video => "Videos",
            FileKind::Audio => "Audio",
            FileKind::Archive => "Archives",
            FileKind::Code => "Code",
            FileKind::Document => "Documents",
            FileKind::Directory | FileKind::Other => "Other",
        }
        .to_owned(),
        OrganizeBy::Extension => {
            ext.map_or_else(|| "No extension".to_owned(), str::to_ascii_uppercase)
        }
        OrganizeBy::Month | OrganizeBy::Year => {
            let modified = std::fs::metadata(path)
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok());
            let Some(secs) = modified else {
                return "Undated".to_owned();
            };
            // ponytail: UTC, so a file saved within hours of a month
            // boundary can land a month off local time. Use the local
            // offset if that ever matters.
            let days = (secs.as_secs() as i64).div_euclid(crate::search::filter::SECS_PER_DAY);
            let (year, month, _) = crate::search::filter::civil_from_days(days);
            if by == OrganizeBy::Year {
                format!("{year}")
            } else {
                format!("{year}-{month:02}")
            }
        }
    }
}

/// The first of `name`, `name 2`, `name 3`… that isn't `taken` (Finder's
/// convention, split at the last dot so `x.tar.gz` → `x.tar 2.gz`).
fn free_variant(name: &str, taken: impl Fn(&str) -> bool) -> String {
    if !taken(name) {
        return name.to_owned();
    }
    let path = Path::new(name);
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or(name);
    let ext = path.extension().and_then(|e| e.to_str());
    for n in 2..10_000u32 {
        let candidate = match ext {
            Some(ext) => format!("{stem} {n}.{ext}"),
            None => format!("{stem} {n}"),
        };
        if !taken(&candidate) {
            return candidate;
        }
    }
    // 10k variants taken: fall back and let execute-time
    // `next_free_name` have one more go.
    name.to_owned()
}

fn rename_ops(matches: &[PathBuf], pattern: &NamePattern) -> Result<Vec<FileOp>, PlanError> {
    if matches.len() > 1 && !pattern.varies_per_file() {
        return Err(PlanError::PatternNotUnique);
    }
    let width = matches.len().to_string().len();
    let mut ops = Vec::with_capacity(matches.len());
    for (ix, path) in matches.iter().enumerate() {
        let new_name = pattern.render(path, ix + 1, width);
        if !is_valid_file_name(&new_name) {
            return Err(PlanError::InvalidName(new_name));
        }
        // Already called that; renaming onto itself trips "already exists".
        if path
            .file_name()
            .is_some_and(|current| current == new_name.as_str())
        {
            continue;
        }
        ops.push(FileOp::Rename {
            path: path.clone(),
            new_name,
        });
    }
    Ok(ops)
}

/// Reject a plan whose own ops would land two files on one path — what a
/// bad rename pattern produces, caught before the preview shows it.
fn check_collisions(ops: &[FileOp]) -> Result<(), PlanError> {
    // A set, not `Vec` + `contains`: this runs per rebuild, and the
    // linear scan was quadratic (~240k `PathBuf` comparisons for a
    // 697-file plan).
    let mut seen: HashSet<PathBuf> = HashSet::with_capacity(ops.len());
    for op in ops {
        if let Some(dest) = op.destination()
            && !seen.insert(dest.clone())
        {
            return Err(PlanError::Collision(dest));
        }
    }
    Ok(())
}

/// [`crate::ops`]'s apply-time name rules, checked before the preview.
fn is_valid_file_name(name: &str) -> bool {
    !name.is_empty() && !name.contains(['/', '\\']) && name != "." && name != ".."
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::listing::FileKind;
    use crate::search::filter::Bound;

    /// 2026-07-26 12:00 UTC, matching `phrases`'s test anchor.
    const NOW: i64 = 1_785_067_200;

    fn cmd(raw: &str) -> Option<Command> {
        parse(raw, NOW)
    }

    /// A context whose cwd is an empty tempdir and whose known folders
    /// are only what a test registers.
    fn ctx<'a>(cwd: &'a Path, dirs: &'a UserDirs) -> PlanContext<'a> {
        PlanContext {
            cwd,
            dirs,
            conflicts: ConflictPolicy::KeepBoth,
        }
    }

    fn paths(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(PathBuf::from).collect()
    }

    // -- parsing --------------------------------------------------------

    #[test]
    fn a_plain_filename_query_is_not_a_command() {
        // The whole point of the classifier gate: ordinary searches must
        // fall straight through here.
        for query in [
            "report.pdf",
            "quarterly earnings",
            "photos from last week",
            "",
        ] {
            assert!(
                cmd(query).is_none(),
                "{query:?} should not parse as a command"
            );
        }
    }

    #[test]
    fn delete_parses_verb_and_selection() {
        let command = cmd("delete screenshots older than 30 days").unwrap();
        assert_eq!(command.verb, Verb::Delete);
        assert_eq!(command.action, Action::Delete);
        assert_eq!(command.selection.source, "screenshots older than 30 days");
        // A screenshot is an image *named* like one, not every image.
        assert_eq!(command.selection.text, "screenshot");
        assert_eq!(
            command.selection.filters,
            vec![
                Filter::Kind(FileKind::Image),
                Filter::Modified(Bound::Lt(NOW - 30 * 86_400)),
            ]
        );
    }

    #[test]
    fn delete_synonyms_all_land_on_the_same_verb() {
        for word in ["delete", "remove", "trash"] {
            let command = cmd(&format!("{word} logs older than 30 days")).unwrap();
            assert_eq!(command.verb, Verb::Delete);
        }
    }

    #[test]
    fn move_parses_target_and_destination() {
        let command = cmd("move pdfs modified this week to Documents").unwrap();
        assert_eq!(command.verb, Verb::Move);
        assert_eq!(
            command.action,
            Action::Move(Destination {
                name: "Documents".into(),
                create: false,
            })
        );
        assert_eq!(
            command.selection.filters,
            vec![
                Filter::Ext("pdf".into()),
                Filter::Modified(Bound::Ge(NOW - 7 * 86_400)),
            ]
        );
        // "modified" is a connective here, not a filename to match on.
        assert_eq!(command.selection.text, "");
    }

    #[test]
    fn into_works_for_move_and_copy_but_not_rename() {
        assert!(cmd("move screenshots into Archive").is_some());
        assert!(cmd("copy invoices into Backup").is_some());
        // "rename x into y" is not English; without a `to` there is no
        // split, so no plan rather than a guessed one.
        assert!(cmd("rename screenshots into shot-{n}.png").is_none());
    }

    #[test]
    fn the_last_separator_splits_target_from_destination() {
        // A target word "to" must not steal the split from the real one.
        let command = cmd("move notes to self to Archive").unwrap();
        assert_eq!(
            command.action,
            Action::Move(Destination {
                name: "Archive".into(),
                create: false,
            })
        );
        assert_eq!(command.selection.source, "notes to self");
    }

    #[test]
    fn a_verb_with_no_destination_produces_no_command() {
        // The doc's explicit example of ambiguity: suppress, never guess.
        for query in [
            "move my screenshots",
            "copy the invoices",
            "rename screenshots",
        ] {
            assert!(cmd(query).is_none(), "{query:?} should not parse");
        }
    }

    #[test]
    fn a_verb_with_no_target_produces_no_command() {
        for query in [
            "delete",
            "delete all",
            "move to Documents",
            "move all to Documents",
        ] {
            assert!(cmd(query).is_none(), "{query:?} should not parse");
        }
    }

    #[test]
    fn leading_quantifiers_are_dropped_from_target_and_destination() {
        let command = cmd("delete all screenshots older than 30 days").unwrap();
        // "all" must not survive as filename text to match against.
        assert_eq!(command.selection.text, "screenshot");

        let command = cmd("move screenshots to the Archive folder").unwrap();
        assert_eq!(
            command.action,
            Action::Move(Destination {
                name: "Archive".into(),
                create: false,
            })
        );
    }

    #[test]
    fn a_delete_of_bare_text_is_not_a_command() {
        // The measured false-positive class: these are all real filenames
        // someone would search for, and every one is command-shaped.
        for query in [
            "delete reminder",
            "remove prefix",
            "delete property or throw",
            "remove index signature",
            "trash 2 svg",
        ] {
            assert!(
                cmd(query).is_none(),
                "{query:?} should not offer to delete anything"
            );
        }
    }

    #[test]
    fn explicit_magic_mode_drops_the_delete_gate() {
        // With the gate off (the user toggled magic mode on), a bare-text
        // delete is taken at face value — this is the recall the v1 gate
        // deliberately cost, recovered once intent is no longer inferred.
        let command = parse_with_gate("delete old logs", 1_785_067_200, false).unwrap();
        assert_eq!(command.verb, Verb::Delete);
        // Auto-switch (gate on) still refuses the same query.
        assert!(parse_with_gate("delete old logs", 1_785_067_200, true).is_none());
        // The default `parse` is the gated, auto-switch reading.
        assert!(parse("delete old logs", 1_785_067_200).is_none());
    }

    #[test]
    fn the_delete_gate_applies_to_no_other_verb() {
        // Move/copy/rename measured zero false positives, so gating them
        // on filter vocabulary would only cost recall.
        assert!(cmd("copy invoices to Backup").is_some());
        assert!(cmd("move notes to Archive").is_some());
        assert!(cmd("rename report to {name}-final.{ext}").is_some());
        // ...and the same bare-text selection is refused for delete.
        assert!(cmd("delete invoices").is_none());
    }

    #[test]
    fn a_delete_describing_a_set_still_parses() {
        // The gate must not cost the phrasings Magic mode exists for.
        for query in [
            "delete screenshots older than 30 days",
            "delete empty folders",
            "delete big videos",
            "delete pdfs bigger than 100mb",
            "delete all screenshots",
        ] {
            assert!(cmd(query).is_some(), "{query:?} should parse");
        }
    }

    #[test]
    fn a_single_word_target_is_a_description_not_a_filename() {
        // Regression: `phrases::expand`'s rule 2 reads a lone word as a
        // filename, which is right for the search bar and wrong here —
        // the plural "screenshots" is in no default screenshot name.
        for query in ["delete screenshots", "delete all screenshots"] {
            let command = cmd(query).unwrap();
            assert_eq!(
                command.selection.filters,
                vec![Filter::Kind(FileKind::Image)],
                "{query:?} should describe a set"
            );
            assert_eq!(
                command.selection.text, "screenshot",
                "{query:?} should match the singular every OS names them by"
            );
        }
    }

    #[test]
    fn quantifiers_are_command_grammar_not_search_grammar() {
        // The same word must stay searchable in an ordinary query — this
        // is why QUANTIFIERS lives here and not in phrases::FILLER.
        assert_eq!(
            phrases::expand("all hands notes", NOW).text,
            "all hands notes"
        );
    }

    #[test]
    fn parsing_is_case_insensitive_on_the_verb() {
        assert_eq!(
            cmd("DELETE logs older than 30 days").unwrap().verb,
            Verb::Delete
        );
        assert_eq!(cmd("Move screenshots TO Archive").unwrap().verb, Verb::Move);
    }

    #[test]
    fn past_tense_is_used_for_completion_notices() {
        // The completion notice reads "deleted 3 items", not the imperative
        // "delete 3 items" that lowercasing `label` used to produce.
        assert_eq!(Verb::Delete.past_tense(), "deleted");
        assert_eq!(Verb::Move.past_tense(), "moved");
        assert_eq!(Verb::Copy.past_tense(), "copied");
        assert_eq!(Verb::Rename.past_tense(), "renamed");
    }

    // -- rename patterns ------------------------------------------------

    #[test]
    fn rename_pattern_parses_placeholders_and_literals() {
        let command = cmd("rename screenshots to shot-{n}.{ext}").unwrap();
        let Action::Rename(pattern) = &command.action else {
            panic!("expected a rename")
        };
        assert!(pattern.varies_per_file());
        assert_eq!(pattern.render(Path::new("/a/b.png"), 3, 2), "shot-03.png");
    }

    #[test]
    fn rename_pattern_placeholders_read_the_original_name() {
        let pattern = NamePattern::parse("{name}-old.{ext}").unwrap();
        assert_eq!(
            pattern.render(Path::new("/a/report.pdf"), 1, 1),
            "report-old.pdf"
        );
        // An extensionless file renders an empty {ext}, not a panic.
        assert_eq!(
            pattern.render(Path::new("/a/Makefile"), 1, 1),
            "Makefile-old."
        );
    }

    #[test]
    fn rename_pattern_rejects_malformed_templates() {
        // Unknown placeholder — almost certainly a typo, and guessing
        // would write braces into real filenames.
        assert!(NamePattern::parse("shot-{nmae}.png").is_none());
        assert!(
            NamePattern::parse("shot-{n.png").is_none(),
            "unclosed brace"
        );
        assert!(
            NamePattern::parse("shot-n}.png").is_none(),
            "unopened brace"
        );
        assert!(NamePattern::parse("").is_none());
    }

    #[test]
    fn a_literal_only_pattern_does_not_vary() {
        assert!(!NamePattern::parse("final.png").unwrap().varies_per_file());
        assert!(NamePattern::parse("{name}.png").unwrap().varies_per_file());
        assert!(NamePattern::parse("{n}.png").unwrap().varies_per_file());
    }

    // -- plan building --------------------------------------------------

    #[test]
    fn delete_plan_is_one_op_per_match_in_path_order() {
        let dirs = UserDirs::default();
        let cwd = tempfile::tempdir().unwrap();
        let command = cmd("delete logs older than 30 days").unwrap();
        // Deliberately unsorted input.
        let plan = build(
            &command,
            &paths(&["/b.log", "/a.log"]),
            &ctx(cwd.path(), &dirs),
        )
        .unwrap();
        assert_eq!(
            plan.ops,
            vec![
                FileOp::Delete {
                    path: "/a.log".into()
                },
                FileOp::Delete {
                    path: "/b.log".into()
                },
            ]
        );
        assert_eq!(plan.skipped, 0);
    }

    #[test]
    fn an_empty_or_oversized_match_set_has_no_plan() {
        let dirs = UserDirs::default();
        let cwd = tempfile::tempdir().unwrap();
        let command = cmd("delete logs older than 30 days").unwrap();
        let context = ctx(cwd.path(), &dirs);

        assert_eq!(build(&command, &[], &context), Err(PlanError::NoMatches));

        let many: Vec<PathBuf> = (0..MAX_PLAN_OPS + 1)
            .map(|i| PathBuf::from(format!("/f{i}.log")))
            .collect();
        assert_eq!(
            build(&command, &many, &context),
            Err(PlanError::TooMany(MAX_PLAN_OPS + 1))
        );
    }

    #[test]
    fn move_plan_targets_a_subfolder_of_the_current_directory() {
        let cwd = tempfile::tempdir().unwrap();
        let archive = cwd.path().join("Archive");
        std::fs::create_dir(&archive).unwrap();
        let dirs = UserDirs::default();

        let command = cmd("move screenshots to archive").unwrap();
        let source = cwd.path().join("shot.png");
        let plan = build(
            &command,
            std::slice::from_ref(&source),
            &ctx(cwd.path(), &dirs),
        )
        .unwrap();

        // Case-insensitive: "archive" found the folder named "Archive".
        assert_eq!(
            plan.ops,
            vec![FileOp::Move {
                from: source,
                to: archive.join("shot.png")
            }]
        );
    }

    #[test]
    fn a_visible_subfolder_beats_a_known_folder_of_the_same_name() {
        let cwd = tempfile::tempdir().unwrap();
        let local = cwd.path().join("Documents");
        std::fs::create_dir(&local).unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let mut dirs = UserDirs::default();
        dirs.insert("documents", elsewhere.path());

        let command = cmd("move pdfs to Documents").unwrap();
        let source = cwd.path().join("a.pdf");
        let plan = build(
            &command,
            std::slice::from_ref(&source),
            &ctx(cwd.path(), &dirs),
        )
        .unwrap();
        assert_eq!(
            plan.ops,
            vec![FileOp::Move {
                from: source,
                to: local.join("a.pdf")
            }]
        );
    }

    #[test]
    fn a_known_folder_resolves_when_the_current_folder_has_no_such_child() {
        let cwd = tempfile::tempdir().unwrap();
        let documents = tempfile::tempdir().unwrap();
        let mut dirs = UserDirs::default();
        dirs.insert("documents", documents.path());

        let command = cmd("move pdfs to Documents").unwrap();
        let source = cwd.path().join("a.pdf");
        let plan = build(
            &command,
            std::slice::from_ref(&source),
            &ctx(cwd.path(), &dirs),
        )
        .unwrap();
        assert_eq!(
            plan.ops,
            vec![FileOp::Move {
                from: source,
                to: documents.path().join("a.pdf")
            }]
        );
    }

    #[test]
    fn an_unresolvable_destination_has_no_plan() {
        let cwd = tempfile::tempdir().unwrap();
        let dirs = UserDirs::default();
        let command = cmd("move pdfs to Nowhere").unwrap();
        assert_eq!(
            build(
                &command,
                &[cwd.path().join("a.pdf")],
                &ctx(cwd.path(), &dirs)
            ),
            Err(PlanError::UnknownDestination("Nowhere".into()))
        );
    }

    /// Regression: browsing This PC (not a real folder) made the cwd a
    /// relative path, and destinations then resolved against the
    /// process's working directory. Tests run from the crate root, where
    /// `src/workspace` exists.
    #[test]
    fn a_relative_cwd_is_never_searched_for_destinations() {
        let dirs = UserDirs::default();
        let command = cmd("move pdfs to workspace").unwrap();
        assert_eq!(
            build(
                &command,
                &[PathBuf::from("/a.pdf")],
                &ctx(Path::new("src"), &dirs)
            ),
            Err(PlanError::UnknownDestination("workspace".into()))
        );
    }

    /// Regression: "move pdfs to Downloads" planned to move PDFs out of
    /// Downloads' own subfolders into Downloads (with renames).
    #[test]
    fn files_inside_the_destination_at_any_depth_stay_put() {
        let cwd = tempfile::tempdir().unwrap();
        let archive = cwd.path().join("Archive");
        std::fs::create_dir_all(archive.join("2024")).unwrap();
        let dirs = UserDirs::default();
        let command = cmd("move screenshots to Archive").unwrap();
        let nested = archive.join("2024").join("c.png");
        let outside = cwd.path().join("a.png");
        let plan = build(&command, &[outside, nested], &ctx(cwd.path(), &dirs)).unwrap();
        assert_eq!(plan.ops.len(), 1);
        assert_eq!(plan.skipped, 1);
    }

    #[test]
    fn files_already_in_the_destination_are_skipped_and_counted() {
        let cwd = tempfile::tempdir().unwrap();
        let archive = cwd.path().join("Archive");
        std::fs::create_dir(&archive).unwrap();
        let dirs = UserDirs::default();

        let command = cmd("move screenshots to Archive").unwrap();
        let outside = cwd.path().join("a.png");
        let already = archive.join("b.png");
        let plan = build(
            &command,
            &[outside.clone(), already],
            &ctx(cwd.path(), &dirs),
        )
        .unwrap();

        assert_eq!(plan.ops.len(), 1);
        assert_eq!(plan.skipped, 1);
        assert_eq!(
            plan.ops[0],
            FileOp::Move {
                from: outside,
                to: archive.join("a.png")
            }
        );
    }

    #[test]
    fn two_matches_sharing_a_name_are_retargeted_not_refused() {
        // The dogfooding case: `move roadmap to Downloads` where two
        // different folders each hold a ROADMAP.md. The old plan refused
        // outright ("two files would both become …"); now the second is
        // retargeted to "ROADMAP 2.md" so the plan is reviewable.
        let cwd = tempfile::tempdir().unwrap();
        let downloads = cwd.path().join("Downloads");
        std::fs::create_dir(&downloads).unwrap();
        std::fs::create_dir(cwd.path().join("a")).unwrap();
        std::fs::create_dir(cwd.path().join("b")).unwrap();
        let dirs = UserDirs::default();

        let command = cmd("move roadmap to Downloads").unwrap();
        let first = cwd.path().join("a/ROADMAP.md");
        let second = cwd.path().join("b/ROADMAP.md");
        let plan = build(
            &command,
            &[first.clone(), second.clone()],
            &ctx(cwd.path(), &dirs),
        )
        .unwrap();

        assert_eq!(plan.ops.len(), 2, "both files are planned, none refused");
        // `matches` is sorted, so a/ROADMAP.md keeps the base name and
        // b/ROADMAP.md takes the numbered variant.
        assert_eq!(
            plan.ops,
            vec![
                FileOp::Move {
                    from: first,
                    to: downloads.join("ROADMAP.md")
                },
                FileOp::Move {
                    from: second,
                    to: downloads.join("ROADMAP 2.md")
                },
            ]
        );
    }

    #[test]
    fn a_match_named_like_an_existing_destination_file_is_retargeted() {
        // A single move whose name is already occupied on disk gets the
        // numbered variant too — the folder is read once, so the preview
        // is truthful without a per-file stat.
        let cwd = tempfile::tempdir().unwrap();
        let downloads = cwd.path().join("Downloads");
        std::fs::create_dir(&downloads).unwrap();
        std::fs::write(downloads.join("notes.md"), b"existing").unwrap();
        std::fs::create_dir(cwd.path().join("src")).unwrap();
        let dirs = UserDirs::default();

        let command = cmd("move notes to Downloads").unwrap();
        let from = cwd.path().join("src/notes.md");
        let plan = build(
            &command,
            std::slice::from_ref(&from),
            &ctx(cwd.path(), &dirs),
        )
        .unwrap();

        assert_eq!(
            plan.ops,
            vec![FileOp::Move {
                from,
                to: downloads.join("notes 2.md")
            }]
        );
    }

    #[test]
    fn a_folder_is_never_moved_inside_itself() {
        // "move folders to Archive" while Archive is one of the matches:
        // planning that op would be a recursive copy, so it is skipped.
        let cwd = tempfile::tempdir().unwrap();
        let archive = cwd.path().join("Archive");
        std::fs::create_dir(&archive).unwrap();
        let dirs = UserDirs::default();

        let command = cmd("move folders to Archive").unwrap();
        let other = cwd.path().join("Other");
        let plan = build(
            &command,
            &[archive.clone(), other.clone()],
            &ctx(cwd.path(), &dirs),
        )
        .unwrap();

        assert_eq!(
            plan.ops,
            vec![FileOp::Move {
                from: other,
                to: archive.join("Other")
            }]
        );
        assert_eq!(plan.skipped, 1);
    }

    #[test]
    fn every_match_already_done_is_a_plan_with_nothing_to_do() {
        let cwd = tempfile::tempdir().unwrap();
        let archive = cwd.path().join("Archive");
        std::fs::create_dir(&archive).unwrap();
        let dirs = UserDirs::default();

        let command = cmd("move screenshots to Archive").unwrap();
        assert_eq!(
            build(&command, &[archive.join("a.png")], &ctx(cwd.path(), &dirs)),
            Err(PlanError::NothingToDo)
        );
    }

    #[test]
    fn rename_numbers_the_batch_and_pads_to_its_width() {
        let cwd = tempfile::tempdir().unwrap();
        let dirs = UserDirs::default();
        let command = cmd("rename screenshots to shot-{n}.png").unwrap();

        let files: Vec<PathBuf> = (0..12)
            .map(|i| cwd.path().join(format!("img{i:02}.png")))
            .collect();
        let plan = build(&command, &files, &ctx(cwd.path(), &dirs)).unwrap();

        // 12 files ⇒ two-digit counter, so names sort in numbered order.
        let FileOp::Rename { new_name, .. } = &plan.ops[0] else {
            panic!()
        };
        assert_eq!(new_name, "shot-01.png");
        let FileOp::Rename { new_name, .. } = &plan.ops[11] else {
            panic!()
        };
        assert_eq!(new_name, "shot-12.png");
    }

    #[test]
    fn a_small_rename_batch_is_not_padded() {
        let cwd = tempfile::tempdir().unwrap();
        let dirs = UserDirs::default();
        let command = cmd("rename screenshots to shot-{n}.png").unwrap();
        let files = vec![cwd.path().join("a.png"), cwd.path().join("b.png")];
        let plan = build(&command, &files, &ctx(cwd.path(), &dirs)).unwrap();
        let FileOp::Rename { new_name, .. } = &plan.ops[0] else {
            panic!()
        };
        assert_eq!(new_name, "shot-1.png");
    }

    #[test]
    fn a_non_varying_rename_pattern_is_refused_for_a_batch() {
        // The collision this question exists to prevent: N files, one name.
        let cwd = tempfile::tempdir().unwrap();
        let dirs = UserDirs::default();
        let command = cmd("rename screenshots to final.png").unwrap();
        let files = vec![cwd.path().join("a.png"), cwd.path().join("b.png")];
        assert_eq!(
            build(&command, &files, &ctx(cwd.path(), &dirs)),
            Err(PlanError::PatternNotUnique)
        );
        // ...but it is exactly right for a single file.
        let plan = build(&command, &files[..1], &ctx(cwd.path(), &dirs)).unwrap();
        assert_eq!(plan.ops.len(), 1);
    }

    #[test]
    fn a_rename_pattern_that_collides_across_folders_is_refused() {
        // {name} varies per file, so the pattern passes the cheap check —
        // but two files in different folders can still share a stem.
        let cwd = tempfile::tempdir().unwrap();
        let dirs = UserDirs::default();
        let command = cmd("rename screenshots to {name}.png").unwrap();
        let files = vec![cwd.path().join("a.jpg"), cwd.path().join("a.gif")];
        assert!(matches!(
            build(&command, &files, &ctx(cwd.path(), &dirs)),
            Err(PlanError::Collision(_))
        ));
    }

    #[test]
    fn a_file_already_carrying_the_rendered_name_is_skipped() {
        let cwd = tempfile::tempdir().unwrap();
        let dirs = UserDirs::default();
        let command = cmd("rename screenshots to {name}.png").unwrap();
        let files = vec![cwd.path().join("a.png"), cwd.path().join("b.jpg")];
        let plan = build(&command, &files, &ctx(cwd.path(), &dirs)).unwrap();
        assert_eq!(plan.skipped, 1, "a.png already renders to a.png");
        assert_eq!(
            plan.ops,
            vec![FileOp::Rename {
                path: cwd.path().join("b.jpg"),
                new_name: "b.png".into()
            }]
        );
    }

    #[test]
    fn a_pattern_rendering_a_path_is_refused() {
        // A separator in a rename would silently relocate the file.
        let cwd = tempfile::tempdir().unwrap();
        let dirs = UserDirs::default();
        let command = cmd("rename screenshots to ../{name}.png").unwrap();
        assert!(matches!(
            build(
                &command,
                &[cwd.path().join("a.jpg")],
                &ctx(cwd.path(), &dirs)
            ),
            Err(PlanError::InvalidName(_))
        ));
    }

    #[test]
    fn plans_only_ever_use_existing_file_op_variants() {
        // Guards the doc's "no new operation types" non-goal: if a verb
        // ever needs something FileOp can't express, this stops matching.
        let cwd = tempfile::tempdir().unwrap();
        let archive = cwd.path().join("Archive");
        std::fs::create_dir(&archive).unwrap();
        let dirs = UserDirs::default();
        let context = ctx(cwd.path(), &dirs);
        let source = cwd.path().join("a.png");

        for query in [
            "delete screenshots",
            "move screenshots to Archive",
            "copy screenshots to Archive",
            "rename screenshots to {name}-1.png",
        ] {
            let command = cmd(query).unwrap();
            let plan = build(&command, std::slice::from_ref(&source), &context).unwrap();
            assert!(matches!(
                plan.ops[0],
                FileOp::Delete { .. }
                    | FileOp::Move { .. }
                    | FileOp::Copy { .. }
                    | FileOp::Rename { .. }
            ));
        }
    }

    // -- explicit-mode grammar ------------------------------------------

    /// What the user gets once they have asked for magic mode.
    fn explicit(raw: &str) -> Option<Command> {
        parse_with_gate(raw, NOW, false)
    }

    #[test]
    fn the_newer_verbs_never_switch_the_mode_on_their_own() {
        // None of these were measured against real filenames, and
        // `zip-files.sh` or `tag-release.yml` are ordinary things to
        // search for. They need the explicit entry.
        for query in [
            "zip pdfs older than 30 days",
            "tag invoices as Work",
            "untag screenshots",
            "organize Downloads by type",
            "delete selected",
        ] {
            assert!(cmd(query).is_none(), "{query:?} must not auto-switch");
            assert!(explicit(query).is_some(), "{query:?} should parse");
        }
    }

    #[test]
    fn from_a_folder_scopes_the_selection_instead_of_naming_files() {
        // Regression: "from" was dropped as filler and "downloads"
        // became filename text, so this matched pdfs *named* downloads.
        let command = cmd("move pdfs from downloads to Documents").unwrap();
        assert_eq!(command.selection.filters, vec![Filter::Ext("pdf".into())]);
        assert_eq!(command.selection.text, "");
        assert_eq!(
            command.selection.within,
            Some(Destination {
                name: "downloads".into(),
                create: false
            })
        );
        // "everything in X" is the folder's contents, not a file name.
        let command = explicit("delete everything in the Scratch folder").unwrap();
        assert_eq!(command.selection.text, "");
        assert_eq!(command.selection.within.unwrap().name, "Scratch");
    }

    #[test]
    fn a_date_after_from_is_still_a_date() {
        let command = cmd("delete photos from last week").unwrap();
        assert_eq!(command.selection.within, None);
        assert!(
            command
                .selection
                .filters
                .iter()
                .any(|f| matches!(f, Filter::Modified(_)))
        );
    }

    #[test]
    fn selected_targets_the_selection_without_a_search() {
        for query in [
            "delete selected",
            "move these to Archive",
            "zip selected files",
        ] {
            let command = explicit(query).unwrap();
            assert!(command.selection.picked, "{query:?}");
            assert!(command.selection.filters.is_empty());
        }
        // A file can be called "selected"; auto mode keeps reading it so.
        assert!(!cmd("move selected to Archive").unwrap().selection.picked);
    }

    #[test]
    fn a_new_folder_destination_is_created_in_the_current_folder() {
        let cwd = tempfile::tempdir().unwrap();
        let dirs = UserDirs::default();
        let command = cmd("move screenshots to a new folder called Old Shots").unwrap();
        assert_eq!(
            command.action,
            Action::Move(Destination {
                name: "Old Shots".into(),
                create: true
            })
        );
        let source = cwd.path().join("a.png");
        let plan = build(
            &command,
            std::slice::from_ref(&source),
            &ctx(cwd.path(), &dirs),
        )
        .unwrap();
        let made = cwd.path().join("Old Shots");
        assert_eq!(plan.creates, vec![made.clone()]);
        assert_eq!(
            plan.ops,
            vec![FileOp::Move {
                from: source.clone(),
                to: made.join("a.png")
            }]
        );
        // Already there: nothing to create, same destination.
        std::fs::create_dir(&made).unwrap();
        let plan = build(&command, &[source], &ctx(cwd.path(), &dirs)).unwrap();
        assert!(plan.creates.is_empty());
    }

    #[test]
    fn a_typed_path_is_its_own_destination() {
        let cwd = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let mut dirs = UserDirs::default();
        dirs.insert("home", elsewhere.path());
        std::fs::create_dir(elsewhere.path().join("inbox")).unwrap();
        let context = ctx(cwd.path(), &dirs);
        let dest = |name: &str| Destination {
            name: name.into(),
            create: false,
        };
        assert_eq!(
            resolve_destination(&dest("~/inbox"), &context),
            Ok(elsewhere.path().join("inbox"))
        );
        assert_eq!(
            resolve_destination(&dest(elsewhere.path().to_str().unwrap()), &context),
            Ok(elsewhere.path().to_path_buf())
        );
        assert!(resolve_destination(&dest("~/nope"), &context).is_err());
    }

    #[test]
    fn organize_reads_past_the_words_for_a_folders_contents() {
        for here in [
            "everything here",
            "all the files in this folder",
            "files",
            "my stuff",
        ] {
            let target = organize_target(&here.split(' ').collect::<Vec<_>>());
            assert_eq!((target.within, target.picked), (None, false), "{here}");
        }
        let target = organize_target(&["everything", "in", "Downloads"]);
        assert_eq!(target.within.unwrap().name, "Downloads");
        assert!(organize_target(&["these"]).picked);
    }

    #[test]
    fn here_and_this_folder_mean_the_folder_on_screen() {
        let cwd = tempfile::tempdir().unwrap();
        let ctx = PlanContext {
            cwd: cwd.path(),
            dirs: &UserDirs::default(),
            conflicts: Default::default(),
        };
        for spoken in [
            "pdfs from here",
            "pdfs in this folder",
            "pdfs in the current folder",
        ] {
            let words: Vec<&str> = spoken.split(' ').collect();
            let (rest, within) = split_within(&words, 0);
            assert_eq!(rest, ["pdfs"], "{spoken}");
            let within = within.unwrap_or_else(|| panic!("{spoken}: no folder"));
            assert_eq!(
                resolve_destination(&within, &ctx).unwrap(),
                cwd.path(),
                "{spoken}"
            );
        }
        // A folder really called "here" is that folder.
        std::fs::create_dir(cwd.path().join("here")).unwrap();
        let here = Destination {
            name: "here".into(),
            create: false,
        };
        assert_eq!(
            resolve_destination(&here, &ctx).unwrap(),
            cwd.path().join("here")
        );
    }

    #[test]
    fn a_misspelt_folder_resolves_to_the_one_it_is_a_slip_from() {
        let cwd = tempfile::tempdir().unwrap();
        std::fs::create_dir(cwd.path().join("Invoices")).unwrap();
        std::fs::write(cwd.path().join("Invoice"), b"a file, not a folder").unwrap();
        let documents = tempfile::tempdir().unwrap();
        let mut dirs = UserDirs::default();
        dirs.insert("documents", documents.path());
        let context = ctx(cwd.path(), &dirs);
        let to = |name: &str| {
            resolve_destination(
                &Destination {
                    name: name.into(),
                    create: false,
                },
                &context,
            )
        };
        assert_eq!(to("invocies"), Ok(cwd.path().join("Invoices")));
        assert_eq!(to("documnets"), Ok(documents.path().to_path_buf()));
        // Not close to anything: still unknown, never a guess.
        assert_eq!(
            to("Archive"),
            Err(PlanError::UnknownDestination("Archive".into()))
        );
    }

    #[test]
    fn the_conflict_policy_decides_what_an_occupied_name_becomes() {
        let cwd = tempfile::tempdir().unwrap();
        let archive = cwd.path().join("Archive");
        std::fs::create_dir(&archive).unwrap();
        std::fs::write(archive.join("a.txt"), b"old").unwrap();
        std::fs::create_dir(cwd.path().join("x")).unwrap();
        std::fs::create_dir(cwd.path().join("y")).unwrap();
        let dirs = UserDirs::default();
        let command = cmd("move notes to Archive").unwrap();
        // Two matches named a.txt (one clash on disk, one in-plan
        // duplicate) and one free name.
        let files = [
            cwd.path().join("x/a.txt"),
            cwd.path().join("y/a.txt"),
            cwd.path().join("x/b.txt"),
        ];
        let targets = |policy| -> Vec<String> {
            let context = PlanContext {
                cwd: cwd.path(),
                dirs: &dirs,
                conflicts: policy,
            };
            let plan = build(&command, &files, &context).unwrap();
            assert_eq!(plan.conflicts, 2);
            plan.ops
                .iter()
                .map(|op| {
                    let dest = op.destination().unwrap();
                    dest.file_name().unwrap().to_string_lossy().into_owned()
                })
                .collect()
        };
        assert_eq!(
            targets(ConflictPolicy::KeepBoth),
            ["a 2.txt", "b.txt", "a 3.txt"]
        );
        assert_eq!(targets(ConflictPolicy::Skip), ["b.txt"]);
        // Only one file can replace the original; the second keeps both
        // rather than silently overwriting the first.
        assert_eq!(
            targets(ConflictPolicy::Replace),
            ["a.txt", "b.txt", "a 2.txt"]
        );
    }

    #[test]
    fn organize_sorts_files_into_kind_folders_beside_them() {
        let cwd = tempfile::tempdir().unwrap();
        let dirs = UserDirs::default();
        std::fs::create_dir(cwd.path().join("Images")).unwrap();
        std::fs::write(cwd.path().join("Images/a.png"), b"").unwrap();
        std::fs::create_dir(cwd.path().join("sub")).unwrap();
        let command = explicit("organize by type").unwrap();
        assert_eq!(command.action, Action::Organize(OrganizeBy::Kind));
        assert_eq!(command.selection.within, None, "no target means here");

        let files = [
            cwd.path().join(".hidden"),
            cwd.path().join("a.png"),
            cwd.path().join("report.pdf"),
            cwd.path().join("sub"),
        ];
        let plan = build(&command, &files, &ctx(cwd.path(), &dirs)).unwrap();
        assert_eq!(
            plan.ops,
            vec![
                // Images already holds an a.png: keep both.
                FileOp::Move {
                    from: files[1].clone(),
                    to: cwd.path().join("Images/a 2.png")
                },
                FileOp::Move {
                    from: files[2].clone(),
                    to: cwd.path().join("Documents/report.pdf")
                },
            ]
        );
        // Only the missing bucket is created; dotfiles and folders stay.
        assert_eq!(plan.creates, vec![cwd.path().join("Documents")]);
        assert_eq!(plan.skipped, 2);
    }

    #[test]
    fn organize_reads_its_folder_and_its_grouping() {
        let command = explicit("organize my Downloads folder by extension").unwrap();
        assert_eq!(command.action, Action::Organize(OrganizeBy::Extension));
        assert_eq!(command.selection.within.unwrap().name, "Downloads");
        assert!(explicit("tidy selected by month").unwrap().selection.picked);
        // An unknown grouping is not guessed at.
        assert!(explicit("organize Downloads by vibes").is_none());
        assert_eq!(
            bucket_name(Path::new("/x/a.PDF"), "a.PDF", OrganizeBy::Extension),
            "PDF"
        );
        assert_eq!(
            bucket_name(Path::new("/x/Makefile"), "Makefile", OrganizeBy::Extension),
            "No extension"
        );
    }

    #[test]
    fn compress_lists_targets_and_names_the_archive() {
        let cwd = tempfile::tempdir().unwrap();
        let dirs = UserDirs::default();
        let command = explicit("zip invoices").unwrap();
        let context = ctx(cwd.path(), &dirs);

        // One file: named after it, beside it.
        let plan = build(&command, &[PathBuf::from("/a/inv.pdf")], &context).unwrap();
        assert!(plan.ops.is_empty());
        assert_eq!(plan.archive, Some(PathBuf::from("/a/inv.zip")));

        // Across folders: in the folder on screen, and a repeated name
        // is left out since a ZIP root holds each name once.
        let files = [
            PathBuf::from("/a/inv.pdf"),
            PathBuf::from("/b/inv.pdf"),
            PathBuf::from("/b/other.pdf"),
        ];
        let plan = build(&command, &files, &context).unwrap();
        assert_eq!(plan.targets, vec![files[0].clone(), files[2].clone()]);
        assert_eq!(plan.skipped, 1);
        assert_eq!(plan.archive, Some(cwd.path().join("Archive.zip")));
        assert_eq!(plan.len(), 2);
        assert_eq!(plan.source(1), Some(files[2].as_path()));
    }

    #[test]
    fn tags_read_a_colour_a_coloured_name_or_a_plain_name() {
        let tag = |raw: &str| match explicit(raw).unwrap().action {
            Action::Tag(tag) => tag,
            other => panic!("expected a tag, got {other:?}"),
        };
        assert_eq!(
            tag("tag invoices as red"),
            Tag::colored("Red", TagColor::Red)
        );
        assert_eq!(
            tag("tag invoices as blue Tax 2026"),
            Tag::colored("Tax 2026", TagColor::Blue)
        );
        assert_eq!(tag("label invoices with Work"), Tag::new("Work"));
    }

    #[test]
    fn untag_clears_every_tag_or_removes_one_by_name() {
        assert_eq!(
            explicit("untag invoices").unwrap().action,
            Action::Untag(None)
        );
        let command = explicit("remove tag Work from invoices in Downloads").unwrap();
        assert_eq!(command.verb, Verb::Untag);
        assert_eq!(command.action, Action::Untag(Some("Work".into())));
        assert_eq!(command.selection.text, "invoices");
        assert_eq!(command.selection.within.unwrap().name, "Downloads");
    }
}

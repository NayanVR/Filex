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
}

impl Verb {
    /// Present-tense label for the confirm button and card heading.
    pub fn label(self) -> &'static str {
        match self {
            Self::Delete => "Delete",
            Self::Move => "Move",
            Self::Copy => "Copy",
            Self::Rename => "Rename",
        }
    }

    /// Past-tense for the completion notice ("deleted 3 items").
    pub fn past_tense(self) -> &'static str {
        match self {
            Self::Delete => "deleted",
            Self::Move => "moved",
            Self::Copy => "copied",
            Self::Rename => "renamed",
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
}

/// Where a move or copy is headed, as spelled. [`resolve_destination`]
/// turns it into a real directory later, keeping the parse pure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Destination {
    /// The cleaned-up folder name ("documents", "project archive").
    pub name: String,
}

/// What the command does to each matched file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Delete,
    Move(Destination),
    Copy(Destination),
    Rename(NamePattern),
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
}

/// Why a parsed command could not become a plan. Each suppresses the
/// card; the variants let the UI say *why*.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    /// The selection matched no files.
    NoMatches,
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

/// [`parse`], with control over the delete gate — the only difference
/// between the two entry paths into magic mode. `true` (auto-switch)
/// requires a delete to carry a filter, so `delete reminder` can't arm a
/// delete off a filename; `false` (explicit toggle) takes the user at
/// their word. See `docs/design-magic-mode.md` §"Where the delete gate
/// goes".
pub fn parse_with_gate(raw: &str, now: i64, require_delete_evidence: bool) -> Option<Command> {
    let words: Vec<&str> = raw.split_whitespace().collect();
    let (first, rest) = words.split_first()?;
    let verb = verb_of(&first.to_ascii_lowercase())?;
    let rest = strip_quantifier(rest);
    if rest.is_empty() {
        return None;
    }

    let (target, action) = match verb {
        Verb::Delete => (rest, Action::Delete),
        Verb::Move | Verb::Copy | Verb::Rename => {
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
                Verb::Delete => unreachable!("delete takes no separator"),
            };
            (target, action)
        }
    };

    let selection = selection_from(target, now)?;
    if require_delete_evidence && !clears_delete_gate(verb, &selection) {
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
        Verb::Delete => false,
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

/// Run the target words through the ordinary search parse, so a command's
/// target supports exactly the search bar's vocabulary. `None` when
/// nothing survives — the "verb with no target" case.
fn selection_from(words: &[&str], now: i64) -> Option<Selection> {
    let source = words.join(" ");
    let parsed = parse_query(&source, now);
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
    if expansion.text.is_empty() && filters.is_empty() {
        return None;
    }
    Some(Selection {
        source,
        text: expansion.text,
        filters,
    })
}

/// Clean a destination phrase to a folder name: drop leading filler and a
/// trailing "folder"/"directory". `None` if nothing is left.
fn parse_destination(words: &[&str]) -> Option<Destination> {
    let mut words = strip_quantifier(words);
    while let Some((last, rest)) = words.split_last() {
        let last = last.to_ascii_lowercase();
        if last == "folder" || last == "directory" {
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
}

/// Turn a spelled destination into a real directory. A subfolder of the
/// folder on screen wins over a same-named known folder ("move these to
/// Archive" means the one you're looking at); known folders are the
/// fallback, which is what makes "to Documents" work from anywhere.
///
/// The current-folder lookup scans the directory preferring an exact-case
/// hit, rather than probing `cwd.join(name)`. A join probe succeeds on
/// case-insensitive Windows/macOS and hands back the user's spelling
/// while resolving nothing on Linux — one command, a different
/// destination per OS. Scanning yields the true on-disk name everywhere.
pub fn resolve_destination(dest: &Destination, ctx: &PlanContext) -> Result<PathBuf, PlanError> {
    let mut case_insensitive: Option<PathBuf> = None;
    if let Ok(entries) = std::fs::read_dir(ctx.cwd) {
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            // Compare names before stat'ing — no metadata call per child.
            let exact = name == dest.name;
            if (!exact && !name.eq_ignore_ascii_case(&dest.name)) || !entry.path().is_dir() {
                continue;
            }
            if exact {
                return Ok(entry.path());
            }
            case_insensitive.get_or_insert_with(|| entry.path());
        }
    }
    if let Some(path) = case_insensitive {
        return Ok(path);
    }
    // A multi-segment destination isn't one directory entry.
    let nested = ctx.cwd.join(&dest.name);
    if nested.is_dir() {
        return Ok(nested);
    }
    ctx.dirs
        .get(&dest.name)
        .filter(|path| path.is_dir())
        .map(Path::to_path_buf)
        .ok_or_else(|| PlanError::UnknownDestination(dest.name.clone()))
}

// ---------------------------------------------------------------------
// Plan building
// ---------------------------------------------------------------------

/// Resolve `command` against the files the search matched.
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

    let ops = match &command.action {
        Action::Delete => matches
            .iter()
            .map(|path| FileOp::Delete { path: path.clone() })
            .collect(),
        Action::Move(dest) => transfer_ops(&matches, dest, ctx, true)?,
        Action::Copy(dest) => transfer_ops(&matches, dest, ctx, false)?,
        Action::Rename(pattern) => rename_ops(&matches, pattern)?,
    };

    if ops.is_empty() {
        return Err(PlanError::NothingToDo);
    }
    check_collisions(&ops)?;
    Ok(Plan {
        verb: command.verb,
        skipped: matches.len() - ops.len(),
        ops,
    })
}

/// Move/copy operations into a resolved destination directory.
///
/// Skipped rather than planned: a file already in the destination, and a
/// directory that *contains* the destination (moving a folder inside
/// itself) — so one degenerate match doesn't void a fine plan.
///
/// **Collisions are resolved, not refused.** Two matches sharing a base
/// name retarget to the first free `name 2` variant, as the execute,
/// paste and drag paths already do, so the preview shows the real
/// outcome. One `read_dir` up front, not a stat per file.
fn transfer_ops(
    matches: &[PathBuf],
    dest: &Destination,
    ctx: &PlanContext,
    moving: bool,
) -> Result<Vec<FileOp>, PlanError> {
    let dir = resolve_destination(dest, ctx)?;
    // Names spoken for in `dir`: on disk now, plus what earlier ops in
    // this plan claimed. In memory, so no per-file I/O.
    let mut taken: HashSet<String> = HashSet::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                taken.insert(name.to_owned());
            }
        }
    }
    let mut ops = Vec::with_capacity(matches.len());
    for from in matches {
        if from.parent() == Some(dir.as_path()) || dir.starts_with(from) {
            continue;
        }
        let Some(name) = from.file_name().and_then(|n| n.to_str()) else {
            return Err(PlanError::InvalidName(from.display().to_string()));
        };
        let free = free_variant(name, &taken);
        taken.insert(free.clone());
        let to = dir.join(&free);
        ops.push(if moving {
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
    Ok(ops)
}

/// The first of `name`, `name 2`, `name 3`… not in `taken` (Finder's
/// convention, split at the last dot so `x.tar.gz` → `x.tar 2.gz`).
fn free_variant(name: &str, taken: &HashSet<String>) -> String {
    if !taken.contains(name) {
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
        if !taken.contains(&candidate) {
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
    // A set, not `Vec` + `contains`: this runs on the UI thread per
    // rebuild, and the linear scan was quadratic (~240k `PathBuf`
    // comparisons for a 697-file plan).
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
        PlanContext { cwd, dirs }
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
        assert_eq!(command.selection.text, "");
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
                name: "Documents".into()
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
                name: "Archive".into()
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
        assert_eq!(command.selection.text, "");

        let command = cmd("move screenshots to the Archive folder").unwrap();
        assert_eq!(
            command.action,
            Action::Move(Destination {
                name: "Archive".into()
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
        // it made `delete screenshots` mean "delete files *named*
        // screenshots" rather than "delete images".
        for query in ["delete screenshots", "delete all screenshots"] {
            let command = cmd(query).unwrap();
            assert_eq!(
                command.selection.filters,
                vec![Filter::Kind(FileKind::Image)],
                "{query:?} should describe a set"
            );
            assert_eq!(
                command.selection.text, "",
                "{query:?} should leave no name to match"
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
}

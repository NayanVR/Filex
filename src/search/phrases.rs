//! Natural-language phrases → the existing `key:value` filter grammar.
//! `photos from last week` becomes `kind:image` + `modified:…`, reusing
//! [`crate::search::filter`]'s [`Filter`] rather than a parallel query
//! path. Strictly rule-based (`docs/design-search-ranking.md` block 4).
//!
//! Three rules keep it from hiding files: every expansion is a removable
//! chip ([`Phrase::source`] is the exact text to strip); a single word
//! that is the whole query never expands (`photos` searches for the name,
//! `photos from last week` expands); explicit `key:value` tokens always
//! win.
//!
//! Deliberately unsupported: seasons. June is summer in Berlin and monsoon
//! in Mumbai, so a fixed mapping is quietly wrong for half its users.
//! Month names and years cover the need without guessing.

use crate::listing::FileKind;
use crate::search::filter::{
    Bound, Filter, SECS_PER_DAY as DAY, civil_from_days, days_from_civil, parse_bytes, start_of_day,
};

/// One recognized phrase and what it means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Phrase {
    /// The matched words, verbatim and in order, so the UI can show what
    /// it understood and remove exactly that text to undo it.
    pub source: String,
    /// What the phrase expands to. Multiple filters AND, as everywhere.
    pub filters: Vec<Filter>,
}

/// The outcome of reading `raw` as natural language.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Expansion {
    /// The words no phrase claimed — the filename text to search for.
    pub text: String,
    pub phrases: Vec<Phrase>,
}

impl Expansion {
    /// Every filter across all phrases, flattened.
    pub fn filters(&self) -> Vec<Filter> {
        self.phrases
            .iter()
            .flat_map(|p| p.filters.iter().cloned())
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.phrases.is_empty()
    }
}

/// `raw` with the first consecutive run of words equal to `source`
/// removed (case-insensitively) — what removing an inferred chip does.
/// The counterpart of
/// [`without_token`](crate::search::filter::without_token), which handles
/// only single `key:value` words.
pub fn without_phrase(raw: &str, source: &str) -> String {
    let words: Vec<&str> = raw.split_whitespace().collect();
    let target: Vec<String> = source
        .split_whitespace()
        .map(|w| w.to_ascii_lowercase())
        .collect();
    if target.is_empty() {
        return raw.trim().to_string();
    }
    for start in 0..words.len().saturating_sub(target.len() - 1) {
        let matches = words[start..start + target.len()]
            .iter()
            .zip(&target)
            .all(|(w, t)| w.to_ascii_lowercase() == *t);
        if matches {
            let mut kept: Vec<&str> = words[..start].to_vec();
            kept.extend_from_slice(&words[start + target.len()..]);
            return kept.join(" ");
        }
    }
    words.join(" ")
}

/// A short, honest label for a filter an expansion produced. Kinds and
/// extensions get their canonical `key:value` spelling, which is what
/// makes the expansion explainable; sizes and dates fall back to the words
/// the user typed, since a raw [`Bound`] has no compact readable form.
pub fn label_for(filter: &Filter, source: &str) -> String {
    match filter {
        Filter::Kind(kind) => format!("kind:{}", kind_word(*kind)),
        Filter::Ext(ext) => format!("ext:{ext}"),
        Filter::Tag(name) => name.clone(),
        Filter::Size(_) | Filter::Modified(_) => source.to_string(),
    }
}

fn kind_word(kind: FileKind) -> &'static str {
    match kind {
        FileKind::Directory => "folder",
        FileKind::Image => "image",
        FileKind::Video => "video",
        FileKind::Audio => "audio",
        FileKind::Archive => "archive",
        FileKind::Code => "code",
        FileKind::Document => "document",
        FileKind::Other => "other",
    }
}

/// Words that glue phrases together but carry no meaning alone. Dropped
/// only when adjacent to something that matched, so a file named `my` or
/// `all` stays findable. The verbs introduce a date or describe what the
/// user did ("photos *modified* last week", "pics I *took* yesterday"),
/// which is why `modified config` keeps both words. `files` reads as
/// noise next to a match ("rust *files*") and as a name on its own.
const FILLER: &[&str] = &[
    "from",
    "in",
    "my",
    "the",
    "of",
    "on",
    "at",
    "a",
    "an",
    "some",
    "any",
    "all",
    "that",
    "those",
    "these",
    "for",
    "i",
    "files",
    "file",
    "really",
    "very",
    "super",
    "modified",
    "created",
    "changed",
    "edited",
    "saved",
    "updated",
    "downloaded",
    "took",
    "taken",
    "made",
    "signed",
    "sent",
    "received",
    "shared",
];

const MONTHS: [&str; 12] = [
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
];

/// Three-letter month abbreviations, plus `sept`. Only ever read after a
/// preposition, like full month names.
const MONTH_ABBREVIATIONS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];

/// Sunday first, matching [`weekday_of`].
const WEEKDAYS: [&str; 7] = [
    "sunday",
    "monday",
    "tuesday",
    "wednesday",
    "thursday",
    "friday",
    "saturday",
];

/// Read `raw` as natural language, returning the residual filename text
/// and the phrases recognized. `now` (unix seconds) anchors relative
/// dates, keeping this pure. Words match greedily longest-first, so
/// `last month` wins over a bare `month`.
pub fn expand(raw: &str, now: i64) -> Expansion {
    expand_inner(raw, now, true)
}

/// [`expand`], minus the single-word rule — a lone word reads as a
/// *description* ("screenshots" ⇒ `kind:image`), not a filename. That rule
/// is specific to the search bar: once a verb has established intent, the
/// object of that verb is unambiguously a description, and applying it
/// would make `delete screenshots` mean "delete files *named*
/// screenshots". Only [`crate::magic`] needs this.
pub fn expand_as_description(raw: &str, now: i64) -> Expansion {
    expand_inner(raw, now, false)
}

fn expand_inner(raw: &str, now: i64, lone_word_is_filename: bool) -> Expansion {
    let words: Vec<&str> = raw.split_whitespace().collect();
    let single_word_query = lone_word_is_filename && words.len() == 1;

    let mut text: Vec<&str> = Vec::new();
    let mut phrases: Vec<Phrase> = Vec::new();
    let mut i = 0;

    while i < words.len() {
        // An explicit key:value token is the user being precise; never
        // touch it, and never let it be swallowed as filler.
        if words[i].contains(':') {
            text.push(words[i]);
            i += 1;
            continue;
        }

        match match_at(&words[i..], now) {
            // Rule 2: a lone word that *is* the entire query is much more
            // likely a filename than a description.
            Some((len, _)) if single_word_query && len == 1 => {
                text.push(words[i]);
                i += 1;
            }
            Some((len, filters)) => {
                // Absorb filler words immediately before the match
                // ("photos *from* last week"), now that we know one
                // followed.
                while text.last().is_some_and(|w| is_filler(w)) {
                    text.pop();
                }
                phrases.push(Phrase {
                    source: words[i..i + len].join(" "),
                    filters,
                });
                i += len;
                // ...and filler immediately after it ("photos *from*
                // meeting") — adjacency is what makes a word noise rather
                // than a filename. But filler that *starts* a phrase isn't
                // filler: "photos from june" needs its "from".
                while words.get(i).is_some_and(|w| is_filler(w))
                    && match_at(&words[i..], now).is_none()
                {
                    i += 1;
                }
            }
            None => {
                // A comparative whose operand didn't parse ("older than
                // yesterday"). The operand must not then expand on its own:
                // "yesterday" is what's being compared *against*, so
                // "modified during yesterday" is the near-opposite. The
                // whole failed attempt stays literal text.
                let failed_comparative = is_comparative_lead(words[i])
                    && words
                        .get(i + 1)
                        .is_some_and(|w| w.eq_ignore_ascii_case("than"));
                let span = if failed_comparative {
                    3.min(words.len() - i)
                } else {
                    1
                };
                text.extend_from_slice(&words[i..i + span]);
                i += span;
            }
        }
    }

    // Trailing filler is only noise if something else matched.
    if !phrases.is_empty() {
        while text.last().is_some_and(|w| is_filler(w)) {
            text.pop();
        }
    }

    Expansion {
        text: text.join(" "),
        phrases,
    }
}

fn is_filler(word: &str) -> bool {
    FILLER.contains(&word.to_ascii_lowercase().as_str())
}

/// Words that open a comparative phrase — see [`comparative`] and
/// [`size_threshold`]. A lead listed here but not handled there just
/// means a failed comparative stays text, which is the safe direction.
const COMPARATIVE_LEADS: &[&str] = &["older", "newer", "bigger", "larger", "smaller"];

fn is_comparative_lead(word: &str) -> bool {
    COMPARATIVE_LEADS.contains(&word.to_ascii_lowercase().as_str())
}

/// The most words one phrase can span. Five is what the longest
/// supported form needs (`a couple of days ago`); the cost is a fixed
/// handful of table probes per word of a short query string, nowhere
/// near the index scan.
const MAX_PHRASE_WORDS: usize = 5;

/// Try to match a phrase at the start of `words`, longest first.
/// Returns how many words it consumed and what they mean.
fn match_at(words: &[&str], now: i64) -> Option<(usize, Vec<Filter>)> {
    for len in (1..=MAX_PHRASE_WORDS.min(words.len())).rev() {
        let phrase = words[..len].join(" ").to_ascii_lowercase();
        if let Some(filters) = lookup(&phrase, now) {
            return Some((len, filters));
        }
    }
    None
}

/// The phrase table. One place to add vocabulary.
fn lookup(phrase: &str, now: i64) -> Option<Vec<Filter>> {
    if let Some(kind) = kind_of(phrase) {
        return Some(vec![Filter::Kind(kind)]);
    }
    if let Some(ext) = ext_of(phrase) {
        return Some(vec![Filter::Ext(ext)]);
    }

    // Sizes. Thresholds are round numbers a person would recognize, not
    // tuned constants.
    match phrase {
        "big" | "large" | "huge" => {
            return Some(vec![Filter::Size(Bound::Gt(100 * 1024 * 1024))]);
        }
        "small" | "tiny" => return Some(vec![Filter::Size(Bound::Lt(1024 * 1024))]),
        "empty" | "zero byte" | "zero bytes" | "zero-byte" | "0 bytes" => {
            return Some(vec![Filter::Size(Bound::Eq(0))]);
        }
        _ => {}
    }

    if let Some(filter) = size_threshold(phrase).or_else(|| comparative(phrase, now)) {
        return Some(vec![filter]);
    }

    time_phrase(phrase, now).map(|bound| vec![Filter::Modified(bound)])
}

/// Words naming a broad kind of file, singular and plural. A screenshot
/// is an image *named* like one; the index has no screenshot bit.
fn kind_of(phrase: &str) -> Option<FileKind> {
    Some(match phrase {
        "photos" | "pictures" | "images" | "photo" | "picture" | "image" | "pics" | "pic"
        | "snaps" | "screenshots" | "screenshot" | "wallpapers" => FileKind::Image,
        "videos" | "movies" | "video" | "movie" | "clips" | "films" | "footage" => FileKind::Video,
        "music" | "songs" | "audio" | "song" | "tracks" | "podcasts" => FileKind::Audio,
        "documents" | "document" | "docs" | "spreadsheets" | "spreadsheet" | "sheets" | "excel"
        | "slides" | "presentations" => FileKind::Document,
        "folders" | "directories" | "folder" | "dirs" => FileKind::Directory,
        "code" | "source code" | "scripts" | "source files" => FileKind::Code,
        "archives" | "zips" | "archive" | "zip" => FileKind::Archive,
        _ => return None,
    })
}

/// Languages people name instead of their extension ("rust files").
const LANGUAGE_EXTENSIONS: &[(&str, &str)] = &[
    ("rust", "rs"),
    ("python", "py"),
    ("javascript", "js"),
    ("typescript", "ts"),
    ("golang", "go"),
    ("ruby", "rb"),
    ("kotlin", "kt"),
];

/// Known extensions that are also everyday words; as a query word they
/// are far likelier to be part of a name.
const EXTENSION_HOMONYMS: &[&str] = &["opus", "swift"];

/// An extension spoken as a word — `pdfs`, `mp3s`, `docx`, `python` —
/// for any extension [`FileKind::of`] already classifies. Two-letter
/// extensions (`rs`, `go`, `md`) are too short to tell from ordinary
/// words, so they only arrive via [`LANGUAGE_EXTENSIONS`].
fn ext_of(phrase: &str) -> Option<String> {
    if let Some((_, ext)) = LANGUAGE_EXTENSIONS.iter().find(|(word, _)| *word == phrase) {
        return Some((*ext).to_string());
    }
    [Some(phrase), phrase.strip_suffix('s')]
        .into_iter()
        .flatten()
        .find(|ext| {
            ext.len() >= 3
                && ext.chars().all(|c| c.is_ascii_alphanumeric())
                && !EXTENSION_HOMONYMS.contains(ext)
                && FileKind::of_extension(ext) != FileKind::Other
        })
        .map(str::to_string)
}

/// Comparative age phrases carrying an open-ended operand: `older than
/// 30 days`, `newer than a week` — parsed rather than looked up in the
/// table above. The literal `than` is required: nobody types `older 30
/// days`, and matching loosely would start claiming words out of
/// filenames.
fn comparative(phrase: &str, now: i64) -> Option<Filter> {
    let (word, operand) = phrase.split_once(" than ")?;
    // Older = modified *before* the threshold, so the comparator flips
    // relative to the word — same inversion `modified:>30d` makes in the
    // `key:value` grammar.
    let threshold = now.saturating_sub(duration_secs(operand)?);
    match word {
        "older" => Some(Filter::Modified(Bound::Lt(threshold))),
        "newer" => Some(Filter::Modified(Bound::Gt(threshold))),
        _ => None,
    }
}

/// Builds a size bound from its operand: `Bound::Gt`, `Bound::Lt`, ….
type SizeBound = fn(u64) -> Bound<u64>;

/// Size comparisons and the comparator each lead implies.
const SIZE_LEADS: &[(&str, SizeBound)] = &[
    ("bigger than", Bound::Gt),
    ("larger than", Bound::Gt),
    ("more than", Bound::Gt),
    ("over", Bound::Gt),
    ("above", Bound::Gt),
    ("smaller than", Bound::Lt),
    ("less than", Bound::Lt),
    ("under", Bound::Lt),
    ("below", Bound::Lt),
    ("at least", Bound::Ge),
    ("at most", Bound::Le),
];

/// `over 20 megabytes`, `smaller than 500kb`, `at least a gigabyte`. The
/// operand must carry a unit — "kids under 5" is a name, not a size.
fn size_threshold(phrase: &str) -> Option<Filter> {
    SIZE_LEADS.iter().find_map(|(lead, bound)| {
        let operand = phrase.strip_prefix(lead)?.strip_prefix(' ')?;
        Some(Filter::Size(bound(spoken_bytes(operand)?)))
    })
}

/// A size with a unit: compact (`10mb`, `1.5gb`) or spoken (`20
/// megabytes`, `a gigabyte`, `2 gigs`).
fn spoken_bytes(text: &str) -> Option<u64> {
    match *text.split_whitespace().collect::<Vec<_>>().as_slice() {
        [compact] if compact.contains(|c: char| c.is_ascii_alphabetic()) => parse_bytes(compact),
        [count, unit] => {
            let count = match count_word(count) {
                Some(n) => n.to_string(),
                None => count.to_string(),
            };
            parse_bytes(&format!("{count}{}", byte_unit(unit)?))
        }
        _ => None,
    }
}

fn byte_unit(word: &str) -> Option<&'static str> {
    Some(match word.strip_suffix('s').unwrap_or(word) {
        "byte" | "b" => "b",
        "kilobyte" | "kb" | "k" => "kb",
        "megabyte" | "meg" | "mb" => "mb",
        "gigabyte" | "gig" | "gb" => "gb",
        "terabyte" | "tb" => "tb",
        _ => return None,
    })
}

/// Quantities people spell out. `couple` and `few` are deliberately
/// round: nobody typing "a few days ago" means exactly three.
fn count_word(word: &str) -> Option<i64> {
    Some(match word {
        "a" | "an" | "one" => 1,
        "two" | "couple" => 2,
        "three" | "few" | "several" => 3,
        "four" => 4,
        "five" => 5,
        "six" => 6,
        "seven" => 7,
        "eight" => 8,
        "nine" => 9,
        "ten" => 10,
        "eleven" => 11,
        "twelve" => 12,
        _ => return None,
    })
}

/// A spoken duration — `30 days`, `a couple of weeks`, `1 year` — or the
/// compact `30d` the `modified:` grammar accepts, in seconds.
fn duration_secs(text: &str) -> Option<i64> {
    let (count, unit) = count_and_unit(text)?;
    count.checked_mul(unit)
}

/// A duration split into its count and its unit in seconds. Months and
/// years are the same rounded 30/365 days used throughout; nobody typing
/// "older than 6 months" means a calendar-exact boundary.
fn count_and_unit(text: &str) -> Option<(i64, i64)> {
    let mut words: Vec<&str> = text.split_whitespace().filter(|w| *w != "of").collect();
    let (count, unit) = match words.as_slice() {
        [compact] => compact.split_at(compact.find(|c: char| c.is_ascii_alphabetic())?),
        [.., _] => {
            let unit = words.pop()?;
            // "a couple", "a few": the article belongs to the quantity.
            if words.len() == 2 && matches!(words[0], "a" | "an") {
                words.remove(0);
            }
            let [count] = words.as_slice() else {
                return None;
            };
            (*count, unit)
        }
        [] => return None,
    };
    let count = count_word(count).or_else(|| count.parse().ok())?;
    if count < 0 {
        return None;
    }
    let secs = match unit.strip_suffix('s').unwrap_or(unit) {
        "hour" | "hr" | "h" => 3_600,
        "day" | "d" => DAY,
        "week" | "wk" | "w" => 7 * DAY,
        "month" => 30 * DAY,
        "year" | "yr" => 365 * DAY,
        _ => return None,
    };
    Some((count, secs))
}

/// Date phrases → an mtime bound. "week"/"month"/"year" are **rolling
/// windows**, matching the existing `modified:week` keyword rather than
/// giving the same word a second meaning. Named months, days and years
/// are true calendar ranges, which is unambiguously what "in June" means.
fn time_phrase(phrase: &str, now: i64) -> Option<Bound<i64>> {
    if let Some(bound) = relative_keyword(phrase, now) {
        return Some(bound);
    }
    if let Some(bound) = rolling_window(phrase, now) {
        return Some(bound);
    }
    if let Some(day) = phrase.strip_prefix("last ").and_then(weekday_index) {
        let start = previous_weekday(start_of_day(now), day);
        return Some(Bound::Range(start, start + DAY - 1));
    }
    if let Some(rest) = phrase.strip_prefix("since ") {
        return date_span(rest, now).map(|(lo, _)| Bound::Ge(lo));
    }
    if let Some(rest) = phrase.strip_prefix("before ") {
        return date_span(rest, now).map(|(lo, _)| Bound::Lt(lo));
    }
    if let Some(rest) = phrase.strip_prefix("after ") {
        return date_span(rest, now).map(|(_, hi)| Bound::Gt(hi));
    }

    // Named months, days and years require a preposition: "in june" /
    // "from 2025". A bare "june" or "2025" is far more likely to be part
    // of a filename ("june-plan.txt", "budget 2025"), and silently turning
    // it into an mtime filter would hide exactly the file being looked for.
    let dated = ["in ", "from ", "during ", "on "]
        .iter()
        .find_map(|p| phrase.strip_prefix(p))?;
    if let Some(day) = weekday_index(dated) {
        let start = previous_weekday(start_of_day(now), day);
        return Some(Bound::Range(start, start + DAY - 1));
    }
    calendar(dated, start_of_day(now)).map(|(lo, hi)| Bound::Range(lo, hi))
}

/// Fixed relative phrases.
fn relative_keyword(phrase: &str, now: i64) -> Option<Bound<i64>> {
    let today = start_of_day(now);
    Some(match phrase {
        "today" | "this morning" | "this afternoon" | "earlier today" | "tonight" => {
            Bound::Ge(today)
        }
        "yesterday" => Bound::Range(today - DAY, today - 1),
        "this week" | "recently" | "lately" | "recent" | "past week" | "earlier this week" => {
            Bound::Ge(now - 7 * DAY)
        }
        "last week" => Bound::Range(now - 14 * DAY, now - 7 * DAY),
        "this month" | "past month" | "earlier this month" => Bound::Ge(now - 30 * DAY),
        "last month" => Bound::Range(now - 60 * DAY, now - 30 * DAY),
        "this year" | "past year" | "earlier this year" => Bound::Ge(now - 365 * DAY),
        "last year" => Bound::Range(now - 730 * DAY, now - 365 * DAY),
        "last weekend" => {
            let saturday = last_weekend(today);
            Bound::Range(saturday, saturday + 2 * DAY - 1)
        }
        _ => return None,
    })
}

/// `past 3 days`, `last 2 weeks`, `within a month` — everything since —
/// and `two weeks ago` — a window one unit either side, since "two weeks
/// ago" is a rough point in time, not a boundary.
fn rolling_window(phrase: &str, now: i64) -> Option<Bound<i64>> {
    if let Some(span) = phrase.strip_suffix(" ago") {
        let (count, unit) = count_and_unit(span)?;
        let lo = now.saturating_sub(count.checked_add(1)?.checked_mul(unit)?);
        let hi = now.saturating_sub((count - 1).max(0).checked_mul(unit)?);
        return Some(Bound::Range(lo, hi));
    }
    let span = ["past ", "last ", "within "]
        .iter()
        .find_map(|lead| phrase.strip_prefix(lead))?;
    Some(Bound::Ge(now.saturating_sub(duration_secs(span)?)))
}

/// The inclusive unix-second span a date names, for `since`/`before`/
/// `after`: anything [`relative_keyword`] knows, a weekday, or a
/// calendar date.
fn date_span(text: &str, now: i64) -> Option<(i64, i64)> {
    let today = start_of_day(now);
    if let Some(bound) = relative_keyword(text, now) {
        return match bound {
            Bound::Range(lo, hi) => Some((lo, hi)),
            Bound::Ge(lo) => Some((lo, now)),
            _ => None,
        };
    }
    let weekday = text.strip_prefix("last ").unwrap_or(text);
    if let Some(day) = weekday_index(weekday) {
        let start = previous_weekday(today, day);
        return Some((start, start + DAY - 1));
    }
    calendar(text, today)
}

/// A calendar month, year, month + year, or month + day, as an
/// inclusive unix-second span. A month or day with no year means the
/// most recent one that has started.
fn calendar(text: &str, today: i64) -> Option<(i64, i64)> {
    let (year, month, _) = civil_from_days(today.div_euclid(DAY));
    let words: Vec<&str> = text.split_whitespace().collect();
    match words.as_slice() {
        [word] => {
            if let Some(m) = month_index(word) {
                let y = if m > month { year - 1 } else { year };
                return Some(month_range(y, m));
            }
            let y = plausible_year(word)?;
            Some((
                days_from_civil(y, 1, 1) * DAY,
                days_from_civil(y + 1, 1, 1) * DAY - 1,
            ))
        }
        [first, second] => {
            if let (Some(m), Some(y)) = (month_index(first), plausible_year(second)) {
                return Some(month_range(y, m));
            }
            let (m, d) = match (month_index(first), month_index(second)) {
                (Some(m), _) => (m, day_of_month(second)?),
                (_, Some(m)) => (m, day_of_month(first)?),
                _ => return None,
            };
            let mut start = days_from_civil(year, m, d) * DAY;
            if start > today {
                start = days_from_civil(year - 1, m, d) * DAY;
            }
            // Reject the 31st of a 30-day month rather than rolling over.
            let (_, real_month, _) = civil_from_days(start.div_euclid(DAY));
            (real_month == m).then_some((start, start + DAY - 1))
        }
        _ => None,
    }
}

fn month_index(word: &str) -> Option<i64> {
    let found = MONTHS
        .iter()
        .position(|m| *m == word)
        .or_else(|| MONTH_ABBREVIATIONS.iter().position(|m| *m == word))
        .or_else(|| (word == "sept").then_some(8))?;
    Some(found as i64 + 1)
}

fn plausible_year(word: &str) -> Option<i64> {
    word.parse()
        .ok()
        .filter(|year| (1970..=2200).contains(year))
}

/// `3`, `3rd`, `21st` → a day of the month.
fn day_of_month(word: &str) -> Option<i64> {
    let digits = word.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    let suffix = &word[digits.len()..];
    if !matches!(suffix, "" | "st" | "nd" | "rd" | "th") {
        return None;
    }
    digits.parse().ok().filter(|d| (1..=31).contains(d))
}

fn weekday_index(word: &str) -> Option<i64> {
    WEEKDAYS.iter().position(|d| *d == word).map(|d| d as i64)
}

/// Day of the week for a UTC midnight, Sunday = 0. 1970-01-01 was a
/// Thursday.
fn weekday_of(day_start: i64) -> i64 {
    (day_start.div_euclid(DAY) + 4).rem_euclid(7)
}

/// The most recent `weekday` strictly before `today` — "last tuesday"
/// said on a Tuesday means a week ago, not today.
fn previous_weekday(today: i64, weekday: i64) -> i64 {
    let back = (weekday_of(today) - weekday).rem_euclid(7);
    let back = if back == 0 { 7 } else { back };
    today - back * DAY
}

/// The Saturday starting the most recent weekend that is over. On a
/// Sunday the current weekend is still running, so it means the one
/// before.
fn last_weekend(today: i64) -> i64 {
    if weekday_of(today) == 0 {
        today - 8 * DAY
    } else {
        previous_weekday(today, 6)
    }
}

/// An inclusive unix-second span covering all of calendar month `m`.
fn month_range(y: i64, m: i64) -> (i64, i64) {
    let start = days_from_civil(y, m, 1) * DAY;
    let (next_y, next_m) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
    (start, days_from_civil(next_y, next_m, 1) * DAY - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-07-26 12:00 UTC — the "now" every date test is anchored to.
    const NOW: i64 = 1_785_067_200;

    fn expand_at(raw: &str) -> Expansion {
        expand(raw, NOW)
    }

    fn filters(raw: &str) -> Vec<Filter> {
        expand_at(raw).filters()
    }

    fn sources(raw: &str) -> Vec<String> {
        expand_at(raw)
            .phrases
            .into_iter()
            .map(|p| p.source)
            .collect()
    }

    /// The single `modified:` bound a query expanded to — most queries
    /// here also carry a `kind:`, so positional indexing is a trap.
    fn modified(raw: &str) -> Bound<i64> {
        let mut found = filters(raw).into_iter().filter_map(|f| match f {
            Filter::Modified(bound) => Some(bound),
            _ => None,
        });
        let bound = found.next().expect("expected a modified filter");
        assert!(
            found.next().is_none(),
            "expected exactly one modified filter"
        );
        bound
    }

    /// The inclusive calendar day range of a `Bound::Range`.
    fn range_days(bound: Bound<i64>) -> ((i64, i64, i64), (i64, i64, i64)) {
        let Bound::Range(lo, hi) = bound else {
            panic!("expected a range, got {bound:?}")
        };
        (
            civil_from_days(lo.div_euclid(DAY)),
            civil_from_days(hi.div_euclid(DAY)),
        )
    }

    #[test]
    fn now_constant_is_the_date_the_tests_assume() {
        // Guards every date assertion below against a typo'd constant.
        assert_eq!(civil_from_days(NOW.div_euclid(DAY)), (2026, 7, 26));
    }

    #[test]
    fn kind_words_expand_when_the_query_has_more_to_it() {
        assert_eq!(
            filters("photos from last week")[0],
            Filter::Kind(FileKind::Image)
        );
        assert_eq!(filters("holiday videos")[0], Filter::Kind(FileKind::Video));
        assert_eq!(
            filters("work documents")[0],
            Filter::Kind(FileKind::Document)
        );
    }

    #[test]
    fn a_lone_word_is_treated_as_a_filename_not_a_description() {
        // The rule that keeps a file named "photos" findable.
        for query in ["photos", "videos", "big", "code", "today"] {
            let expansion = expand_at(query);
            assert!(expansion.is_empty(), "{query:?} should not expand alone");
            assert_eq!(expansion.text, query);
        }
    }

    #[test]
    fn a_multi_word_phrase_expands_even_as_the_whole_query() {
        // Nobody names a file "last week".
        let expansion = expand_at("last week");
        assert_eq!(expansion.phrases.len(), 1);
        assert_eq!(expansion.text, "");
    }

    #[test]
    fn residual_text_survives_alongside_filters() {
        let expansion = expand_at("invoice pdfs");
        assert_eq!(expansion.text, "invoice");
        assert_eq!(expansion.filters(), vec![Filter::Ext("pdf".into())]);
    }

    #[test]
    fn filler_words_are_dropped_only_next_to_a_match() {
        assert_eq!(expand_at("photos from last week").text, "");
        // No phrase matched, so "from" is just text the user typed.
        assert_eq!(expand_at("notes from meeting").text, "notes from meeting");
    }

    #[test]
    fn explicit_key_value_tokens_are_never_rewritten() {
        let expansion = expand_at("kind:video photos");
        assert_eq!(expansion.text, "kind:video");
        // The phrase still expands; the caller ANDs both and gets nothing,
        // which is the honest result of asking for two kinds at once.
        assert_eq!(expansion.filters(), vec![Filter::Kind(FileKind::Image)]);
    }

    #[test]
    fn longest_phrase_wins() {
        // "last month" must not be read as filler + "month".
        assert_eq!(sources("last month reports"), ["last month"]);
    }

    #[test]
    fn source_text_is_exactly_what_to_remove_to_undo() {
        let expansion = expand_at("photos from last week");
        let sources: Vec<&str> = expansion
            .phrases
            .iter()
            .map(|p| p.source.as_str())
            .collect();
        assert_eq!(sources, ["photos", "last week"]);
    }

    #[test]
    fn today_and_yesterday_bound_the_right_days() {
        let today = start_of_day(NOW);
        assert_eq!(modified("photos today"), Bound::Ge(today));
        assert_eq!(
            modified("photos yesterday"),
            Bound::Range(today - DAY, today - 1)
        );
    }

    #[test]
    fn named_month_resolves_to_the_most_recent_occurrence() {
        // June 2026 has already happened at NOW (2026-07-26).
        assert_eq!(
            range_days(modified("photos in june")),
            ((2026, 6, 1), (2026, 6, 30))
        );
    }

    #[test]
    fn a_month_still_to_come_this_year_means_last_year() {
        // December 2026 hasn't happened yet at NOW.
        assert_eq!(
            range_days(modified("photos in december")),
            ((2025, 12, 1), (2025, 12, 31))
        );
    }

    #[test]
    fn december_january_boundary_does_not_wrap() {
        let Bound::Range(lo, hi) = modified("photos in december") else {
            panic!("expected a range");
        };
        assert!(hi > lo);
        assert_eq!(hi - lo + 1, 31 * DAY, "December is 31 days");
    }

    #[test]
    fn from_works_as_a_date_preposition_too() {
        assert_eq!(
            range_days(modified("photos from june")),
            ((2026, 6, 1), (2026, 6, 30))
        );
    }

    #[test]
    fn a_bare_month_or_year_stays_filename_text() {
        // "june-plan.txt" and "budget 2025" must stay findable — turning
        // these into mtime filters would hide the very file being sought.
        assert_eq!(expand_at("june plan").text, "june plan");
        assert_eq!(expand_at("budget 2025").text, "budget 2025");
        assert!(expand_at("june plan").is_empty());
    }

    #[test]
    fn february_length_follows_the_leap_rule() {
        // 2024 was a leap year; anchor "now" just after it.
        let after = days_from_civil(2024, 3, 15) * DAY;
        let Some(Bound::Range(lo, hi)) = time_phrase("in february", after) else {
            panic!("expected a range");
        };
        assert_eq!(hi - lo + 1, 29 * DAY, "February 2024 had 29 days");
    }

    #[test]
    fn explicit_year_needs_the_preposition() {
        // "in 2025" is a date; a bare "2025" is probably part of a name.
        assert!(time_phrase("in 2025", NOW).is_some());
        assert!(time_phrase("2025", NOW).is_none());
        assert_eq!(
            range_days(time_phrase("in 2025", NOW).unwrap()),
            ((2025, 1, 1), (2025, 12, 31))
        );
    }

    #[test]
    fn implausible_years_are_not_dates() {
        assert!(time_phrase("in 12", NOW).is_none());
        assert!(time_phrase("in 99999", NOW).is_none());
    }

    #[test]
    fn size_words_expand() {
        assert_eq!(
            filters("big videos"),
            vec![
                Filter::Size(Bound::Gt(100 * 1024 * 1024)),
                Filter::Kind(FileKind::Video)
            ]
        );
    }

    #[test]
    fn comparative_age_phrases_bound_mtime() {
        // "older" means modified *before* the threshold.
        assert_eq!(
            modified("screenshots older than 30 days"),
            Bound::Lt(NOW - 30 * DAY)
        );
        assert_eq!(
            modified("photos newer than 2 weeks"),
            Bound::Gt(NOW - 14 * DAY)
        );
        assert_eq!(
            modified("videos older than 1 year"),
            Bound::Lt(NOW - 365 * DAY)
        );
        assert_eq!(
            modified("logs older than 6 hours"),
            Bound::Lt(NOW - 6 * 3_600)
        );
    }

    #[test]
    fn comparative_size_phrases_bound_size() {
        assert_eq!(
            filters("videos bigger than 100mb"),
            vec![
                Filter::Kind(FileKind::Video),
                Filter::Size(Bound::Gt(100 * (1 << 20)))
            ]
        );
        assert_eq!(
            filters("photos smaller than 500kb"),
            vec![
                Filter::Kind(FileKind::Image),
                Filter::Size(Bound::Lt(500 * (1 << 10)))
            ]
        );
        // "larger" is the same comparator as "bigger".
        assert_eq!(
            filters("larger than 1gb"),
            vec![Filter::Size(Bound::Gt(1 << 30))]
        );
    }

    #[test]
    fn comparative_operands_accept_both_spellings() {
        // Spoken and compact forms of the same duration agree.
        assert_eq!(duration_secs("30 days"), duration_secs("30d"));
        assert_eq!(duration_secs("2 weeks"), duration_secs("2w"));
        // Singular and plural units agree too.
        assert_eq!(duration_secs("1 day"), Some(DAY));
        // ...and so do spaced and unspaced sizes.
        assert_eq!(filters("bigger than 10 mb"), filters("bigger than 10mb"),);
    }

    #[test]
    fn a_comparative_without_than_stays_filename_text() {
        // The literal "than" is what separates a comparative from words
        // that merely start with "older"/"bigger".
        for query in ["older 30 days", "bigger report", "older files"] {
            assert!(expand_at(query).is_empty(), "{query:?} should not expand");
        }
    }

    #[test]
    fn comparatives_with_nonsense_operands_do_not_expand() {
        for query in [
            "older than yesterday", // not a duration
            "bigger than lots",     // not a size
            "older than -5 days",   // negative
            "older than 5 fortnights",
        ] {
            assert!(expand_at(query).is_empty(), "{query:?} should not expand");
            assert_eq!(expand_at(query).text, query);
        }
    }

    #[test]
    fn a_comparative_is_undone_by_removing_its_words() {
        // The four-word phrase has to round-trip through the chip UI's
        // remove path like every shorter one.
        let expansion = expand_at("screenshots older than 30 days");
        assert_eq!(
            sources("screenshots older than 30 days"),
            ["screenshots", "older than 30 days"]
        );
        let mut query = "screenshots older than 30 days".to_string();
        for phrase in expansion.phrases {
            query = without_phrase(&query, &phrase.source);
        }
        assert_eq!(query, "");
    }

    #[test]
    fn a_huge_comparative_operand_does_not_overflow() {
        // i64 seconds overflows long before the year count does.
        assert_eq!(duration_secs("9223372036854775807 years"), None);
        assert!(expand_at("older than 9223372036854775807 years").is_empty());
    }

    #[test]
    fn unrecognized_text_is_left_completely_alone() {
        let expansion = expand_at("quarterly earnings deck");
        assert!(expansion.is_empty());
        assert_eq!(expansion.text, "quarterly earnings deck");
    }

    #[test]
    fn empty_and_whitespace_queries_are_safe() {
        for query in ["", "   ", "\t"] {
            let expansion = expand_at(query);
            assert!(expansion.is_empty());
            assert_eq!(expansion.text, "");
        }
    }

    #[test]
    fn matching_is_case_insensitive() {
        assert_eq!(
            filters("Photos From Last Week")[0],
            Filter::Kind(FileKind::Image)
        );
    }

    #[test]
    fn seasons_are_deliberately_not_recognized() {
        // Documented non-goal: "summer" is hemisphere- and culture-
        // dependent, so it stays plain text rather than becoming a
        // quietly-wrong date range.
        let expansion = expand_at("photos from last summer");
        assert_eq!(expansion.text, "last summer");
        assert_eq!(sources("photos from last summer"), ["photos"]);
    }

    #[test]
    fn filler_is_dropped_after_a_match_as_well_as_before() {
        assert_eq!(expand_at("photos from meeting").text, "meeting");
    }

    #[test]
    fn date_verbs_are_filler_only_next_to_a_date() {
        // "photos modified last week" is one phrase's worth of meaning
        // plus a connective; the connective must not survive as filename
        // text, or the search asks for files *named* "modified".
        let expansion = expand_at("pdfs modified this week");
        assert_eq!(expansion.text, "");
        assert_eq!(
            expansion.filters(),
            vec![
                Filter::Ext("pdf".into()),
                Filter::Modified(Bound::Ge(NOW - 7 * DAY)),
            ]
        );
        // With nothing matching around it, it stays a searchable word.
        assert_eq!(expand_at("modified config").text, "modified config");
    }

    #[test]
    fn without_phrase_removes_single_and_multi_word_sources() {
        assert_eq!(
            without_phrase("photos from last week", "last week"),
            "photos from"
        );
        assert_eq!(
            without_phrase("photos from last week", "photos"),
            "from last week"
        );
        assert_eq!(
            without_phrase("Photos From Last Week", "last week"),
            "Photos From"
        );
    }

    #[test]
    fn without_phrase_leaves_a_query_it_cannot_find_untouched() {
        assert_eq!(
            without_phrase("invoice notes", "last week"),
            "invoice notes"
        );
        assert_eq!(without_phrase("invoice", ""), "invoice");
    }

    #[test]
    fn without_phrase_removes_only_the_first_occurrence() {
        assert_eq!(without_phrase("photos photos", "photos"), "photos");
    }

    #[test]
    fn labels_are_canonical_for_kinds_and_verbatim_for_dates() {
        assert_eq!(
            label_for(&Filter::Kind(FileKind::Image), "photos"),
            "kind:image"
        );
        assert_eq!(label_for(&Filter::Ext("pdf".into()), "pdfs"), "ext:pdf");
        assert_eq!(
            label_for(&Filter::Modified(Bound::Ge(0)), "last week"),
            "last week"
        );
        assert_eq!(label_for(&Filter::Size(Bound::Gt(0)), "big"), "big");
    }

    #[test]
    fn every_expansion_can_be_undone_by_removing_its_source() {
        // The property the chip UI relies on: strip each phrase's source
        // and the query stops expanding.
        let raw = "invoice photos from last week";
        let mut query = raw.to_string();
        for phrase in expand_at(raw).phrases {
            query = without_phrase(&query, &phrase.source);
        }
        assert!(expand(&query, NOW).is_empty(), "left over: {query:?}");
        assert!(query.contains("invoice"), "the real search text survives");
    }

    #[test]
    fn now_is_a_sunday() {
        // The weekday tests below lean on this.
        assert_eq!(weekday_of(start_of_day(NOW)), 0);
    }

    #[test]
    fn everyday_kind_words_expand() {
        assert_eq!(
            filters("pics from last week")[0],
            Filter::Kind(FileKind::Image)
        );
        assert_eq!(filters("holiday snaps")[0], Filter::Kind(FileKind::Image));
        assert_eq!(
            filters("spreadsheets today")[0],
            Filter::Kind(FileKind::Document)
        );
        assert_eq!(filters("phone clips")[0], Filter::Kind(FileKind::Video));
    }

    #[test]
    fn spoken_extensions_become_ext_filters() {
        let expansion = expand_at("resume docx");
        assert_eq!(expansion.text, "resume");
        assert_eq!(expansion.filters(), vec![Filter::Ext("docx".into())]);
        assert_eq!(filters("mp3s today")[0], Filter::Ext("mp3".into()));
        assert_eq!(filters("rust files today")[0], Filter::Ext("rs".into()));
        assert_eq!(filters("python scripts")[0], Filter::Ext("py".into()));
    }

    #[test]
    fn short_or_homonym_extensions_stay_text() {
        // `go`, `md`, `swift` are words long before they are extensions.
        for query in ["go home", "md notes", "swift notes"] {
            assert!(expand_at(query).is_empty(), "{query:?} should not expand");
        }
    }

    #[test]
    fn ago_is_a_window_around_the_point() {
        assert_eq!(
            modified("photos two weeks ago"),
            Bound::Range(NOW - 21 * DAY, NOW - 7 * DAY)
        );
        assert_eq!(
            modified("zip from a couple of days ago"),
            Bound::Range(NOW - 3 * DAY, NOW - DAY)
        );
    }

    #[test]
    fn past_and_last_n_units_are_rolling() {
        assert_eq!(modified("logs past 3 days"), Bound::Ge(NOW - 3 * DAY));
        let expansion = expand_at("logs in the last 2 weeks");
        assert_eq!(expansion.text, "logs");
        assert_eq!(
            expansion.filters(),
            vec![Filter::Modified(Bound::Ge(NOW - 14 * DAY))]
        );
        assert_eq!(
            modified("notes from the past few days"),
            Bound::Ge(NOW - 3 * DAY)
        );
    }

    #[test]
    fn last_weekday_is_the_most_recent_one_before_today() {
        assert_eq!(
            range_days(modified("contract last tuesday")),
            ((2026, 7, 21), (2026, 7, 21))
        );
        // NOW is a Sunday: "last sunday" is a week ago, not today.
        assert_eq!(
            range_days(modified("notes last sunday")),
            ((2026, 7, 19), (2026, 7, 19))
        );
        assert_eq!(
            range_days(modified("notes on friday")),
            ((2026, 7, 24), (2026, 7, 24))
        );
    }

    #[test]
    fn last_weekend_on_a_sunday_means_the_previous_one() {
        assert_eq!(
            range_days(modified("pics I took last weekend")),
            ((2026, 7, 18), (2026, 7, 19))
        );
        let weekday_now = days_from_civil(2026, 7, 22) * DAY; // Wednesday
        let Some(Bound::Range(lo, hi)) = time_phrase("last weekend", weekday_now) else {
            panic!("expected a range");
        };
        assert_eq!(
            range_days(Bound::Range(lo, hi)),
            ((2026, 7, 18), (2026, 7, 19))
        );
    }

    #[test]
    fn since_before_and_after_bound_one_side() {
        assert_eq!(
            modified("acme sheets since march 3"),
            Bound::Ge(days_from_civil(2026, 3, 3) * DAY)
        );
        assert_eq!(
            modified("backups before 2020"),
            Bound::Lt(days_from_civil(2020, 1, 1) * DAY)
        );
        assert_eq!(
            modified("photos after june"),
            Bound::Gt(days_from_civil(2026, 7, 1) * DAY - 1)
        );
        assert_eq!(
            modified("logs since yesterday"),
            Bound::Ge(start_of_day(NOW) - DAY)
        );
    }

    #[test]
    fn calendar_dates_with_a_preposition() {
        assert_eq!(
            range_days(modified("holiday snaps from december 2023")),
            ((2023, 12, 1), (2023, 12, 31))
        );
        assert_eq!(
            range_days(modified("receipts on 3rd march")),
            ((2026, 3, 3), (2026, 3, 3))
        );
        // August 15 hasn't come yet at NOW, so it means last year's.
        assert_eq!(
            range_days(modified("photos from aug 15")),
            ((2025, 8, 15), (2025, 8, 15))
        );
    }

    #[test]
    fn impossible_days_do_not_roll_over() {
        assert!(time_phrase("in february 30", NOW).is_none());
        assert!(time_phrase("in june 31", NOW).is_none());
    }

    #[test]
    fn bare_weekdays_and_dates_stay_filename_text() {
        for query in ["monday notes", "march 3 minutes", "before after photos"] {
            let expansion = expand_at(query);
            assert!(
                !expansion
                    .filters()
                    .iter()
                    .any(|f| matches!(f, Filter::Modified(_))),
                "{query:?} should not get a date"
            );
        }
    }

    #[test]
    fn spoken_size_thresholds() {
        assert_eq!(
            filters("mp3s over 20 megabytes")[1],
            Filter::Size(Bound::Gt(20 << 20))
        );
        let expansion = expand_at("files over a gigabyte");
        assert_eq!(expansion.text, "");
        assert_eq!(expansion.filters(), vec![Filter::Size(Bound::Gt(1 << 30))]);
        assert_eq!(
            filters("videos at least 2 gigs")[1],
            Filter::Size(Bound::Ge(2 << 30))
        );
        assert_eq!(filters("zero byte files"), vec![Filter::Size(Bound::Eq(0))]);
    }

    #[test]
    fn a_size_threshold_needs_a_unit() {
        // "under 5" is an age group in a filename, not five bytes.
        assert!(expand_at("kids under 5").is_empty());
    }

    #[test]
    fn what_the_user_did_is_filler_next_to_a_match() {
        assert_eq!(expand_at("contract I signed last tuesday").text, "contract");
        assert_eq!(expand_at("folders I made yesterday").text, "");
        // ...but stays searchable on its own.
        assert_eq!(expand_at("signed contract").text, "signed contract");
    }
}

//! Natural-language phrases → the existing `key:value` filter grammar.
//!
//! `photos from last week` becomes `kind:image` + `modified:…`, reusing
//! [`crate::search_filter`]'s [`Filter`] rather than inventing a parallel
//! query path. Strictly rule-based — no model, no inference, no runtime
//! cost worth measuring. This is block 4 of `docs/design-search-ranking.md`.
//!
//! Three rules keep it from feeling like magic that hides files:
//!
//! 1. **Every expansion is visible and removable.** The caller renders
//!    each [`Phrase`] as a chip; [`Phrase::source`] is the exact text to
//!    strip from the query to undo it.
//! 2. **A single word that is the whole query never expands.** Typing
//!    `photos` searches for files named "photos", because that is far
//!    more likely to be what a file explorer's user meant. `photos from
//!    last week` expands, because nobody names a file that.
//! 3. **Existing `key:value` tokens are untouched.** An explicit
//!    `kind:video` always wins; phrases never rewrite what the user
//!    spelled out.
//!
//! Deliberately **not** supported: seasons (`last summer`). They are
//! hemisphere- and culture-dependent — June is summer in Berlin and
//! monsoon season in Mumbai — so a fixed mapping would be quietly wrong
//! for half its users. Month names and years cover the same need without
//! guessing.

use crate::listing::FileKind;
use crate::search_filter::{
    Bound, Filter, SECS_PER_DAY as DAY, civil_from_days, days_from_civil, start_of_day,
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
        self.phrases.iter().flat_map(|p| p.filters.iter().cloned()).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.phrases.is_empty()
    }
}

/// `raw` with the first consecutive run of words equal to `source`
/// removed (case-insensitively) — the effect of removing an inferred
/// chip. The counterpart of
/// [`without_token`](crate::search_filter::without_token), which only
/// handles single `key:value` words.
pub fn without_phrase(raw: &str, source: &str) -> String {
    let words: Vec<&str> = raw.split_whitespace().collect();
    let target: Vec<String> =
        source.split_whitespace().map(|w| w.to_ascii_lowercase()).collect();
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

/// A short, honest label for a filter an expansion produced.
///
/// Kinds and extensions get their canonical `key:value` spelling, which
/// is what makes the expansion explainable ("my words became this
/// filter"). Sizes and dates fall back to the words the user actually
/// typed, since a raw [`Bound`] has no compact readable form.
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

/// Words that carry no meaning on their own but glue phrases together.
/// Dropped only when adjacent to something that matched, so a file named
/// `my` or `all` is still findable.
const FILLER: &[&str] = &["from", "my", "the", "of", "on", "at", "a", "an", "some", "any"];

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

/// Read `raw` as natural language, returning the residual filename text
/// and the phrases recognized. `now` (unix seconds) anchors every
/// relative date, so the function is pure and deterministic.
///
/// Words are matched greedily longest-first at each position, so
/// `last month` wins over a bare `month`.
pub fn expand(raw: &str, now: i64) -> Expansion {
    let words: Vec<&str> = raw.split_whitespace().collect();
    let single_word_query = words.len() == 1;

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
                phrases.push(Phrase { source: words[i..i + len].join(" "), filters });
                i += len;
                // ...and filler immediately after it ("photos *from*
                // meeting"): adjacency on either side is what makes a
                // word noise rather than a filename. But a filler word
                // that *starts* a phrase is not filler — "photos from
                // june" needs its "from" to read the date.
                while words.get(i).is_some_and(|w| is_filler(w))
                    && match_at(&words[i..], now).is_none()
                {
                    i += 1;
                }
            }
            None => {
                text.push(words[i]);
                i += 1;
            }
        }
    }

    // Trailing filler is only noise if something else matched.
    if !phrases.is_empty() {
        while text.last().is_some_and(|w| is_filler(w)) {
            text.pop();
        }
    }

    Expansion { text: text.join(" "), phrases }
}

fn is_filler(word: &str) -> bool {
    FILLER.contains(&word.to_ascii_lowercase().as_str())
}

/// Try to match a phrase at the start of `words`, longest first.
/// Returns how many words it consumed and what they mean.
fn match_at(words: &[&str], now: i64) -> Option<(usize, Vec<Filter>)> {
    for len in (1..=3.min(words.len())).rev() {
        let phrase = words[..len].join(" ").to_ascii_lowercase();
        if let Some(filters) = lookup(&phrase, now) {
            return Some((len, filters));
        }
    }
    None
}

/// The phrase table. One place to add vocabulary.
fn lookup(phrase: &str, now: i64) -> Option<Vec<Filter>> {
    // Kinds.
    let kind = match phrase {
        "photos" | "pictures" | "images" | "photo" | "picture" | "image" => Some(FileKind::Image),
        "videos" | "movies" | "video" | "movie" => Some(FileKind::Video),
        "music" | "songs" | "audio" | "song" => Some(FileKind::Audio),
        "documents" | "document" | "docs" => Some(FileKind::Document),
        "folders" | "directories" | "folder" => Some(FileKind::Directory),
        "code" | "source code" => Some(FileKind::Code),
        "archives" | "zips" | "archive" => Some(FileKind::Archive),
        _ => None,
    };
    if let Some(kind) = kind {
        return Some(vec![Filter::Kind(kind)]);
    }

    // Extensions that read as words.
    match phrase {
        "pdfs" | "pdf" => return Some(vec![Filter::Ext("pdf".into())]),
        // A screenshot is an image *named* like one; the index has no
        // screenshot bit, so this is deliberately two filters and the
        // caller sees both.
        "screenshots" | "screenshot" => {
            return Some(vec![Filter::Kind(FileKind::Image)]);
        }
        _ => {}
    }

    // Sizes. Thresholds are round numbers a person would recognize, not
    // tuned constants.
    match phrase {
        "big" | "large" | "huge" => {
            return Some(vec![Filter::Size(Bound::Gt(100 * 1024 * 1024))]);
        }
        "small" | "tiny" => return Some(vec![Filter::Size(Bound::Lt(1024 * 1024))]),
        "empty" => return Some(vec![Filter::Size(Bound::Eq(0))]),
        _ => {}
    }

    time_phrase(phrase, now).map(|bound| vec![Filter::Modified(bound)])
}

/// Date phrases → an mtime bound.
///
/// "week"/"month"/"year" are **rolling windows**, matching the existing
/// `modified:week` keyword rather than inventing a second meaning for the
/// same word. Named months and years are true calendar ranges, since
/// that is unambiguously what "in June" means.
fn time_phrase(phrase: &str, now: i64) -> Option<Bound<i64>> {
    let today = start_of_day(now);
    match phrase {
        "today" => return Some(Bound::Ge(today)),
        "yesterday" => return Some(Bound::Range(today - DAY, today - 1)),
        "this week" | "recently" | "lately" | "recent" => {
            return Some(Bound::Ge(now - 7 * DAY));
        }
        "last week" => return Some(Bound::Range(now - 14 * DAY, now - 7 * DAY)),
        "this month" => return Some(Bound::Ge(now - 30 * DAY)),
        "last month" => return Some(Bound::Range(now - 60 * DAY, now - 30 * DAY)),
        "this year" => return Some(Bound::Ge(now - 365 * DAY)),
        "last year" => return Some(Bound::Range(now - 730 * DAY, now - 365 * DAY)),
        _ => {}
    }

    // Named months and years require a preposition: "in june" / "from
    // 2025". A bare "june" or "2025" is far more likely to be part of a
    // filename ("june-plan.txt", "budget 2025"), and silently turning it
    // into an mtime filter would hide exactly the file being looked for.
    let dated = phrase
        .strip_prefix("in ")
        .or_else(|| phrase.strip_prefix("from "))?;

    if let Some(month) = MONTHS.iter().position(|m| *m == dated) {
        let month = month as i64 + 1;
        let (year, current_month, _) = civil_from_days(today.div_euclid(DAY));
        // If that month hasn't arrived yet this year, they mean last year.
        let year = if month > current_month { year - 1 } else { year };
        return Some(month_range(year, month));
    }

    if let Ok(year) = dated.parse::<i64>()
        && (1970..=2200).contains(&year)
    {
        return Some(Bound::Range(
            days_from_civil(year, 1, 1) * DAY,
            days_from_civil(year + 1, 1, 1) * DAY - 1,
        ));
    }

    None
}

/// An inclusive unix-second range covering all of calendar month `m`.
fn month_range(y: i64, m: i64) -> Bound<i64> {
    let start = days_from_civil(y, m, 1) * DAY;
    let (next_y, next_m) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
    Bound::Range(start, days_from_civil(next_y, next_m, 1) * DAY - 1)
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
        expand_at(raw).phrases.into_iter().map(|p| p.source).collect()
    }

    /// The single `modified:` bound a query expanded to — most queries
    /// here also carry a `kind:`, so positional indexing is a trap.
    fn modified(raw: &str) -> Bound<i64> {
        let mut found = filters(raw).into_iter().filter_map(|f| match f {
            Filter::Modified(bound) => Some(bound),
            _ => None,
        });
        let bound = found.next().expect("expected a modified filter");
        assert!(found.next().is_none(), "expected exactly one modified filter");
        bound
    }

    /// The inclusive calendar day range of a `Bound::Range`.
    fn range_days(bound: Bound<i64>) -> ((i64, i64, i64), (i64, i64, i64)) {
        let Bound::Range(lo, hi) = bound else { panic!("expected a range, got {bound:?}") };
        (civil_from_days(lo.div_euclid(DAY)), civil_from_days(hi.div_euclid(DAY)))
    }

    #[test]
    fn now_constant_is_the_date_the_tests_assume() {
        // Guards every date assertion below against a typo'd constant.
        assert_eq!(civil_from_days(NOW.div_euclid(DAY)), (2026, 7, 26));
    }

    #[test]
    fn kind_words_expand_when_the_query_has_more_to_it() {
        assert_eq!(filters("photos from last week")[0], Filter::Kind(FileKind::Image));
        assert_eq!(filters("holiday videos")[0], Filter::Kind(FileKind::Video));
        assert_eq!(filters("work documents")[0], Filter::Kind(FileKind::Document));
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
        let sources: Vec<&str> = expansion.phrases.iter().map(|p| p.source.as_str()).collect();
        assert_eq!(sources, ["photos", "last week"]);
    }

    #[test]
    fn today_and_yesterday_bound_the_right_days() {
        let today = start_of_day(NOW);
        assert_eq!(modified("photos today"), Bound::Ge(today));
        assert_eq!(modified("photos yesterday"), Bound::Range(today - DAY, today - 1));
    }

    #[test]
    fn named_month_resolves_to_the_most_recent_occurrence() {
        // June 2026 has already happened at NOW (2026-07-26).
        assert_eq!(range_days(modified("photos in june")), ((2026, 6, 1), (2026, 6, 30)));
    }

    #[test]
    fn a_month_still_to_come_this_year_means_last_year() {
        // December 2026 hasn't happened yet at NOW.
        assert_eq!(range_days(modified("photos in december")), ((2025, 12, 1), (2025, 12, 31)));
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
        assert_eq!(range_days(modified("photos from june")), ((2026, 6, 1), (2026, 6, 30)));
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
            vec![Filter::Size(Bound::Gt(100 * 1024 * 1024)), Filter::Kind(FileKind::Video)]
        );
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
        assert_eq!(filters("Photos From Last Week")[0], Filter::Kind(FileKind::Image));
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
    fn without_phrase_removes_single_and_multi_word_sources() {
        assert_eq!(without_phrase("photos from last week", "last week"), "photos from");
        assert_eq!(without_phrase("photos from last week", "photos"), "from last week");
        assert_eq!(without_phrase("Photos From Last Week", "last week"), "Photos From");
    }

    #[test]
    fn without_phrase_leaves_a_query_it_cannot_find_untouched() {
        assert_eq!(without_phrase("invoice notes", "last week"), "invoice notes");
        assert_eq!(without_phrase("invoice", ""), "invoice");
    }

    #[test]
    fn without_phrase_removes_only_the_first_occurrence() {
        assert_eq!(without_phrase("photos photos", "photos"), "photos");
    }

    #[test]
    fn labels_are_canonical_for_kinds_and_verbatim_for_dates() {
        assert_eq!(label_for(&Filter::Kind(FileKind::Image), "photos"), "kind:image");
        assert_eq!(label_for(&Filter::Ext("pdf".into()), "pdfs"), "ext:pdf");
        assert_eq!(label_for(&Filter::Modified(Bound::Ge(0)), "last week"), "last week");
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
}

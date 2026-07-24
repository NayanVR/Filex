//! Structured search filters (Phase 2d, block 8 item 2 — see
//! `docs/design-search-chips.md`).
//!
//! One pure tokenizer, [`parse_query`], splits a raw search string into
//! filename [`text`](Query::text) plus a set of typed [`Filter`]s
//! (`kind:`, `ext:`, `size:`, `modified:`, and the item-1 `tag:`).
//! Unrecognized `key:` tokens — and known keys with an unparseable value —
//! fall back to literal text, so the search field never rejects input.
//!
//! This is the shared `key:value` grammar the roadmap calls for; it holds
//! no state and no GPUI, so it is unit-tested in isolation. `kind:`/`ext:`
//! are derivable from the filename the index already stores; `size:` and
//! `modified:` need the size/mtime fields that block-8's index schema
//! change adds (until then, [`Filter::matches`] reports them unknown, so
//! they never positively match).

use serde::{Deserialize, Serialize};

use crate::listing::FileKind;

/// Seconds in a day — the unit the date grammar reasons in.
const DAY: i64 = 86_400;

/// A comparison a numeric field must satisfy: `<op> operand`, or an
/// inclusive `lo..=hi` range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Bound<T> {
    Lt(T),
    Le(T),
    Gt(T),
    Ge(T),
    Eq(T),
    /// Inclusive on both ends.
    Range(T, T),
}

impl<T: PartialOrd + Copy> Bound<T> {
    /// Does `value` satisfy this bound?
    pub fn matches(&self, value: T) -> bool {
        match *self {
            Bound::Lt(x) => value < x,
            Bound::Le(x) => value <= x,
            Bound::Gt(x) => value > x,
            Bound::Ge(x) => value >= x,
            Bound::Eq(x) => value == x,
            Bound::Range(lo, hi) => lo <= value && value <= hi,
        }
    }
}

/// One structured filter parsed out of the query. Multiple filters AND.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Filter {
    /// `tag:NAME` — lowercased; evaluated against the sidecar tag store,
    /// not the index (see [`crate::tags`]).
    Tag(String),
    /// `kind:image` — a [`FileKind`], derived from the filename.
    Kind(FileKind),
    /// `ext:pdf` — a lowercased extension, no leading dot.
    Ext(String),
    /// `size:>2mb` — bytes (base-1024 units).
    Size(Bound<u64>),
    /// `modified:today` — unix seconds.
    Modified(Bound<i64>),
}

/// A search query split into filename text and its structured filters.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Query {
    /// Filename text, with the filter tokens removed (may be empty).
    pub text: String,
    /// The filters; a path must satisfy all of them (AND).
    pub filters: Vec<Filter>,
}

impl Query {
    /// Nothing to search on — no text and no filters.
    pub fn is_empty(&self) -> bool {
        self.text.is_empty() && self.filters.is_empty()
    }
}

/// The metadata a [`Filter`] is evaluated against. `size`/`mtime` are
/// `None` when the index doesn't (yet) carry them for this entry — a
/// best-effort unknown that never positively matches a `size:`/`modified:`
/// filter.
#[derive(Debug, Clone, Copy)]
pub struct ItemMeta<'a> {
    pub name: &'a str,
    pub is_dir: bool,
    pub size: Option<u64>,
    pub mtime: Option<i64>,
}

impl Filter {
    /// Does `item` satisfy this filter? [`Filter::Tag`] always returns
    /// `true` here — tags live in the sidecar store and are intersected
    /// separately by the caller, so a tag filter never rejects on
    /// filename metadata alone.
    pub fn matches(&self, item: &ItemMeta) -> bool {
        match self {
            Filter::Tag(_) => true,
            Filter::Kind(kind) => FileKind::of(item.name, item.is_dir) == *kind,
            Filter::Ext(ext) => extension_of(item.name).as_deref() == Some(ext.as_str()),
            Filter::Size(bound) => item.size.is_some_and(|s| bound.matches(s)),
            Filter::Modified(bound) => item.mtime.is_some_and(|m| bound.matches(m)),
        }
    }
}

/// Split a raw query into filename text and structured filters. `now` is
/// the reference instant (unix seconds) for relative/keyword dates, passed
/// in so the parse is pure and deterministic.
pub fn parse_query(raw: &str, now: i64) -> Query {
    let mut text_words = Vec::new();
    let mut filters = Vec::new();
    for word in raw.split_whitespace() {
        match parse_token(word, now) {
            Some(filter) => {
                if !filters.contains(&filter) {
                    filters.push(filter);
                }
            }
            None => text_words.push(word),
        }
    }
    Query { text: text_words.join(" "), filters }
}

/// Try to read one word as a `key:value` filter. `None` means "treat it as
/// filename text" — an unknown key, a missing value, or a value the key
/// can't parse.
fn parse_token(word: &str, now: i64) -> Option<Filter> {
    let (key, value) = word.split_once(':')?;
    if value.is_empty() {
        return None;
    }
    match key.to_ascii_lowercase().as_str() {
        "tag" => Some(Filter::Tag(value.to_lowercase())),
        "kind" => parse_kind(value).map(Filter::Kind),
        "ext" => Some(Filter::Ext(value.trim_start_matches('.').to_ascii_lowercase())),
        "size" => parse_size_bound(value).map(Filter::Size),
        "modified" => parse_time_bound(value, now).map(Filter::Modified),
        _ => None,
    }
}

/// Map a `kind:` word onto a [`FileKind`], accepting a few natural
/// synonyms. `None` for an unknown kind (falls back to text).
fn parse_kind(value: &str) -> Option<FileKind> {
    Some(match value.to_ascii_lowercase().as_str() {
        "image" | "img" | "photo" | "picture" => FileKind::Image,
        "video" | "movie" => FileKind::Video,
        "audio" | "music" | "sound" => FileKind::Audio,
        "archive" | "zip" => FileKind::Archive,
        "code" | "source" => FileKind::Code,
        "document" | "doc" | "docs" => FileKind::Document,
        "folder" | "dir" | "directory" => FileKind::Directory,
        "other" | "file" => FileKind::Other,
        _ => return None,
    })
}

/// The lowercased final extension of a filename, or `None` (no extension,
/// or a dotfile like `.bashrc`). Matches [`FileKind::of`]'s notion of the
/// extension.
fn extension_of(name: &str) -> Option<String> {
    std::path::Path::new(name).extension().map(|e| e.to_string_lossy().to_ascii_lowercase())
}

/// Split a leading comparator (`>=`, `<=`, `>`, `<`) off a value.
/// Returns the comparator constructor and the remaining string.
fn split_comparator<T>(value: &str) -> (fn(T) -> Bound<T>, &str) {
    if let Some(rest) = value.strip_prefix(">=") {
        (Bound::Ge, rest)
    } else if let Some(rest) = value.strip_prefix("<=") {
        (Bound::Le, rest)
    } else if let Some(rest) = value.strip_prefix('>') {
        (Bound::Gt, rest)
    } else if let Some(rest) = value.strip_prefix('<') {
        (Bound::Lt, rest)
    } else {
        (Bound::Eq, value)
    }
}

/// Parse a `size:` value: `>2mb`, `<=500kb`, `1mb..5mb`, or a bare `2gb`
/// (exact). Base-1024 units (b/kb/mb/gb/tb), decimals allowed.
fn parse_size_bound(value: &str) -> Option<Bound<u64>> {
    if let Some((lo, hi)) = value.split_once("..") {
        return Some(Bound::Range(parse_bytes(lo)?, parse_bytes(hi)?));
    }
    let (make, rest) = split_comparator::<u64>(value);
    Some(make(parse_bytes(rest)?))
}

/// Parse a size like `2mb`, `1.5gb`, `500kb`, or a bare `1024` into bytes.
/// Base-1024; the unit is optional (bytes). `None` on anything malformed.
fn parse_bytes(text: &str) -> Option<u64> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let split = text.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let value: f64 = number.trim().parse().ok()?;
    if !value.is_finite() || value < 0. {
        return None;
    }
    let multiplier: u64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "kb" | "k" | "kib" => 1 << 10,
        "mb" | "m" | "mib" => 1 << 20,
        "gb" | "g" | "gib" => 1 << 30,
        "tb" | "t" | "tib" => 1 << 40,
        _ => return None,
    };
    let bytes = value * multiplier as f64;
    if bytes > u64::MAX as f64 {
        return None;
    }
    Some(bytes as u64)
}

/// Parse a `modified:` value into a bound on unix-seconds mtime:
/// keywords (`today`, `yesterday`, `week`, `month`, `year`), relative ages
/// (`<7d`, `>30d`, `<2h`, `<1w`), or ISO dates (`2026-01-01`,
/// `2026-01-01..2026-02-01`, `>2026-01-01`). `now` anchors the relative
/// forms.
fn parse_time_bound(value: &str, now: i64) -> Option<Bound<i64>> {
    let lower = value.to_ascii_lowercase();
    // Keywords.
    match lower.as_str() {
        "today" => return Some(Bound::Ge(start_of_day(now))),
        "yesterday" => {
            let today = start_of_day(now);
            return Some(Bound::Range(today - DAY, today - 1));
        }
        "week" => return Some(Bound::Ge(now - 7 * DAY)),
        "month" => return Some(Bound::Ge(now - 30 * DAY)),
        "year" => return Some(Bound::Ge(now - 365 * DAY)),
        _ => {}
    }
    // Relative age: `<7d`, `>30d`, `<2h`, `<1w`. The comparator is on
    // *age*, so it inverts to a bound on mtime (younger = larger mtime).
    if let Some(bound) = parse_relative_age(&lower, now) {
        return Some(bound);
    }
    // Absolute ISO date or date range.
    if let Some((lo, hi)) = value.split_once("..") {
        let lo = parse_iso_date(lo.trim())?;
        let hi = parse_iso_date(hi.trim())?;
        // Inclusive of all of `hi`'s calendar day.
        return Some(Bound::Range(lo, hi + DAY - 1));
    }
    let (make, rest) = split_comparator::<i64>(value);
    let day = parse_iso_date(rest.trim())?;
    Some(match make(0) {
        // A bare date means "that whole day"; comparators anchor at its
        // midnight.
        Bound::Eq(_) => Bound::Range(day, day + DAY - 1),
        Bound::Gt(_) => Bound::Gt(day + DAY - 1),
        Bound::Ge(_) => Bound::Ge(day),
        Bound::Lt(_) => Bound::Lt(day),
        Bound::Le(_) => Bound::Le(day + DAY - 1),
        Bound::Range(..) => unreachable!(),
    })
}

/// Parse `<7d` / `>30d` / `<2h` / `<1w` into an mtime bound, or `None`.
fn parse_relative_age(value: &str, now: i64) -> Option<Bound<i64>> {
    let (younger, rest) = if let Some(rest) = value.strip_prefix('<') {
        (true, rest) // age < N ⇒ modified more recently than now-N
    } else if let Some(rest) = value.strip_prefix('>') {
        (false, rest) // age > N ⇒ modified before now-N
    } else {
        return None;
    };
    let (num, unit) = rest.split_at(rest.find(|c: char| c.is_ascii_alphabetic())?);
    let n: i64 = num.trim().parse().ok()?;
    if n < 0 {
        return None;
    }
    let secs = match unit.trim() {
        "h" => n * 3_600,
        "d" => n * DAY,
        "w" => n * 7 * DAY,
        _ => return None,
    };
    let threshold = now - secs;
    Some(if younger { Bound::Gt(threshold) } else { Bound::Lt(threshold) })
}

/// Unix seconds at the UTC midnight starting `t`'s day.
fn start_of_day(t: i64) -> i64 {
    t.div_euclid(DAY) * DAY
}

/// Parse `YYYY-MM-DD` into unix seconds at UTC midnight, or `None`.
fn parse_iso_date(text: &str) -> Option<i64> {
    let mut parts = text.split('-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: i64 = parts.next()?.parse().ok()?;
    let day: i64 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    Some(days_from_civil(year, month, day) * DAY)
}

/// Days since the unix epoch for a proleptic-Gregorian Y-M-D (Howard
/// Hinnant's `days_from_civil`). Valid for the whole range we care about;
/// no external date crate needed.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(raw: &str) -> Query {
        parse_query(raw, 0)
    }

    #[test]
    fn plain_text_has_no_filters() {
        assert_eq!(q("annual report"), Query { text: "annual report".into(), filters: vec![] });
    }

    #[test]
    fn unknown_key_and_bad_value_fall_back_to_text() {
        // Unknown key, and a known key with an unparseable value, stay text.
        assert_eq!(
            parse_query("foo:bar size:huge kind:image", 0),
            Query {
                text: "foo:bar size:huge".into(),
                filters: vec![Filter::Kind(FileKind::Image)]
            }
        );
    }

    #[test]
    fn parses_kind_ext_tag_and_dedups() {
        let query = parse_query("shot kind:image ext:.PNG tag:Work tag:work", 0);
        assert_eq!(query.text, "shot");
        assert_eq!(query.filters, vec![
            Filter::Kind(FileKind::Image),
            Filter::Ext("png".into()),
            Filter::Tag("work".into()),
        ]);
    }

    #[test]
    fn size_bounds_are_base_1024() {
        assert_eq!(parse_size_bound(">2mb"), Some(Bound::Gt(2 * (1 << 20))));
        assert_eq!(parse_size_bound("<=500kb"), Some(Bound::Le(500 * (1 << 10))));
        assert_eq!(parse_size_bound("1gb"), Some(Bound::Eq(1 << 30)));
        assert_eq!(parse_size_bound("1.5gb"), Some(Bound::Eq(1536 * (1 << 20))));
        assert_eq!(parse_size_bound("1mb..5mb"), Some(Bound::Range(1 << 20, 5 * (1 << 20))));
        assert_eq!(parse_size_bound("1024"), Some(Bound::Eq(1024)));
        assert_eq!(parse_size_bound("nope"), None);
    }

    #[test]
    fn size_filter_matches_item() {
        let big = ItemMeta { name: "a.bin", is_dir: false, size: Some(3 << 20), mtime: None };
        let small = ItemMeta { size: Some(100), ..big };
        let unknown = ItemMeta { size: None, ..big };
        let f = Filter::Size(Bound::Gt(2 << 20));
        assert!(f.matches(&big));
        assert!(!f.matches(&small));
        assert!(!f.matches(&unknown)); // unknown size never positively matches
    }

    #[test]
    fn kind_and_ext_match_from_name() {
        let png = ItemMeta { name: "pic.PNG", is_dir: false, size: None, mtime: None };
        let dir = ItemMeta { name: "src", is_dir: true, size: None, mtime: None };
        assert!(Filter::Kind(FileKind::Image).matches(&png));
        assert!(Filter::Ext("png".into()).matches(&png));
        assert!(!Filter::Ext("jpg".into()).matches(&png));
        assert!(Filter::Kind(FileKind::Directory).matches(&dir));
    }

    #[test]
    fn modified_keywords_and_relative() {
        // now = 100 days past the epoch, at midday.
        let now = 100 * DAY + 12 * 3_600;
        // today: at/after the day's midnight.
        assert_eq!(parse_time_bound("today", now), Some(Bound::Ge(100 * DAY)));
        // week: within the last 7 days.
        assert_eq!(parse_time_bound("week", now), Some(Bound::Ge(now - 7 * DAY)));
        // <7d ⇒ modified more recently than now-7d.
        assert_eq!(parse_time_bound("<7d", now), Some(Bound::Gt(now - 7 * DAY)));
        // >30d ⇒ older than 30 days.
        assert_eq!(parse_time_bound(">30d", now), Some(Bound::Lt(now - 30 * DAY)));
        assert_eq!(parse_time_bound("<2h", now), Some(Bound::Gt(now - 2 * 3_600)));
    }

    #[test]
    fn iso_dates_and_ranges() {
        // 1970-01-02 is exactly one day past the epoch.
        assert_eq!(parse_iso_date("1970-01-02"), Some(DAY));
        // A bare date is that whole calendar day.
        assert_eq!(
            parse_time_bound("1970-01-02", 0),
            Some(Bound::Range(DAY, 2 * DAY - 1))
        );
        // A comparator anchors at midnight.
        assert_eq!(parse_time_bound(">=1970-01-02", 0), Some(Bound::Ge(DAY)));
        // Range spans inclusive of the end day.
        assert_eq!(
            parse_time_bound("1970-01-02..1970-01-03", 0),
            Some(Bound::Range(DAY, 3 * DAY - 1))
        );
        assert_eq!(parse_iso_date("2026-13-01"), None); // bad month
        assert_eq!(parse_iso_date("2026-01"), None); // not enough parts
    }

    #[test]
    fn modified_filter_matches_mtime() {
        let now = 100 * DAY;
        let recent = ItemMeta { name: "x", is_dir: false, size: None, mtime: Some(now - DAY) };
        let old = ItemMeta { mtime: Some(now - 40 * DAY), ..recent };
        let f = Filter::Modified(parse_time_bound("week", now).unwrap());
        assert!(f.matches(&recent));
        assert!(!f.matches(&old));
    }

    #[test]
    fn everything_composed() {
        let now = 1_000_000;
        let query = parse_query("report tag:work kind:pdf size:>1mb modified:<7d", now);
        // kind:pdf isn't a FileKind word → stays text; the rest parse.
        assert_eq!(query.text, "report kind:pdf");
        assert_eq!(query.filters, vec![
            Filter::Tag("work".into()),
            Filter::Size(Bound::Gt(1 << 20)),
            Filter::Modified(Bound::Gt(now - 7 * DAY)),
        ]);
    }
}

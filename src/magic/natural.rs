//! Reading what people actually type, not just the grammar's own words.
//!
//! [`interpret`] is strict: one spelling per idea (`delete`, `move … to`,
//! `how big is`). People say "get rid of", "put … in", "what's taking up
//! space", say please, and make typos. Rather than teach every rule in
//! the grammar every phrasing, this module **rewrites the sentence into
//! the grammar's spelling first** and hands that to the strict parser:
//!
//! 1. *Trim* — punctuation, lead-ins ("could you please"), sign-offs.
//! 2. *App phrases* — "turn on dark mode", "switch to grid" are matched
//!    against the command table by stripping the framing verb.
//! 3. *Paraphrase* — fixed rewrites of verbs and sentence shapes into the
//!    canonical ones ("throw away" → `delete`, "put X in Y" → `move X to
//!    Y`, "more than 30 days old" → `older than 30 days`).
//! 4. *Typos* — only if the sentence still doesn't read: words within one
//!    edit of the grammar's closed vocabulary are corrected and it is
//!    tried again. A sentence that reads is never "corrected", so a file
//!    really called `form` is not silently turned into `from`.
//!
//! This works because Magic's language is a small closed vocabulary
//! around a few open slots (which files, which folder). Everything
//! flexible lives here, as tables; the grammar stays small enough to
//! reason about, and — the property that matters for a tool that moves
//! files — the result is always shown back in canonical words
//! ([`Understood::read_as`]) before anything runs.
//!
//! Pure string work: no disk, no model, microseconds.

use super::intent::{AppCommand, Intent, command_named, interpret};
use crate::settings::ThemeMode;

/// What a sentence was taken to mean.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Understood {
    pub intent: Intent,
    /// The sentence in the grammar's own words, when that differs from
    /// what was typed by more than pleasantries — shown as "Read as …"
    /// so a paraphrase or a typo fix is never applied invisibly.
    pub read_as: Option<String>,
}

/// Read `raw` the way a person would mean it. `None`: not understood,
/// even allowing for paraphrase and typos.
pub fn understand(raw: &str, now: i64) -> Option<Understood> {
    let typed = trim(raw);
    if typed.is_empty() {
        return None;
    }
    let plain = typed.join(" ");
    let shown = |canonical: String| (!canonical.eq_ignore_ascii_case(&plain)).then_some(canonical);
    if let Some((intent, canonical)) = read(&typed, now) {
        return Some(Understood {
            intent,
            read_as: shown(canonical),
        });
    }
    let corrected = correct(&typed)?;
    let (intent, canonical) = read(&corrected, now)?;
    Some(Understood {
        intent,
        read_as: Some(canonical),
    })
}

/// A typo-corrected version of `raw` that reads as a command, for a
/// "Did you mean …?" offer when the sentence read fine but found nothing
/// (`delete screenshtos` is a valid search for files named that). `None`
/// when no word is a near miss.
pub fn did_you_mean(raw: &str, now: i64) -> Option<String> {
    let corrected = correct(&trim(raw))?;
    read(&corrected, now).map(|_| corrected.join(" "))
}

/// Canonicalize and parse. Returns the intent with the canonical text.
fn read(words: &[String], now: i64) -> Option<(Intent, String)> {
    let lower: Vec<String> = words.iter().map(|w| w.to_ascii_lowercase()).collect();
    let lower: Vec<&str> = lower.iter().map(String::as_str).collect();
    if let Some((label, command)) = app_phrase(&lower) {
        return Some((Intent::App(command), label.to_owned()));
    }
    let rewritten = paraphrase(words);
    let text = rewritten.join(" ");
    if let Some(intent) = interpret(&text, now) {
        return Some((intent, text));
    }
    find(&rewritten).map(|what| {
        let text = format!("find {what}");
        (Intent::Find(what), text)
    })
}

// ---------------------------------------------------------------------
// 1. Trim
// ---------------------------------------------------------------------

/// Things said before the request. Longest match first, repeatedly, so
/// "hey could you please" all goes.
const LEAD_INS: &[&[&str]] = &[
    &["i", "would", "like", "you", "to"],
    &["i", "would", "like", "to"],
    &["i", "want", "you", "to"],
    &["i", "need", "you", "to"],
    &["go", "ahead", "and"],
    &["i'd", "like", "to"],
    &["id", "like", "to"],
    &["i", "want", "to"],
    &["i", "need", "to"],
    &["help", "me", "to"],
    &["can", "you"],
    &["could", "you"],
    &["would", "you"],
    &["will", "you"],
    &["can", "u"],
    &["i", "wanna"],
    &["let", "us"],
    &["help", "me"],
    &["hey", "filex"],
    &["try", "to"],
    &["please"],
    &["pls"],
    &["plz"],
    &["kindly"],
    &["hey"],
    &["ok"],
    &["okay"],
    &["just"],
    &["let's"],
    &["lets"],
    &["filex"],
];

/// Things said after it.
const SIGN_OFFS: &[&[&str]] = &[
    &["if", "you", "can"],
    &["thank", "you"],
    &["right", "now"],
    &["for", "me"],
    &["please"],
    &["pls"],
    &["plz"],
    &["thanks"],
    &["asap"],
];

fn starts_with(words: &[&str], phrase: &[&str]) -> bool {
    words.len() >= phrase.len() && words[..phrase.len()] == *phrase
}

/// How many leading words the longest phrase in `table` covers.
fn leading(words: &[&str], table: &[&[&str]]) -> usize {
    table
        .iter()
        .filter(|phrase| starts_with(words, phrase))
        .map(|phrase| phrase.len())
        .max()
        .unwrap_or(0)
}

/// Split into words, drop sentence punctuation, lead-ins and sign-offs.
/// Never empties the sentence: a file can be called `please`.
fn trim(raw: &str) -> Vec<String> {
    let mut words: Vec<&str> = raw
        .split_whitespace()
        .map(|w| w.trim_end_matches(['?', '!', ',', ';']))
        .filter(|w| !w.is_empty())
        .collect();
    loop {
        let lower: Vec<String> = words.iter().map(|w| w.to_ascii_lowercase()).collect();
        let lower: Vec<&str> = lower.iter().map(String::as_str).collect();
        let cut = leading(&lower, LEAD_INS);
        if cut > 0 && cut < words.len() {
            words.drain(..cut);
            continue;
        }
        let tail = SIGN_OFFS
            .iter()
            .filter(|phrase| lower.ends_with(phrase))
            .map(|phrase| phrase.len())
            .max()
            .unwrap_or(0);
        if tail > 0 && tail < words.len() {
            words.truncate(words.len() - tail);
            continue;
        }
        break;
    }
    words.into_iter().map(str::to_owned).collect()
}

// ---------------------------------------------------------------------
// 2. App phrases
// ---------------------------------------------------------------------

/// Verbs that frame turning something on. "show" is here too: the table
/// spells visibility commands `show hidden files`.
const TURN_ON: &[&[&str]] = &[
    &["show", "me"],
    &["turn", "on"],
    &["switch", "on"],
    &["enable"],
    &["activate"],
    &["show"],
    &["display"],
    &["unhide"],
    &["reveal"],
];

const TURN_OFF: &[&[&str]] = &[
    &["do", "not", "show"],
    &["stop", "showing"],
    &["don't", "show"],
    &["dont", "show"],
    &["turn", "off"],
    &["switch", "off"],
    &["disable"],
    &["deactivate"],
    &["hide"],
];

/// Verbs that frame choosing a state or opening a thing.
const SWITCH_TO: &[&[&str]] = &[
    &["set", "the", "theme", "to"],
    &["change", "the", "theme", "to"],
    &["set", "theme", "to"],
    &["change", "theme", "to"],
    &["set", "the", "view", "to"],
    &["change", "the", "view", "to"],
    &["set", "view", "to"],
    &["change", "view", "to"],
    &["set", "it", "to"],
    &["switch", "to"],
    &["change", "to"],
    &["make", "everything"],
    &["make", "app"],
    &["make", "it"],
    &["view", "as"],
    &["show", "as"],
    &["display", "as"],
    &["bring", "up"],
    &["open", "up"],
    &["open", "a"],
    &["open", "the"],
    &["create", "a"],
    &["make", "a"],
    &["add", "a"],
    &["i", "want"],
    &["give", "me"],
    &["set", "to"],
    &["use"],
    &["set"],
    &["go"],
    &["toggle"],
    &["open"],
    &["launch"],
];

/// A command said with a framing verb: the verb is dropped and what is
/// left is looked up in the command table, as itself and in the few
/// shapes the table spells things (`dark` → `dark mode`).
fn app_phrase(lower: &[&str]) -> Option<(&'static str, AppCommand)> {
    // "sort files by size", "order by name", "arrange it by date".
    let sorted = [
        &["sort", "files", "by"][..],
        &["sort", "everything", "by"],
        &["sort", "them", "by"],
        &["sort", "it", "by"],
        &["order", "by"],
        &["arrange", "by"],
        &["sort", "on"],
    ];
    if let Some(n) = sorted
        .iter()
        .find(|phrase| starts_with(lower, phrase))
        .map(|phrase| phrase.len())
        && lower.len() > n
    {
        return command_named(&format!("sort by {}", lower[n..].join(" ")));
    }

    let words: Vec<&str> = lower
        .iter()
        .copied()
        .filter(|w| !matches!(*w, "the" | "my" | "all" | "this" | "current"))
        .collect();
    let named = |rest: &[&str]| {
        let rest = rest.join(" ");
        // "show …" first: turning hidden files *on* is not the toggle
        // that the bare words name.
        [
            format!("show {rest}"),
            rest.clone(),
            format!("{rest} mode"),
            format!("{rest} view"),
        ]
        .iter()
        .find_map(|phrase| command_named(phrase))
    };

    let off = leading(&words, TURN_OFF);
    if off > 0 && off < words.len() {
        let rest = &words[off..];
        if let Some(hit) = command_named(&format!("hide {}", rest.join(" "))) {
            return Some(hit);
        }
        // "turn off dark mode" is a request for the other one.
        return match named(rest)?.1 {
            AppCommand::Theme(ThemeMode::Dark | ThemeMode::Oled) => command_named("light mode"),
            AppCommand::Theme(ThemeMode::Light) => command_named("dark mode"),
            toggle @ AppCommand::TogglePreview => Some(("Toggle details panel", toggle)),
            _ => None,
        };
    }
    let on = leading(&words, TURN_ON);
    if on > 0
        && on < words.len()
        && let Some(hit) = named(&words[on..])
    {
        return Some(hit);
    }
    let to = leading(&words, SWITCH_TO);
    if to > 0
        && to < words.len()
        && let Some(hit) = named(&words[to..])
    {
        return Some(hit);
    }
    // "go to settings" — but only for places inside the app; "go to
    // list" is a folder called list, not the list view.
    if starts_with(&words, &["go", "to"]) && words.len() > 2 {
        return named(&words[2..])
            .filter(|(_, c)| matches!(c, AppCommand::Settings | AppCommand::Shortcuts));
    }
    // "close this tab": the table's own words around an article.
    if words.len() < lower.len() {
        return command_named(&words.join(" "));
    }
    None
}

// ---------------------------------------------------------------------
// 3. Paraphrase
// ---------------------------------------------------------------------

/// Other ways to open a sentence, and the grammar's way. Longest match
/// wins; first words are unique enough that order doesn't matter.
const OPENERS: &[(&[&str], &[&str])] = &[
    // delete
    (&["get", "rid", "of"], &["delete"]),
    (&["throw", "away"], &["delete"]),
    (&["throw", "out"], &["delete"]),
    (&["clear", "out"], &["delete"]),
    (&["erase"], &["delete"]),
    (&["wipe"], &["delete"]),
    (&["bin"], &["delete"]),
    (&["nuke"], &["delete"]),
    (&["discard"], &["delete"]),
    (&["junk"], &["delete"]),
    (&["rm"], &["delete"]),
    (&["del"], &["delete"]),
    // move
    (&["put"], &["move"]),
    (&["place"], &["move"]),
    (&["stick"], &["move"]),
    (&["stash"], &["move"]),
    (&["drop"], &["move"]),
    (&["send"], &["move"]),
    (&["shift"], &["move"]),
    (&["transfer"], &["move"]),
    (&["relocate"], &["move"]),
    (&["bring"], &["move"]),
    (&["migrate"], &["move"]),
    (&["mv"], &["move"]),
    // copy
    (&["make", "a", "copy", "of"], &["copy"]),
    (&["make", "copies", "of"], &["copy"]),
    (&["back", "up"], &["copy"]),
    (&["backup"], &["copy"]),
    (&["duplicate"], &["copy"]),
    (&["clone"], &["copy"]),
    (&["cp"], &["copy"]),
    (&["ren"], &["rename"]),
    // compress
    (&["make", "a", "zip", "file", "of"], &["zip"]),
    (&["create", "a", "zip", "file", "of"], &["zip"]),
    (&["make", "a", "zip", "of"], &["zip"]),
    (&["create", "a", "zip", "of"], &["zip"]),
    (&["make", "a", "zip", "from"], &["zip"]),
    (&["make", "an", "archive", "of"], &["zip"]),
    (&["create", "an", "archive", "of"], &["zip"]),
    (&["zip", "up"], &["zip"]),
    (&["pack", "up"], &["zip"]),
    (&["bundle", "up"], &["zip"]),
    (&["pack"], &["zip"]),
    (&["bundle"], &["zip"]),
    // organize — "clean up" lands here, not on delete: the reading that
    // moves files into folders is the one that can't lose anything.
    (&["straighten", "up"], &["organize"]),
    (&["clean", "up"], &["organize"]),
    (&["tidy", "up"], &["organize"]),
    (&["sort", "out"], &["organize"]),
    (&["cleanup"], &["organize"]),
    (&["arrange"], &["organize"]),
    (&["group"], &["organize"]),
    (&["declutter"], &["organize"]),
    (&["separate"], &["organize"]),
    (&["seperate"], &["organize"]),
    (&["split"], &["organize"]),
    // tag
    (&["mark"], &["tag"]),
    (&["flag"], &["tag"]),
    (&["clear", "tags", "from"], &["untag"]),
    (&["clear", "tags", "on"], &["untag"]),
    (&["remove", "all", "tags", "from"], &["untag"]),
    (&["remove", "tags", "from"], &["untag"]),
    (&["strip", "tags", "from"], &["untag"]),
    // go
    (&["bring", "me", "to"], &["go", "to"]),
    (&["take", "me", "to"], &["go", "to"]),
    (&["get", "me", "to"], &["go", "to"]),
    (&["take", "me"], &["go", "to"]),
    (&["navigate", "to"], &["go", "to"]),
    (&["jump", "to"], &["go", "to"]),
    (&["head", "to"], &["go", "to"]),
    (&["switch", "to"], &["go", "to"]),
    (&["browse", "to"], &["go", "to"]),
    (&["go", "into"], &["go", "to"]),
    (&["show", "me"], &["go", "to"]),
    (&["open", "up"], &["go", "to"]),
    (&["visit"], &["go", "to"]),
    // connect
    (&["connect", "me", "to"], &["connect", "to"]),
    (&["mount"], &["connect", "to"]),
    (&["safely", "remove"], &["eject"]),
    // questions
    (&["what", "is", "the", "size", "of"], &["size", "of"]),
    (&["what's", "the", "size", "of"], &["size", "of"]),
    (&["whats", "the", "size", "of"], &["size", "of"]),
    (&["tell", "me", "the", "size", "of"], &["size", "of"]),
    (&["show", "me", "the", "size", "of"], &["size", "of"]),
    (&["the", "size", "of"], &["size", "of"]),
    (&["how", "heavy", "is"], &["how", "big", "is"]),
    (&["how", "heavy", "are"], &["how", "big", "are"]),
    (&["how", "much", "space", "does"], &["how", "big", "is"]),
    (&["how", "much", "space", "is"], &["how", "big", "is"]),
    (&["how", "much", "space", "do"], &["how", "big", "are"]),
    (&["how", "much", "space", "are"], &["how", "big", "are"]),
    (&["count", "how", "many"], &["count"]),
    (&["count", "the"], &["count"]),
    (&["count", "my"], &["count"]),
    (&["count", "all"], &["count"]),
    (&["how", "many", "of", "my"], &["how", "many"]),
];

/// Whole questions about disk space, all of which mean `largest files`.
/// Anything after them ("… in Downloads") carries over.
const SPACE_HOGS: &[&[&str]] = &[
    &["what", "is", "taking", "up", "the", "most", "space"],
    &["what's", "taking", "up", "the", "most", "space"],
    &["what's", "taking", "up", "all", "my", "space"],
    &["what", "takes", "up", "the", "most", "space"],
    &["what's", "using", "the", "most", "space"],
    &["what", "is", "taking", "up", "space"],
    &["what's", "taking", "up", "my", "disk"],
    &["what's", "taking", "up", "space"],
    &["whats", "taking", "up", "space"],
    &["what", "is", "using", "space"],
    &["why", "is", "my", "disk", "full"],
    &["what's", "filling", "my", "disk"],
    &["what's", "eating", "my", "space"],
    &["what's", "eating", "my", "disk"],
    &["what's", "using", "my", "disk"],
    &["what", "can", "i", "delete"],
    &["what's", "using", "space"],
    &["free", "up", "some", "space"],
    &["free", "up", "space"],
    &["space", "hogs"],
    &["disk", "usage"],
];

/// Words that end "how much space does X …".
const SPACE_TAILS: &[&[&str]] = &[
    &["take", "up"],
    &["taking", "up"],
    &["take"],
    &["use"],
    &["using"],
    &["taking"],
    &["occupy"],
    &["occupying"],
    &["need"],
];

/// Where a `put`-like verb's destination follows from. Only for those
/// verbs: "move pdfs in Downloads to Documents" needs its `in` to stay a
/// scope while it is being typed.
const PUT_VERBS: &[&str] = &["put", "place", "stick", "stash", "drop"];
const PUT_PREPOSITIONS: &[&str] = &["in", "inside", "under", "onto", "on"];

const TRASH: &[&str] = &[
    "trash",
    "bin",
    "recycle bin",
    "rubbish",
    "garbage",
    "wastebasket",
];

/// Rewrite a trimmed sentence into the grammar's spelling. What it
/// doesn't recognize it leaves alone, so canonical input is unchanged.
fn paraphrase(words: &[String]) -> Vec<String> {
    let lower: Vec<String> = words.iter().map(|w| w.to_ascii_lowercase()).collect();
    let low: Vec<&str> = lower.iter().map(String::as_str).collect();
    let owned = |fixed: &[&str]| -> Vec<String> { fixed.iter().map(|w| (*w).to_owned()).collect() };

    // "how much room/storage …" is "how much space …".
    if starts_with(&low, &["how", "much"]) && matches!(low.get(2), Some(&("room" | "storage"))) {
        let mut respelt = words.to_vec();
        respelt[2] = "space".to_owned();
        return paraphrase(&respelt);
    }
    // "throw these away", "toss the old logs out".
    if matches!(low[0], "throw" | "toss" | "chuck")
        && low.len() > 2
        && matches!(low[low.len() - 1], "away" | "out")
    {
        let mut respelt = owned(&["delete"]);
        respelt.extend_from_slice(&words[1..words.len() - 1]);
        return paraphrase(&respelt);
    }
    if let Some(phrase) = SPACE_HOGS
        .iter()
        .filter(|phrase| starts_with(&low, phrase))
        .max_by_key(|phrase| phrase.len())
    {
        let mut out = owned(&["largest", "files"]);
        out.extend_from_slice(&words[phrase.len()..]);
        return out;
    }
    if let Some(out) = largest(words, &low) {
        return here(out);
    }
    if let Some(out) = new_folder(words, &low) {
        return out;
    }
    if let Some(out) = tag_shapes(words, &low) {
        return out;
    }

    // The opener, then everything after it in the user's own casing.
    let opener = OPENERS
        .iter()
        .filter(|(from, _)| starts_with(&low, from))
        .max_by_key(|(from, _)| from.len());
    let put = PUT_VERBS.contains(&low[0]) || starts_with(&low, &["make", "a", "copy", "of"]);
    let space = starts_with(&low, &["how", "much", "space"]);
    let mut out: Vec<String> = match opener {
        Some((from, to)) if low.len() > from.len() => {
            let mut out = owned(to);
            out.extend_from_slice(&words[from.len()..]);
            out
        }
        _ => words.to_vec(),
    };
    if space {
        let lower: Vec<String> = out.iter().map(|w| w.to_ascii_lowercase()).collect();
        let lower: Vec<&str> = lower.iter().map(String::as_str).collect();
        if let Some(tail) = SPACE_TAILS.iter().find(|tail| lower.ends_with(tail)) {
            out.truncate(out.len() - tail.len());
        }
    }

    out = smooth(out);
    let verb = out[0].to_ascii_lowercase();
    if matches!(verb.as_str(), "move" | "copy") {
        out = destination_shapes(out, put);
    }
    let is =
        |word: &String, options: &[&str]| options.contains(&word.to_ascii_lowercase().as_str());
    // "label the invoices red": a trailing colour is the tag.
    if matches!(verb.as_str(), "tag" | "label")
        && out.len() > 2
        && !out.iter().any(|w| is(w, &["as", "with"]))
        && is(&out[out.len() - 1], COLOURS)
    {
        out.insert(out.len() - 1, "as".to_owned());
    }
    if matches!(
        verb.as_str(),
        "largest" | "count" | "how" | "delete" | "zip"
    ) {
        out = here(out);
    }
    if verb == "rename" && !out.iter().any(|w| w.eq_ignore_ascii_case("to")) {
        // "rename X as Y"
        if let Some(at) = out.iter().rposition(|w| w.eq_ignore_ascii_case("as")) {
            out[at] = "to".to_owned();
        }
    }
    out
}

/// "the 5 largest files here": a bare trailing "here" is a place.
fn here(mut words: Vec<String>) -> Vec<String> {
    let is = |ix: usize, options: &[&str]| {
        words
            .get(ix)
            .is_some_and(|w| options.contains(&w.to_ascii_lowercase().as_str()))
    };
    let last = words.len().saturating_sub(1);
    if last >= 2 && is(last, &["here"]) && !is(last - 1, &["from", "in", "inside"]) {
        words.insert(last, "in".to_owned());
    }
    words
}

/// Tag colours, as `magic::tag_from` reads them.
const COLOURS: &[&str] = &[
    "red", "orange", "yellow", "green", "blue", "purple", "grey", "gray",
];

/// Rewrites that apply anywhere in the sentence.
fn smooth(words: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(words.len());
    let mut i = 0;
    while i < words.len() {
        let at = |n: usize| words.get(i + n).map(|w| w.to_ascii_lowercase());
        let word = at(0).unwrap_or_default();
        let next = at(1).unwrap_or_default();
        // Arrows are a destination.
        if matches!(word.as_str(), "→" | "->" | "=>" | "-->") {
            out.push("to".to_owned());
            i += 1;
            continue;
        }
        // "files that are older than…", "the ones which were…"
        if matches!(word.as_str(), "that" | "which")
            && matches!(next.as_str(), "are" | "were" | "is" | "was")
        {
            i += 2;
            continue;
        }
        // "all of my pdfs", "all the screenshots" — right after the verb.
        if out.len() == 1
            && word == "all"
            && matches!(next.as_str(), "of" | "the" | "my")
            && i + 2 < words.len()
        {
            i += if next == "of" && matches!(at(2).as_deref(), Some("the" | "my")) {
                3
            } else {
                2
            };
            continue;
        }
        // "more than 30 days old" → "older than 30 days"; "less than a
        // week old" → "newer than a week".
        let lead = match (word.as_str(), next.as_str()) {
            ("more", "than") | ("at", "least") => Some((2, "older")),
            ("less", "than") => Some((2, "newer")),
            ("over", _) => Some((1, "older")),
            ("under", _) => Some((1, "newer")),
            _ => None,
        };
        if let Some((skip, comparative)) = lead
            && let Some(old) = (i + skip + 1..words.len().min(i + skip + 4))
                .find(|&j| words[j].eq_ignore_ascii_case("old"))
        {
            out.push(comparative.to_owned());
            out.push("than".to_owned());
            out.extend_from_slice(&words[i + skip..old]);
            i = old + 1;
            continue;
        }
        // Sizes: "over 100mb", "more than 2 gb", "under 1mb".
        if let Some((skip, _)) = lead
            && words
                .get(i + skip)
                .is_some_and(|w| is_size(w, words.get(i + skip + 1)))
        {
            let bigger = matches!(word.as_str(), "more" | "at" | "over");
            out.push(if bigger { "bigger" } else { "smaller" }.to_owned());
            out.push("than".to_owned());
            i += skip;
            continue;
        }
        if matches!(word.as_str(), "above" | "exceeding")
            && words
                .get(i + 1)
                .is_some_and(|w| is_size(w, words.get(i + 2)))
        {
            out.push("bigger".to_owned());
            out.push("than".to_owned());
            i += 1;
            continue;
        }
        if word == "below"
            && words
                .get(i + 1)
                .is_some_and(|w| is_size(w, words.get(i + 2)))
        {
            out.push("smaller".to_owned());
            out.push("than".to_owned());
            i += 1;
            continue;
        }
        if matches!(word.as_str(), "greater" | "larger") && next == "than" {
            out.push("bigger".to_owned());
            i += 1;
            continue;
        }
        out.push(words[i].clone());
        i += 1;
    }
    out
}

/// `100mb`, or `2` followed by `gb`.
fn is_size(word: &str, next: Option<&String>) -> bool {
    const UNITS: &[&str] = &[
        "b",
        "kb",
        "mb",
        "gb",
        "tb",
        "bytes",
        "megabytes",
        "gigabytes",
    ];
    let word = word.to_ascii_lowercase();
    let digits = word.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    let unit = &word[digits.len()..];
    let numeric = !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit() || c == '.');
    numeric
        && (UNITS.contains(&unit)
            || (unit.is_empty()
                && next.is_some_and(|n| UNITS.contains(&n.to_ascii_lowercase().as_str()))))
}

/// Move/copy sentences whose destination isn't spelled `to <folder>`:
/// "put X in Y" (for `put`-like verbs), "… to the trash" (a delete), and
/// "put X in a zip" (a compress).
fn destination_shapes(mut words: Vec<String>, put: bool) -> Vec<String> {
    let is =
        |word: &String, options: &[&str]| options.contains(&word.to_ascii_lowercase().as_str());
    let has_to = words.iter().any(|w| is(w, &["to", "into"]));
    if put
        && !has_to
        && let Some(at) = words.iter().rposition(|w| is(w, PUT_PREPOSITIONS))
        && at > 1
    {
        words[at] = "to".to_owned();
    }
    let Some(at) = words.iter().rposition(|w| is(w, &["to", "into"])) else {
        return words;
    };
    if at < 2 {
        return words;
    }
    let tail: Vec<String> = words[at + 1..]
        .iter()
        .map(|w| w.to_ascii_lowercase())
        .filter(|w| !matches!(w.as_str(), "the" | "my"))
        .collect();
    let tail = tail.join(" ");
    let target = words[1..at].to_vec();
    if TRASH.contains(&tail.as_str()) {
        return std::iter::once("delete".to_owned()).chain(target).collect();
    }
    if matches!(
        tail.as_str(),
        "a zip" | "a zip file" | "an archive" | "a zip archive" | "one zip"
    ) {
        return std::iter::once("zip".to_owned()).chain(target).collect();
    }
    words
}

/// "what are the 10 biggest videos", "top 5 largest files", "show me my
/// largest downloads" → `largest [N] …`.
fn largest(words: &[String], low: &[&str]) -> Option<Vec<String>> {
    const ASKERS: &[&[&str]] = &[
        &["what", "are"],
        &["which", "are"],
        &["show", "me"],
        &["give", "me"],
        &["tell", "me"],
        &["find", "me"],
        &["show"],
        &["list"],
        &["find"],
        &["get"],
    ];
    // "which files are the largest" → `largest files`.
    if matches!(low.first(), Some(&("which" | "what")))
        && matches!(low.last(), Some(&("largest" | "biggest" | "heaviest")))
        && let Some(are) = low.iter().position(|w| matches!(*w, "are" | "is"))
        && are > 1
    {
        let mut out = vec!["largest".to_owned()];
        out.extend_from_slice(&words[1..are]);
        return Some(out);
    }
    let mut i = leading(low, ASKERS);
    if matches!(low.get(i), Some(&("the" | "my"))) {
        i += 1;
    }
    if low.get(i) == Some(&"top") {
        i += 1;
    }
    let count = low.get(i).filter(|w| w.parse::<usize>().is_ok()).copied();
    if count.is_some() {
        i += 1;
    }
    if !matches!(low.get(i), Some(&("largest" | "biggest" | "heaviest"))) {
        return None;
    }
    // Already canonical: leave it to the grammar.
    if i == 0 {
        return None;
    }
    let mut out = vec!["largest".to_owned()];
    out.extend(count.map(str::to_owned));
    out.extend_from_slice(&words[i + 1..]);
    Some(out)
}

/// "make a new folder called X", "create folder X", "add a directory X".
fn new_folder(words: &[String], low: &[&str]) -> Option<Vec<String>> {
    if !matches!(low.first(), Some(&("make" | "create" | "add"))) {
        return None;
    }
    let mut i = 1;
    for filler in ["me", "a", "new"] {
        if low.get(i) == Some(&filler) {
            i += 1;
        }
    }
    if !matches!(low.get(i), Some(&("folder" | "directory"))) {
        return None;
    }
    let mut out = vec!["new".to_owned(), "folder".to_owned()];
    out.extend_from_slice(&words[i + 1..]);
    Some(out)
}

/// "add the Work tag to invoices" → `tag invoices as Work`; "remove the
/// Work tag from invoices" → `remove tag Work from invoices`.
fn tag_shapes(words: &[String], low: &[&str]) -> Option<Vec<String>> {
    let noise = |w: &&str| matches!(*w, "the" | "a" | "tag" | "label");
    let named = |range: std::ops::Range<usize>| -> Vec<String> {
        range
            .filter(|&ix| !noise(&low[ix]))
            .map(|ix| words[ix].clone())
            .collect()
    };
    let tagged = |upto: usize| low[1..upto].iter().any(|w| matches!(*w, "tag" | "label"));
    match *low.first()? {
        "add" | "apply" | "give" => {
            let at = low.iter().rposition(|w| *w == "to")?;
            let (name, target) = (named(1..at), &words[at + 1..]);
            (tagged(at) && !name.is_empty() && !target.is_empty()).then(|| {
                let mut out = vec!["tag".to_owned()];
                out.extend_from_slice(target);
                out.push("as".to_owned());
                out.extend(name);
                out
            })
        }
        "remove" | "take" | "strip" => {
            let at = low.iter().position(|w| matches!(*w, "from" | "off"))?;
            let name = named(1..at);
            // "remove tag Work from …" is already the grammar's spelling.
            if !tagged(at) || name.is_empty() || (low[0] == "remove" && low.get(1) == Some(&"tag"))
            {
                return None;
            }
            let mut out = vec!["remove".to_owned(), "tag".to_owned()];
            out.extend(name);
            out.push("from".to_owned());
            out.extend_from_slice(&words[at + 1..]);
            Some(out)
        }
        _ => None,
    }
}

/// "find invoices", "where is my resume", "search for tax 2024": a plain
/// filename search, asked for in words. Returns what to search for.
fn find(words: &[String]) -> Option<String> {
    const FINDERS: &[&[&str]] = &[
        &["show", "me", "all"],
        &["search", "for"],
        &["look", "for"],
        &["look", "up"],
        &["where", "is"],
        &["where", "are"],
        &["where's"],
        &["wheres"],
        &["find", "me"],
        &["find"],
        &["search"],
        &["locate"],
        &["list"],
    ];
    let lower: Vec<String> = words.iter().map(|w| w.to_ascii_lowercase()).collect();
    let low: Vec<&str> = lower.iter().map(String::as_str).collect();
    let mut i = leading(&low, FINDERS);
    if i == 0 {
        return None;
    }
    while matches!(low.get(i), Some(&("my" | "the" | "all" | "any"))) && i + 1 < words.len() {
        i += 1;
    }
    (i < words.len()).then(|| words[i..].join(" "))
}

// ---------------------------------------------------------------------
// 4. Typos
// ---------------------------------------------------------------------

/// The words the grammar itself gives meaning to. A typed word one edit
/// from exactly one of these is taken for it — when the sentence doesn't
/// otherwise read.
const VOCABULARY: &[&str] = &[
    // verbs and their paraphrases
    "delete",
    "remove",
    "trash",
    "move",
    "copy",
    "rename",
    "compress",
    "archive",
    "label",
    "zip",
    "tag",
    "untag",
    "organize",
    "organise",
    "tidy",
    "sort",
    "open",
    "show",
    "goto",
    "connect",
    "eject",
    "disconnect",
    "unmount",
    "count",
    "find",
    "search",
    "locate",
    "erase",
    "wipe",
    "discard",
    "send",
    "transfer",
    "duplicate",
    "clone",
    "backup",
    "bundle",
    "arrange",
    "group",
    "navigate",
    "jump",
    "create",
    "make",
    "enable",
    "disable",
    "turn",
    "switch",
    "toggle",
    "hide",
    // structure
    "from",
    "into",
    "inside",
    "with",
    "than",
    "called",
    "named",
    "folder",
    "selected",
    "selection",
    "these",
    "them",
    "everything",
    "files",
    "items",
    // describing files
    "older",
    "newer",
    "bigger",
    "larger",
    "smaller",
    "large",
    "huge",
    "small",
    "tiny",
    "empty",
    "photos",
    "pictures",
    "images",
    "screenshots",
    "videos",
    "movies",
    "music",
    "songs",
    "audio",
    "documents",
    "folders",
    "archives",
    "code",
    "scripts",
    "pdfs",
    "today",
    "yesterday",
    "days",
    "weeks",
    "months",
    "years",
    "week",
    "month",
    "year",
    "last",
    "this",
    "modified",
    "created",
    "downloaded",
    // questions
    "many",
    "largest",
    "biggest",
    "size",
    "much",
    "space",
    "what",
    "where",
    // organize
    "type",
    "kind",
    "extension",
    "date",
    // app commands
    "settings",
    "preferences",
    "shortcuts",
    "keyboard",
    "dark",
    "light",
    "oled",
    "mode",
    "theme",
    "system",
    "compact",
    "comfortable",
    "density",
    "hidden",
    "dotfiles",
    "name",
    "ascending",
    "descending",
    "list",
    "grid",
    "view",
    "details",
    "preview",
    "panel",
    "server",
    "terminal",
    "path",
    "properties",
    "favorites",
    "favorite",
    "feedback",
    "quit",
    "refresh",
    "reload",
    "undo",
    "back",
    "forward",
    "parent",
    "close",
    "next",
    "previous",
    // places everyone has
    "downloads",
    "desktop",
    "home",
];

/// Short slips too small for the one-edit rule to be safe in general.
const SLIPS: &[(&str, &str)] = &[
    ("too", "to"),
    ("ot", "to"),
    ("fro", "from"),
    ("form", "from"),
    ("frm", "from"),
    ("inot", "into"),
    ("ina", "in"),
    ("teh", "the"),
    ("adn", "and"),
    ("zpi", "zip"),
    ("tga", "tag"),
    ("mvoe", "move"),
    ("cpoy", "copy"),
];

/// Edit distance counting a swap of neighbours as one (optimal string
/// alignment), capped: returns `limit + 1` once it can't be within.
fn distance(a: &str, b: &str, limit: usize) -> usize {
    if a.is_ascii() && b.is_ascii() {
        return ascii_distance(a.as_bytes(), b.as_bytes(), limit);
    }
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    if a.len().abs_diff(b.len()) > limit {
        return limit + 1;
    }
    alignment(&a, &b, |x, y| x == y).min(limit + 1)
}

/// How many edits apart two names are (ASCII, ignoring case), stopping
/// counting past `limit`: for "did you mean" folder suggestions.
pub fn name_distance(a: &str, b: &str, limit: usize) -> usize {
    ascii_distance(a.as_bytes(), b.as_bytes(), limit)
}

/// [`distance`] for ASCII, ignoring case, without allocating in the
/// common cases: this runs for every vocabulary word on every word of a
/// sentence that didn't read. What two words share at each end can't
/// matter, so it is cut off first; a one-edit answer is then visible
/// from what is left.
fn ascii_distance(mut a: &[u8], mut b: &[u8], limit: usize) -> usize {
    if a.len().abs_diff(b.len()) > limit {
        return limit + 1;
    }
    let same = |x: u8, y: u8| x.eq_ignore_ascii_case(&y);
    while let (Some((x, rest_a)), Some((y, rest_b))) = (a.split_first(), b.split_first())
        && same(*x, *y)
    {
        (a, b) = (rest_a, rest_b);
    }
    while let (Some((x, rest_a)), Some((y, rest_b))) = (a.split_last(), b.split_last())
        && same(*x, *y)
    {
        (a, b) = (rest_a, rest_b);
    }
    let d = match (a.len(), b.len()) {
        (0, n) | (n, 0) => n,
        (1, 1) => 1,
        (2, 2) if same(a[0], b[1]) && same(a[1], b[0]) => 1,
        // Both ends differ and it isn't one of the above: two or more.
        _ if limit <= 1 => 2,
        _ => alignment(a, b, same),
    };
    d.min(limit + 1)
}

/// Optimal string alignment distance by dynamic programming.
fn alignment<T: Copy>(a: &[T], b: &[T], same: impl Fn(T, T) -> bool) -> usize {
    let mut before: Vec<usize> = Vec::new();
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut row = vec![i; b.len() + 1];
        for j in 1..=b.len() {
            let cost = usize::from(!same(a[i - 1], b[j - 1]));
            row[j] = (previous[j] + 1)
                .min(row[j - 1] + 1)
                .min(previous[j - 1] + cost);
            if i > 1 && j > 1 && same(a[i - 1], b[j - 2]) && same(a[i - 2], b[j - 1]) {
                row[j] = row[j].min(before[j - 2] + 1);
            }
        }
        before = std::mem::replace(&mut previous, row);
    }
    previous[b.len()]
}

/// The closest of `candidates` to `word`, if it is a near miss (one edit;
/// two for words of eight letters or more) and the only one that close.
/// Case-insensitive; an exact match is not a correction.
pub(crate) fn nearest<'a>(
    word: &str,
    candidates: impl Iterator<Item = &'a str>,
) -> Option<&'a str> {
    let word = word.to_ascii_lowercase();
    let limit = if word.chars().count() >= 8 { 2 } else { 1 };
    let mut best: Option<(usize, &str)> = None;
    let mut tied = false;
    let len = word.chars().count();
    for candidate in candidates {
        // Most of the vocabulary is the wrong length to be a near miss:
        // skip it before paying for a comparison.
        if candidate.chars().count().abs_diff(len) > limit {
            continue;
        }
        let d = if candidate.is_ascii() {
            distance(&word, candidate, limit)
        } else {
            distance(&word, &candidate.to_ascii_lowercase(), limit)
        };
        if d == 0 {
            return None;
        }
        if d > limit {
            continue;
        }
        match best {
            Some((least, _)) if d > least => {}
            Some((least, held)) if d == least => {
                // Two words equally close: only a shared first letter
                // breaks the tie, and only one way.
                let first = |s: &str| s.chars().next().map(|c| c.to_ascii_lowercase());
                match (
                    first(held) == first(&word),
                    first(candidate) == first(&word),
                ) {
                    (false, true) => {
                        best = Some((d, candidate));
                        tied = false;
                    }
                    (true, false) => {}
                    _ => tied = true,
                }
            }
            _ => {
                best = Some((d, candidate));
                tied = false;
            }
        }
    }
    best.filter(|_| !tied).map(|(_, candidate)| candidate)
}

/// Real words a single edit from the vocabulary. The typo fixer leaves
/// them alone: when one slip stops a sentence reading ("mvoe game saves
/// to Backup"), fixing it must not also turn `game` into `name` or
/// `here` into `where`. Derived offline — every dictionary word within
/// the fixer's reach of [`VOCABULARY`], kept if it is a word people use.
// ponytail: a curated list, so a rare real word can still be "fixed"
// alongside a genuine typo; the "Read as" line shows it. Swap in a
// frequency-ranked dictionary if that is ever reported.
const REAL_WORDS: &str = "\
aback allocate alone apparent archival audit bags bake bank bark bath bear bears bend \
bind black blast buck bump burn cake came cast chat chose closed closer closet coda \
coder codes codex coke come compressed compressor comprise cone congress connected \
connector cope copies core cosy county court cove cozy crash crate crease creative \
creature dale dame dare dart data dates daze dear delegate deletion delicate descendant \
detail detailed detection disabled disconnected discord document dome dove dump ease \
east eight ejection elder elect election electron eraser erect erection expansion \
extensive fake fame fast fate fear fears fight file filed filer films fine fines fires \
fist fled flies flight fond forename foreword fort forwards frog fund game games gate \
gates gear gears geek grim grin grind grip grit hack hate hear hears here hike hind hire \
hive holder hole homer homes hone hope hose hump identity image imagines item jack king \
lack lager lake lame lapel late lays least lend lest libel lift lines lint lisp lists \
locale lone lose lost love lump lust made maker male mall mane mare mark mary mast mate \
math maze meek mend might mike miles mind mist model modes modifier mole more moth mount \
mouth moved mover moves movie muck munch mush naked names navigator near neat nest never \
newt night node nodes omen onto opens order organism organist organized organizer ouch \
oven pace pack pane park past patch patent pays pear peek photo photon picture piles \
pipe plight port ports precious preference previously pump quiet quilt quite quits quiz \
rack rake rash rate rear rearrange reconnect recreate reference reflection reject \
rejection relocate remote removed reorganize replicate research review ride right ripe \
rode rump sack sake same sand says script seed seek seeks seize selective selector sent \
serve setting sever sewer shall shoe shoes shop short shot shown shows showy side sides \
sight sine sire site sized sizes sled slight slow smell snow soft solder some song sons \
soot sore sorts spade spare spend spending spice sport stag stall starch stitch stow \
such suit swatch swipe switched systemic tack tags take tame tape tear tears tend \
terminate text thank that thaw thee then there they thin those thus thyme tide tight \
tiles times tint tome torn transform turf turns twitch typo tyre unable uncomfortable \
unmounted unselected unto vast vend video views wake ways weak wear wears weed weeds \
weep were wheat whit wide width wife wind wine wire wise wish witch wove yeah yearn zany";

/// `words` with near-miss vocabulary words fixed, or `None` if none were.
/// Words that carry a name's punctuation (`report.pdf`, `~/x`, `{n}`,
/// `kind:image`) or a digit are never touched.
fn correct(words: &[String]) -> Option<Vec<String>> {
    let mut changed = false;
    let fixed = words
        .iter()
        .map(|word| {
            let lower = word.to_ascii_lowercase();
            if let Some((_, right)) = SLIPS.iter().find(|(slip, _)| *slip == lower) {
                changed = true;
                return (*right).to_owned();
            }
            let plain = lower.len() >= 4
                && lower.chars().all(|c| c.is_ascii_alphabetic())
                && !REAL_WORDS.split(' ').any(|real| real == lower);
            match plain
                .then(|| nearest(&lower, VOCABULARY.iter().copied()))
                .flatten()
            {
                Some(right) => {
                    changed = true;
                    // Keep a capital: it may be a folder's name.
                    let mut right = right.to_owned();
                    if word.starts_with(|c: char| c.is_ascii_uppercase()) {
                        right[..1].make_ascii_uppercase();
                    }
                    right
                }
                None => word.clone(),
            }
        })
        .collect();
    changed.then_some(fixed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::magic::{Action, Question, Verb};

    const NOW: i64 = 1_785_067_200;

    /// A sentence's reading, compactly: the kind of intent and the slots
    /// that matter, so the table below stays one line per phrasing.
    fn reading(raw: &str) -> String {
        let Some(understood) = understand(raw, NOW) else {
            return "-".into();
        };
        match understood.intent {
            Intent::Plan(command) => {
                let mut out = command.verb.label().to_lowercase();
                if command.selection.picked {
                    out += " selected";
                }
                match &command.action {
                    Action::Move(dest) | Action::Copy(dest) => {
                        out += &format!(" > {}{}", if dest.create { "+" } else { "" }, dest.name);
                    }
                    Action::Tag(tag) => out += &format!(" as {}", tag.name),
                    Action::Untag(Some(name)) => out += &format!(" -{name}"),
                    _ => {}
                }
                if let Some(within) = &command.selection.within {
                    out += &format!(" in {}", within.name);
                }
                out
            }
            Intent::Go(go) => format!("go {}", go.name),
            Intent::App(command) => format!("{command:?}"),
            Intent::Ask(Question::Count(_)) => "count".into(),
            Intent::Ask(Question::Size { .. }) => "size".into(),
            Intent::Ask(Question::Largest { top, .. }) => match top {
                Some(n) => format!("largest {n}"),
                None => "largest".into(),
            },
            Intent::Find(what) => format!("find {what}"),
        }
    }

    /// The corpus: how people phrase things, and what each must mean.
    /// This is the measure of "natural" — add a phrasing here first.
    const CORPUS: &[(&str, &str)] = &[
        // -- the grammar's own spelling is untouched
        ("delete screenshots older than 30 days", "delete"),
        (
            "move pdfs from Downloads to Documents",
            "move > Documents in Downloads",
        ),
        ("go to Downloads", "go Downloads"),
        ("how big is Downloads", "size"),
        // -- politeness and framing
        ("please delete screenshots older than 30 days", "delete"),
        (
            "could you please move pdfs to Documents?",
            "move > Documents",
        ),
        ("hey can you zip selected for me", "compress selected"),
        (
            "I want to rename selected to photo-{n}.{ext}",
            "rename selected",
        ),
        ("just go to Downloads, thanks", "go Downloads"),
        ("let's organize Downloads by type", "organize in Downloads"),
        // -- other verbs
        ("get rid of screenshots older than 30 days", "delete"),
        ("throw away old logs", "delete"),
        ("trash these", "delete selected"),
        ("rm selected", "delete selected"),
        ("put pdfs in Documents", "move > Documents"),
        (
            "put the selected files inside Archive",
            "move selected > Archive",
        ),
        ("send screenshots to Desktop", "move > Desktop"),
        (
            "stash these in a new folder called Later",
            "move selected > +Later",
        ),
        ("duplicate selected to Desktop", "copy selected > Desktop"),
        ("back up invoices to Documents", "copy > Documents"),
        (
            "make a copy of selected in Desktop",
            "copy selected > Desktop",
        ),
        ("zip up invoices", "compress"),
        ("make a zip of selected", "compress selected"),
        ("put selected in a zip", "compress selected"),
        ("clean up Downloads", "organize in Downloads"),
        ("tidy up this folder", "organize"),
        ("group Downloads by month", "organize in Downloads"),
        ("mark invoices as red", "tag as Red"),
        ("add the Work tag to invoices", "tag as Work"),
        ("remove the Work tag from invoices", "untag -Work"),
        ("remove all tags from selected", "untag selected"),
        // -- the trash is a delete, wherever it is said
        ("move screenshots to the trash", "delete"),
        ("put selected in the bin", "delete selected"),
        ("send these to trash", "delete selected"),
        // -- sentence shapes
        ("move pdfs -> Documents", "move > Documents"),
        ("rename selected as shot-{n}.png", "rename selected"),
        ("delete files that are older than 30 days", "delete"),
        ("delete screenshots more than 30 days old", "delete"),
        ("delete videos over 1gb", "delete"),
        ("delete all of my screenshots", "delete"),
        ("move all the pdfs to Documents", "move > Documents"),
        ("delete it", "delete selected"),
        ("zip those", "compress selected"),
        // -- going places
        ("take me to Documents", "go Documents"),
        ("navigate to ~/code", "go ~/code"),
        ("show me Downloads", "go Downloads"),
        ("jump to Desktop", "go Desktop"),
        // -- finding
        ("find invoices", "find invoices"),
        ("where is my resume", "find resume"),
        ("search for tax 2024", "find tax 2024"),
        // -- questions
        ("what's the size of Downloads", "size"),
        ("how much space does Downloads take up?", "size"),
        ("how much space is Downloads using", "size"),
        ("how many pdfs do I have", "count"),
        ("count the screenshots", "count"),
        ("what's taking up space", "largest"),
        ("what is taking up the most space in Downloads", "largest"),
        ("what are the 10 biggest videos", "largest 10"),
        ("top 5 largest files", "largest 5"),
        ("show me the largest files", "largest"),
        ("free up space", "largest"),
        // -- app commands, said as sentences
        ("turn on dark mode", "Theme(Dark)"),
        ("switch to dark mode", "Theme(Dark)"),
        ("make it dark", "Theme(Dark)"),
        ("turn off dark mode", "Theme(Light)"),
        ("use light theme", "Theme(Light)"),
        ("enable hidden files", "HiddenFiles(Some(true))"),
        ("turn off hidden files", "HiddenFiles(Some(false))"),
        ("show me hidden files", "HiddenFiles(Some(true))"),
        ("switch to grid view", "View(Grid)"),
        ("view as list", "View(List)"),
        ("sort files by size", "Sort(Size)"),
        ("order by name", "Sort(Name)"),
        ("open a new tab", "NewTab"),
        ("open the settings", "Settings"),
        ("go to settings", "Settings"),
        (
            "make a new folder called Tax 2026",
            "NewFolder(Some(\"Tax 2026\"))",
        ),
        ("create folder Projects", "NewFolder(Some(\"Projects\"))"),
        ("open terminal here please", "Terminal"),
        (
            "mount smb://nas/media",
            "Connect(Some(\"smb://nas/media\"))",
        ),
        // -- typos, fixed only because the sentence didn't read
        ("delte screenshots older than 30 days", "delete"),
        ("mvoe pdfs to Documents", "move > Documents"),
        ("move pdfs too Documents", "move > Documents"),
        ("zipp selected", "compress selected"),
        ("drak mode", "Theme(Dark)"),
        ("setings", "Settings"),
        ("how mnay pdfs", "count"),
        ("largset videos", "largest"),
        ("orgnaize Downloads by type", "organize in Downloads"),
        // -- and what must stay not understood
        ("quarterly earnings", "-"),
        ("report.pdf", "-"),
        // -- held out: written after the tables, to measure rather than fit
        ("bin the screenshots from last month", "delete"),
        ("can u move all my photos into Pictures", "move > Pictures"),
        (
            "stick these in a folder called Trip",
            "move selected > +Trip",
        ),
        ("i wanna zip up the selected files", "compress selected"),
        ("how much room do my videos take", "size"),
        ("show me the biggest files in Downloads", "largest"),
        ("what are the 5 largest files here", "largest 5"),
        ("clean up this folder", "organize"),
        ("sort this folder by date", "organize"),
        ("get me to the desktop", "go desktop"),
        ("open up Documents in a new tab", "go Documents"),
        ("make the app dark", "Theme(Dark)"),
        ("go dark", "Theme(Dark)"),
        ("use light theme", "Theme(Light)"),
        ("hide the hidden files", "HiddenFiles(Some(false))"),
        ("dont show hidden files", "HiddenFiles(Some(false))"),
        ("mark these as Important", "tag selected as Important"),
        ("label the invoices red", "tag as Red"),
        ("remove the Work tag from these", "untag selected -Work"),
        ("rename these to holiday-{n}", "rename selected"),
        (
            "copy everything from Downloads to Backup",
            "copy > Backup in Downloads",
        ),
        ("move files bigger than 1gb to External", "move > External"),
        ("delete empty folders", "delete"),
        (
            "trash everything older than a year in Downloads",
            "delete in Downloads",
        ),
        ("how many screenshots do i have", "count"),
        ("count the pdfs in here", "count"),
        ("whats the size of this folder", "size"),
        ("undo that", "Undo"),
        ("undo", "Undo"),
        ("go back", "Back"),
        (
            "create a folder named Taxes 2026",
            "NewFolder(Some(\"Taxes 2026\"))",
        ),
        ("new folder", "NewFolder(None)"),
        ("find my resume", "find resume"),
        ("where is my resume", "find resume"),
        ("look for budget.xlsx", "find budget.xlsx"),
        ("which files are the largest", "largest"),
        ("compress Downloads", "compress"),
        ("zip the pdfs in Documents", "compress in Documents"),
        ("archive old logs", "compress"),
        ("move it to the trash", "delete selected"),
        ("throw these away", "delete selected"),
        ("copy path", "CopyPath"),
        ("open terminal here", "Terminal"),
        ("open a terminal", "Terminal"),
        ("switch to list view", "View(List)"),
        ("show as grid", "View(Grid)"),
        ("close this tab", "CloseTab"),
        ("open settings", "Settings"),
        ("take me home", "go home"),
        ("go up", "Up"),
        ("go up a folder", "Up"),
        ("organise downloads by file type", "organize in downloads"),
        ("group these by month", "organize selected"),
        ("seperate these by extension", "organize selected"),
        ("delet teh screenshots form last week", "delete"),
        ("moev selected too Documents", "move selected > Documents"),
    ];

    #[test]
    fn the_corpus_reads_as_intended() {
        let misses: Vec<String> = CORPUS
            .iter()
            .filter(|(raw, want)| reading(raw) != *want)
            .map(|(raw, want)| format!("{raw:?}: want {want:?}, got {:?}", reading(raw)))
            .collect();
        assert!(
            misses.is_empty(),
            "{} of {} phrasings misread:\n{}",
            misses.len(),
            CORPUS.len(),
            misses.join("\n")
        );
    }

    #[test]
    fn canonical_sentences_are_not_echoed_back() {
        // "Read as" is for when something was changed.
        for raw in [
            "delete screenshots older than 30 days",
            "go to Downloads",
            "dark mode",
            "Dark Mode",
            "how big is Downloads?",
        ] {
            assert_eq!(understand(raw, NOW).unwrap().read_as, None, "{raw:?}");
        }
        // Pleasantries alone are not worth an echo either.
        assert_eq!(
            understand("please delete old logs", NOW).unwrap().read_as,
            None
        );
    }

    #[test]
    fn a_paraphrase_or_a_fix_is_always_shown_in_canonical_words() {
        let read_as = |raw: &str| understand(raw, NOW).unwrap().read_as.unwrap();
        assert_eq!(read_as("get rid of old logs"), "delete old logs");
        assert_eq!(read_as("put pdfs in Documents"), "move pdfs to Documents");
        assert_eq!(read_as("turn on dark mode"), "Dark theme");
        assert_eq!(read_as("mvoe pdfs to Documents"), "move pdfs to Documents");
        assert_eq!(
            read_as("delete screenshots more than 30 days old"),
            "delete screenshots older than 30 days"
        );
        assert_eq!(
            read_as("what's taking up space in Downloads"),
            "largest files in Downloads"
        );
    }

    #[test]
    fn fixing_a_typo_leaves_the_real_words_beside_it_alone() {
        let read_as = |raw: &str| understand(raw, NOW).unwrap().read_as.unwrap();
        // `here` is one edit from `where`, `game` from `name`.
        assert_eq!(
            read_as("mvoe game saves form here to Archive"),
            "move game saves from here to Archive"
        );
        assert_eq!(
            read_as("delte films oldr than 30 days"),
            "delete films older than 30 days"
        );
        assert_eq!(read_as("mvoe pdfs to Archiv"), "move pdfs to Archive");
        // The list protects only words the vocabulary doesn't own.
        for real in REAL_WORDS.split(' ') {
            assert!(!VOCABULARY.contains(&real), "{real} is vocabulary");
            assert!(real.len() >= 4, "{real:?}: malformed list");
        }
    }

    #[test]
    fn the_fast_distance_agrees_with_the_table() {
        // Every vocabulary word against every other, and against itself
        // with each kind of single slip.
        let lower = |x: u8, y: u8| x.eq_ignore_ascii_case(&y);
        let mut words: Vec<String> = VOCABULARY.iter().map(|w| (*w).to_owned()).collect();
        for word in VOCABULARY.iter().take(40) {
            let mut swapped = word.as_bytes().to_vec();
            swapped.swap(0, 1);
            words.push(String::from_utf8(swapped).unwrap());
            words.push(word[1..].to_owned());
            words.push(format!("{word}x"));
            words.push(format!("x{}", &word[1..]).to_uppercase());
        }
        for a in &words {
            for b in &words {
                for limit in [1, 2] {
                    let slow = if a.len().abs_diff(b.len()) > limit {
                        limit + 1
                    } else {
                        alignment(a.as_bytes(), b.as_bytes(), lower).min(limit + 1)
                    };
                    assert_eq!(distance(a, b, limit), slow, "{a} / {b} / {limit}");
                }
            }
        }
    }

    #[test]
    fn a_sentence_that_reads_is_never_corrected() {
        // "form" is one swap from "from" — and a fine thing to name a
        // file. The sentence parses, so it means what it says.
        let Intent::Plan(command) = understand("delete form older than 30 days", NOW)
            .unwrap()
            .intent
        else {
            panic!("expected a plan")
        };
        assert_eq!(command.selection.text, "form");
        assert_eq!(command.selection.within, None);
        // Names are never touched, even on the fallback path.
        assert_eq!(
            correct(&["delte".into(), "Reprot.pdf".into(), "~/Donwloads".into()]),
            Some(vec![
                "delete".into(),
                "Reprot.pdf".into(),
                "~/Donwloads".into()
            ])
        );
    }

    #[test]
    fn did_you_mean_offers_the_fix_a_valid_sentence_would_not_get() {
        // Reads fine (files named "screenshtos"), finds nothing — the
        // offer is what recovers it.
        assert_eq!(
            did_you_mean("delete screenshtos older than 30 days", NOW).as_deref(),
            Some("delete screenshots older than 30 days")
        );
        assert_eq!(
            did_you_mean("delete screenshots older than 30 days", NOW),
            None
        );
        assert_eq!(did_you_mean("delete invoices", NOW), None);
    }

    #[test]
    fn nearest_needs_a_single_close_candidate() {
        let pick = |word: &str, among: &[&'static str]| nearest(word, among.iter().copied());
        assert_eq!(
            pick("donwloads", &["Downloads", "Documents"]),
            Some("Downloads")
        );
        assert_eq!(
            pick("documnets", &["Downloads", "Documents"]),
            Some("Documents")
        );
        // Exactly right is not a correction.
        assert_eq!(pick("downloads", &["Downloads"]), None);
        // Too far, or two equally near with nothing to choose between.
        assert_eq!(pick("music", &["Movies"]), None);
        assert_eq!(pick("cat", &["car", "cap"]), None);
        // A swap of neighbours is one edit, not two.
        assert_eq!(distance("mvoe", "move", 1), 1);
        assert_eq!(distance("delte", "delete", 1), 1);
        assert_eq!(distance("abc", "xyz", 1), 2);
    }

    #[test]
    fn trimming_never_empties_a_sentence() {
        assert_eq!(trim("please"), ["please"]);
        assert_eq!(trim("thanks"), ["thanks"]);
        assert_eq!(trim("  could you   please delete x ?"), ["delete", "x"]);
        assert!(trim("  ?  ").is_empty());
    }

    #[test]
    fn every_verb_paraphrase_lands_on_a_real_verb() {
        // Guards the opener table against a typo in a replacement.
        for (from, to) in OPENERS {
            let sentence = format!("{} selected to Archive as Work", to.join(" "));
            let known = interpret(&sentence, NOW).is_some()
                || to.starts_with(&["size"])
                || to.starts_with(&["how"])
                || to.starts_with(&["count"])
                || to.starts_with(&["connect"])
                || to.starts_with(&["eject"]);
            assert!(known, "{from:?} rewrites to {to:?}, which reads as nothing");
        }
        assert_eq!(Verb::Delete.label(), "Delete");
    }
}

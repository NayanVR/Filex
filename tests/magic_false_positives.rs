//! Empirical false-positive rate for `magic::parse` as a card-or-no-card
//! gate.
//!
//! The question this answers: if the Magic card is shown whenever
//! `magic::parse` returns `Some`, how often does an *ordinary filename
//! search* get a card it shouldn't? That is exactly the set an intent
//! classifier would exist to suppress, so measuring it is what decides
//! whether §1 of `docs/design-magic-mode.md` is needed.
//!
//! Corpus: a newline-delimited list of real lowercased filenames, path
//! given by `FILEX_NAME_CORPUS`. Skipped when unset, so CI stays
//! hermetic — this is a measurement harness, not a regression test.
//!
//! Queries are derived the way a person actually produces them: search
//! is incremental, so every *word prefix* on the way to a filename is a
//! query the gate sees. `delete-reminder.js` is typed through "delete",
//! then "delete reminder", then "delete reminder js" — and the gate
//! fires on each.

use std::collections::BTreeMap;

const NOW: i64 = 1_785_067_200;

/// Word prefixes of `name` with separators normalized to spaces, which
/// is how a filename reads as typed search text.
fn queries_for(name: &str) -> Vec<String> {
    let normalized: Vec<&str> = name
        .split(|c: char| c == '-' || c == '_' || c == '.' || c == ' ')
        .filter(|w| !w.is_empty())
        .collect();
    (1..=normalized.len())
        .map(|n| normalized[..n].join(" "))
        .collect()
}

#[test]
fn measure_false_positive_rate_over_real_filenames() {
    let Ok(path) = std::env::var("FILEX_NAME_CORPUS") else {
        eprintln!("FILEX_NAME_CORPUS unset — skipping measurement");
        return;
    };
    let corpus = std::fs::read_to_string(&path).expect("reading corpus");

    let mut total = 0usize;
    let mut fired = 0usize;
    let mut by_verb: BTreeMap<String, usize> = BTreeMap::new();
    let mut samples: Vec<(String, String)> = Vec::new();

    for name in corpus.lines().filter(|l| !l.trim().is_empty()) {
        for query in queries_for(name) {
            total += 1;
            if let Some(command) = filex::magic::parse(&query, NOW) {
                fired += 1;
                *by_verb.entry(command.verb.label().to_string()).or_default() += 1;
                if samples.len() < 40 {
                    samples.push((query.clone(), name.to_string()));
                }
            }
        }
    }

    eprintln!("\n=== magic::parse as a card gate ===");
    eprintln!("queries derived from filenames : {total}");
    eprintln!(
        "parsed as a command (would card): {fired}  ({:.4}%)",
        100.0 * fired as f64 / total as f64
    );
    eprintln!("by verb: {by_verb:?}");
    eprintln!("\n--- sample false positives (query <- real filename) ---");
    for (query, name) in &samples {
        eprintln!("  {query:40} <- {name}");
    }
}

/// Evaluate the structured-evidence gate on both classes at once: how
/// many of the real-filename false positives it kills, and how many
/// genuine commands it costs.
#[test]
fn measure_structured_evidence_gate() {
    let Ok(path) = std::env::var("FILEX_NAME_CORPUS") else {
        eprintln!("FILEX_NAME_CORPUS unset — skipping measurement");
        return;
    };
    let corpus = std::fs::read_to_string(&path).expect("reading corpus");

    let (mut fired, mut survives_gate) = (0usize, 0usize);
    let mut survivors = Vec::new();
    for name in corpus.lines().filter(|l| !l.trim().is_empty()) {
        for query in queries_for(name) {
            if let Some(command) = filex::magic::parse(&query, NOW) {
                fired += 1;
                if has_structured_evidence(&command) {
                    survives_gate += 1;
                    survivors.push(format!("{query}  <- {name}"));
                }
            }
        }
    }

    let kept: Vec<&str> = COMMANDS
        .iter()
        .copied()
        .filter(|c| has_structured_evidence(&parse(c)))
        .collect();
    let lost: Vec<&str> = COMMANDS
        .iter()
        .copied()
        .filter(|c| !has_structured_evidence(&parse(c)))
        .collect();

    eprintln!("\n=== structured-evidence gate ===");
    eprintln!("false positives before gate : {fired}");
    eprintln!("false positives after gate  : {survives_gate}");
    for survivor in survivors.iter().take(10) {
        eprintln!("    SURVIVED: {survivor}");
    }
    eprintln!(
        "real commands kept          : {}/{}",
        kept.len(),
        COMMANDS.len()
    );
    eprintln!("real commands lost          : {}", lost.len());
    for command in &lost {
        eprintln!("    LOST: {command}");
    }
}

fn parse(raw: &str) -> filex::magic::Command {
    filex::magic::parse(raw, NOW).unwrap_or_else(|| panic!("{raw:?} should parse"))
}

/// Does the selection carry *structured* evidence — a kind, date, size
/// or extension filter — rather than only free text?
///
/// The hypothesis this tests: filter vocabulary is how a person
/// describes a **set** ("screenshots older than 30 days"), while bare
/// text is how they name a **thing** ("reminder"). If that holds, it
/// separates the two classes without any index lookup at all.
fn has_structured_evidence(command: &filex::magic::Command) -> bool {
    !command.selection.filters.is_empty()
}

/// Real command phrasings, in the shapes the v1 grammar accepts.
const COMMANDS: &[&str] = &[
    "delete screenshots older than 30 days",
    "delete all screenshots",
    "delete empty folders",
    "delete big videos",
    "delete old logs",
    "delete downloads older than 6 months",
    "remove duplicate photos",
    "trash screenshots from last week",
    "delete pdfs bigger than 100mb",
    "delete images older than 1 year",
    "move pdfs modified this week to Documents",
    "copy invoices to Backup",
    "rename screenshots to shot-{n}.png",
];

/// Does the whole query read as a literal fragment of some real
/// filename? This is *index* evidence, not text evidence — the thing an
/// n-gram classifier structurally cannot see.
fn matches_a_real_filename(query: &str, corpus: &[String]) -> bool {
    corpus.iter().any(|name| name.contains(query))
}

/// The decisive experiment.
///
/// The false positives above ("delete reminder" ← `delete-reminder.js`)
/// are command-*shaped* by any linguistic measure — verb plus object,
/// exactly like "delete old logs". What actually distinguishes them is
/// not in the text at all: it is that a file by that name exists. So the
/// question is whether "does this query literally match a filename?"
/// separates the two classes, and in particular whether it ever
/// suppresses a genuine command.
#[test]
fn filename_evidence_separates_commands_from_lookalikes() {
    let Ok(path) = std::env::var("FILEX_NAME_CORPUS") else {
        eprintln!("FILEX_NAME_CORPUS unset — skipping measurement");
        return;
    };
    let corpus: Vec<String> = std::fs::read_to_string(&path)
        .expect("reading corpus")
        .lines()
        .map(|name| {
            name.split(|c: char| c == '-' || c == '_' || c == '.' || c == ' ')
                .filter(|w| !w.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();

    let mut wrongly_suppressed = Vec::new();
    for command in COMMANDS {
        assert!(
            filex::magic::parse(command, NOW).is_some(),
            "{command:?} should parse"
        );
        if matches_a_real_filename(command, &corpus) {
            wrongly_suppressed.push(*command);
        }
    }

    eprintln!("\n=== filename evidence as the delete gate ===");
    eprintln!("real commands tested            : {}", COMMANDS.len());
    eprintln!(
        "wrongly suppressed by the rule  : {}",
        wrongly_suppressed.len()
    );
    for command in &wrongly_suppressed {
        eprintln!("  MISSED: {command}");
    }
    assert!(
        wrongly_suppressed.is_empty(),
        "the rule suppressed genuine commands: {wrongly_suppressed:?}"
    );
}

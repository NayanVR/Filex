//! Bounded acronym and symmetric-delete candidate dictionaries. No corpus scan.
use crate::catalog::{
    postings::{Postings, PostingsImage},
    storage::{Reader, Writer},
};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
const MAX_KEYS: usize = 100_000;
const MAX_NAMES_PER_KEY: usize = 64;
const MAX_CANDIDATES: usize = 256;

pub struct FuzzyIndex {
    keys: Vec<String>,
    postings: Postings,
}
#[derive(Serialize, Deserialize)]
pub struct FuzzyImage {
    keys: Vec<String>,
    postings: PostingsImage,
}
impl FuzzyIndex {
    pub(crate) fn remap(&mut self, reader: &Reader) -> Result<()> {
        self.postings.remap(reader)?;
        Ok(())
    }

    pub fn build<'a>(names: impl Iterator<Item = (u32, &'a str, &'a [usize])>) -> Result<Self> {
        // Keys are admitted in first-seen order until MAX_KEYS, so the
        // order below (acronym, then per word: the word and its deletes in
        // sorted order) decides which keys a large disk keeps. Lookups go
        // through one reused buffer: allocating a String per probe made this
        // the costliest step of a first index (~3 s of 11 for 642k names).
        let mut dict = HashMap::<String, Vec<u32>>::new();
        let mut key = String::new();
        let mut deleted = Vec::<String>::new();
        for (id, name, boundaries) in names {
            let mut add = |key: &str| {
                if let Some(list) = dict.get_mut(key) {
                    if list.len() < MAX_NAMES_PER_KEY && list.last() != Some(&id) {
                        list.push(id);
                    }
                } else if dict.len() < MAX_KEYS {
                    dict.insert(key.to_owned(), vec![id]);
                }
            };
            let stem = name.rfind('.').unwrap_or(name.len());
            key.clear();
            key.push_str("a:");
            key.extend(
                boundaries
                    .iter()
                    .filter(|&&p| p < stem)
                    .filter_map(|&p| name.get(p..)?.chars().next())
                    .filter(|c| c.is_alphanumeric())
                    .take(12),
            );
            if key.chars().count() >= 4 {
                add(&key);
            }
            for word in name.split(|c: char| !c.is_alphanumeric()) {
                if !(3..=24).contains(&word.chars().count()) {
                    continue;
                }
                key.clear();
                key.push_str("d:");
                key.push_str(word);
                add(&key);
                let count = deletes_into(word, &mut deleted);
                for (i, removed) in deleted[..count].iter().enumerate() {
                    if i > 0 && *removed == deleted[i - 1] {
                        continue;
                    }
                    key.clear();
                    key.push_str("d:");
                    key.push_str(removed);
                    add(&key);
                }
            }
        }
        let mut entries: Vec<_> = dict.into_iter().collect();
        entries.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));
        let (keys, lists) = entries.into_iter().unzip::<_, _, Vec<_>, Vec<_>>();
        Ok(Self {
            keys,
            postings: Postings::build(lists)?,
        })
    }
    pub fn candidates(&self, query: &str) -> Vec<(u32, bool)> {
        if !(2..=24).contains(&query.chars().count()) {
            return Vec::new();
        }
        let mut found = BTreeMap::new();
        let mut look = |key: String, acronym: bool| {
            if let Ok(i) = self.keys.binary_search(&key) {
                for id in self.postings.get(i as u32) {
                    found
                        .entry(id)
                        .and_modify(|v| *v |= acronym)
                        .or_insert(acronym);
                }
            }
        };
        look(format!("a:{query}"), true);
        look(format!("d:{query}"), false);
        for deletion in deletes(query) {
            look(format!("d:{deletion}"), false);
        }
        found.into_iter().take(MAX_CANDIDATES).collect()
    }
    pub fn save(&self, w: &mut Writer) -> Result<FuzzyImage> {
        Ok(FuzzyImage {
            keys: self.keys.clone(),
            postings: self.postings.save(w)?,
        })
    }
    pub fn load(r: &Reader, i: FuzzyImage) -> Result<Self> {
        let postings = Postings::load(r, i.postings)?;
        anyhow::ensure!(
            i.keys.len() == postings.len() && i.keys.windows(2).all(|w| w[0] < w[1]),
            "invalid fuzzy keys"
        );
        Ok(Self {
            keys: i.keys,
            postings,
        })
    }
    pub fn validate(&self, names: usize) -> Result<()> {
        for i in 0..self.keys.len() {
            anyhow::ensure!(
                self.postings.get(i as u32).all(|id| (id as usize) < names),
                "invalid fuzzy name reference"
            );
        }
        Ok(())
    }
    pub fn bytes(&self) -> usize {
        self.keys.capacity() * std::mem::size_of::<String>()
            + self.keys.iter().map(|s| s.capacity()).sum::<usize>()
            + self.postings.bytes()
    }
}
/// Every one-char deletion of `s`, sorted (duplicates adjacent), written
/// into `out`'s first entries so their buffers are reused across words.
/// Returns how many entries are filled.
fn deletes_into(s: &str, out: &mut Vec<String>) -> usize {
    let count = s.chars().count();
    while out.len() < count {
        out.push(String::new());
    }
    for (remove, slot) in out[..count].iter_mut().enumerate() {
        slot.clear();
        slot.extend(
            s.chars()
                .enumerate()
                .filter_map(|(i, c)| (i != remove).then_some(c)),
        );
    }
    out[..count].sort_unstable();
    count
}
fn deletes(s: &str) -> BTreeSet<String> {
    let chars: Vec<char> = s.chars().collect();
    (0..chars.len())
        .map(|remove| {
            chars
                .iter()
                .enumerate()
                .filter_map(|(i, c)| (i != remove).then_some(*c))
                .collect()
        })
        .collect()
}
pub fn one_edit(a: &str, b: &str) -> bool {
    let a: Vec<_> = a.chars().collect();
    let b: Vec<_> = b.chars().collect();
    if a.len().abs_diff(b.len()) > 1 {
        return false;
    }
    let (mut i, mut j, mut edits) = (0, 0, 0);
    while i < a.len() && j < b.len() {
        if a[i] == b[j] {
            i += 1;
            j += 1;
        } else {
            edits += 1;
            if edits > 1 {
                return false;
            }
            if a.len() >= b.len() {
                i += 1;
            }
            if b.len() >= a.len() {
                j += 1;
            }
        }
    }
    edits + usize::from(i < a.len() || j < b.len()) <= 1
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds from `(name, word-start byte offsets)` with ids in list order,
    /// the shape `LiteralIndex` passes (folded names, ascending ids).
    fn index(names: &[(&str, &[usize])]) -> FuzzyIndex {
        FuzzyIndex::build(
            names
                .iter()
                .enumerate()
                .map(|(id, (name, starts))| (id as u32, *name, *starts)),
        )
        .unwrap()
    }

    fn ids(found: &[(u32, bool)]) -> Vec<u32> {
        found.iter().map(|(id, _)| *id).collect()
    }

    #[test]
    fn one_edit_accepts_equal_strings() {
        // Zero edits is within "at most one"; callers rely on exact words
        // matching too.
        assert!(one_edit("report", "report"));
        assert!(one_edit("", ""));
    }

    #[test]
    fn one_edit_accepts_a_single_insert_or_delete_anywhere() {
        for (a, b) in [
            ("report", "xreport"),
            ("report", "repxort"),
            ("report", "reportx"),
            ("report", "eport"),
            ("report", "repot"),
            ("report", "repor"),
            ("", "a"),
        ] {
            assert!(one_edit(a, b), "{a} -> {b}");
            assert!(one_edit(b, a), "{b} -> {a}");
        }
    }

    #[test]
    fn one_edit_accepts_a_single_substitution_anywhere() {
        for b in ["xeport", "rexort", "reporx"] {
            assert!(one_edit("report", b), "{b}");
        }
    }

    #[test]
    fn one_edit_rejects_two_edits() {
        for (a, b) in [
            ("report", "xexort"),
            ("report", "rpot"),
            ("report", "xreportx"),
            ("report", "repo"),
            ("", "ab"),
            ("report", "reportxx"),
        ] {
            assert!(!one_edit(a, b), "{a} -> {b}");
            assert!(!one_edit(b, a), "{b} -> {a}");
        }
    }

    #[test]
    fn one_edit_rejects_adjacent_transposition() {
        // A swap is two substitutions here, not Damerau's one.
        assert!(!one_edit("report", "rpeort"));
        assert!(!one_edit("ab", "ba"));
    }

    #[test]
    fn one_edit_counts_chars_not_bytes() {
        assert!(one_edit("cafe", "café"));
        assert!(one_edit("café", "cafè"));
        assert!(one_edit("日本語", "日本"));
        assert!(one_edit("日本語", "日文語"));
        assert!(one_edit("ok", "ok🙂"));
        assert!(!one_edit("日本語", "日"));
    }

    #[test]
    fn deletes_into_matches_deletes() {
        let mut out = Vec::new();
        for s in [
            "report",
            "aab",
            "aaaa",
            "bookkeeper",
            "café",
            "日本語",
            "a🙂b",
            "x",
        ] {
            let count = deletes_into(s, &mut out);
            assert_eq!(count, s.chars().count(), "{s}");
            assert!(out[..count].windows(2).all(|w| w[0] <= w[1]), "{s}");
            let set: BTreeSet<String> = out[..count].iter().cloned().collect();
            assert_eq!(set, deletes(s), "{s}");
        }
    }

    #[test]
    fn deletes_into_reuses_buffers_without_stale_entries() {
        let mut out = Vec::new();
        assert_eq!(deletes_into("abcdef", &mut out), 6);
        assert_eq!(deletes_into("xy", &mut out), 2);
        assert_eq!(out[..2], ["x".to_owned(), "y".to_owned()]);
        assert_eq!(deletes_into("pqrs", &mut out), 4);
        assert_eq!(out[..4], ["pqr", "pqs", "prs", "qrs"]);
    }

    #[test]
    fn acronym_query_finds_name_by_word_starts() {
        let fuzzy = index(&[("xmlhttprequest.rs", &[0, 3, 7]), ("notes.txt", &[0])]);
        assert_eq!(fuzzy.candidates("xhr"), [(0, true)]);
    }

    #[test]
    fn acronym_ignores_extension_and_needs_two_letters() {
        // "a:" plus at least two word starts, all before the last '.'.
        let fuzzy = index(&[("foo_bar.txt", &[0, 4, 8]), ("solo.md", &[0, 5])]);
        assert_eq!(fuzzy.candidates("fb"), [(0, true)]);
        assert!(fuzzy.candidates("fbt").is_empty());
        assert!(fuzzy.candidates("sm").is_empty());
    }

    #[test]
    fn one_char_typo_finds_the_word() {
        // Candidates are a superset: a transposition shares a delete with the
        // word, and `one_edit` filters it out later.
        let fuzzy = index(&[("report.pdf", &[0, 7]), ("invoice.pdf", &[0, 8])]);
        for query in ["report", "rexort", "repor", "reports", "repxort", "rpeort"] {
            assert_eq!(fuzzy.candidates(query), [(0, false)], "{query}");
        }
        assert!(fuzzy.candidates("rxpxrt").is_empty());
    }

    #[test]
    fn short_and_long_words_are_not_indexed() {
        let long = "a".repeat(25);
        let name = format!("ab {long}");
        let fuzzy = index(&[(&name, &[0, 3])]);
        assert!(fuzzy.keys.iter().all(|k| !k.starts_with("d:ab")));
        assert!(fuzzy.candidates(&long[..24]).is_empty());
    }

    #[test]
    fn candidates_rejects_queries_outside_two_to_24_chars() {
        let fuzzy = index(&[("ab cd", &[0, 3])]);
        assert!(fuzzy.candidates("a").is_empty());
        assert!(fuzzy.candidates(&"a".repeat(25)).is_empty());
        assert_eq!(fuzzy.candidates("ac"), [(0, true)]);
    }

    #[test]
    fn per_key_list_is_capped() {
        let names = vec![("report", &[0usize][..]); MAX_NAMES_PER_KEY + 10];
        let fuzzy = index(&names);
        let found = fuzzy.candidates("report");
        assert_eq!(
            ids(&found),
            (0..MAX_NAMES_PER_KEY as u32).collect::<Vec<_>>()
        );
    }

    #[test]
    fn candidates_are_capped_and_lowest_ids_win() {
        // "abcd" probes d:abcd and its four deletes. Each of the first four
        // words reaches exactly one of those deletes, 64 names apiece, so
        // they alone fill MAX_CANDIDATES; "abcd" itself adds ten more.
        let mut names = Vec::new();
        for word in ["xbcd", "axcd", "abxd", "abcx"] {
            names.extend(std::iter::repeat_n(
                (word, &[0usize][..]),
                MAX_NAMES_PER_KEY,
            ));
        }
        names.extend(std::iter::repeat_n(("abcd", &[0usize][..]), 10));
        let fuzzy = index(&names);
        let found = fuzzy.candidates("abcd");
        assert_eq!(ids(&found), (0..MAX_CANDIDATES as u32).collect::<Vec<_>>());
    }

    #[test]
    fn key_count_is_capped_in_first_seen_order() {
        // A unique five-letter prefix on a run of distinct letters: each
        // 24-letter word brings at least 20 keys of its own (itself and its
        // suffix deletes), so this offers well over MAX_KEYS.
        let words: Vec<String> = (0..MAX_KEYS / 20 + 10)
            .map(|i| {
                let prefix = (0..5).map(|k| char::from(b'a' + (i / 26usize.pow(k) % 26) as u8));
                prefix.chain("abcdefghijklmnopqrs".chars()).collect()
            })
            .collect();
        let names: Vec<(&str, &[usize])> = words.iter().map(|w| (w.as_str(), &[0][..])).collect();
        let fuzzy = index(&names);
        assert_eq!(fuzzy.keys.len(), MAX_KEYS);
        // The first word's keys were admitted; the last word's own key was
        // not (it may still share delete keys admitted earlier).
        assert!(ids(&fuzzy.candidates(&words[0])).contains(&0));
        let last = format!("d:{}", words[words.len() - 1]);
        assert!(fuzzy.keys.binary_search(&last).is_err());
    }
}

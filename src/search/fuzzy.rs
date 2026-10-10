//! Bounded acronym and symmetric-delete candidate dictionaries. No corpus scan.
use crate::catalog::{
    postings::{Postings, PostingsImage},
    storage::{Reader, Writer},
};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
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
        let mut dict = BTreeMap::<String, Vec<u32>>::new();
        for (id, name, boundaries) in names {
            let mut add = |key: String| {
                if dict.len() < MAX_KEYS || dict.contains_key(&key) {
                    let list = dict.entry(key).or_default();
                    if list.len() < MAX_NAMES_PER_KEY && list.last() != Some(&id) {
                        list.push(id);
                    }
                }
            };
            let acronym: String = boundaries
                .iter()
                .filter(|&&p| p < name.rfind('.').unwrap_or(name.len()))
                .filter_map(|&p| name.get(p..)?.chars().next())
                .filter(|c| c.is_alphanumeric())
                .take(12)
                .collect();
            if acronym.chars().count() >= 2 {
                add(format!("a:{acronym}"));
            }
            for word in name.split(|c: char| !c.is_alphanumeric()) {
                if !(3..=24).contains(&word.chars().count()) {
                    continue;
                }
                add(format!("d:{word}"));
                for removed in deletes(word) {
                    add(format!("d:{removed}"));
                }
            }
        }
        let (keys, lists) = dict.into_iter().unzip::<_, _, Vec<_>, Vec<_>>();
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

//! Recently-opened folders and files, and the visit history that feeds
//! search frecency.
//!
//! A capped set of [`Visit`]s persisted as JSON in the local data dir.
//! Local-only by design — filenames are private (the telemetry stance in
//! docs/roadmap.md), so this never leaves the machine. Pure I/O + list
//! logic, no GPUI, so it's unit-tested in isolation; the app records into
//! it and persists off-thread.
//!
//! This store serves two consumers with different needs (see
//! `docs/design-search-ranking.md`):
//!
//! - the **sidebar**, which wants the last [`DISPLAY`] paths in
//!   most-recent-first order ([`Recents::recent_paths`]);
//! - **search stage-B re-ranking**, which wants a decayed weight per path
//!   ([`Recents::score_table`]) — hence [`CAP`] is far larger than what
//!   the sidebar shows.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use crate::frecency;

/// How many visits are kept. Sized for ranking, not for display: it is
/// the pool search re-ranks against. Eviction is lowest-score-first (see
/// [`Recents::record_at`]), so a long-lived favourite is not pushed out
/// by a burst of one-offs.
pub const CAP: usize = 200;

/// How many the sidebar shows.
pub const DISPLAY: usize = 20;

/// Default location: `<data_local_dir>/filex/recents.json`.
pub fn default_recents_file() -> Option<PathBuf> {
    Some(dirs::data_local_dir()?.join("filex").join("recents.json"))
}

/// One visited path: its decayed open weight and when that weight was
/// last updated (unix seconds). See [`crate::frecency`] for the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Visit {
    pub path: PathBuf,
    pub weight: f32,
    pub last_opened: i64,
    /// Whether the user ever opened this path itself, as opposed to it
    /// only earning parent credit from a child. Ranking counts both;
    /// the sidebar shows only the former, since a directory the user
    /// never opened is not something they "recently opened".
    ///
    /// Defaults to `true` so pre-parent-credit records (and the legacy
    /// migration, which only ever held opened paths) stay displayed.
    #[serde(default = "default_true")]
    pub opened: bool,
}

fn default_true() -> bool {
    true
}

/// The parent directory worth crediting when `path` is opened, if any.
///
/// Filesystem roots are excluded: `/` (and `C:\`, and a UNC share root)
/// is an ancestor of everything, so crediting it ranks nothing relative
/// to anything else while adding a junk entry to the store. Detected as
/// "a parent with no parent of its own", which holds on all three target
/// platforms without a `#[cfg]`.
fn creditable_parent(path: &Path) -> Option<PathBuf> {
    let parent = path.parent()?;
    if parent == path || parent.parent().is_none() {
        return None;
    }
    Some(parent.to_path_buf())
}

/// Visits, most-recently-opened first.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Recents {
    visits: Vec<Visit>,
}

impl Recents {
    /// Load from `file`. A missing or unreadable/corrupt file is treated
    /// as empty — recents are a convenience, never worth failing over.
    ///
    /// Also accepts the **legacy format** (a bare JSON array of paths,
    /// what shipped before frecency): those migrate to weight 1 stamped
    /// at load time, so an existing user's list survives the upgrade with
    /// their most-recent entries ordered as they were.
    pub fn load(file: &Path) -> Self {
        let Ok(contents) = std::fs::read_to_string(file) else {
            return Self::default();
        };
        Self::from_json(&contents, frecency::now_secs())
    }

    /// The parse half of [`load`](Self::load), with an explicit clock so
    /// the legacy migration is testable.
    fn from_json(contents: &str, now: i64) -> Self {
        if let Ok(current) = serde_json::from_str::<Self>(contents) {
            return current;
        }
        match serde_json::from_str::<Vec<PathBuf>>(contents) {
            // Legacy entries were most-recent-first with no timestamps.
            // Stamping them all at `now` keeps that order (ties break by
            // position) without inventing history.
            Ok(paths) => Self {
                visits: paths
                    .into_iter()
                    .map(|path| Visit {
                        path,
                        weight: 1.0,
                        last_opened: now,
                        opened: true,
                    })
                    .collect(),
            },
            Err(_) => Self::default(),
        }
    }

    /// The most-recently-opened paths, newest first, capped at
    /// [`DISPLAY`] — what the sidebar renders. Paths that only earned
    /// parent credit are excluded (see [`Visit::opened`]).
    pub fn recent_paths(&self) -> impl Iterator<Item = &Path> {
        self.visits
            .iter()
            .filter(|v| v.opened)
            .take(DISPLAY)
            .map(|v| v.path.as_path())
    }

    /// Every visit, for callers that need the weights.
    pub fn visits(&self) -> &[Visit] {
        &self.visits
    }

    pub fn is_empty(&self) -> bool {
        self.visits.is_empty()
    }

    /// Record `path` as opened now.
    pub fn record(&mut self, path: PathBuf) {
        self.record_at(path, frecency::now_secs());
    }

    /// [`record`](Self::record) with an explicit clock.
    ///
    /// Adds a full open to `path` and [`PARENT_CREDIT`] to its parent
    /// directory, decaying each existing weight to `now` first (the
    /// decay-on-write model). The visited path moves to the front, so
    /// stored order stays most-recent-first for the sidebar; eviction
    /// past [`CAP`] drops the *lowest-scoring* visit, not the oldest.
    ///
    /// [`PARENT_CREDIT`]: crate::frecency::PARENT_CREDIT
    pub fn record_at(&mut self, path: PathBuf, now: i64) {
        if let Some(parent) = creditable_parent(&path) {
            self.credit(parent, frecency::PARENT_CREDIT, now, false);
        }
        self.credit(path, 1.0, now, true);
        self.evict(now);
    }

    /// Add `amount` to `path`'s decayed weight. `direct` distinguishes a
    /// real open (moves to the front of the display list, marks the visit
    /// [`opened`](Visit::opened)) from parent credit, which updates the
    /// score without claiming the parent is what the user opened.
    fn credit(&mut self, path: PathBuf, amount: f32, now: i64, direct: bool) {
        let existing = self.visits.iter().position(|v| v.path == path);
        let mut visit = match existing {
            Some(ix) => {
                let mut visit = self.visits.remove(ix);
                visit.weight = frecency::decay(visit.weight, now - visit.last_opened);
                visit
            }
            None => Visit {
                path,
                weight: 0.0,
                last_opened: now,
                opened: false,
            },
        };
        visit.weight += amount;
        visit.last_opened = now;
        visit.opened |= direct;
        match (direct, existing) {
            (true, _) => self.visits.insert(0, visit),
            // Keep display order: a parent credit is not a visit.
            (false, Some(ix)) => self.visits.insert(ix, visit),
            (false, None) => self.visits.push(visit),
        }
    }

    /// Drop the lowest-scoring visits until at most [`CAP`] remain.
    fn evict(&mut self, now: i64) {
        if self.visits.len() <= CAP {
            return;
        }
        let mut ranked: Vec<(usize, f32)> = self
            .visits
            .iter()
            .enumerate()
            .map(|(ix, v)| (ix, frecency::score(v.weight, v.last_opened, now)))
            .collect();
        // Worst first; ties break toward evicting the older position.
        ranked.sort_by(|a, b| a.1.total_cmp(&b.1).then(b.0.cmp(&a.0)));
        let doomed: std::collections::HashSet<usize> = ranked
            .into_iter()
            .take(self.visits.len() - CAP)
            .map(|(ix, _)| ix)
            .collect();
        let mut ix = 0;
        self.visits.retain(|_| {
            let keep = !doomed.contains(&ix);
            ix += 1;
            keep
        });
    }

    /// Path → current frecency score, for search re-ranking (stage B).
    ///
    /// Owned so the app can cache it behind an `Arc` and hand it to a
    /// background search task; it is rebuilt when visits change, not per
    /// keystroke. Never consulted per scanned entry — resolving a path
    /// inside the index scan is exactly what the design forbids.
    pub fn score_table(&self, now: i64) -> HashMap<PathBuf, f32> {
        self.visits
            .iter()
            .map(|v| {
                (
                    v.path.clone(),
                    frecency::score(v.weight, v.last_opened, now),
                )
            })
            .collect()
    }

    /// Forget everything.
    pub fn clear(&mut self) {
        self.visits.clear();
    }

    /// Write as JSON via a sibling temp file + rename. Creates parent
    /// dirs as needed.
    pub fn save(&self, file: &Path) -> Result<()> {
        let parent = file.parent().context("recents file has no parent dir")?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
        let json = serde_json::to_string(self).context("serializing recents")?;
        let tmp = file.with_extension("json.tmp");
        std::fs::write(&tmp, json).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, file).with_context(|| format!("replacing {}", file.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 86_400;

    fn paths(recents: &Recents) -> Vec<&str> {
        recents
            .recent_paths()
            .map(|p| p.to_str().unwrap())
            .collect()
    }

    fn score_of(recents: &Recents, path: &str, now: i64) -> f32 {
        recents
            .score_table(now)
            .get(Path::new(path))
            .copied()
            .unwrap_or(0.0)
    }

    #[test]
    fn record_puts_newest_first() {
        let mut r = Recents::default();
        r.record_at("/a".into(), 0);
        r.record_at("/b".into(), DAY);
        assert_eq!(paths(&r), ["/b", "/a"]);
    }

    #[test]
    fn record_dedups_by_moving_to_front() {
        let mut r = Recents::default();
        r.record_at("/a".into(), 0);
        r.record_at("/b".into(), DAY);
        r.record_at("/a".into(), 2 * DAY);
        assert_eq!(paths(&r), ["/a", "/b"]);
    }

    #[test]
    fn recent_paths_caps_at_display_but_store_keeps_more() {
        let mut r = Recents::default();
        for i in 0..(DISPLAY + 5) {
            r.record_at(PathBuf::from(format!("/p{i}")), i as i64 * DAY);
        }
        assert_eq!(r.recent_paths().count(), DISPLAY);
        assert!(r.visits().len() > DISPLAY);
    }

    #[test]
    fn record_caps_store_length() {
        let mut r = Recents::default();
        for i in 0..(CAP + 50) {
            r.record_at(PathBuf::from(format!("/dir{i}/p{i}")), i as i64 * DAY);
        }
        assert!(r.visits().len() <= CAP);
    }

    #[test]
    fn eviction_keeps_the_frequently_opened_over_a_burst() {
        let mut r = Recents::default();
        // A favourite, opened every day for a long time.
        for day in 0..60 {
            r.record_at("/fav".into(), day * DAY);
        }
        // Then a burst of one-off paths, enough to overflow the cap.
        let burst_start = 60 * DAY;
        for i in 0..(CAP + 50) {
            r.record_at(PathBuf::from(format!("/junk{i}")), burst_start + i as i64);
        }
        let now = burst_start + CAP as i64;
        assert!(
            score_of(&r, "/fav", now) > 0.0,
            "the daily-opened favourite must survive a burst of one-offs"
        );
    }

    #[test]
    fn repeated_opens_outrank_a_single_open() {
        let mut r = Recents::default();
        r.record_at("/once".into(), 0);
        for day in 0..5 {
            r.record_at("/often".into(), day * DAY);
        }
        let now = 5 * DAY;
        assert!(score_of(&r, "/often", now) > score_of(&r, "/once", now));
    }

    #[test]
    fn stale_opens_decay_below_recent_ones() {
        let mut r = Recents::default();
        r.record_at("/old".into(), 0);
        r.record_at("/new".into(), 180 * DAY);
        let now = 180 * DAY;
        assert!(score_of(&r, "/new", now) > score_of(&r, "/old", now));
    }

    #[test]
    fn parent_directory_gets_partial_credit() {
        let mut r = Recents::default();
        r.record_at("/projects/filex/main.rs".into(), 0);
        let parent = score_of(&r, "/projects/filex", 0);
        let file = score_of(&r, "/projects/filex/main.rs", 0);
        assert!(parent > 0.0, "parent dir should be credited");
        assert!(parent < file, "parent credit is partial, not a full open");
    }

    #[test]
    fn parent_credit_does_not_claim_a_visit_slot_at_the_front() {
        let mut r = Recents::default();
        r.record_at("/a/b/c.txt".into(), 0);
        // The file the user opened leads; its parent must not displace it.
        assert_eq!(paths(&r)[0], "/a/b/c.txt");
    }

    #[test]
    fn parent_credited_dirs_are_not_shown_as_recently_opened() {
        // Regression: parent credit used to push the parent into the
        // sidebar list, showing folders the user never opened.
        let mut r = Recents::default();
        r.record_at("/work/report.pdf".into(), 0);
        assert_eq!(paths(&r), ["/work/report.pdf"]);
        assert!(score_of(&r, "/work", 0) > 0.0, "still credited for ranking");

        // Opening it directly promotes it into the display list.
        r.record_at("/work".into(), DAY);
        assert_eq!(paths(&r), ["/work", "/work/report.pdf"]);
    }

    #[test]
    fn filesystem_root_is_never_credited() {
        // Regression: "/a".parent() is "/", which is an ancestor of
        // everything — crediting it ranks nothing and adds junk.
        let mut r = Recents::default();
        r.record_at("/a".into(), 0);
        assert_eq!(paths(&r), ["/a"]);
        assert_eq!(r.visits().len(), 1, "no root visit was stored");
    }

    #[test]
    fn parent_credit_accumulates_across_different_children() {
        let mut r = Recents::default();
        r.record_at("/work/a.txt".into(), 0);
        let after_one = score_of(&r, "/work", 0);
        r.record_at("/work/b.txt".into(), 0);
        assert!(score_of(&r, "/work", 0) > after_one);
    }

    #[test]
    fn clear_empties() {
        let mut r = Recents::default();
        r.record_at("/a".into(), 0);
        r.clear();
        assert!(r.is_empty());
    }

    #[test]
    fn save_then_load_round_trips_and_missing_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("nested").join("recents.json");
        assert!(Recents::load(&file).is_empty()); // missing → empty
        let mut r = Recents::default();
        r.record_at("/x".into(), 0);
        r.record_at("/y".into(), DAY);
        r.save(&file).unwrap();
        assert_eq!(Recents::load(&file), r);
    }

    #[test]
    fn corrupt_file_loads_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("recents.json");
        std::fs::write(&file, "{ not json").unwrap();
        assert!(Recents::load(&file).is_empty());
    }

    #[test]
    fn legacy_path_array_migrates_preserving_order() {
        // The pre-frecency on-disk format.
        let r = Recents::from_json(r#"["/a","/b","/c"]"#, 1_000);
        assert_eq!(paths(&r), ["/a", "/b", "/c"]);
        assert!(
            score_of(&r, "/a", 1_000) > 0.0,
            "migrated visits carry weight"
        );
        assert_eq!(r.visits()[0].last_opened, 1_000, "stamped at load time");
    }

    #[test]
    fn current_format_is_preferred_over_legacy_parse() {
        let mut r = Recents::default();
        r.record_at("/a".into(), 42);
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(Recents::from_json(&json, 9_999), r);
    }
}

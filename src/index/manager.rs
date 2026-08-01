//! Multi-root coordination: the persisted root list, validation for
//! adding roots, and merged search across several volume indexes.
//!
//! Each root is its own [`super::LiveIndex`] (own watcher, writer, and
//! snapshot); this module only holds the logic that spans them, kept
//! GUI-free so it's testable — the async orchestration lives in the app.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context as _, Result, bail};

use super::watcher::SharedIndex;
use super::{Score, SearchHit};
use crate::search_filter::Filter;

/// A rayon pool dedicated to search scans, isolated from the global pool
/// the writer's directory walks (`jwalk`) use.
///
/// Optimization B removed the reader/writer *lock* convoy, but reads and
/// the writer's walks still shared rayon's global pool — so a big
/// filesystem burst (a `jwalk` over a large moved-in tree) could saturate
/// every worker and starve keystroke searches of threads, spiking search
/// latency to ~1 s during the burst even with no lock involved. A separate
/// pool guarantees a search always has threads of its own to run on; the
/// two pools may briefly oversubscribe the cores during a burst, but the
/// OS time-slices them, so search progresses instead of waiting behind the
/// walk's tasks.
fn search_pool() -> &'static rayon::ThreadPool {
    static POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();
    POOL.get_or_init(|| {
        rayon::ThreadPoolBuilder::new()
            .thread_name(|i| format!("filex-search-{i}"))
            .build()
            .expect("build search thread pool")
    })
}

/// Default location of the root list: `<data_local_dir>/filex/roots.list`,
/// one absolute path per line (UTF-8; blank lines ignored).
pub fn default_roots_file() -> Option<PathBuf> {
    Some(dirs::data_local_dir()?.join("filex").join("roots.list"))
}

/// Load the configured roots. A missing file is an empty list, not an
/// error (first launch).
pub fn load_roots(file: &Path) -> Vec<PathBuf> {
    let Ok(contents) = std::fs::read_to_string(file) else {
        return Vec::new();
    };
    contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .collect()
}

pub fn save_roots(file: &Path, roots: &[PathBuf]) -> Result<()> {
    let parent = file.parent().context("roots file has no parent dir")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("creating {}", parent.display()))?;
    let mut contents = String::new();
    for root in roots {
        let Some(as_str) = root.to_str() else {
            bail!("root path {} is not UTF-8", root.display());
        };
        contents.push_str(as_str);
        contents.push('\n');
    }
    std::fs::write(file, contents).with_context(|| format!("writing {}", file.display()))
}

/// Canonicalize a candidate root and reject duplicates and nesting in
/// either direction — nested roots would double-index and produce
/// duplicate search results. `existing` must already be canonical.
pub fn validate_new_root(existing: &[PathBuf], candidate: &Path) -> Result<PathBuf> {
    let canonical = candidate
        .canonicalize()
        .with_context(|| format!("resolving {}", candidate.display()))?;
    if !canonical.is_dir() {
        bail!("{} is not a directory", canonical.display());
    }
    for root in existing {
        if canonical == *root {
            bail!("{} is already indexed", canonical.display());
        }
        if canonical.starts_with(root) {
            bail!(
                "{} is inside the already-indexed root {}",
                canonical.display(),
                root.display()
            );
        }
        if root.starts_with(&canonical) {
            bail!(
                "{} contains the already-indexed root {} — remove that first",
                canonical.display(),
                root.display()
            );
        }
    }
    Ok(canonical)
}

/// One display-ready search hit from a merged multi-root query.
#[derive(Debug, Clone)]
pub struct MergedHit {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    /// Raw match quality from the index, before any frecency boost.
    pub score: Score,
    /// Final sort value within [`Score::kind`], lower first: match
    /// quality and name length, less the frecency boost. Exposed so the
    /// ordering is assertable in tests.
    pub rank: i64,
}

/// How many candidates per index the scan keeps beyond `limit`, so the
/// frecency re-rank has something to reorder. Only heap depth, not scan
/// work — see `docs/design-search-ranking.md` decision 1.
const OVERFETCH: usize = 4;

/// Weight of one point of match penalty in rank units. Above any
/// possible `name_len`, so match quality always dominates the
/// name-length tiebreak, exactly as the pre-frecency tuple ordering did.
const PENALTY_SCALE: i64 = 1_024;

/// The most a frecency boost can improve a hit's rank.
///
/// Applied *within* a [`MatchKind`] tier, never across tiers, so a
/// frequently-opened file can win a near-tie but can never hijack a
/// query it barely matches. Comfortably larger than any `name_len`
/// (so frecency beats the name-length tiebreak between two equally good
/// literal matches) and a few `PENALTY_SCALE` units (so among fuzzy hits
/// it only overturns close calls).
const MAX_BOOST: i64 = 4 * PENALTY_SCALE;

/// Frecency score at which a hit earns the full [`MAX_BOOST`]. Roughly
/// "opened most days for a few weeks" under the decay in
/// [`crate::frecency`].
const BOOST_SATURATION: f32 = 8.0;

/// Map a frecency score to rank units, saturating at [`MAX_BOOST`].
fn boost_for(score: f32) -> i64 {
    if score <= 0.0 {
        return 0;
    }
    let fraction = (score / BOOST_SATURATION).min(1.0);
    (fraction * MAX_BOOST as f32) as i64
}

/// The sort value for a hit within its match tier — lower ranks first.
fn rank_of(score: Score, boost: i64) -> i64 {
    score.penalty as i64 * PENALTY_SCALE + score.name_len as i64 - boost
}

/// Search every index and merge by the same ranking a single index uses.
/// Locks are taken one index at a time, read-only; a poisoned index still
/// answers.
///
/// `frecency` is the path → score table from [`crate::recents`], applied
/// as **stage B**: the scan overfetches, paths get resolved for those
/// candidates only, and the boost reorders them. Pass an empty table to
/// rank on match quality alone.
pub fn search_all(
    indexes: &[SharedIndex],
    query: &str,
    filters: &[Filter],
    limit: usize,
    frecency: &HashMap<PathBuf, f32>,
) -> Vec<MergedHit> {
    search_all_scoped(
        indexes,
        query,
        filters,
        limit,
        frecency,
        &crate::index::NEVER_CANCEL,
        None,
    )
}

/// [`search_all`], abortable mid-scan and optionally scoped to a
/// directory ("Current Dir").
///
/// A newer keystroke sets `cancel`, and both the arena scan and the path
/// materialization below wind down — the latter matters because
/// materializing paths for an overfetched (`limit * OVERFETCH`) candidate
/// set walks a parent chain per hit under the read lock, which must not
/// keep running once the query it belongs to is stale.
///
/// `scope_dir`, when set, limits results to that directory's subtree.
/// Resolution happens *here*, under each index's read lock, rather than on
/// the UI thread: an index whose root does not contain `scope_dir` is
/// skipped, and within the containing index the directory resolves to the
/// [`ScanCtl::scope`] entry. A `scope_dir` that no index has caught up to
/// yet simply yields nothing — the folder is not searchable until indexed.
pub fn search_all_scoped(
    indexes: &[SharedIndex],
    query: &str,
    filters: &[Filter],
    limit: usize,
    frecency: &HashMap<PathBuf, f32>,
    cancel: &std::sync::atomic::AtomicBool,
    scope_dir: Option<&std::path::Path>,
) -> Vec<MergedHit> {
    use crate::index::ScanCtl;
    use std::sync::atomic::Ordering;

    let fetch = limit.saturating_mul(OVERFETCH);
    let mut merged: Vec<MergedHit> = Vec::new();
    for shared in indexes {
        if cancel.load(Ordering::Relaxed) {
            return Vec::new();
        }
        // Lock-free snapshot (optimization B): readers never block the
        // writer and vice versa. `load_full` (owned `Arc`) rather than the
        // load guard, so the snapshot can cross into the search pool below.
        let index = shared.load_full();
        // Resolve the "Current Dir" scope against *this* index. A dir the
        // index doesn't contain (different root, or not yet indexed) means
        // this index contributes nothing to a scoped search.
        let scope = match scope_dir {
            None => None,
            Some(dir) => match dir.strip_prefix(index.root_path()) {
                // The root itself: scope is the whole index (ROOT).
                Ok(rel) if rel.as_os_str().is_empty() => Some(crate::index::ROOT),
                Ok(rel) => match index.resolve(rel) {
                    Some(id) => Some(id),
                    None => continue, // not indexed here
                },
                Err(_) => continue, // dir is under a different root
            },
        };
        let ctl = ScanCtl { cancel, scope };
        // Run the parallel scan on the dedicated search pool, so a
        // concurrent filesystem walk on the global pool cannot starve it.
        let hits = search_pool().install(|| index.search_filtered_cancellable(query, filters, fetch, &ctl));
        merged.extend(
            hits.into_iter().filter_map(
                |SearchHit { id, score }| {
                    if cancel.load(Ordering::Relaxed) {
                        return None;
                    }
                    let path = index.path_of(id)?;
                    let boost = if frecency.is_empty() {
                        0
                    } else {
                        boost_for(frecency.get(path.as_path()).copied().unwrap_or(0.0))
                    };
                    Some(MergedHit {
                        name: index.name_of(id)?.to_string(),
                        is_dir: index.is_dir(id)?,
                        score,
                        rank: rank_of(score, boost),
                        path,
                    })
                },
            ),
        );
    }
    // A cancelled scan produces a partial, unranked set the caller will
    // discard by generation check — don't bother sorting it.
    if cancel.load(Ordering::Relaxed) {
        return Vec::new();
    }
    // Tier first, so a boost never lifts a hit out of its match kind.
    merged.sort_by(|a, b| {
        (a.score.kind, a.rank)
            .cmp(&(b.score.kind, b.rank))
            .then_with(|| a.path.cmp(&b.path))
    });
    merged.truncate(limit);
    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{ROOT, VolumeIndex};

    fn shared(index: VolumeIndex) -> SharedIndex {
        SharedIndex::new(index)
    }

    #[test]
    fn roots_file_roundtrips_and_missing_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("cfg").join("roots.list");
        assert!(load_roots(&file).is_empty());

        let roots = vec![PathBuf::from("/vol/a"), PathBuf::from("/vol/b c")];
        save_roots(&file, &roots).unwrap();
        assert_eq!(load_roots(&file), roots);
    }

    #[test]
    fn validate_rejects_duplicates_and_nesting_both_ways() {
        let base = tempfile::tempdir().unwrap();
        let parent = base.path().canonicalize().unwrap();
        let child = parent.join("child");
        let sibling = parent.join("sibling");
        std::fs::create_dir(&child).unwrap();
        std::fs::create_dir(&sibling).unwrap();

        let existing = vec![child.clone()];
        assert!(validate_new_root(&existing, &child).is_err()); // duplicate
        assert!(validate_new_root(&existing, &child.join("..")).is_err()); // contains child
        let err = validate_new_root(&existing, &parent).unwrap_err();
        assert!(err.to_string().contains("contains"));
        assert_eq!(validate_new_root(&existing, &sibling).unwrap(), sibling);

        // Inside an existing root.
        let existing = vec![parent.clone()];
        let err = validate_new_root(&existing, &sibling).unwrap_err();
        assert!(err.to_string().contains("inside"));
    }

    #[test]
    fn validate_rejects_files_and_missing_paths() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        std::fs::write(&file, b"x").unwrap();
        assert!(validate_new_root(&[], &file).is_err());
        assert!(validate_new_root(&[], &dir.path().join("missing")).is_err());
    }

    #[test]
    fn search_all_merges_ranked_across_indexes() {
        let mut a = VolumeIndex::new("/vol-a");
        a.insert(ROOT, "main.rs", false).unwrap(); // prefix match
        a.insert(ROOT, "domain.txt", false).unwrap(); // substring
        let mut b = VolumeIndex::new("/vol-b");
        b.insert(ROOT, "main", true).unwrap(); // exact match
        b.insert(ROOT, "test_main.py", false).unwrap(); // word boundary

        let hits = search_all(&[shared(a), shared(b)], "main", &[], 10, &none());
        let names: Vec<&str> = hits.iter().map(|h| h.name.as_str()).collect();
        // exact (b) > prefix (a) > boundary (b) > substring (a)
        assert_eq!(names, ["main", "main.rs", "test_main.py", "domain.txt"]);
        assert_eq!(hits[0].path, PathBuf::from("/vol-b/main"));
        assert!(hits[0].is_dir);
        assert_eq!(hits[1].path, PathBuf::from("/vol-a/main.rs"));
    }

    #[test]
    fn search_all_respects_limit_and_empty_inputs() {
        let mut a = VolumeIndex::new("/vol-a");
        for i in 0..10 {
            a.insert(ROOT, &format!("file-{i}.txt"), false).unwrap();
        }
        let indexes = [shared(a)];
        assert_eq!(search_all(&indexes, "file", &[], 3, &none()).len(), 3);
        assert!(search_all(&indexes, "", &[], 10, &none()).is_empty());
        assert!(search_all(&[], "file", &[], 10, &none()).is_empty());
    }

    /// No visit history — rank on match quality alone.
    fn none() -> HashMap<PathBuf, f32> {
        HashMap::new()
    }

    fn frecency(entries: &[(&str, f32)]) -> HashMap<PathBuf, f32> {
        entries.iter().map(|(p, s)| (PathBuf::from(p), *s)).collect()
    }

    fn names_for(hits: &[MergedHit]) -> Vec<&str> {
        hits.iter().map(|h| h.name.as_str()).collect()
    }

    #[test]
    fn frecency_reorders_within_a_match_tier() {
        let mut a = VolumeIndex::new("/vol");
        // Both are word-boundary matches; the shorter name wins on the
        // built-in tiebreak...
        a.insert(ROOT, "notes_report.txt", false).unwrap();
        a.insert(ROOT, "quarterly_report_archive.txt", false).unwrap();
        let indexes = [shared(a)];

        assert_eq!(
            names_for(&search_all(&indexes, "report", &[], 10, &none())),
            ["notes_report.txt", "quarterly_report_archive.txt"]
        );

        // ...until the longer-named one is the file the user actually opens.
        let hot = frecency(&[("/vol/quarterly_report_archive.txt", 10.0)]);
        assert_eq!(
            names_for(&search_all(&indexes, "report", &[], 10, &hot)),
            ["quarterly_report_archive.txt", "notes_report.txt"]
        );
    }

    #[test]
    fn frecency_never_lifts_a_hit_out_of_its_match_tier() {
        let mut a = VolumeIndex::new("/vol");
        a.insert(ROOT, "report", false).unwrap(); // exact
        a.insert(ROOT, "old_report_draft.txt", false).unwrap(); // boundary
        let indexes = [shared(a)];

        // Even a maximally hot boundary match stays below the exact one.
        let hot = frecency(&[("/vol/old_report_draft.txt", 1_000.0)]);
        assert_eq!(
            names_for(&search_all(&indexes, "report", &[], 10, &hot)),
            ["report", "old_report_draft.txt"]
        );
    }

    #[test]
    fn frecency_cannot_promote_a_fuzzy_hit_over_a_literal_one() {
        let mut a = VolumeIndex::new("/vol");
        a.insert(ROOT, "buried_dsr_notes.txt", false).unwrap(); // literal
        a.insert(ROOT, "Design System Review.pdf", false).unwrap(); // fuzzy
        let indexes = [shared(a)];

        let hot = frecency(&[("/vol/Design System Review.pdf", 1_000.0)]);
        let hits = search_all(&indexes, "dsr", &[], 10, &hot);
        assert_eq!(
            names_for(&hits),
            ["buried_dsr_notes.txt", "Design System Review.pdf"],
            "a fuzzy hit must stay below every literal hit, however hot"
        );
    }

    #[test]
    fn overfetch_lets_a_frecent_hit_survive_a_small_limit() {
        // A hot file that the scan's own ranking would place outside
        // `limit` must still make the final list — that is what the
        // stage-A overfetch buys.
        const HOT: &str = "report_with_a_much_longer_name.txt";
        let mut a = VolumeIndex::new("/vol");
        for i in 0..5 {
            a.insert(ROOT, &format!("report_{i:02}.txt"), false).unwrap();
        }
        a.insert(ROOT, HOT, false).unwrap();
        let indexes = [shared(a)];

        // Ranked last on name length, so a limit of 2 excludes it...
        let cold = search_all(&indexes, "report", &[], 2, &none());
        assert!(!names_for(&cold).contains(&HOT));

        // ...but the overfetched scan still carried it into stage B,
        // where the boost brings it to the top.
        let hits = search_all(&indexes, "report", &[], 2, &frecency(&[("/vol/report_with_a_much_longer_name.txt", 10.0)]));
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].name, HOT);
    }

    #[test]
    fn overfetch_is_bounded_so_a_cold_tail_is_still_cut() {
        // The flip side: overfetch is `limit * OVERFETCH`, not unbounded.
        // A hot file ranked beyond that window is genuinely gone, and the
        // result set is still exactly `limit` long.
        let mut a = VolumeIndex::new("/vol");
        for i in 0..40 {
            a.insert(ROOT, &format!("report_{i:02}.txt"), false).unwrap();
        }
        let indexes = [shared(a)];
        let hot = frecency(&[("/vol/report_39.txt", 10.0)]);
        let hits = search_all(&indexes, "report", &[], 2, &hot);
        assert_eq!(hits.len(), 2);
    }

    /// Regression for the fuzzy-gate bug, at the layer where it lived: a
    /// *composition* of two individually-correct decisions. Decision 1
    /// overfetches by `OVERFETCH` for stage-B re-ranking; decision 2 runs
    /// the fuzzy pass only when the literal pass came up short. But
    /// `search_all` passes its overfetched width down as `limit`, and
    /// `finish` reused that same number as the gate — so decision 1
    /// silently widened decision 2's gate fourfold, putting a second full
    /// scan of the arena (~20-24 ms at 1.2M entries, against ~4-7 ms for
    /// the literal pass) onto essentially every keystroke.
    ///
    /// Note what this test can and cannot see. The purely wasteful window
    /// — `limit <= literal hits < limit * OVERFETCH` — is **invisible to
    /// any assertion on output**, because fuzzy hits rank below every
    /// literal hit and get truncated away before they are returned. That
    /// is precisely why the bug survived a full suite: it cost only
    /// latency. What is observable, and what this pins, is the adjacent
    /// window `FUZZY_GATE <= literal hits < limit`, where the old gate
    /// showed fuzzy filler and the new one does not.
    #[test]
    fn a_useful_literal_result_list_gets_no_fuzzy_filler() {
        use crate::index::{FUZZY_GATE, MatchKind};

        let mut a = VolumeIndex::new("/vol");
        for i in 0..FUZZY_GATE + 10 {
            a.insert(ROOT, &format!("dsr_{i}.txt"), false).unwrap();
        }
        a.insert(ROOT, "Design System Review.pdf", false).unwrap();
        let indexes = [shared(a)];

        // Room to spare, so nothing is truncated: if the fuzzy pass ran,
        // its hit would be in the output.
        let hits = search_all(&indexes, "dsr", &[], 500, &none());

        assert_eq!(hits.len(), FUZZY_GATE + 10, "the literal hits, and only those");
        assert!(
            hits.iter().all(|h| h.score.kind != MatchKind::Fuzzy),
            "{} literal hits is already a useful result list; the second \
             scan is latency the user did not need to pay",
            FUZZY_GATE + 10
        );
    }
}

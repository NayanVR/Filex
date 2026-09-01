//! Subsequence ("fuzzy") filename matching — `dsr` → `Design System
//! Review.pdf`. See `docs/design-search-ranking.md`.
//!
//! Two greedy O(n) passes, best score wins: an **acronym pass** that may
//! only match at word starts (so `dsr` hits `Design System Review` but
//! not `dadsrock.mp3`), then a plain greedy pass anywhere. Not the
//! optimal O(n·m) DP fzf uses — worse alignment on adversarial names is
//! the accepted trade. Only runs when literal matching found fewer than
//! `limit` hits.

/// Credit for a needle char landing on a word start or immediately after
/// the previous match. Awarded once, not twice — the two signals are
/// alternatives. The dominant term: it is why an acronym hit outranks a
/// name that merely contains the letters.
const MATCH_BONUS: i32 = 30;

/// Cost per character skipped *between* matched characters. Much smaller
/// than [`MATCH_BONUS`] so a gappy acronym still beats a dense junk match.
const GAP_COST: i32 = 2;

/// Cost per character before the match starts — a mild preference for
/// matches near the front of the name.
const START_COST: i32 = 1;

/// Cost per unmatched trailing character — the name-length tiebreak.
const TAIL_COST: i32 = 1;

/// Subtracted from to produce the "lower is better" penalty. Above any
/// achievable score, so penalties stay non-negative.
const SCORE_BASELINE: i32 = 10_000;

/// Case-fold one char. ASCII fast path; the fallback takes the first char
/// of the Unicode lowercase mapping.
fn fold(c: char) -> char {
    if c.is_ascii() {
        c.to_ascii_lowercase()
    } else {
        c.to_lowercase().next().unwrap_or(c)
    }
}

/// Does a char at this position start a word? True at the name start,
/// after any non-alphanumeric, at a camelCase hump (`fileName`), and at a
/// letter→digit transition (`report2024`). Deliberately generous — a
/// missed boundary silently costs an acronym match.
fn is_boundary(prev: Option<char>, cur: char) -> bool {
    match prev {
        None => true,
        Some(prev) => {
            !prev.is_alphanumeric()
                || (prev.is_lowercase() && cur.is_uppercase())
                || (prev.is_alphabetic() && cur.is_numeric())
        }
    }
}

/// Greedily match every char of `needle` (already case-folded) against
/// `haystack`, taking the first acceptable position for each, and score
/// the alignment. With `boundary_only`, a char may only match where
/// [`is_boundary`] holds. `None` if the needle can't be completed.
/// Higher is better.
fn align(haystack: &str, needle: &str, boundary_only: bool) -> Option<i32> {
    let mut wanted = needle.chars();
    let mut want = wanted.next()?;

    let mut score = 0i32;
    let mut first: Option<i32> = None;
    let mut last = 0i32;
    let mut prev: Option<char> = None;
    let mut ix = 0i32;
    let mut done = false;

    for cur in haystack.chars() {
        if !done && fold(cur) == want && (!boundary_only || is_boundary(prev, cur)) {
            let consecutive = first.is_some() && ix == last + 1;
            if consecutive || is_boundary(prev, cur) {
                score += MATCH_BONUS;
            }
            match first {
                None => first = Some(ix),
                Some(_) => score -= (ix - last - 1) * GAP_COST,
            }
            last = ix;
            match wanted.next() {
                Some(next) => want = next,
                None => done = true,
            }
        }
        prev = Some(cur);
        ix += 1;
    }

    if !done {
        return None;
    }
    let tail = (ix - last - 1).max(0);
    Some(score - first.unwrap_or(0) * START_COST - tail * TAIL_COST)
}

/// Fuzzy-match `needle` against `haystack`, returning a **penalty** (lower
/// is better, composing with the index's "lower wins" key), or `None` if
/// `needle` is not a subsequence. `needle` must already be case-folded;
/// `haystack` keeps its original case, since camelCase humps are a
/// word-boundary signal.
pub fn penalty(haystack: &str, needle: &str) -> Option<u16> {
    if needle.is_empty() {
        return None;
    }
    // Boundary pass usually fails fast, so this is rarely two full scans.
    let boundary = align(haystack, needle, true);
    let greedy = align(haystack, needle, false);
    let score = boundary.into_iter().chain(greedy).max()?;
    Some((SCORE_BASELINE - score).clamp(0, u16::MAX as i32) as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rank several names against one needle, best first.
    fn ranked<'a>(needle: &str, names: &[&'a str]) -> Vec<&'a str> {
        let mut scored: Vec<(u16, &str)> = names
            .iter()
            .filter_map(|n| penalty(n, needle).map(|p| (p, *n)))
            .collect();
        scored.sort();
        scored.into_iter().map(|(_, n)| n).collect()
    }

    #[test]
    fn matches_a_subsequence() {
        assert!(penalty("Design System Review.pdf", "dsr").is_some());
        assert!(penalty("dadsrock.mp3", "dsr").is_some());
    }

    #[test]
    fn rejects_a_non_subsequence() {
        assert!(penalty("report.pdf", "xyz").is_none());
        // Order matters — the chars are all present, but not in order.
        assert!(penalty("abc.txt", "cba").is_none());
    }

    #[test]
    fn empty_needle_never_matches() {
        assert!(penalty("anything", "").is_none());
    }

    #[test]
    fn acronym_beats_incidental_letters() {
        // The headline case from the design doc.
        assert_eq!(
            ranked("dsr", &["dadsrock.mp3", "Design System Review.pdf"]),
            ["Design System Review.pdf", "dadsrock.mp3"]
        );
    }

    #[test]
    fn word_boundary_signals() {
        assert!(is_boundary(None, 'a'), "name start");
        for sep in ['_', '-', '.', ' ', '/', '(', '+'] {
            assert!(is_boundary(Some(sep), 'a'), "after {sep:?}");
        }
        assert!(is_boundary(Some('e'), 'S'), "camelCase hump");
        assert!(is_boundary(Some('t'), '2'), "letter → digit");
        assert!(!is_boundary(Some('a'), 'b'), "mid-word");
        assert!(!is_boundary(Some('A'), 'B'), "inside an ALLCAPS run");
    }

    #[test]
    fn word_start_matches_beat_buried_ones() {
        const BURIED: &str = "xdesignsystemreviewx";
        for name in [
            "design_system_review",
            "design-system-review",
            "design.system.review",
            "design system review",
        ] {
            assert_eq!(ranked("dsr", &[BURIED, name]), [name, BURIED]);
        }
    }

    #[test]
    fn camel_case_humps_beat_a_flat_name() {
        assert_eq!(
            ranked("dsr", &["designsystemreview", "designSystemReview"]),
            ["designSystemReview", "designsystemreview"]
        );
    }

    #[test]
    fn consecutive_runs_beat_scattered_gaps() {
        assert_eq!(
            ranked("abc", &["a_x_b_x_c.txt", "abc.txt"]),
            ["abc.txt", "a_x_b_x_c.txt"]
        );
    }

    #[test]
    fn earlier_matches_beat_later_ones() {
        assert_eq!(
            ranked("abc", &["zzzzabc", "abczzzz"]),
            ["abczzzz", "zzzzabc"]
        );
    }

    #[test]
    fn shorter_names_win_an_otherwise_equal_match() {
        assert_eq!(
            ranked("abc", &["abc_long_tail.txt", "abc.txt"]),
            ["abc.txt", "abc_long_tail.txt"]
        );
    }

    #[test]
    fn matching_is_case_insensitive() {
        assert!(penalty("REPORT.PDF", "report").is_some());
        assert!(penalty("Report.pdf", "rp").is_some());
    }

    #[test]
    fn handles_non_ascii_names() {
        // Char-indexed, not byte-indexed: a multi-byte name must not
        // shift the gap/offset math or panic.
        assert!(penalty("Café Ürün Raporu.pdf", "cür").is_some());
        assert!(penalty("日本語のファイル.txt", "日ファ").is_some());
        // Case folding is Unicode-aware...
        assert!(penalty("naïve.txt", "naï").is_some());
        // ...but diacritics are *not* stripped, matching how the index
        // folds names elsewhere: "u" does not find "ü".
        assert!(penalty("Ürün.txt", "urun").is_none());
        // The needle arrives already folded (the index lowercases the
        // query once per search), so an unfolded one is not expected to
        // match — asserted so a future caller change is caught here.
        assert!(penalty("naïve.txt", "NAÏ").is_none());
    }

    #[test]
    fn penalty_saturates_instead_of_overflowing() {
        // A pathological name must not wrap the u16 and score as "best".
        let haystack = format!("a{}b", "x".repeat(70_000));
        let p = penalty(&haystack, "ab").expect("should match");
        assert_eq!(p, u16::MAX);
    }
}

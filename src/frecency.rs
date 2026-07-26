//! Frecency scoring: how "hot" a path is, from how often it was opened
//! and how recently.
//!
//! Pure math, no I/O and no GPUI — the store that uses it is
//! [`crate::recents`], and the search re-rank that consumes it is stage B
//! in `docs/design-search-ranking.md`. Kept separate so the decay model
//! can be unit-tested (and tuned) without touching persistence.
//!
//! The model is a single exponentially-decayed weight per path. Each open
//! decays the existing weight to *now*, then adds one:
//!
//! ```text
//! weight' = weight * 0.5^(age_days / HALF_LIFE_DAYS) + 1
//! ```
//!
//! so a path opened daily converges to a high plateau, and one opened
//! twice a year ago falls away. This is exact under decay-on-write —
//! there is no per-open history to keep, and no bucket cliff edges the
//! way Mozilla-style weighting has.

/// Days for an untouched weight to halve. 30 is the "a month of not
/// caring about this folder means it stops leading my results" setting.
pub const HALF_LIFE_DAYS: f32 = 30.0;

/// Credit a parent directory gets when a file inside it is opened.
/// Makes "the folder I live in surfaces first" work without needing a
/// visit record for every individual file in it.
pub const PARENT_CREDIT: f32 = 0.25;

const SECS_PER_DAY: f32 = 86_400.0;

/// Decay `weight` over `age_secs`. A negative age (clock skew, a file
/// touched "in the future") decays nothing rather than amplifying.
pub fn decay(weight: f32, age_secs: i64) -> f32 {
    if age_secs <= 0 {
        return weight;
    }
    let half_lives = age_secs as f32 / SECS_PER_DAY / HALF_LIFE_DAYS;
    weight * 0.5f32.powf(half_lives)
}

/// The score of a visit whose `weight` was last updated at
/// `last_opened`, evaluated at `now` (both unix seconds).
pub fn score(weight: f32, last_opened: i64, now: i64) -> f32 {
    decay(weight, now - last_opened)
}

/// Current unix time in seconds. Saturates rather than panicking on a
/// pre-epoch clock, since nothing here is worth failing a search over.
pub fn now_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 86_400;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    #[test]
    fn fresh_weight_does_not_decay() {
        assert!(close(decay(1.0, 0), 1.0));
    }

    #[test]
    fn one_half_life_halves() {
        assert!(close(decay(1.0, (HALF_LIFE_DAYS as i64) * DAY), 0.5));
    }

    #[test]
    fn many_half_lives_approach_zero() {
        let decayed = decay(1.0, (HALF_LIFE_DAYS as i64) * DAY * 10);
        assert!(decayed < 0.001, "{decayed} should be ~0 after 10 half-lives");
    }

    #[test]
    fn negative_age_does_not_amplify() {
        // Clock skew must not turn into a ranking boost.
        assert!(close(decay(2.0, -DAY * 365), 2.0));
    }

    #[test]
    fn daily_opens_beat_a_stale_burst() {
        // Ten opens a year ago vs. one open a day for the last week.
        let stale = decay(10.0, 365 * DAY);

        let mut daily = 0.0;
        let mut last = 0;
        for day in 0..7 {
            let now = day * DAY;
            daily = decay(daily, now - last) + 1.0;
            last = now;
        }
        let daily = score(daily, last, 7 * DAY);

        assert!(daily > stale, "daily {daily} should outrank stale burst {stale}");
    }

    #[test]
    fn repeated_opens_accumulate() {
        let once = decay(0.0, 0) + 1.0;
        let twice = decay(once, DAY) + 1.0;
        assert!(twice > once);
        // But not linearly — the first open has already decayed a little.
        assert!(twice < 2.0);
    }
}

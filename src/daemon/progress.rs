//! Live progress of a full walk, for the first-run loading screen: how many
//! entries the walk has reached, whether it has moved on to writing the
//! index, and a few names it passed. Written by the segment builder thread,
//! read by status requests; nothing waits on it.

use super::ipc::Progress;
use std::{
    collections::VecDeque,
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

/// Names kept for the loading screen to cycle through.
const RECENT: usize = 16;
/// Keep one name in this many: plenty to refresh a screen polled ~10 times
/// a second, at almost no cost to the walk.
const SAMPLE_EVERY: u64 = 61;
/// Longer names would be truncated on screen anyway.
const MAX_NAME_CHARS: usize = 40;

#[derive(Default)]
pub struct WalkProgress {
    active: AtomicBool,
    indexing: AtomicBool,
    scanned: AtomicU64,
    recent: Mutex<VecDeque<String>>,
}

impl WalkProgress {
    /// A walk began: counts restart from zero.
    pub fn start(&self) {
        self.scanned.store(0, Ordering::Relaxed);
        self.indexing.store(false, Ordering::Relaxed);
        if let Ok(mut recent) = self.recent.lock() {
            recent.clear();
        }
        self.active.store(true, Ordering::Release);
    }

    /// The walk reached one more entry, named `name` (raw, native bytes).
    pub fn note(&self, name: &[u8]) {
        let n = self.scanned.fetch_add(1, Ordering::Relaxed);
        if !n.is_multiple_of(SAMPLE_EVERY) {
            return;
        }
        let Some(name) = displayable(name) else {
            return;
        };
        if let Ok(mut recent) = self.recent.lock() {
            if recent.len() == RECENT {
                recent.pop_front();
            }
            recent.push_back(name);
        }
    }

    /// The walk is done; the index is being written.
    pub fn indexing(&self) {
        self.indexing.store(true, Ordering::Relaxed);
    }

    pub fn finish(&self) {
        self.active.store(false, Ordering::Release);
    }

    /// What a status request reports: `None` outside a walk.
    pub fn snapshot(&self) -> Option<Progress> {
        if !self.active.load(Ordering::Acquire) {
            return None;
        }
        Some(Progress {
            scanned: self.scanned.load(Ordering::Relaxed),
            indexing: self.indexing.load(Ordering::Relaxed),
            recent: self
                .recent
                .lock()
                .map(|r| r.iter().cloned().collect())
                .unwrap_or_default(),
        })
    }
}

/// Ends a walk's progress however the build ends (success, error, cancel).
pub struct Finish<'a>(pub &'a WalkProgress);
impl Drop for Finish<'_> {
    fn drop(&mut self) {
        self.0.finish();
    }
}

/// A name worth showing: valid UTF-8, not hidden or a system entry
/// (`.git`, `$Recycle.Bin`), short enough to read at a glance.
fn displayable(raw: &[u8]) -> Option<String> {
    let name = std::str::from_utf8(raw).ok()?;
    let hidden = name.starts_with(['.', '$', '~']);
    (!name.is_empty() && !hidden && name.chars().count() <= MAX_NAME_CHARS).then(|| name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_only_during_a_walk() {
        let progress = WalkProgress::default();
        assert_eq!(progress.snapshot(), None);
        progress.start();
        for i in 0..(SAMPLE_EVERY * 3) {
            progress.note(format!("file-{i}.txt").as_bytes());
        }
        let live = progress.snapshot().unwrap();
        assert_eq!(live.scanned, SAMPLE_EVERY * 3);
        assert!(!live.indexing);
        assert_eq!(
            live.recent,
            ["file-0.txt", "file-61.txt", "file-122.txt"].map(String::from)
        );
        progress.indexing();
        assert!(progress.snapshot().unwrap().indexing);
        {
            let _finish = Finish(&progress);
        }
        assert_eq!(progress.snapshot(), None, "a finished walk reports nothing");
        progress.start();
        assert_eq!(
            progress.snapshot(),
            Some(Progress::default()),
            "a new walk starts from zero"
        );
    }

    #[test]
    fn keeps_a_bounded_window_of_readable_names() {
        let progress = WalkProgress::default();
        progress.start();
        for i in 0..(SAMPLE_EVERY * (RECENT as u64 + 5)) {
            progress.note(format!("n{i}").as_bytes());
        }
        let recent = progress.snapshot().unwrap().recent;
        assert_eq!(recent.len(), RECENT);
        assert_eq!(
            recent.last().map(String::as_str),
            Some(format!("n{}", SAMPLE_EVERY * (RECENT as u64 + 4)).as_str())
        );
        for hidden in [".git", "$Recycle.Bin", "~lock.docx", ""] {
            assert_eq!(displayable(hidden.as_bytes()), None, "{hidden:?}");
        }
        assert_eq!(displayable(&[0xff, 0xfe]), None, "not UTF-8");
        assert_eq!(displayable("x".repeat(41).as_bytes()), None);
        assert_eq!(
            displayable("Résumé.pdf".as_bytes()).as_deref(),
            Some("Résumé.pdf")
        );
    }
}

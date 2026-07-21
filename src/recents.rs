//! Recently-opened folders and files.
//!
//! A small, capped, most-recent-first list persisted as JSON in the
//! local data dir. Local-only by design — filenames are private (the
//! telemetry stance in docs/roadmap.md), so this never leaves the
//! machine. Pure I/O + list logic, no GPUI, so it's unit-tested in
//! isolation; the app records into it and persists off-thread.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

/// How many entries are kept; older ones fall off the end.
pub const CAP: usize = 20;

/// Default location: `<data_local_dir>/filex/recents.json`.
pub fn default_recents_file() -> Option<PathBuf> {
    Some(dirs::data_local_dir()?.join("filex").join("recents.json"))
}

/// The most-recent-first list of opened paths.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Recents {
    entries: Vec<PathBuf>,
}

impl Recents {
    /// Load from `file`. A missing or unreadable/corrupt file is treated
    /// as empty — recents are a convenience, never worth failing over.
    pub fn load(file: &Path) -> Self {
        std::fs::read_to_string(file)
            .ok()
            .and_then(|contents| serde_json::from_str(&contents).ok())
            .unwrap_or_default()
    }

    /// The entries, most-recent first.
    pub fn entries(&self) -> &[PathBuf] {
        &self.entries
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Record `path` as the most recent, de-duplicating (an existing
    /// occurrence moves to the front) and capping the list length.
    pub fn record(&mut self, path: PathBuf) {
        self.entries.retain(|p| p != &path);
        self.entries.insert(0, path);
        self.entries.truncate(CAP);
    }

    /// Forget everything.
    pub fn clear(&mut self) {
        self.entries.clear();
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

    fn paths(recents: &Recents) -> Vec<&str> {
        recents.entries().iter().map(|p| p.to_str().unwrap()).collect()
    }

    #[test]
    fn record_puts_newest_first() {
        let mut r = Recents::default();
        r.record("/a".into());
        r.record("/b".into());
        assert_eq!(paths(&r), ["/b", "/a"]);
    }

    #[test]
    fn record_dedups_by_moving_to_front() {
        let mut r = Recents::default();
        r.record("/a".into());
        r.record("/b".into());
        r.record("/a".into());
        assert_eq!(paths(&r), ["/a", "/b"]);
    }

    #[test]
    fn record_caps_length() {
        let mut r = Recents::default();
        for i in 0..(CAP + 5) {
            r.record(PathBuf::from(format!("/p{i}")));
        }
        assert_eq!(r.entries().len(), CAP);
        // The most recent survive; the oldest fell off.
        assert_eq!(r.entries()[0], PathBuf::from(format!("/p{}", CAP + 4)));
    }

    #[test]
    fn clear_empties() {
        let mut r = Recents::default();
        r.record("/a".into());
        r.clear();
        assert!(r.is_empty());
    }

    #[test]
    fn save_then_load_round_trips_and_missing_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("nested").join("recents.json");
        assert!(Recents::load(&file).is_empty()); // missing → empty
        let mut r = Recents::default();
        r.record("/x".into());
        r.record("/y".into());
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
}

//! Multi-root coordination: the persisted root list, validation for
//! adding roots, and merged search across several volume indexes.
//!
//! Each root is its own [`super::LiveIndex`] (own watcher, writer, and
//! snapshot); this module only holds the logic that spans them, kept
//! GUI-free so it's testable — the async orchestration lives in the app.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};

use super::watcher::SharedIndex;
use super::{MatchKind, SearchHit};
use crate::search_filter::Filter;

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
    pub kind: MatchKind,
    pub name_len: u16,
}

/// Search every index and merge by the same ranking a single index uses
/// (match kind, then name length). Locks are taken one index at a time,
/// read-only; a poisoned index still answers.
pub fn search_all(
    indexes: &[SharedIndex],
    query: &str,
    filters: &[Filter],
    limit: usize,
) -> Vec<MergedHit> {
    let mut merged: Vec<MergedHit> = Vec::new();
    for shared in indexes {
        let index = shared
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        merged.extend(index.search_filtered(query, filters, limit).into_iter().filter_map(
            |SearchHit { id, kind, name_len }| {
                Some(MergedHit {
                    name: index.name_of(id)?.to_string(),
                    path: index.path_of(id)?,
                    is_dir: index.is_dir(id)?,
                    kind,
                    name_len,
                })
            },
        ));
    }
    merged.sort_by(|a, b| {
        (a.kind, a.name_len)
            .cmp(&(b.kind, b.name_len))
            .then_with(|| a.path.cmp(&b.path))
    });
    merged.truncate(limit);
    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{ROOT, VolumeIndex};
    use std::sync::{Arc, RwLock};

    fn shared(index: VolumeIndex) -> SharedIndex {
        Arc::new(RwLock::new(index))
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

        let hits = search_all(&[shared(a), shared(b)], "main", &[], 10);
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
        assert_eq!(search_all(&indexes, "file", &[], 3).len(), 3);
        assert!(search_all(&indexes, "", &[], 10).is_empty());
        assert!(search_all(&[], "file", &[], 10).is_empty());
    }
}

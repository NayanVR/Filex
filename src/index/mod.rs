//! OS-agnostic volume index: the in-memory structure that answers
//! filename searches instantly (see docs/indexing-architecture.md §3).
//!
//! Layout follows the "Everything" model: entries hold a parent link and a
//! reference into one contiguous name pool; full paths are never stored and
//! are materialized on demand by chasing parent links. A second, case-folded
//! copy of every name backs case-insensitive substring search.

pub mod walker;

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

use anyhow::{Result, bail};
use memchr::memmem;
use rayon::prelude::*;

/// Stable handle to an entry; index into `VolumeIndex::entries`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EntryId(u32);

pub const ROOT: EntryId = EntryId(0);

/// Maximum parent-chain length tolerated when materializing a path,
/// guarding against cycles introduced by a buggy delta stream.
const MAX_PATH_DEPTH: usize = 4096;

const FLAG_DIR: u8 = 1 << 0;
const FLAG_TOMBSTONE: u8 = 1 << 1;

/// (offset, len) into a name pool. Filenames are ≤255 bytes on every
/// filesystem we target, so u16 lengths are safe (enforced on insert).
#[derive(Debug, Clone, Copy)]
struct NameRef {
    offset: u32,
    len: u16,
}

#[derive(Debug, Clone, Copy)]
struct FileEntry {
    name: NameRef,
    name_lower: NameRef,
    parent: EntryId,
    flags: u8,
}

impl FileEntry {
    fn is_dir(&self) -> bool {
        self.flags & FLAG_DIR != 0
    }

    fn is_tombstone(&self) -> bool {
        self.flags & FLAG_TOMBSTONE != 0
    }
}

/// How a hit matched the query, in ranking order (lower is better).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MatchKind {
    Exact,
    Prefix,
    WordBoundary,
    Substring,
}

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub id: EntryId,
    pub kind: MatchKind,
}

#[derive(Debug)]
pub struct VolumeIndex {
    root_path: PathBuf,
    entries: Vec<FileEntry>,
    name_pool: Vec<u8>,
    name_pool_lower: Vec<u8>,
    /// Child lists, used for browse-style listing and path resolution.
    children: HashMap<EntryId, Vec<EntryId>>,
    /// Bumped on every mutation; lets async consumers detect staleness.
    generation: u64,
}

impl VolumeIndex {
    /// Create an index whose root entry represents `root_path`.
    pub fn new(root_path: impl Into<PathBuf>) -> Self {
        let mut index = Self {
            root_path: root_path.into(),
            entries: Vec::new(),
            name_pool: Vec::new(),
            name_pool_lower: Vec::new(),
            children: HashMap::new(),
            generation: 0,
        };
        // Root points at itself; its name is empty (path comes from root_path).
        let name = index.intern("");
        index.entries.push(FileEntry {
            name,
            name_lower: name,
            parent: ROOT,
            flags: FLAG_DIR,
        });
        index
    }

    pub fn root_path(&self) -> &Path {
        &self.root_path
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Number of live (non-tombstoned) entries, excluding the root.
    pub fn len(&self) -> usize {
        self.entries
            .iter()
            .skip(1)
            .filter(|e| !e.is_tombstone())
            .count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn intern(&mut self, name: &str) -> NameRef {
        let offset = self.name_pool.len() as u32;
        self.name_pool.extend_from_slice(name.as_bytes());
        NameRef {
            offset,
            len: name.len() as u16,
        }
    }

    fn intern_lower(&mut self, name: &str) -> NameRef {
        let lower = name.to_lowercase();
        let offset = self.name_pool_lower.len() as u32;
        self.name_pool_lower.extend_from_slice(lower.as_bytes());
        NameRef {
            offset,
            len: lower.len() as u16,
        }
    }

    fn name_bytes(&self, r: NameRef) -> &[u8] {
        &self.name_pool[r.offset as usize..r.offset as usize + r.len as usize]
    }

    fn name_lower_bytes(&self, r: NameRef) -> &[u8] {
        &self.name_pool_lower[r.offset as usize..r.offset as usize + r.len as usize]
    }

    fn entry(&self, id: EntryId) -> Option<&FileEntry> {
        self.entries.get(id.0 as usize)
    }

    /// Append a new entry under `parent`. Does not check for duplicate names —
    /// bootstrap feeds are already unique per directory; delta sources must
    /// resolve first (see `resolve_child`).
    pub fn insert(&mut self, parent: EntryId, name: &str, is_dir: bool) -> Result<EntryId> {
        if name.len() > u16::MAX as usize {
            bail!("file name longer than {} bytes: {name:?}", u16::MAX);
        }
        let parent_entry = self
            .entry(parent)
            .ok_or_else(|| anyhow::anyhow!("insert under unknown parent {parent:?}"))?;
        if !parent_entry.is_dir() {
            bail!("insert under non-directory parent {parent:?}");
        }

        let name_ref = self.intern(name);
        let lower_ref = self.intern_lower(name);
        let id = EntryId(self.entries.len() as u32);
        self.entries.push(FileEntry {
            name: name_ref,
            name_lower: lower_ref,
            parent,
            flags: if is_dir { FLAG_DIR } else { 0 },
        });
        self.children.entry(parent).or_default().push(id);
        self.generation += 1;
        Ok(id)
    }

    /// Tombstone an entry and all its descendants. Pool bytes and arena slots
    /// are leaked until compaction (future work); tombstones are skipped by
    /// search and listing.
    pub fn remove(&mut self, id: EntryId) -> Result<()> {
        if id == ROOT {
            bail!("cannot remove the root entry");
        }
        let mut stack = vec![id];
        while let Some(current) = stack.pop() {
            let Some(entry) = self.entries.get_mut(current.0 as usize) else {
                bail!("remove of unknown entry {current:?}");
            };
            entry.flags |= FLAG_TOMBSTONE;
            if let Some(children) = self.children.remove(&current) {
                stack.extend(children);
            }
        }
        self.generation += 1;
        Ok(())
    }

    /// Rename and/or move an entry. Old name bytes are leaked until compaction.
    pub fn rename(&mut self, id: EntryId, new_parent: EntryId, new_name: &str) -> Result<()> {
        if id == ROOT {
            bail!("cannot rename the root entry");
        }
        if self.entry(id).is_none_or(FileEntry::is_tombstone) {
            bail!("rename of unknown or removed entry {id:?}");
        }
        if self.entry(new_parent).is_none_or(|p| !p.is_dir()) {
            bail!("rename target parent {new_parent:?} is not a directory");
        }

        let name_ref = self.intern(new_name);
        let lower_ref = self.intern_lower(new_name);
        let old_parent = self.entries[id.0 as usize].parent;
        let entry = &mut self.entries[id.0 as usize];
        entry.name = name_ref;
        entry.name_lower = lower_ref;
        entry.parent = new_parent;

        if old_parent != new_parent {
            if let Some(siblings) = self.children.get_mut(&old_parent) {
                siblings.retain(|&c| c != id);
            }
            self.children.entry(new_parent).or_default().push(id);
        }
        self.generation += 1;
        Ok(())
    }

    pub fn name_of(&self, id: EntryId) -> Option<&str> {
        let entry = self.entry(id)?;
        std::str::from_utf8(self.name_bytes(entry.name)).ok()
    }

    pub fn is_dir(&self, id: EntryId) -> Option<bool> {
        self.entry(id).map(FileEntry::is_dir)
    }

    /// Materialize the full path of an entry by chasing parent links.
    pub fn path_of(&self, id: EntryId) -> Option<PathBuf> {
        let mut components: Vec<&str> = Vec::new();
        let mut current = id;
        for _ in 0..MAX_PATH_DEPTH {
            if current == ROOT {
                let mut path = self.root_path.clone();
                path.extend(components.iter().rev());
                return Some(path);
            }
            let entry = self.entry(current)?;
            components.push(std::str::from_utf8(self.name_bytes(entry.name)).ok()?);
            current = entry.parent;
        }
        None // cycle or absurd depth
    }

    /// Find the live child of `parent` with the given name (exact match).
    pub fn resolve_child(&self, parent: EntryId, name: &str) -> Option<EntryId> {
        self.children.get(&parent)?.iter().copied().find(|&c| {
            self.entry(c).is_some_and(|e| {
                !e.is_tombstone() && self.name_bytes(e.name) == name.as_bytes()
            })
        })
    }

    /// Resolve a path relative to the index root.
    pub fn resolve(&self, relative: &Path) -> Option<EntryId> {
        let mut current = ROOT;
        for component in relative.components() {
            match component {
                Component::Normal(name) => {
                    current = self.resolve_child(current, name.to_str()?)?;
                }
                Component::CurDir => {}
                _ => return None,
            }
        }
        Some(current)
    }

    /// Case-insensitive substring search over all live entries, ranked
    /// exact > prefix > word-boundary > substring, then by name length.
    /// Runs as a rayon parallel scan over the entry arena; at
    /// millions-of-entries scale this is milliseconds (see benches/).
    pub fn search(&self, query: &str, limit: usize) -> Vec<SearchHit> {
        if query.is_empty() || limit == 0 {
            return Vec::new();
        }
        let needle = query.to_lowercase();
        let finder = memmem::Finder::new(needle.as_bytes());

        let mut hits: Vec<(MatchKind, u16, EntryId)> = self
            .entries
            .par_iter()
            .enumerate()
            .skip(1) // root
            .filter(|(_, e)| !e.is_tombstone())
            .filter_map(|(ix, entry)| {
                let haystack = self.name_lower_bytes(entry.name_lower);
                let pos = finder.find(haystack)?;
                let kind = if pos == 0 {
                    if haystack.len() == needle.len() {
                        MatchKind::Exact
                    } else {
                        MatchKind::Prefix
                    }
                } else if !haystack[pos - 1].is_ascii_alphanumeric() {
                    MatchKind::WordBoundary
                } else {
                    MatchKind::Substring
                };
                Some((kind, entry.name_lower.len, EntryId(ix as u32)))
            })
            .collect();

        hits.par_sort_unstable_by_key(|&(kind, len, id)| (kind, len, id.0));
        hits.truncate(limit);
        hits.into_iter()
            .map(|(kind, _, id)| SearchHit { id, kind })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build:  root/{docs/{Report.pdf, notes.txt}, src/main.rs}
    fn sample_index() -> (VolumeIndex, EntryId, EntryId, EntryId, EntryId, EntryId) {
        let mut index = VolumeIndex::new("/vol");
        let docs = index.insert(ROOT, "docs", true).unwrap();
        let report = index.insert(docs, "Report.pdf", false).unwrap();
        let notes = index.insert(docs, "notes.txt", false).unwrap();
        let src = index.insert(ROOT, "src", true).unwrap();
        let main_rs = index.insert(src, "main.rs", false).unwrap();
        (index, docs, report, notes, src, main_rs)
    }

    #[test]
    fn materializes_full_paths_from_parent_links() {
        let (index, docs, report, ..) = sample_index();
        assert_eq!(index.path_of(ROOT).unwrap(), PathBuf::from("/vol"));
        assert_eq!(index.path_of(docs).unwrap(), PathBuf::from("/vol/docs"));
        assert_eq!(
            index.path_of(report).unwrap(),
            PathBuf::from("/vol/docs/Report.pdf")
        );
    }

    #[test]
    fn resolves_relative_paths() {
        let (index, docs, report, ..) = sample_index();
        assert_eq!(index.resolve(Path::new("docs")), Some(docs));
        assert_eq!(index.resolve(Path::new("docs/Report.pdf")), Some(report));
        assert_eq!(index.resolve(Path::new("docs/missing")), None);
    }

    #[test]
    fn search_is_case_insensitive() {
        let (index, _, report, ..) = sample_index();
        let hits = index.search("REPORT", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, report);
    }

    #[test]
    fn search_ranks_exact_then_prefix_then_boundary_then_substring() {
        let mut index = VolumeIndex::new("/vol");
        let substring = index.insert(ROOT, "xmain.rs", false).unwrap();
        let boundary = index.insert(ROOT, "test_main.rs", false).unwrap();
        let exact = index.insert(ROOT, "main", false).unwrap();
        let prefix = index.insert(ROOT, "main.rs", false).unwrap();

        let ids: Vec<EntryId> = index.search("main", 10).into_iter().map(|h| h.id).collect();
        assert_eq!(ids, vec![exact, prefix, boundary, substring]);
    }

    #[test]
    fn search_respects_limit_and_skips_root() {
        let mut index = VolumeIndex::new("/match-in-root-name");
        for i in 0..20 {
            index.insert(ROOT, &format!("file{i}.txt"), false).unwrap();
        }
        assert_eq!(index.search("file", 5).len(), 5);
        // Root's own (empty) name never matches.
        assert!(index.search("match-in-root-name", 10).is_empty());
    }

    #[test]
    fn removed_entries_disappear_from_search_and_resolution() {
        let (mut index, docs, report, notes, ..) = sample_index();
        index.remove(docs).unwrap();

        assert!(index.search("report", 10).is_empty());
        assert!(index.search("notes", 10).is_empty());
        assert_eq!(index.resolve(Path::new("docs")), None);
        assert_eq!(index.len(), 2); // src + main.rs survive
        let _ = (report, notes);
    }

    #[test]
    fn rename_moves_entry_and_updates_search() {
        let (mut index, _docs, report, _notes, src, _main) = sample_index();
        index.rename(report, src, "summary.pdf").unwrap();

        assert!(index.search("report", 10).is_empty());
        let hits = index.search("summary", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(
            index.path_of(hits[0].id).unwrap(),
            PathBuf::from("/vol/src/summary.pdf")
        );
        assert_eq!(index.resolve(Path::new("docs/Report.pdf")), None);
        assert_eq!(index.resolve(Path::new("src/summary.pdf")), Some(report));
    }

    #[test]
    fn insert_rejects_bad_parents() {
        let (mut index, _docs, report, ..) = sample_index();
        assert!(index.insert(report, "child", false).is_err()); // file parent
        assert!(index.insert(EntryId(9999), "child", false).is_err());
    }

    #[test]
    fn mutations_bump_generation() {
        let (mut index, _docs, report, ..) = sample_index();
        let g0 = index.generation();
        index.rename(report, ROOT, "r.pdf").unwrap();
        assert!(index.generation() > g0);
        let g1 = index.generation();
        index.remove(report).unwrap();
        assert!(index.generation() > g1);
    }

    #[test]
    fn unicode_names_search_case_insensitively() {
        let mut index = VolumeIndex::new("/vol");
        let id = index.insert(ROOT, "Übersicht.md", false).unwrap();
        let hits = index.search("übersicht", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, id);
    }
}

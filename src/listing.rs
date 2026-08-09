use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context as _, Result};

use crate::settings::{SortBy, SortSettings};

/// A single directory entry, sorted for display. GUI-free so it can be unit
/// tested and later swapped to be fed from the volume index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub size: u64,
    /// Last modification time; `None` when the filesystem couldn't
    /// report one (sorts before every real timestamp).
    pub modified: Option<SystemTime>,
    /// Hidden by the platform's convention; the browse view filters on
    /// this unless the show-hidden-files setting is on.
    pub is_hidden: bool,
}

/// Whether an entry counts as hidden. The dotfile convention applies on
/// every platform (macOS Finder hides dotfiles too, and dot-named files
/// on Windows are almost always ported Unix tooling); Windows adds the
/// FILE_ATTRIBUTE_HIDDEN bit and macOS the Finder UF_HIDDEN flag, both
/// read from metadata we already fetched for the size.
fn is_hidden_entry(name: &str, metadata: Option<&std::fs::Metadata>) -> bool {
    if name.starts_with('.') {
        return true;
    }
    #[cfg(target_os = "windows")]
    if let Some(meta) = metadata {
        use std::os::windows::fs::MetadataExt as _;
        const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
        if meta.file_attributes() & FILE_ATTRIBUTE_HIDDEN != 0 {
            return true;
        }
    }
    #[cfg(target_os = "macos")]
    if let Some(meta) = metadata {
        use std::os::macos::fs::MetadataExt as _;
        const UF_HIDDEN: u32 = 0x8000;
        if meta.st_flags() & UF_HIDDEN != 0 {
            return true;
        }
    }
    #[cfg(target_os = "linux")]
    let _ = metadata;
    false
}

/// Read a directory and return entries ordered by `sort`. Entries whose
/// metadata cannot be read are skipped rather than failing the whole
/// listing. One stat per entry is deliberate: browse directories are
/// small, and the search index never stats (see docs/roadmap.md).
pub fn read_dir_sorted(path: &Path, sort: &SortSettings) -> Result<Vec<Entry>> {
    let read_dir =
        std::fs::read_dir(path).with_context(|| format!("reading directory {}", path.display()))?;

    let mut entries: Vec<Entry> = read_dir
        .filter_map(|dirent| {
            let dirent = dirent.ok()?;
            let file_type = dirent.file_type().ok()?;
            let metadata = dirent.metadata().ok();
            let name = dirent.file_name().to_string_lossy().into_owned();
            let is_hidden = is_hidden_entry(&name, metadata.as_ref());
            Some(Entry {
                name,
                path: dirent.path(),
                is_dir: file_type.is_dir(),
                size: metadata.as_ref().map_or(0, |m| m.len()),
                modified: metadata.and_then(|m| m.modified().ok()),
                is_hidden,
            })
        })
        .collect();

    sort_entries(&mut entries, sort);
    Ok(entries)
}

/// Order `entries` per the sort settings. The `ascending` flag reverses
/// only the primary key: directories stay grouped first when
/// `directories_first` is set, and equal keys always tie-break by name
/// ascending so reversed orders don't scramble within groups.
pub fn sort_entries(entries: &mut [Entry], sort: &SortSettings) {
    entries.sort_by(|a, b| compare_entries(a, b, sort));
}

fn compare_entries(a: &Entry, b: &Entry, sort: &SortSettings) -> Ordering {
    if sort.directories_first {
        let group = b.is_dir.cmp(&a.is_dir);
        if group != Ordering::Equal {
            return group;
        }
    }
    let primary = match sort.by {
        SortBy::Name => name_order(a, b),
        SortBy::Size => a.size.cmp(&b.size),
        // `None < Some(_)`: unknown mtimes sort before every real one.
        SortBy::Modified => a.modified.cmp(&b.modified),
        SortBy::Kind => kind_rank(a).cmp(&kind_rank(b)),
    };
    let primary = if sort.ascending {
        primary
    } else {
        primary.reverse()
    };
    primary.then_with(|| name_order(a, b))
}

fn name_order(a: &Entry, b: &Entry) -> Ordering {
    a.name.to_lowercase().cmp(&b.name.to_lowercase())
}

/// Kind sorting groups by [`FileKind`] (in declaration order), then by
/// extension so e.g. all `.png`s sit together within Image.
fn kind_rank(entry: &Entry) -> (u8, String) {
    let kind = FileKind::of(&entry.name, entry.is_dir);
    let rank = match kind {
        FileKind::Directory => 0,
        FileKind::Image => 1,
        FileKind::Video => 2,
        FileKind::Audio => 3,
        FileKind::Archive => 4,
        FileKind::Code => 5,
        FileKind::Document => 6,
        FileKind::Other => 7,
    };
    let ext = entry
        .name
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default();
    (rank, ext)
}

/// Broad category of a file, derived from its extension — drives the
/// list icon and decides which files get thumbnails.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum FileKind {
    Directory,
    Image,
    Video,
    Audio,
    Archive,
    Code,
    Document,
    Other,
}

impl FileKind {
    pub fn of(name: &str, is_dir: bool) -> Self {
        if is_dir {
            return Self::Directory;
        }
        let Some(ext) = name
            .rsplit_once('.')
            .map(|(_, ext)| ext.to_ascii_lowercase())
        else {
            return Self::Other;
        };
        match ext.as_str() {
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "tiff" | "tif" | "ico" => Self::Image,
            "mp4" | "mkv" | "mov" | "avi" | "webm" | "m4v" => Self::Video,
            "mp3" | "wav" | "flac" | "ogg" | "m4a" | "aac" | "opus" => Self::Audio,
            "zip" | "tar" | "gz" | "bz2" | "xz" | "zst" | "7z" | "rar" | "dmg" | "iso" => {
                Self::Archive
            }
            "rs" | "py" | "js" | "ts" | "tsx" | "jsx" | "c" | "h" | "cpp" | "hpp" | "go"
            | "java" | "rb" | "sh" | "swift" | "kt" | "toml" | "yaml" | "yml" | "json" | "html"
            | "css" | "sql" => Self::Code,
            "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "txt" | "md" | "rtf" => {
                Self::Document
            }
            _ => Self::Other,
        }
    }

    /// Human-readable kind name for the details panel.
    pub fn label(self) -> &'static str {
        match self {
            Self::Directory => "Folder",
            Self::Image => "Image",
            Self::Video => "Video",
            Self::Audio => "Audio",
            Self::Archive => "Archive",
            Self::Code => "Code",
            Self::Document => "Document",
            Self::Other => "File",
        }
    }

    /// Emoji glyph shown in list rows (until a real icon set lands).
    pub fn glyph(self) -> &'static str {
        match self {
            Self::Directory => "📁",
            Self::Image => "🖼",
            Self::Video => "🎬",
            Self::Audio => "🎵",
            Self::Archive => "📦",
            Self::Code => "⌨",
            Self::Document => "📄",
            Self::Other => "·",
        }
    }
}

/// Split a path into breadcrumb segments: each is the label to show
/// and the full path clicking it navigates to. The filesystem root
/// comes out as "/" on Unix; on Windows the drive prefix and root fold
/// into a single navigable "C:\" segment (the bare prefix "C:" is a
/// drive-relative path, not a location).
pub fn path_segments(path: &Path) -> Vec<(String, PathBuf)> {
    use std::path::Component;
    let mut segments = Vec::new();
    let mut acc = PathBuf::new();
    for comp in path.components() {
        acc.push(comp.as_os_str());
        match comp {
            Component::Prefix(_) => {}
            Component::RootDir => {
                segments.push((acc.to_string_lossy().into_owned(), acc.clone()));
            }
            _ => segments.push((comp.as_os_str().to_string_lossy().into_owned(), acc.clone())),
        }
    }
    segments
}

/// Compact relative age for the Modified column ("just now", "5m",
/// "3h", "2d", "4w", "7mo", "2y"); "—" when the mtime is unknown.
/// Relative beats absolute here: no date-formatting dependency, and a
/// future mtime (clock skew, restored backups) degrades to "just now"
/// instead of a nonsense date.
pub fn format_modified(modified: Option<SystemTime>, now: SystemTime) -> String {
    let Some(modified) = modified else {
        return "—".to_string();
    };
    let Ok(age) = now.duration_since(modified) else {
        return "just now".to_string(); // modified in the future
    };
    let secs = age.as_secs();
    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;
    const WEEK: u64 = 7 * DAY;
    const MONTH: u64 = 30 * DAY;
    const YEAR: u64 = 365 * DAY;
    match secs {
        0..MINUTE => "just now".to_string(),
        MINUTE..HOUR => format!("{}m", secs / MINUTE),
        HOUR..DAY => format!("{}h", secs / HOUR),
        DAY..WEEK => format!("{}d", secs / DAY),
        WEEK..MONTH => format!("{}w", secs / WEEK),
        MONTH..YEAR => format!("{}mo", secs / MONTH),
        _ => format!("{}y", secs / YEAR),
    }
}

pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn entry(name: &str, is_dir: bool) -> Entry {
        Entry {
            name: name.to_string(),
            path: PathBuf::from(name),
            is_dir,
            size: 0,
            modified: None,
            is_hidden: false,
        }
    }

    fn sized(name: &str, size: u64) -> Entry {
        Entry {
            size,
            ..entry(name, false)
        }
    }

    fn names(entries: &[Entry]) -> Vec<&str> {
        entries.iter().map(|e| e.name.as_str()).collect()
    }

    #[test]
    fn sorts_directories_before_files() {
        let mut entries = vec![
            entry("aaa.txt", false),
            entry("zzz", true),
            entry("bbb.txt", false),
        ];
        sort_entries(&mut entries, &SortSettings::default());
        assert_eq!(names(&entries), ["zzz", "aaa.txt", "bbb.txt"]);
    }

    #[test]
    fn sorts_names_case_insensitively() {
        let mut entries = vec![
            entry("Beta", false),
            entry("alpha", false),
            entry("GAMMA", false),
        ];
        sort_entries(&mut entries, &SortSettings::default());
        assert_eq!(names(&entries), ["alpha", "Beta", "GAMMA"]);
    }

    #[test]
    fn descending_keeps_directories_first_and_name_ties_ascending() {
        let sort = SortSettings {
            by: SortBy::Size,
            ascending: false,
            directories_first: true,
        };
        let mut entries = vec![
            sized("small.txt", 1),
            sized("big.txt", 100),
            entry("dir", true),
            sized("tie-b.txt", 5),
            sized("tie-a.txt", 5),
        ];
        sort_entries(&mut entries, &sort);
        // Directory stays on top despite descending; equal sizes keep
        // ascending name order.
        assert_eq!(
            names(&entries),
            ["dir", "big.txt", "tie-a.txt", "tie-b.txt", "small.txt"]
        );
    }

    #[test]
    fn directories_mix_in_when_grouping_is_off() {
        let sort = SortSettings {
            by: SortBy::Name,
            ascending: true,
            directories_first: false,
        };
        let mut entries = vec![entry("zeta", true), entry("alpha.txt", false)];
        sort_entries(&mut entries, &sort);
        assert_eq!(names(&entries), ["alpha.txt", "zeta"]);
    }

    #[test]
    fn sorts_by_modified_with_unknown_first() {
        let sort = SortSettings {
            by: SortBy::Modified,
            ascending: true,
            directories_first: true,
        };
        let epoch = SystemTime::UNIX_EPOCH;
        let mut old = entry("old.txt", false);
        old.modified = Some(epoch);
        let mut new = entry("new.txt", false);
        new.modified = Some(epoch + std::time::Duration::from_secs(1000));
        let unknown = entry("unknown.txt", false);
        let mut entries = vec![new.clone(), old.clone(), unknown.clone()];
        sort_entries(&mut entries, &sort);
        assert_eq!(names(&entries), ["unknown.txt", "old.txt", "new.txt"]);

        let sort = SortSettings {
            ascending: false,
            ..sort
        };
        let mut entries = vec![old, unknown, new];
        sort_entries(&mut entries, &sort);
        assert_eq!(names(&entries), ["new.txt", "old.txt", "unknown.txt"]);
    }

    #[test]
    fn sorts_by_kind_grouping_extensions_together() {
        let sort = SortSettings {
            by: SortBy::Kind,
            ascending: true,
            directories_first: true,
        };
        let mut entries = vec![
            entry("b.txt", false),
            entry("a.png", false),
            entry("c.rs", false),
            entry("z.jpg", false),
            entry("folder", true),
        ];
        sort_entries(&mut entries, &sort);
        // Directory first, then Image (.jpg before .png by extension),
        // then Code, then Document.
        assert_eq!(
            names(&entries),
            ["folder", "z.jpg", "a.png", "c.rs", "b.txt"]
        );
    }

    #[test]
    fn reads_real_directory_with_metadata() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("subdir")).unwrap();
        fs::write(dir.path().join("file.txt"), b"hello").unwrap();

        let entries = read_dir_sorted(dir.path(), &SortSettings::default()).unwrap();

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "subdir");
        assert!(entries[0].is_dir);
        assert_eq!(entries[1].name, "file.txt");
        assert!(!entries[1].is_dir);
        assert_eq!(entries[1].size, 5);
        assert_eq!(entries[1].path, dir.path().join("file.txt"));
    }

    #[test]
    fn dotfiles_are_flagged_hidden_but_still_listed() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".dotfile"), b"").unwrap();
        fs::write(dir.path().join("visible.txt"), b"").unwrap();

        let entries = read_dir_sorted(dir.path(), &SortSettings::default()).unwrap();
        let flags: Vec<(&str, bool)> = entries
            .iter()
            .map(|e| (e.name.as_str(), e.is_hidden))
            .collect();
        assert_eq!(flags, [(".dotfile", true), ("visible.txt", false)]);
    }

    #[test]
    fn hidden_classification_is_name_based_without_metadata() {
        assert!(is_hidden_entry(".git", None));
        assert!(!is_hidden_entry("src", None));
        assert!(!is_hidden_entry("a.txt", None));
    }

    #[test]
    fn empty_directory_yields_no_entries() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            read_dir_sorted(dir.path(), &SortSettings::default())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn missing_directory_reports_path_in_error() {
        let err = read_dir_sorted(
            Path::new("/nonexistent/filex-test"),
            &SortSettings::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("/nonexistent/filex-test"));
    }

    /// Non-UTF-8 filenames must browse correctly: lossy name for display,
    /// but the untouched OS path for navigation/opening. (The search
    /// index deliberately skips them — see index::walker.) Linux-only:
    /// APFS rejects such names outright (EILSEQ), which is exactly why
    /// the limitation is acceptable — CI's ubuntu runner executes this.
    #[cfg(target_os = "linux")]
    #[test]
    fn non_utf8_names_are_displayed_lossily_but_openable() {
        use std::os::unix::ffi::OsStrExt as _;
        let dir = tempfile::tempdir().unwrap();
        let raw = std::ffi::OsStr::from_bytes(b"caf\xE9.txt"); // latin-1 é
        let real_path = dir.path().join(raw);
        fs::write(&real_path, b"x").unwrap();

        let entries = read_dir_sorted(dir.path(), &SortSettings::default()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "caf\u{FFFD}.txt"); // replacement char
        assert_eq!(entries[0].path, real_path); // raw bytes preserved
        assert!(entries[0].path.exists());
    }

    #[test]
    fn file_kinds_classify_by_extension_case_insensitively() {
        assert_eq!(FileKind::of("x", true), FileKind::Directory);
        assert_eq!(FileKind::of("photo.JPG", false), FileKind::Image);
        assert_eq!(FileKind::of("song.flac", false), FileKind::Audio);
        assert_eq!(FileKind::of("clip.mkv", false), FileKind::Video);
        assert_eq!(FileKind::of("main.rs", false), FileKind::Code);
        assert_eq!(FileKind::of("notes.md", false), FileKind::Document);
        assert_eq!(FileKind::of("backup.tar.gz", false), FileKind::Archive);
        assert_eq!(FileKind::of("Makefile", false), FileKind::Other);
        assert_eq!(FileKind::of("weird.xyz", false), FileKind::Other);
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn path_segments_walk_from_root() {
        assert_eq!(
            path_segments(Path::new("/")),
            [("/".to_string(), PathBuf::from("/"))]
        );
        assert_eq!(
            path_segments(Path::new("/Users/nayan/code")),
            [
                ("/".to_string(), PathBuf::from("/")),
                ("Users".to_string(), PathBuf::from("/Users")),
                ("nayan".to_string(), PathBuf::from("/Users/nayan")),
                ("code".to_string(), PathBuf::from("/Users/nayan/code")),
            ]
        );
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn path_segments_fold_drive_prefix_into_root() {
        assert_eq!(
            path_segments(Path::new(r"C:\Users\nayan")),
            [
                (r"C:\".to_string(), PathBuf::from(r"C:\")),
                ("Users".to_string(), PathBuf::from(r"C:\Users")),
                ("nayan".to_string(), PathBuf::from(r"C:\Users\nayan")),
            ]
        );
    }

    #[test]
    fn formats_modified_ages_compactly() {
        let now = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(10 * 365 * 24 * 3600);
        let ago = |secs: u64| Some(now - std::time::Duration::from_secs(secs));
        assert_eq!(format_modified(None, now), "—");
        assert_eq!(format_modified(ago(0), now), "just now");
        assert_eq!(format_modified(ago(59), now), "just now");
        assert_eq!(format_modified(ago(60), now), "1m");
        assert_eq!(format_modified(ago(59 * 60), now), "59m");
        assert_eq!(format_modified(ago(3600), now), "1h");
        assert_eq!(format_modified(ago(24 * 3600), now), "1d");
        assert_eq!(format_modified(ago(7 * 24 * 3600), now), "1w");
        assert_eq!(format_modified(ago(30 * 24 * 3600), now), "1mo");
        assert_eq!(format_modified(ago(365 * 24 * 3600), now), "1y");
        // Future mtime (clock skew) degrades gracefully.
        assert_eq!(
            format_modified(Some(now + std::time::Duration::from_secs(60)), now),
            "just now"
        );
    }

    #[test]
    fn formats_sizes_across_units() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(1024 * 1024), "1.0 MB");
        assert_eq!(format_size(5 * 1024 * 1024 * 1024), "5.0 GB");
        assert_eq!(format_size(u64::MAX), "16777216.0 TB");
    }
}

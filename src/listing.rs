use std::cmp::Ordering;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

/// A single directory entry, sorted for display. GUI-free so it can be unit
/// tested and later swapped to be fed from the volume index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub size: u64,
}

/// Read a directory and return entries sorted directories-first, then
/// case-insensitively by name. Entries whose metadata cannot be read are
/// skipped rather than failing the whole listing.
pub fn read_dir_sorted(path: &Path) -> Result<Vec<Entry>> {
    let read_dir = std::fs::read_dir(path)
        .with_context(|| format!("reading directory {}", path.display()))?;

    let mut entries: Vec<Entry> = read_dir
        .filter_map(|dirent| {
            let dirent = dirent.ok()?;
            let file_type = dirent.file_type().ok()?;
            let size = dirent.metadata().map_or(0, |m| m.len());
            Some(Entry {
                name: dirent.file_name().to_string_lossy().into_owned(),
                path: dirent.path(),
                is_dir: file_type.is_dir(),
                size,
            })
        })
        .collect();

    entries.sort_by(compare_entries);
    Ok(entries)
}

fn compare_entries(a: &Entry, b: &Entry) -> Ordering {
    b.is_dir
        .cmp(&a.is_dir)
        .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
}

/// Broad category of a file, derived from its extension — drives the
/// list icon and decides which files get thumbnails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
        let Some(ext) = name.rsplit_once('.').map(|(_, ext)| ext.to_ascii_lowercase()) else {
            return Self::Other;
        };
        match ext.as_str() {
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "tiff" | "tif" | "ico" => {
                Self::Image
            }
            "mp4" | "mkv" | "mov" | "avi" | "webm" | "m4v" => Self::Video,
            "mp3" | "wav" | "flac" | "ogg" | "m4a" | "aac" | "opus" => Self::Audio,
            "zip" | "tar" | "gz" | "bz2" | "xz" | "zst" | "7z" | "rar" | "dmg" | "iso" => {
                Self::Archive
            }
            "rs" | "py" | "js" | "ts" | "tsx" | "jsx" | "c" | "h" | "cpp" | "hpp" | "go"
            | "java" | "rb" | "sh" | "swift" | "kt" | "toml" | "yaml" | "yml" | "json"
            | "html" | "css" | "sql" => Self::Code,
            "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "txt" | "md" | "rtf" => {
                Self::Document
            }
            _ => Self::Other,
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
        }
    }

    #[test]
    fn sorts_directories_before_files() {
        let mut entries = vec![
            entry("aaa.txt", false),
            entry("zzz", true),
            entry("bbb.txt", false),
        ];
        entries.sort_by(compare_entries);
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["zzz", "aaa.txt", "bbb.txt"]);
    }

    #[test]
    fn sorts_names_case_insensitively() {
        let mut entries = vec![
            entry("Beta", false),
            entry("alpha", false),
            entry("GAMMA", false),
        ];
        entries.sort_by(compare_entries);
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["alpha", "Beta", "GAMMA"]);
    }

    #[test]
    fn reads_real_directory_with_metadata() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("subdir")).unwrap();
        fs::write(dir.path().join("file.txt"), b"hello").unwrap();

        let entries = read_dir_sorted(dir.path()).unwrap();

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "subdir");
        assert!(entries[0].is_dir);
        assert_eq!(entries[1].name, "file.txt");
        assert!(!entries[1].is_dir);
        assert_eq!(entries[1].size, 5);
        assert_eq!(entries[1].path, dir.path().join("file.txt"));
    }

    #[test]
    fn empty_directory_yields_no_entries() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_dir_sorted(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn missing_directory_reports_path_in_error() {
        let err = read_dir_sorted(Path::new("/nonexistent/filex-test")).unwrap_err();
        assert!(err.to_string().contains("/nonexistent/filex-test"));
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

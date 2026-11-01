//! File families: the twelve groups the icon system draws (see
//! `docs/design-icons.md`). Finer than [`crate::listing::FileKind`], which
//! keeps driving search chips and thumbnail decisions: documents split into
//! text, PDF, spreadsheet and presentation, and fonts and apps get their own
//! family. Pure and extension-only — never reads the file.

/// One icon family. Each has its own hue and symbol in the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileFamily {
    Text,
    Pdf,
    Spreadsheet,
    Presentation,
    Image,
    Video,
    Audio,
    Archive,
    Code,
    Font,
    App,
    Other,
}

impl FileFamily {
    /// Every family, in display order.
    pub const ALL: [FileFamily; 12] = [
        Self::Text,
        Self::Pdf,
        Self::Spreadsheet,
        Self::Presentation,
        Self::Image,
        Self::Video,
        Self::Audio,
        Self::Archive,
        Self::Code,
        Self::Font,
        Self::App,
        Self::Other,
    ];

    /// The family of a file name (not a directory), from its extension.
    pub fn of_name(name: &str) -> Self {
        match extension(name) {
            Some(ext) => Self::of_extension(&ext.to_ascii_lowercase()),
            None => Self::Other,
        }
    }

    /// The family a lowercase extension (no dot) implies.
    pub fn of_extension(ext: &str) -> Self {
        match ext {
            "txt" | "md" | "markdown" | "rtf" | "doc" | "docx" | "odt" | "pages" | "tex"
            | "log" | "epub" => Self::Text,
            "pdf" => Self::Pdf,
            "xls" | "xlsx" | "xlsm" | "csv" | "tsv" | "ods" | "numbers" => Self::Spreadsheet,
            "ppt" | "pptx" | "odp" | "key" => Self::Presentation,
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "tiff" | "tif" | "ico" | "heic"
            | "heif" | "avif" | "svg" | "raw" | "cr2" | "nef" | "dng" | "psd" => Self::Image,
            "mp4" | "mkv" | "mov" | "avi" | "webm" | "m4v" | "wmv" | "flv" | "mpg" | "mpeg" => {
                Self::Video
            }
            "mp3" | "wav" | "flac" | "ogg" | "m4a" | "aac" | "opus" | "aiff" | "wma" | "mid"
            | "midi" => Self::Audio,
            "zip" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "7z" | "rar" | "lz4" => {
                Self::Archive
            }
            "rs" | "py" | "js" | "mjs" | "ts" | "tsx" | "jsx" | "c" | "h" | "cc" | "cpp"
            | "hpp" | "cs" | "go" | "java" | "rb" | "php" | "sh" | "bash" | "zsh" | "ps1"
            | "swift" | "kt" | "lua" | "toml" | "yaml" | "yml" | "json" | "xml" | "html"
            | "css" | "scss" | "sql" | "ini" | "cfg" => Self::Code,
            "ttf" | "otf" | "woff" | "woff2" | "ttc" => Self::Font,
            "exe" | "msi" | "msix" | "appx" | "app" | "dmg" | "pkg" | "deb" | "rpm"
            | "appimage" | "apk" | "iso" | "bat" | "cmd" => Self::App,
            _ => Self::Other,
        }
    }

    /// Human-readable family name.
    pub fn label(self) -> &'static str {
        match self {
            Self::Text => "Text",
            Self::Pdf => "PDF",
            Self::Spreadsheet => "Spreadsheet",
            Self::Presentation => "Presentation",
            Self::Image => "Image",
            Self::Video => "Video",
            Self::Audio => "Audio",
            Self::Archive => "Archive",
            Self::Code => "Code",
            Self::Font => "Font",
            Self::App => "App",
            Self::Other => "File",
        }
    }
}

/// The extension after the last dot, if the name has a non-empty one and
/// isn't a dotfile with no other dot (".bashrc" has no extension).
fn extension(name: &str) -> Option<&str> {
    let (stem, ext) = name.rsplit_once('.')?;
    (!stem.is_empty() && !ext.is_empty()).then_some(ext)
}

/// The short label a card icon prints: the extension in capitals, at most
/// four characters (longer ones are cut, never split mid-character). `None`
/// when the name has no extension.
pub fn extension_label(name: &str) -> Option<String> {
    let ext = extension(name)?;
    Some(ext.chars().take(4).flat_map(char::to_uppercase).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::listing::FileKind;

    #[test]
    fn documents_split_into_their_own_families() {
        assert_eq!(FileFamily::of_name("Report.PDF"), FileFamily::Pdf);
        assert_eq!(FileFamily::of_name("budget.xlsx"), FileFamily::Spreadsheet);
        assert_eq!(FileFamily::of_name("deck.pptx"), FileFamily::Presentation);
        assert_eq!(FileFamily::of_name("notes.md"), FileFamily::Text);
    }

    #[test]
    fn fonts_and_apps_have_families() {
        assert_eq!(FileFamily::of_name("Inter.ttf"), FileFamily::Font);
        assert_eq!(FileFamily::of_name("setup.exe"), FileFamily::App);
        assert_eq!(FileFamily::of_name("Filex.dmg"), FileFamily::App);
    }

    #[test]
    fn names_without_an_extension_are_other() {
        for name in ["Makefile", ".bashrc", "trailing.", ""] {
            assert_eq!(FileFamily::of_name(name), FileFamily::Other, "{name:?}");
        }
        assert_eq!(FileFamily::of_name("archive.tar.gz"), FileFamily::Archive);
    }

    /// Families refine kinds; they must never contradict them for the
    /// extensions both know (disk images moved from Archive to App on
    /// purpose: users open them, they don't extract them).
    #[test]
    fn families_agree_with_file_kinds() {
        let pairs = [
            (FileKind::Image, FileFamily::Image),
            (FileKind::Video, FileFamily::Video),
            (FileKind::Audio, FileFamily::Audio),
            (FileKind::Code, FileFamily::Code),
        ];
        for ext in [
            "png", "jpg", "gif", "webp", "mp4", "mkv", "mov", "mp3", "flac", "rs", "py", "json",
        ] {
            let kind = FileKind::of_extension(ext);
            let family = FileFamily::of_extension(ext);
            assert!(
                pairs.contains(&(kind, family)),
                "{ext}: {kind:?} vs {family:?}"
            );
        }
        for ext in ["zip", "tar", "gz", "7z", "rar"] {
            assert_eq!(FileFamily::of_extension(ext), FileFamily::Archive, "{ext}");
        }
        for ext in ["pdf", "docx", "xlsx", "pptx", "txt", "md"] {
            assert_eq!(FileKind::of_extension(ext), FileKind::Document);
            assert!(matches!(
                FileFamily::of_extension(ext),
                FileFamily::Text
                    | FileFamily::Pdf
                    | FileFamily::Spreadsheet
                    | FileFamily::Presentation
            ));
        }
    }

    #[test]
    fn extension_labels_are_short_capitals() {
        assert_eq!(extension_label("a.pdf").as_deref(), Some("PDF"));
        assert_eq!(extension_label("b.woff2").as_deref(), Some("WOFF"));
        assert_eq!(extension_label("c.tar.gz").as_deref(), Some("GZ"));
        assert_eq!(extension_label("README"), None);
        assert_eq!(extension_label(".gitignore"), None);
        // Multi-byte extensions are cut by character, not byte.
        assert_eq!(extension_label("d.ñññññ").as_deref(), Some("ÑÑÑÑ"));
    }

    #[test]
    fn every_family_has_a_label() {
        for family in FileFamily::ALL {
            assert!(!family.label().is_empty());
        }
    }
}

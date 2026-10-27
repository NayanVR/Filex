//! The Open/Save picker protocol (`filex --pick`).
//!
//! Whoever wants a file chosen — the Windows dialog shim today, a Linux
//! portal backend later — spawns `filex --pick`, writes one
//! [`PickRequest`] as JSON to its stdin, and reads one [`PickResponse`]
//! line from its stdout. The rules the window applies (which files a
//! filter shows, what path a typed Save name becomes) live here as plain
//! functions so they are testable without GPUI.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Bumped on any incompatible change; the window refuses other versions so
/// a stale shim falls back to the system dialog instead of misbehaving.
pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PickMode {
    OpenFile,
    SaveFile,
    PickFolder,
}

/// One "Text files (*.txt;*.md)" entry of the type dropdown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileFilter {
    pub name: String,
    /// Glob patterns such as `*.txt`; `*` or `*.*` matches everything.
    pub patterns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PickRequest {
    pub version: u32,
    pub mode: PickMode,
    #[serde(default)]
    pub multiple: bool,
    #[serde(default)]
    pub title: Option<String>,
    /// Label for the confirm button ("Open", "Save", "Export", …).
    #[serde(default)]
    pub ok_label: Option<String>,
    /// Folder to start in. `None` uses the last folder or home.
    #[serde(default)]
    pub folder: Option<PathBuf>,
    /// Initial text of the file-name box (Save).
    #[serde(default)]
    pub file_name: Option<String>,
    #[serde(default)]
    pub filters: Vec<FileFilter>,
    /// Index into `filters` selected at start.
    #[serde(default)]
    pub filter_index: usize,
    /// Extension (without the dot) appended to a Save name typed without
    /// one, e.g. `txt`.
    #[serde(default)]
    pub default_extension: Option<String>,
    /// Ask before overwriting an existing file on Save.
    #[serde(default = "yes")]
    pub confirm_overwrite: bool,
    /// The asking app's window (an HWND on Windows). The picker becomes
    /// its owned window, as the system dialog is: centered over it and
    /// never hidden behind it.
    #[serde(default)]
    pub owner: Option<u64>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum PickResponse {
    Picked {
        paths: Vec<PathBuf>,
        filter_index: usize,
    },
    Cancelled,
}

impl PickRequest {
    /// The filter in effect at `index`, if any (no filters = show all).
    pub fn filter(&self, index: usize) -> Option<&FileFilter> {
        self.filters.get(index)
    }

    pub fn confirm_label(&self) -> &str {
        if let Some(label) = self.ok_label.as_deref().filter(|l| !l.is_empty()) {
            return label;
        }
        match self.mode {
            PickMode::OpenFile => "Open",
            PickMode::SaveFile => "Save",
            PickMode::PickFolder => "Select Folder",
        }
    }

    pub fn window_title(&self) -> &str {
        if let Some(title) = self.title.as_deref().filter(|t| !t.is_empty()) {
            return title;
        }
        match self.mode {
            PickMode::OpenFile => "Open",
            PickMode::SaveFile => "Save As",
            PickMode::PickFolder => "Select Folder",
        }
    }
}

impl FileFilter {
    /// Whether a file named `name` is listed under this filter. Folders are
    /// never filtered (the caller shows them regardless).
    pub fn matches(&self, name: &str) -> bool {
        self.patterns.is_empty() || self.patterns.iter().any(|p| glob_match(p, name))
    }

    /// The first concrete extension of this filter (`*.txt` → `txt`), used
    /// when a Save name is typed without one and no default is given.
    pub fn first_extension(&self) -> Option<&str> {
        self.patterns.iter().find_map(|p| {
            let ext = p.strip_prefix("*.")?;
            (!ext.is_empty() && !ext.contains(['*', '?'])).then_some(ext)
        })
    }
}

/// Case-insensitive `*`/`?` glob over a whole file name. `*.*` matches
/// names without a dot too, as it does in every Windows dialog.
pub fn glob_match(pattern: &str, name: &str) -> bool {
    let pattern = pattern.trim();
    if pattern == "*" || pattern == "*.*" {
        return true;
    }
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let n: Vec<char> = name.to_lowercase().chars().collect();
    wildcard(&p, &n)
}

/// Iterative wildcard match with single-star backtracking.
fn wildcard(p: &[char], n: &[char]) -> bool {
    let (mut pi, mut ni) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ni));
            pi += 1;
        } else if let Some((sp, sn)) = star {
            pi = sp + 1;
            ni = sn + 1;
            star = Some((sp, sn + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

/// The paths as the asking app should receive them: without a verbatim
/// `\\?\` prefix (which typed-path navigation picks up from
/// `canonicalize`) whenever the plain form names the same file. Shell
/// parsing APIs — what the Windows dialog shim turns these into — reject
/// verbatim paths.
pub fn host_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths
        .into_iter()
        .map(|path| crate::listing::plain_path(&path).into_owned())
        .collect()
}

/// Byte length of `name` without its extension (`report.tar.gz` →
/// `report.tar`; `.bashrc` and `Makefile` → the whole name), which the
/// Save box selects so typing keeps the extension.
pub fn stem_len(name: &str) -> usize {
    match name.rfind('.') {
        Some(dot) if dot > 0 => dot,
        _ => name.len(),
    }
}

/// Why a typed Save name cannot be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaveNameError {
    Empty,
    /// Contains a character the file system rejects.
    InvalidCharacter(char),
}

/// Turn the text typed in the Save box into the path to save to. A name
/// with a directory part (`sub\a.txt`, or an absolute path) is honoured,
/// as in the system dialog. An extension is appended only when the name
/// has none: the request's default extension first, else the active
/// filter's first concrete one.
pub fn resolve_save_path(
    folder: &Path,
    typed: &str,
    request: &PickRequest,
    filter_index: usize,
) -> Result<PathBuf, SaveNameError> {
    let typed = typed.trim().trim_matches('"');
    if typed.is_empty() {
        return Err(SaveNameError::Empty);
    }
    if let Some(bad) = typed.chars().find(|c| INVALID_NAME_CHARS.contains(c)) {
        return Err(SaveNameError::InvalidCharacter(bad));
    }
    let typed_path = Path::new(typed);
    let mut path = if typed_path.is_absolute() {
        typed_path.to_path_buf()
    } else {
        folder.join(typed_path)
    };
    let has_extension = path.extension().is_some_and(|e| !e.is_empty());
    if !has_extension {
        let ext = request
            .default_extension
            .as_deref()
            .map(|e| e.trim_start_matches('.'))
            .filter(|e| !e.is_empty())
            .or_else(|| request.filter(filter_index)?.first_extension());
        if let Some(ext) = ext {
            let mut name = path.file_name().unwrap_or_default().to_os_string();
            name.push(".");
            name.push(ext);
            path.set_file_name(name);
        }
    }
    Ok(path)
}

/// Characters no Windows file name may hold (the strictest of the three
/// platforms; `/` and `\` are allowed as separators, `:` only in a drive
/// prefix, which `Path::is_absolute` already handles).
const INVALID_NAME_CHARS: &[char] = &['<', '>', '"', '|', '?', '*'];

#[cfg(test)]
mod tests {
    use super::*;

    fn request(mode: PickMode) -> PickRequest {
        PickRequest {
            version: PROTOCOL_VERSION,
            mode,
            multiple: false,
            title: None,
            ok_label: None,
            folder: None,
            file_name: None,
            filters: vec![
                FileFilter {
                    name: "Images".into(),
                    patterns: vec!["*.png".into(), "*.JPG".into()],
                },
                FileFilter {
                    name: "All files".into(),
                    patterns: vec!["*.*".into()],
                },
            ],
            filter_index: 0,
            default_extension: None,
            confirm_overwrite: true,
            owner: None,
        }
    }

    #[test]
    fn globs_are_case_insensitive_and_whole_name() {
        assert!(glob_match("*.png", "Shot.PNG"));
        assert!(!glob_match("*.png", "shot.png.bak"));
        assert!(glob_match("report-??.txt", "report-07.txt"));
        assert!(!glob_match("report-??.txt", "report-7.txt"));
        assert!(glob_match("*a*b*", "xxaxxbxx"));
    }

    #[test]
    fn star_dot_star_matches_extensionless_names() {
        assert!(glob_match("*.*", "Makefile"));
        assert!(glob_match("*", "Makefile"));
    }

    #[test]
    fn filters_match_any_pattern_and_empty_matches_all() {
        let req = request(PickMode::OpenFile);
        let images = req.filter(0).unwrap();
        assert!(images.matches("a.jpg"));
        assert!(images.matches("a.png"));
        assert!(!images.matches("a.txt"));
        let empty = FileFilter {
            name: String::new(),
            patterns: vec![],
        };
        assert!(empty.matches("anything"));
    }

    #[test]
    fn save_appends_default_extension_only_when_missing() {
        let mut req = request(PickMode::SaveFile);
        req.default_extension = Some(".txt".into());
        let dir = Path::new("base");
        assert_eq!(
            resolve_save_path(dir, "notes", &req, 0),
            Ok(dir.join("notes.txt"))
        );
        assert_eq!(
            resolve_save_path(dir, "notes.md", &req, 0),
            Ok(dir.join("notes.md"))
        );
    }

    #[test]
    fn save_falls_back_to_the_filter_extension() {
        let req = request(PickMode::SaveFile);
        let dir = Path::new("base");
        assert_eq!(
            resolve_save_path(dir, "pic", &req, 0),
            Ok(dir.join("pic.png"))
        );
        // "All files" (*.*) has no concrete extension.
        assert_eq!(resolve_save_path(dir, "pic", &req, 1), Ok(dir.join("pic")));
    }

    #[test]
    fn save_honours_subfolders_and_rejects_bad_names() {
        let req = request(PickMode::SaveFile);
        let dir = Path::new("base");
        assert_eq!(
            resolve_save_path(dir, "sub/a.png", &req, 0),
            Ok(dir.join("sub/a.png"))
        );
        assert_eq!(
            resolve_save_path(dir, "  ", &req, 0),
            Err(SaveNameError::Empty)
        );
        assert_eq!(
            resolve_save_path(dir, "a?.png", &req, 0),
            Err(SaveNameError::InvalidCharacter('?'))
        );
    }

    /// Regression: after navigating by typed path, picks came back as
    /// `\\?\C:\…`, which Chrome's dialog (through the shim) rejected with
    /// E_INVALIDARG, so the page received no file.
    #[cfg(windows)]
    #[test]
    fn picks_reach_the_host_without_a_verbatim_prefix() {
        assert_eq!(
            host_paths(vec![
                PathBuf::from(r"\\?\C:\Users\me\a.txt"),
                PathBuf::from(r"\\?\UNC\server\share\b.txt"),
                PathBuf::from(r"D:\plain.txt"),
            ]),
            vec![
                PathBuf::from(r"C:\Users\me\a.txt"),
                PathBuf::from(r"\\server\share\b.txt"),
                PathBuf::from(r"D:\plain.txt"),
            ]
        );
    }

    #[test]
    fn stems_exclude_only_the_last_extension() {
        assert_eq!(stem_len("report.md"), 6);
        assert_eq!(stem_len("archive.tar.gz"), "archive.tar".len());
        assert_eq!(stem_len(".bashrc"), 7);
        assert_eq!(stem_len("Makefile"), 8);
        assert_eq!(stem_len("ünï.txt"), "ünï".len());
    }

    #[test]
    fn labels_default_by_mode_and_respect_overrides() {
        let mut req = request(PickMode::SaveFile);
        assert_eq!(req.confirm_label(), "Save");
        assert_eq!(req.window_title(), "Save As");
        req.ok_label = Some("Export".into());
        assert_eq!(req.confirm_label(), "Export");
    }

    /// The Windows dialog shim builds this JSON by hand (it does not link
    /// the filex crate); this keeps the two sides of the protocol in step.
    #[test]
    fn the_dialog_shims_request_parses() {
        let json = filex_dialog_shim::protocol::sample_request();
        let req: PickRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(req.version, PROTOCOL_VERSION);
        assert_eq!(req.mode, PickMode::SaveFile);
        assert_eq!(req.filters[0].patterns, vec!["*.txt", "*.md"]);
        assert_eq!(req.default_extension.as_deref(), Some("txt"));
        assert_eq!(req.owner, Some(0x1234));
        let picked = PickResponse::Picked {
            paths: vec![PathBuf::from("C:/a.txt")],
            filter_index: 0,
        };
        assert!(
            filex_dialog_shim::protocol::parse_response(&serde_json::to_string(&picked).unwrap())
                .is_some()
        );
    }

    #[test]
    fn protocol_round_trips_through_json() {
        let req = request(PickMode::OpenFile);
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(serde_json::from_str::<PickRequest>(&json).unwrap(), req);
        let minimal: PickRequest =
            serde_json::from_str(r#"{"version":1,"mode":"pick_folder"}"#).unwrap();
        assert!(minimal.confirm_overwrite);
        assert!(minimal.filters.is_empty());
        let resp = PickResponse::Picked {
            paths: vec![PathBuf::from("a")],
            filter_index: 1,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains(r#""result":"picked""#));
        assert_eq!(serde_json::from_str::<PickResponse>(&json).unwrap(), resp);
    }
}

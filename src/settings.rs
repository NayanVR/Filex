//! Persisted application settings.
//!
//! One JSON file at `<config_dir>/filex/settings.json` is the single
//! source of truth (decision in docs/roadmap.md: JSON, not TOML). It
//! folds in the legacy `roots.list`: a missing settings file migrates
//! roots from there on first load, and the old file keeps working as a
//! read-only fallback for one version.
//!
//! This module is pure I/O + serde — no GPUI. The app wraps it in an
//! entity that emits change events; the index daemon reads it directly.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

/// Bumped when a load-time migration becomes necessary. Unknown fields
/// are ignored and missing fields take defaults, so additive changes
/// don't need a bump.
pub const CURRENT_VERSION: u32 = 1;

/// Default location: `<config_dir>/filex/settings.json`.
pub fn default_settings_file() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("filex").join("settings.json"))
}

/// Everything filex persists about how the app should behave. Fields
/// deliberately cover Phase 2a features that aren't built yet (sort,
/// delete behavior) so the on-disk shape stays stable as they land.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub version: u32,
    /// Indexed roots (absolute paths). Replaces the legacy roots.list.
    pub roots: Vec<PathBuf>,
    pub show_hidden_files: bool,
    pub sort: SortSettings,
    pub confirm_delete: bool,
    pub delete_to_trash: bool,
    pub thumbnails_enabled: bool,
    /// Which color theme to render: a fixed light/dark palette, or one
    /// that follows the OS appearance.
    pub theme: ThemeMode,
    /// Browse layout: a detailed list or a card grid.
    pub view: ViewMode,
    /// Grid card size, as an index into the app's fixed size steps
    /// (clamped to a valid step when consumed).
    pub grid_zoom: u8,
    /// Whether the right-hand details/preview panel is shown.
    pub preview_open: bool,
    /// Width of the details panel in logical pixels (clamped when used).
    pub preview_width: f32,
    /// User-pinned folders shown in the sidebar's Favorites section,
    /// in display order.
    pub favorites: Vec<PathBuf>,
    /// Ids of sidebar sections the user has collapsed (e.g. "recents").
    pub collapsed_sections: Vec<String>,
    /// Consent for sending scrubbed crash reports (Phase 2c). `None` =
    /// not yet asked (a one-time prompt sets it); `Some(false)` = never
    /// upload (crashes are still captured locally); `Some(true)` = upload.
    pub crash_reports: Option<bool>,
}

/// The two browse layouts (block 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ViewMode {
    #[default]
    List,
    Grid,
}

/// The three theme choices exposed in settings. `System` follows the
/// window's OS appearance; the app maps this to a concrete palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ThemeMode {
    #[default]
    System,
    Light,
    Dark,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: CURRENT_VERSION,
            roots: Vec::new(),
            show_hidden_files: false,
            sort: SortSettings::default(),
            confirm_delete: true,
            delete_to_trash: true,
            thumbnails_enabled: true,
            theme: ThemeMode::System,
            view: ViewMode::List,
            grid_zoom: 1,
            preview_open: false,
            preview_width: 280.,
            favorites: Vec::new(),
            collapsed_sections: Vec::new(),
            crash_reports: None,
        }
    }
}

/// How directory listings are ordered (consumed from Phase 2a block 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SortSettings {
    pub by: SortBy,
    pub ascending: bool,
    pub directories_first: bool,
}

impl Default for SortSettings {
    fn default() -> Self {
        Self { by: SortBy::Name, ascending: true, directories_first: true }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SortBy {
    #[default]
    Name,
    Size,
    Modified,
    Kind,
}

impl Settings {
    /// Load settings from `file`. A missing file is first launch, not an
    /// error: returns defaults, with roots migrated from the legacy
    /// `roots.list` (`legacy_roots_file`) when that exists. A file that
    /// exists but doesn't parse is an error — the caller decides
    /// (typically: log it and run on defaults rather than silently
    /// overwriting a file the user may want to fix).
    pub fn load(file: &Path, legacy_roots_file: Option<&Path>) -> Result<Self> {
        let contents = match std::fs::read_to_string(file) {
            Ok(contents) => contents,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let mut settings = Self::default();
                if let Some(legacy) = legacy_roots_file {
                    settings.roots = crate::index::manager::load_roots(legacy);
                }
                return Ok(settings);
            }
            Err(err) => {
                return Err(err).with_context(|| format!("reading {}", file.display()));
            }
        };
        serde_json::from_str(&contents).with_context(|| format!("parsing {}", file.display()))
    }

    /// Write settings as pretty JSON via a sibling temp file + rename,
    /// so a crash mid-write can't truncate the previous settings.
    /// Creates parent directories as needed. Always writes
    /// [`CURRENT_VERSION`].
    pub fn save(&self, file: &Path) -> Result<()> {
        let parent = file.parent().context("settings file has no parent dir")?;
        std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        let on_disk = Self { version: CURRENT_VERSION, ..self.clone() };
        let json = serde_json::to_string_pretty(&on_disk).context("serializing settings")?;
        let tmp = file.with_extension("json.tmp");
        std::fs::write(&tmp, json).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, file)
            .with_context(|| format!("replacing {}", file.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_documented_shape() {
        let settings = Settings::default();
        assert_eq!(settings.version, CURRENT_VERSION);
        assert!(settings.roots.is_empty());
        assert!(!settings.show_hidden_files);
        assert_eq!(settings.sort.by, SortBy::Name);
        assert!(settings.sort.ascending);
        assert!(settings.sort.directories_first);
        assert!(settings.confirm_delete);
        assert!(settings.delete_to_trash);
        assert!(settings.thumbnails_enabled);
        assert_eq!(settings.theme, ThemeMode::System);
        assert_eq!(settings.view, ViewMode::List);
        assert_eq!(settings.grid_zoom, 1);
        assert!(!settings.preview_open);
        assert_eq!(settings.preview_width, 280.);
        assert!(settings.favorites.is_empty());
        assert!(settings.collapsed_sections.is_empty());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("nested").join("settings.json");
        let mut settings = Settings::default();
        settings.roots = vec![PathBuf::from("/tmp/a"), PathBuf::from("/tmp/b")];
        settings.show_hidden_files = true;
        settings.sort.by = SortBy::Modified;
        settings.sort.ascending = false;

        settings.save(&file).unwrap();
        let loaded = Settings::load(&file, None).unwrap();
        assert_eq!(loaded, settings);
    }

    #[test]
    fn missing_file_migrates_roots_from_legacy_list() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("roots.list");
        std::fs::write(&legacy, "/tmp/x\n/tmp/y\n").unwrap();

        let settings =
            Settings::load(&dir.path().join("settings.json"), Some(&legacy)).unwrap();
        assert_eq!(settings.roots, vec![PathBuf::from("/tmp/x"), PathBuf::from("/tmp/y")]);
        // Everything else is defaults.
        assert!(!settings.show_hidden_files);
    }

    #[test]
    fn missing_file_and_no_legacy_is_plain_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let missing_legacy = dir.path().join("roots.list");
        let settings =
            Settings::load(&dir.path().join("settings.json"), Some(&missing_legacy)).unwrap();
        assert_eq!(settings, Settings::default());
    }

    #[test]
    fn corrupt_file_is_an_error_not_silent_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("settings.json");
        std::fs::write(&file, "{ not json").unwrap();
        let err = Settings::load(&file, None).unwrap_err();
        assert!(err.to_string().contains("settings.json"));
    }

    #[test]
    fn sparse_and_unknown_fields_are_tolerated() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("settings.json");
        // A newer filex may write fields this version doesn't know, and
        // an older file may lack fields this version added.
        std::fs::write(&file, r#"{ "version": 1, "show_hidden_files": true, "future": 42 }"#)
            .unwrap();
        let settings = Settings::load(&file, None).unwrap();
        assert!(settings.show_hidden_files);
        assert_eq!(settings.sort, SortSettings::default());
    }

    #[test]
    fn save_is_atomic_enough_to_leave_no_temp_behind() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("settings.json");
        Settings::default().save(&file).unwrap();
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["settings.json"]);
    }
}

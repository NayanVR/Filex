//! Persisted application settings: one JSON file at
//! `<config_dir>/filex/settings.json`.
//!
//! Pure I/O + serde, no GPUI. The app wraps it in an entity that emits
//! change events; the index daemon reads it directly.

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
    /// Indexed roots (absolute paths).
    pub roots: Vec<PathBuf>,
    pub show_hidden_files: bool,
    /// Index OS/system folders. Off by default — on Windows they dominate
    /// the index and nobody searches them, so excluding them is a large
    /// memory win, and they stay browsable either way. Takes full effect on
    /// the next index rebuild.
    pub index_system_files: bool,
    pub sort: SortSettings,
    pub confirm_delete: bool,
    pub delete_to_trash: bool,
    pub thumbnails_enabled: bool,
    /// Which color theme to render: a fixed light/dark/OLED palette, or
    /// one that follows the OS appearance.
    pub theme: ThemeMode,
    /// The accent color applied over whichever palette is active.
    pub accent: AccentColor,
    /// Per-folder icon choices. An absent path uses the automatic kind.
    pub folder_icons: std::collections::BTreeMap<PathBuf, FolderIcon>,
    /// List density (row height / icon size).
    pub density: Density,
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
    /// Consent for Sentry diagnostics: scrubbed crash reports, anonymous
    /// measurements, release-health sessions. On by default (opt-out). Data
    /// carries only crash/metric details — never file names, paths, tags or
    /// queries — and nothing sends without an embedded DSN.
    pub share_diagnostics: bool,
    /// App shortcut overrides by stable command id; empty means unassigned.
    pub keyboard_shortcuts: std::collections::BTreeMap<String, String>,
}

/// The two browse layouts (block 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ViewMode {
    #[default]
    List,
    Grid,
}

/// The theme choices exposed in settings. `System` follows the window's
/// OS appearance; the app maps this to a concrete palette. `Oled` is a
/// true-black variant of dark for OLED screens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ThemeMode {
    #[default]
    System,
    Light,
    Dark,
    Oled,
}

/// The accent color the user picked. `Default` keeps each palette's
/// built-in accent (tuned per light/dark); the rest override it, and the
/// app derives the matching ink and selection tints. Named presets rather
/// than raw hex, so the on-disk value is always valid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AccentColor {
    #[default]
    Default,
    Blue,
    Purple,
    Pink,
    Red,
    Orange,
    Green,
    Teal,
    /// A user-entered `0xRRGGBB` color from the hex field.
    Custom(u32),
}

/// Symbol drawn on a folder's front face. `Plain` explicitly hides a
/// symbol, even when the folder name would otherwise imply a type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FolderIcon {
    Plain,
    Pictures,
    Music,
    Videos,
    Documents,
    Downloads,
    Code,
    Archives,
    Desktop,
}

/// List density: row heights and icon sizes. `Compact` packs more rows in
/// view; `Comfortable` is the roomier default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Density {
    #[default]
    Comfortable,
    Compact,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: CURRENT_VERSION,
            roots: Vec::new(),
            show_hidden_files: false,
            index_system_files: false,
            sort: SortSettings::default(),
            confirm_delete: true,
            delete_to_trash: true,
            thumbnails_enabled: true,
            theme: ThemeMode::System,
            accent: AccentColor::Default,
            folder_icons: Default::default(),
            density: Density::Comfortable,
            view: ViewMode::List,
            grid_zoom: 1,
            preview_open: false,
            preview_width: 280.,
            favorites: Vec::new(),
            collapsed_sections: Vec::new(),
            share_diagnostics: true,
            keyboard_shortcuts: Default::default(),
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
        Self {
            by: SortBy::Name,
            ascending: true,
            directories_first: true,
        }
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
    /// Carry explicit folder symbols through a move or rename. Copies keep
    /// the source choices and duplicate them for the new path. Descendant
    /// choices follow a moved/copied parent folder too.
    pub fn remap_folder_icons(&mut self, from: &Path, to: &Path, copy: bool) {
        let mapped: Vec<_> = self
            .folder_icons
            .iter()
            .filter_map(|(path, icon)| {
                path.strip_prefix(from).ok().map(|suffix| {
                    let new_path = if suffix.as_os_str().is_empty() {
                        to.to_path_buf()
                    } else {
                        to.join(suffix)
                    };
                    (path.clone(), new_path, *icon)
                })
            })
            .collect();
        for (old, new, icon) in mapped {
            if !copy {
                self.folder_icons.remove(&old);
            }
            self.folder_icons.insert(new, icon);
        }
    }

    /// Remove choices for a deleted copy. A trashed folder's choices are
    /// retained so undo can restore them at the original path.
    pub fn clear_folder_icons_under(&mut self, root: &Path) {
        self.folder_icons.retain(|path, _| !path.starts_with(root));
    }

    /// Load settings from `file`. A missing file is first launch, not an
    /// error: first-run defaults.
    /// A file that exists but doesn't parse *is* an error, so the caller
    /// can log and run on defaults rather than silently overwriting a file
    /// the user may want to fix.
    pub fn load(file: &Path) -> Result<Self> {
        let contents = match std::fs::read_to_string(file) {
            Ok(contents) => contents,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::first_run(installer::diagnostics_choice()));
            }
            Err(err) => {
                return Err(err).with_context(|| format!("reading {}", file.display()));
            }
        };
        serde_json::from_str(&contents).with_context(|| format!("parsing {}", file.display()))
    }

    /// Defaults for a machine with no settings file yet, seeded with the
    /// diagnostics choice made in the installer (if any). Only consulted
    /// while the file is missing, so a reinstall or silent update that
    /// rewrites the installer value never overrides the user's later toggle.
    fn first_run(installer_share_diagnostics: Option<bool>) -> Self {
        Self {
            share_diagnostics: installer_share_diagnostics.unwrap_or(true),
            ..Self::default()
        }
    }

    /// Write settings as pretty JSON via a sibling temp file + rename,
    /// so a crash mid-write can't truncate the previous settings.
    /// Creates parent directories as needed. Always writes
    /// [`CURRENT_VERSION`].
    pub fn save(&self, file: &Path) -> Result<()> {
        let parent = file.parent().context("settings file has no parent dir")?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
        let on_disk = Self {
            version: CURRENT_VERSION,
            ..self.clone()
        };
        let json = serde_json::to_string_pretty(&on_disk).context("serializing settings")?;
        let tmp = file.with_extension("json.tmp");
        std::fs::write(&tmp, json).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, file).with_context(|| format!("replacing {}", file.display()))
    }
}

/// Choices recorded by the platform installer. Only the Windows MSI asks
/// anything today; the macOS cask and Linux tarball have no install UI.
mod installer {
    /// The "Send crash reports" checkbox from the MSI's privacy page,
    /// stored as `HKLM\Software\Filex\CrashReports` (REG_DWORD 0/1) by
    /// `wix/main.wxs`. Assumes a 64-bit process reading the 64-bit view,
    /// which is where the x64 MSI writes. Missing key or value = no choice.
    #[cfg(target_os = "windows")]
    pub fn diagnostics_choice() -> Option<bool> {
        use windows::Win32::System::Registry::{
            HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RegGetValueW,
        };
        use windows::core::w;
        let mut value = 0u32;
        let mut size = std::mem::size_of::<u32>() as u32;
        // SAFETY: `value`/`size` are a valid DWORD buffer and its length.
        let status = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                w!("Software\\Filex"),
                w!("CrashReports"),
                RRF_RT_REG_DWORD,
                None,
                Some(std::ptr::from_mut(&mut value).cast()),
                Some(&mut size),
            )
        };
        status.is_ok().then_some(value != 0)
    }

    #[cfg(not(target_os = "windows"))]
    pub fn diagnostics_choice() -> Option<bool> {
        None
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
        assert!(!settings.index_system_files); // OS folders excluded by default
        assert_eq!(settings.sort.by, SortBy::Name);
        assert!(settings.sort.ascending);
        assert!(settings.sort.directories_first);
        assert!(settings.confirm_delete);
        assert!(settings.delete_to_trash);
        assert!(settings.thumbnails_enabled);
        assert_eq!(settings.theme, ThemeMode::System);
        assert!(settings.folder_icons.is_empty());
        assert_eq!(settings.view, ViewMode::List);
        assert_eq!(settings.grid_zoom, 1);
        assert!(!settings.preview_open);
        assert_eq!(settings.preview_width, 280.);
        assert!(settings.favorites.is_empty());
        assert!(settings.collapsed_sections.is_empty());
        assert!(settings.keyboard_shortcuts.is_empty());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("nested").join("settings.json");
        let mut settings = Settings {
            roots: vec![PathBuf::from("/tmp/a"), PathBuf::from("/tmp/b")],
            show_hidden_files: true,
            ..Default::default()
        };
        settings
            .folder_icons
            .insert(PathBuf::from("/tmp/holiday"), FolderIcon::Pictures);
        settings.sort.by = SortBy::Modified;
        settings.sort.ascending = false;
        settings
            .keyboard_shortcuts
            .insert("search".into(), "ctrl-alt-s".into());
        settings
            .keyboard_shortcuts
            .insert("view".into(), String::new());

        settings.save(&file).unwrap();
        let loaded = Settings::load(&file).unwrap();
        assert_eq!(loaded, settings);
    }

    #[test]
    fn folder_icon_choices_follow_parent_moves_and_copies() {
        let mut settings = Settings::default();
        settings
            .folder_icons
            .insert(PathBuf::from("/old/Photos"), FolderIcon::Pictures);
        settings.remap_folder_icons(Path::new("/old"), Path::new("/new"), false);
        assert!(!settings.folder_icons.contains_key(Path::new("/old/Photos")));
        assert_eq!(
            settings.folder_icons.get(Path::new("/new/Photos")),
            Some(&FolderIcon::Pictures)
        );
        settings.remap_folder_icons(Path::new("/new"), Path::new("/copy"), true);
        assert!(settings.folder_icons.contains_key(Path::new("/new/Photos")));
        settings.clear_folder_icons_under(Path::new("/copy"));
        assert!(
            !settings
                .folder_icons
                .contains_key(Path::new("/copy/Photos"))
        );
    }

    #[test]
    fn missing_file_is_plain_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let settings = Settings::load(&dir.path().join("settings.json")).unwrap();
        assert_eq!(settings, Settings::default());
    }

    #[test]
    fn first_run_honours_the_installer_crash_report_choice() {
        assert!(!Settings::first_run(Some(false)).share_diagnostics);
        assert!(Settings::first_run(Some(true)).share_diagnostics);
        // No installer choice (macOS/Linux, or a pre-privacy-page MSI) keeps
        // the opt-out default.
        assert_eq!(Settings::first_run(None), Settings::default());
    }

    #[test]
    fn corrupt_file_is_an_error_not_silent_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("settings.json");
        std::fs::write(&file, "{ not json").unwrap();
        let err = Settings::load(&file).unwrap_err();
        assert!(err.to_string().contains("settings.json"));
    }

    #[test]
    fn sparse_and_unknown_fields_are_tolerated() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("settings.json");
        // A newer filex may write fields this version doesn't know, and
        // an older file may lack fields this version added.
        std::fs::write(
            &file,
            r#"{ "version": 1, "show_hidden_files": true, "future": 42 }"#,
        )
        .unwrap();
        let settings = Settings::load(&file).unwrap();
        assert!(settings.show_hidden_files);
        assert_eq!(settings.sort, SortSettings::default());
        assert!(settings.keyboard_shortcuts.is_empty());
        assert!(settings.folder_icons.is_empty());
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

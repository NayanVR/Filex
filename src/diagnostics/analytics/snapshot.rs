//! Settings → typed events: the launch snapshot and the change diff.
//! Lists (roots, favorites, excluded names, icon choices) are reported as
//! lengths and never listed. `share_diagnostics` is never reported:
//! consent is applied, not measured.

use crate::settings::{AccentColor, Density, Settings, SortBy, ThemeMode, ViewMode};

use super::catalog::{SettingChanged, SettingKey, SettingValue, SettingsSnapshot};

pub fn theme_label(theme: ThemeMode) -> &'static str {
    match theme {
        ThemeMode::System => "system",
        ThemeMode::Light => "light",
        ThemeMode::Dark => "dark",
        ThemeMode::Oled => "oled",
    }
}

/// A custom hex accent is reported as `custom`, not its value.
pub fn accent_label(accent: AccentColor) -> &'static str {
    match accent {
        AccentColor::Default => "default",
        AccentColor::Blue => "blue",
        AccentColor::Purple => "purple",
        AccentColor::Pink => "pink",
        AccentColor::Red => "red",
        AccentColor::Orange => "orange",
        AccentColor::Green => "green",
        AccentColor::Teal => "teal",
        AccentColor::Custom(_) => "custom",
    }
}

pub fn view_label(view: ViewMode) -> &'static str {
    match view {
        ViewMode::List => "list",
        ViewMode::Grid => "grid",
    }
}

pub fn density_label(density: Density) -> &'static str {
    match density {
        Density::Comfortable => "comfortable",
        Density::Compact => "compact",
    }
}

pub fn sort_label(by: SortBy) -> &'static str {
    match by {
        SortBy::Name => "name",
        SortBy::Size => "size",
        SortBy::Modified => "modified",
        SortBy::Kind => "kind",
    }
}

pub fn snapshot(s: &Settings) -> SettingsSnapshot {
    SettingsSnapshot {
        theme: theme_label(s.theme),
        accent: accent_label(s.accent),
        view: view_label(s.view),
        density: density_label(s.density),
        grid_zoom: s.grid_zoom,
        preview_open: s.preview_open,
        sort_by: sort_label(s.sort.by),
        sort_ascending: s.sort.ascending,
        dirs_first: s.sort.directories_first,
        show_hidden: s.show_hidden_files,
        index_system: s.index_system_files,
        exclude_dev: s.exclude_dev_folders,
        confirm_delete: s.confirm_delete,
        delete_to_trash: s.delete_to_trash,
        thumbnails: s.thumbnails_enabled,
        transparency: s.transparency_effects,
        roots_count: s.roots.len(),
        favorites_count: s.favorites.len(),
        excluded_names_count: s.excluded_folder_names.len(),
        folder_icons_count: s.folder_icons.len(),
        custom_shortcuts_count: s.keyboard_shortcuts.len(),
        collapsed_sections_count: s.collapsed_sections.len(),
    }
}

/// One `setting_changed` per setting that differs, with its new value.
/// `preview_width` changes on every drag frame, so it is left out.
pub fn changes(old: &Settings, new: &Settings) -> Vec<SettingChanged> {
    let mut out = Vec::new();
    let mut push = |setting, value| out.push(SettingChanged { setting, value });
    macro_rules! label {
        ($key:ident, $f:ident, $($field:tt)+) => {
            if old.$($field)+ != new.$($field)+ {
                push(SettingKey::$key, SettingValue::Label($f(new.$($field)+)));
            }
        };
    }
    macro_rules! flag {
        ($key:ident, $($field:tt)+) => {
            if old.$($field)+ != new.$($field)+ {
                push(SettingKey::$key, SettingValue::Bool(new.$($field)+));
            }
        };
    }
    macro_rules! count {
        ($key:ident, $field:ident) => {
            if old.$field != new.$field {
                push(
                    SettingKey::$key,
                    SettingValue::Count(new.$field.len() as u64),
                );
            }
        };
    }
    label!(Theme, theme_label, theme);
    label!(Accent, accent_label, accent);
    label!(View, view_label, view);
    label!(Density, density_label, density);
    label!(SortBy, sort_label, sort.by);
    flag!(SortAscending, sort.ascending);
    flag!(DirsFirst, sort.directories_first);
    if old.grid_zoom != new.grid_zoom {
        push(
            SettingKey::GridZoom,
            SettingValue::Count(u64::from(new.grid_zoom)),
        );
    }
    flag!(PreviewOpen, preview_open);
    flag!(ShowHidden, show_hidden_files);
    flag!(IndexSystem, index_system_files);
    flag!(ExcludeDev, exclude_dev_folders);
    flag!(ConfirmDelete, confirm_delete);
    flag!(DeleteToTrash, delete_to_trash);
    flag!(Thumbnails, thumbnails_enabled);
    flag!(Transparency, transparency_effects);
    count!(Roots, roots);
    count!(Favorites, favorites);
    count!(ExcludedNames, excluded_folder_names);
    count!(FolderIcons, folder_icons);
    count!(CustomShortcuts, keyboard_shortcuts);
    count!(CollapsedSections, collapsed_sections);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::analytics::schema::AnalyticsEvent;

    #[test]
    fn changes_report_new_values_and_list_lengths_only() {
        let old = Settings::default();
        let mut new = old.clone();
        new.theme = ThemeMode::Oled;
        new.show_hidden_files = true;
        new.favorites.push("/Users/ann/Taxes".into());
        new.share_diagnostics = false;
        assert_eq!(
            changes(&old, &new),
            vec![
                SettingChanged {
                    setting: SettingKey::Theme,
                    value: SettingValue::Label("oled")
                },
                SettingChanged {
                    setting: SettingKey::ShowHidden,
                    value: SettingValue::Bool(true)
                },
                SettingChanged {
                    setting: SettingKey::Favorites,
                    value: SettingValue::Count(1)
                },
            ]
        );
        assert!(changes(&old, &old).is_empty());
    }

    #[test]
    fn snapshot_reports_lists_as_counts_only() {
        let mut settings = Settings::default();
        settings.favorites = vec!["/Users/ann/Secret Project".into()];
        settings.accent = AccentColor::Custom(0x123456);
        let event = snapshot(&settings);
        let json = format!("{:?}", event.props());
        assert!(!json.contains("Secret"), "{json}");
        assert!(!json.contains("1193046"), "custom accent value leaked");
        assert_eq!(event.favorites_count, 1);
        assert_eq!(event.accent, "custom");
    }
}

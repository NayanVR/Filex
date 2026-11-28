//! A settings snapshot for `app_launched` and labels for `setting_changed`.
//! It answers "which defaults do people keep?" with enums, bools and
//! counts only. Folder lists (roots, favorites, excluded names, icon
//! choices) are reported as counts and never listed.

use crate::settings::{AccentColor, Density, Settings, SortBy, ThemeMode, ViewMode};

use super::event::Event;

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

/// Add the snapshot to `event` under `setting_*` keys.
pub fn with_settings(event: Event, s: &Settings) -> Event {
    event
        .with("setting_theme", theme_label(s.theme))
        .with("setting_accent", accent_label(s.accent))
        .with("setting_view", view_label(s.view))
        .with("setting_density", density_label(s.density))
        .with("setting_grid_zoom", s.grid_zoom)
        .with("setting_preview_open", s.preview_open)
        .with("setting_sort_by", sort_label(s.sort.by))
        .with("setting_sort_ascending", s.sort.ascending)
        .with("setting_dirs_first", s.sort.directories_first)
        .with("setting_show_hidden", s.show_hidden_files)
        .with("setting_index_system", s.index_system_files)
        .with("setting_exclude_dev", s.exclude_dev_folders)
        .with("setting_confirm_delete", s.confirm_delete)
        .with("setting_delete_to_trash", s.delete_to_trash)
        .with("setting_thumbnails", s.thumbnails_enabled)
        .with("setting_transparency", s.transparency_effects)
        .with("roots_count", s.roots.len())
        .with("favorites_count", s.favorites.len())
        .with("excluded_names_count", s.excluded_folder_names.len())
        .with("folder_icons_count", s.folder_icons.len())
        .with("custom_shortcuts_count", s.keyboard_shortcuts.len())
        .with("collapsed_sections_count", s.collapsed_sections.len())
}

/// One `setting_changed` event per setting that differs between `old`
/// and `new`, with the new value (a label, bool or count). List-valued
/// settings report their new length, never their contents.
/// `share_diagnostics` is excluded: consent is applied, not reported.
pub fn changes(old: &Settings, new: &Settings) -> Vec<Event> {
    let mut out = Vec::new();
    let mut push = |setting: &'static str, value: super::event::Prop| {
        out.push(
            Event::new(super::event::names::SETTING_CHANGED)
                .with("setting", setting)
                .with("value", value),
        );
    };
    macro_rules! flag {
        ($name:literal, $($field:tt)+) => {
            if old.$($field)+ != new.$($field)+ {
                push($name, new.$($field)+.into());
            }
        };
    }
    macro_rules! label {
        ($name:literal, $f:ident, $($field:tt)+) => {
            if old.$($field)+ != new.$($field)+ {
                push($name, $f(new.$($field)+).into());
            }
        };
    }
    macro_rules! count {
        ($name:literal, $field:ident) => {
            if old.$field != new.$field {
                push($name, new.$field.len().into());
            }
        };
    }
    label!("theme", theme_label, theme);
    label!("accent", accent_label, accent);
    label!("view", view_label, view);
    label!("density", density_label, density);
    label!("sort_by", sort_label, sort.by);
    flag!("sort_ascending", sort.ascending);
    flag!("dirs_first", sort.directories_first);
    flag!("grid_zoom", grid_zoom);
    flag!("preview_open", preview_open);
    flag!("show_hidden", show_hidden_files);
    flag!("index_system", index_system_files);
    flag!("exclude_dev", exclude_dev_folders);
    flag!("confirm_delete", confirm_delete);
    flag!("delete_to_trash", delete_to_trash);
    flag!("thumbnails", thumbnails_enabled);
    flag!("transparency", transparency_effects);
    count!("roots", roots);
    count!("favorites", favorites);
    count!("excluded_names", excluded_folder_names);
    count!("folder_icons", folder_icons);
    count!("custom_shortcuts", keyboard_shortcuts);
    count!("collapsed_sections", collapsed_sections);
    // `preview_width` changes on every drag frame, so it is left out.
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::analytics::event::Prop;

    #[test]
    fn changes_report_new_values_and_list_lengths_only() {
        let old = Settings::default();
        let mut new = old.clone();
        new.theme = ThemeMode::Oled;
        new.show_hidden_files = true;
        new.favorites.push("/Users/ann/Taxes".into());
        new.share_diagnostics = false;
        let events = changes(&old, &new);
        let pairs: Vec<(Prop, Prop)> = events
            .iter()
            .map(|e| (e.props[0].1.clone(), e.props[1].1.clone()))
            .collect();
        assert_eq!(
            pairs,
            vec![
                (Prop::Label("theme"), Prop::Label("oled")),
                (Prop::Label("show_hidden"), Prop::Bool(true)),
                (Prop::Label("favorites"), Prop::Int(1)),
            ]
        );
        assert!(changes(&old, &old).is_empty());
    }

    #[test]
    fn snapshot_reports_lists_as_counts_only() {
        let mut settings = Settings::default();
        settings.favorites = vec!["/Users/ann/Secret Project".into()];
        settings.accent = AccentColor::Custom(0x123456);
        let event = with_settings(Event::new("app_launched"), &settings);
        let json = serde_json::Value::Object(event.props_json()).to_string();
        assert!(!json.contains("Secret"), "{json}");
        assert!(!json.contains("1193046"), "custom accent value leaked");
        assert!(event.props.contains(&("favorites_count", Prop::Int(1))));
        assert!(
            event
                .props
                .contains(&("setting_accent", Prop::Label("custom")))
        );
    }
}

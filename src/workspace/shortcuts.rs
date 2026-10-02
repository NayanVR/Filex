//! One registry drives keyboard settings, persisted overrides, and live bindings.
use super::*;
use std::collections::BTreeMap;

pub struct Shortcut {
    pub id: &'static str,
    pub label: &'static str,
    pub group: &'static str,
    pub default: &'static str,
    pub aliases: &'static [&'static str],
    pub in_editor: bool,
    action: fn() -> Box<dyn gpui::Action>,
}

pub fn catalog() -> Vec<Shortcut> {
    let mac = cfg!(target_os = "macos");
    macro_rules! entry {
        ($id:literal, $label:literal, $group:literal, $key:expr, $aliases:expr, $editor:expr, $action:expr) => {
            Shortcut {
                id: $id,
                label: $label,
                group: $group,
                default: $key,
                aliases: $aliases,
                in_editor: $editor,
                action: || Box::new($action),
            }
        };
    }
    vec![
        entry!(
            "search",
            "Focus search",
            "Navigation",
            if mac { "cmd-k" } else { "ctrl-k" },
            if mac {
                &["cmd-f", "/"]
            } else {
                &["ctrl-f", "/"]
            },
            true,
            FocusSearch
        ),
        entry!(
            "location",
            "Edit path",
            "Navigation",
            if mac { "cmd-l" } else { "ctrl-l" },
            &[],
            true,
            FocusPath
        ),
        entry!(
            "back",
            "Go back",
            "Navigation",
            if mac { "cmd-[" } else { "alt-left" },
            &[],
            true,
            GoBack
        ),
        entry!(
            "forward",
            "Go forward",
            "Navigation",
            if mac { "cmd-]" } else { "alt-right" },
            &[],
            true,
            GoForward
        ),
        entry!(
            "parent",
            "Parent folder",
            "Navigation",
            if mac { "cmd-up" } else { "alt-up" },
            &[],
            true,
            GoUp
        ),
        entry!(
            "refresh",
            "Refresh folder",
            "Navigation",
            if mac { "cmd-r" } else { "ctrl-r" },
            &["f5"],
            true,
            Refresh
        ),
        entry!(
            "previous",
            "Previous item",
            "Selection & files",
            "up",
            &[],
            true,
            SelectPrevious
        ),
        entry!(
            "next",
            "Next item",
            "Selection & files",
            "down",
            &[],
            true,
            SelectNext
        ),
        entry!(
            "extend_previous",
            "Extend selection up",
            "Selection & files",
            "shift-up",
            &[],
            true,
            ExtendPrevious
        ),
        entry!(
            "extend_next",
            "Extend selection down",
            "Selection & files",
            "shift-down",
            &[],
            true,
            ExtendNext
        ),
        entry!(
            "open",
            "Open selected item",
            "Selection & files",
            "enter",
            &[],
            true,
            OpenSelected
        ),
        entry!(
            "select_all",
            "Select all files",
            "Selection & files",
            if mac { "cmd-a" } else { "ctrl-a" },
            &[],
            false,
            search_input::SelectAll
        ),
        entry!(
            "copy",
            "Copy files",
            "Selection & files",
            if mac { "cmd-c" } else { "ctrl-c" },
            &[],
            false,
            search_input::Copy
        ),
        entry!(
            "cut",
            "Cut files",
            "Selection & files",
            if mac { "cmd-x" } else { "ctrl-x" },
            &[],
            false,
            search_input::Cut
        ),
        entry!(
            "paste",
            "Paste files",
            "Selection & files",
            if mac { "cmd-v" } else { "ctrl-v" },
            &[],
            false,
            search_input::Paste
        ),
        entry!(
            "rename",
            "Rename selected item",
            "Selection & files",
            "f2",
            &[],
            false,
            RenameSelected
        ),
        entry!(
            "trash",
            "Move to Trash",
            "Selection & files",
            if mac { "cmd-backspace" } else { "ctrl-delete" },
            &[],
            false,
            DeleteSelected
        ),
        entry!(
            "undo",
            "Undo file operation",
            "Selection & files",
            if mac { "cmd-z" } else { "ctrl-z" },
            &[],
            false,
            Undo
        ),
        entry!(
            "view",
            "Switch list / grid",
            "View",
            "v",
            &[],
            false,
            ToggleView
        ),
        entry!(
            "preview",
            "Toggle details",
            "View",
            if mac { "cmd-i" } else { "ctrl-i" },
            &["p"],
            true,
            TogglePreview
        ),
        #[cfg(target_os = "macos")]
        entry!(
            "quick_look",
            "Quick Look selected files",
            "View",
            "space",
            &[],
            false,
            QuickLook
        ),
        entry!(
            "settings",
            "Open settings",
            "View",
            if mac { "cmd-," } else { "ctrl-," },
            &[],
            true,
            ToggleSettings
        ),
        entry!(
            "keyboard",
            "Keyboard settings",
            "View",
            "shift-/",
            &["?"],
            false,
            ToggleShortcuts
        ),
        entry!(
            "new_tab",
            "New tab",
            "Tabs & window",
            if mac { "cmd-t" } else { "ctrl-t" },
            &[],
            true,
            NewTab
        ),
        entry!(
            "close_tab",
            "Close tab",
            "Tabs & window",
            if mac { "cmd-w" } else { "ctrl-w" },
            &[],
            true,
            CloseTab
        ),
        entry!(
            "next_tab",
            "Next tab",
            "Tabs & window",
            "ctrl-tab",
            &[],
            true,
            NextTab
        ),
        entry!(
            "previous_tab",
            "Previous tab",
            "Tabs & window",
            "ctrl-shift-tab",
            &[],
            true,
            PrevTab
        ),
        entry!(
            "quit",
            "Quit filex",
            "Tabs & window",
            if mac { "cmd-q" } else { "ctrl-q" },
            &[],
            true,
            Quit
        ),
    ]
}

pub fn active_keys(spec: &Shortcut, overrides: &BTreeMap<String, String>) -> Vec<String> {
    match overrides.get(spec.id) {
        Some(key) => {
            if key.is_empty() {
                vec![]
            } else {
                vec![key.clone()]
            }
        }
        None => std::iter::once(spec.default)
            .chain(spec.aliases.iter().copied())
            .map(str::to_owned)
            .collect(),
    }
}

fn normalize(chord: &str) -> Result<String, String> {
    if chord.split_whitespace().count() != 1 {
        return Err("Use one key combination.".into());
    }
    gpui::Keystroke::parse(chord)
        .map(|k| k.unparse())
        .map_err(|_| "That key combination isn’t supported.".into())
}

pub fn validate(
    id: &str,
    chord: &str,
    overrides: &BTreeMap<String, String>,
) -> Result<String, String> {
    let normalized = normalize(chord)?;
    let key = gpui::Keystroke::parse(&normalized).map_err(|_| "Unsupported shortcut")?;
    if matches!(
        key.key.as_str(),
        "escape" | "shift" | "control" | "alt" | "cmd" | "platform" | "fn"
    ) {
        return Err("Escape cancels editing. Choose another combination.".into());
    }
    if !key.modifiers.modified()
        && matches!(key.key.as_str(), "backspace" | "delete" | "tab" | "space")
        && !(id == "quick_look" && key.key == "space")
    {
        return Err("This key is reserved for editing and navigation.".into());
    }
    let specs = catalog();
    let Some(spec) = specs.iter().find(|s| s.id == id) else {
        return Err("Unknown shortcut.".into());
    };
    for other in specs.iter().filter(|s| s.id != id) {
        if active_keys(other, overrides)
            .iter()
            .any(|k| normalize(k).ok().as_ref() == Some(&normalized))
        {
            return Err(format!("Already used by {}.", other.label));
        }
    }
    // File commands retain standard editing keys only in their own default mapping.
    // A remap must never consume Copy, Paste, or caret movement in a text field.
    let own_default = normalize(spec.default).ok().as_ref() == Some(&normalized);
    let editing = if cfg!(target_os = "macos") {
        &[
            "cmd-a",
            "cmd-c",
            "cmd-x",
            "cmd-v",
            "cmd-z",
            "cmd-backspace",
            "cmd-left",
            "cmd-right",
            "alt-left",
            "alt-right",
            "alt-backspace",
        ][..]
    } else {
        &[
            "ctrl-a",
            "ctrl-c",
            "ctrl-x",
            "ctrl-v",
            "ctrl-z",
            "ctrl-left",
            "ctrl-right",
            "ctrl-backspace",
            "ctrl-delete",
        ][..]
    };
    if !own_default
        && editing
            .iter()
            .any(|k| normalize(k).ok().as_ref() == Some(&normalized))
    {
        return Err("Reserved for text editing. Add a different modifier or key.".into());
    }
    Ok(normalized)
}

pub fn bindings(overrides: &BTreeMap<String, String>) -> Vec<KeyBinding> {
    let mut output = vec![];
    for spec in catalog() {
        // Invalid hand-edited settings fall back to the default instead of panicking.
        let keys = if let Some(custom) = overrides.get(spec.id) {
            if custom.is_empty() {
                vec![]
            } else if validate(spec.id, custom, overrides).is_ok() {
                vec![custom.clone()]
            } else {
                active_keys(&spec, &BTreeMap::new())
            }
        } else {
            active_keys(&spec, overrides)
        };
        for key in keys {
            let Ok(stroke) = gpui::Keystroke::parse(&key) else {
                continue;
            };
            let text_key = !stroke.modifiers.control
                && !stroke.modifiers.platform
                && !stroke.modifiers.alt
                && !matches!(stroke.key.as_str(), "up" | "down" | "enter" | "f5");
            let context = if !spec.in_editor || text_key {
                "Workspace && !Settings && !SearchInput"
            } else if matches!(
                spec.id,
                "search" | "location" | "settings" | "quit" | "new_tab" | "close_tab"
            ) {
                "Workspace && !Settings"
            } else {
                "Workspace && !Settings && !PathInput"
            };
            if let Ok(binding) = KeyBinding::load(
                &key,
                (spec.action)(),
                Some(
                    gpui::KeyBindingContextPredicate::parse(context)
                        .expect("static context")
                        .into(),
                ),
                false,
                None,
                &gpui::DummyKeyboardMapper,
            ) {
                output.push(binding);
            }
        }
    }
    output.push(KeyBinding::new(
        "enter",
        SubmitPath,
        Some("PathInput && !Settings"),
    ));
    output.push(KeyBinding::new(
        "escape",
        CancelPath,
        Some("PathInput && !Settings"),
    ));
    output.push(KeyBinding::new(
        "escape",
        search_input::ClearInput,
        Some("Workspace && !SearchInput && !PathInput && !Settings"),
    ));
    output
}

pub fn install(overrides: &BTreeMap<String, String>, cx: &mut App) {
    cx.clear_key_bindings();
    cx.bind_keys(bindings(overrides));
    search_input::bind_keys(cx);
}

pub fn label(chord: &str) -> String {
    let Ok(key) = gpui::Keystroke::parse(chord) else {
        return chord.to_owned();
    };
    let mut parts = vec![];
    if key.modifiers.control {
        parts.push("Ctrl".to_owned());
    }
    if key.modifiers.alt {
        parts.push(
            if cfg!(target_os = "macos") {
                "⌥"
            } else {
                "Alt"
            }
            .to_owned(),
        );
    }
    if key.modifiers.shift {
        parts.push("⇧".to_owned());
    }
    if key.modifiers.platform {
        parts.push(
            if cfg!(target_os = "macos") {
                "⌘"
            } else {
                "Super"
            }
            .to_owned(),
        );
    }
    parts.push(match key.key.as_str() {
        "up" => "↑".into(),
        "down" => "↓".into(),
        "left" => "←".into(),
        "right" => "→".into(),
        "enter" => "↵".into(),
        "backspace" => "⌫".into(),
        "tab" => "Tab".into(),
        "delete" => "Delete".into(),
        "space" => "Space".into(),
        _ => key.key.to_uppercase(),
    });
    parts.join(" ")
}

pub fn hint(id: &str, overrides: &BTreeMap<String, String>) -> String {
    catalog()
        .iter()
        .find(|s| s.id == id)
        .and_then(|s| active_keys(s, overrides).first().cloned())
        .map(|s| label(&s))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "macos")]
    #[test]
    fn quick_look_space_is_only_bound_outside_text_editing_and_settings() {
        let overrides = BTreeMap::new();
        assert_eq!(
            actions_for("space", &["Workspace"], &overrides),
            vec!["filex::QuickLook"]
        );
        for contexts in [
            vec!["Workspace", "SearchInput"],
            vec!["Workspace", "PathInput", "SearchInput"],
            vec!["Workspace", "Settings"],
        ] {
            assert!(actions_for("space", &contexts, &overrides).is_empty());
        }
        assert!(validate("quick_look", "space", &overrides).is_ok());
        assert!(validate("search", "space", &overrides).is_err());
        assert_eq!(label("space"), "Space");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn quick_look_can_be_disabled_or_remapped_without_taking_text_keys() {
        let mut overrides = BTreeMap::from([("quick_look".into(), String::new())]);
        assert!(actions_for("space", &["Workspace"], &overrides).is_empty());
        overrides.insert("quick_look".into(), "cmd-y".into());
        assert!(validate("quick_look", "cmd-y", &overrides).is_ok());
        assert!(actions_for("space", &["Workspace"], &overrides).is_empty());
        assert_eq!(
            actions_for("cmd-y", &["Workspace"], &overrides),
            vec!["filex::QuickLook"]
        );
        assert!(actions_for("cmd-y", &["Workspace", "SearchInput"], &overrides).is_empty());
    }
    fn actions_for(
        chord: &str,
        contexts: &[&str],
        overrides: &BTreeMap<String, String>,
    ) -> Vec<String> {
        let mut all_bindings = bindings(overrides);
        all_bindings.extend(search_input::key_bindings());
        let keymap = gpui::Keymap::new(all_bindings);
        let contexts: Vec<_> = contexts
            .iter()
            .map(|s| gpui::KeyContext::parse(s).unwrap())
            .collect();
        keymap
            .bindings_for_input(&[gpui::Keystroke::parse(chord).unwrap()], &contexts)
            .0
            .iter()
            .map(|b| b.action().name().to_string())
            .collect()
    }
    #[test]
    fn path_submission_and_cancel_have_dedicated_actions() {
        let contexts = &["Workspace", "PathInput", "SearchInput"];
        assert_eq!(
            actions_for("escape", contexts, &BTreeMap::new()),
            vec!["filex::CancelPath"]
        );
        assert_eq!(
            actions_for("enter", contexts, &BTreeMap::new()),
            vec!["filex::SubmitPath"]
        );
    }

    #[test]
    fn clearing_and_resetting_a_shortcut_updates_its_bindings() {
        let mut overrides = BTreeMap::from([("search".into(), String::new())]);
        assert!(actions_for("/", &["Workspace"], &overrides).is_empty());
        overrides.remove("search");
        assert!(
            actions_for("/", &["Workspace"], &overrides)
                .iter()
                .any(|a| a.ends_with("FocusSearch"))
        );
    }

    #[test]
    fn command_k_searches_even_while_editing_a_path() {
        let key = if cfg!(target_os = "macos") {
            "cmd-k"
        } else {
            "ctrl-k"
        };
        assert!(
            actions_for(
                key,
                &["Workspace", "PathInput", "SearchInput"],
                &BTreeMap::new()
            )
            .iter()
            .any(|a| a.ends_with("FocusSearch"))
        );
    }
    #[test]
    fn remapping_removes_old_binding_and_aliases() {
        let overrides = BTreeMap::from([("search".into(), "ctrl-alt-s".into())]);
        assert!(
            actions_for("ctrl-alt-s", &["Workspace"], &overrides)
                .iter()
                .any(|a| a.ends_with("FocusSearch"))
        );
        assert!(
            !actions_for("/", &["Workspace"], &overrides)
                .iter()
                .any(|a| a.ends_with("FocusSearch"))
        );
    }
    #[test]
    fn settings_recording_and_text_entry_do_not_trigger_file_shortcuts() {
        for context in [
            vec!["Workspace", "Settings"],
            vec!["Workspace", "SearchInput"],
        ] {
            assert!(actions_for("v", &context, &BTreeMap::new()).is_empty());
        }
    }
    #[test]
    fn duplicate_and_reserved_shortcuts_are_rejected() {
        assert!(
            validate("search", "v", &BTreeMap::new())
                .unwrap_err()
                .contains("Switch list")
        );
        assert!(validate("search", "escape", &BTreeMap::new()).is_err());
        assert!(validate("search", "ctrl-k ctrl-l", &BTreeMap::new()).is_err());
    }
    #[test]
    fn invalid_persisted_shortcuts_do_not_panic() {
        assert!(
            !bindings(&BTreeMap::from([(
                "search".into(),
                "bad-key-sequence".into()
            )]))
            .is_empty()
        );
    }
}

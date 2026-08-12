//! Keycap chips and the app's shortcut catalog.
//!
//! [`keycap`] is the small rounded "keyboard key" badge that advertises a
//! shortcut — the `/` pinned in the search box, and every cap in the
//! shortcuts overlay. [`catalog`] is the single, platform-aware list of
//! documented shortcuts the overlay renders; it is deliberately a plain
//! data structure (no GPUI) so it can be unit-tested and can't drift out
//! of sync with what the overlay shows.

use gpui::{Div, SharedString, div, prelude::*, px};

use super::theme::Theme;

/// One keycap: a single short label ("/", "⌘", "R", "Esc") in a rounded,
/// bordered box styled from the palette so it reads on light and dark.
pub fn keycap(theme: &Theme, label: impl Into<SharedString>) -> Div {
    div()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .h(px(18.))
        .min_w(px(18.))
        .px(px(5.))
        .rounded(px(5.))
        .border_1()
        .border_color(theme.border)
        .bg(theme.bg)
        .text_xs()
        .text_color(theme.text_dim)
        .child(label.into())
}

/// A row of keycaps for a chord (`⌘ R`, `Ctrl ⇧ Tab`): one cap per token.
pub fn keys_row(theme: &Theme, keys: &[SharedString]) -> Div {
    let mut row = div().flex().flex_none().items_center().gap_1();
    for key in keys {
        row = row.child(keycap(theme, key.clone()));
    }
    row
}

/// One documented shortcut: what it does and its keys, already split into
/// per-cap tokens for this platform.
pub struct Shortcut {
    pub action: SharedString,
    pub keys: Vec<SharedString>,
}

/// A titled group of related shortcuts (one section in the overlay).
pub struct ShortcutGroup {
    pub title: &'static str,
    pub shortcuts: Vec<Shortcut>,
}

/// The primary modifier cap: Command on macOS, Ctrl elsewhere.
fn m() -> SharedString {
    if cfg!(target_os = "macos") {
        "⌘".into()
    } else {
        "Ctrl".into()
    }
}

fn sc(action: &'static str, keys: Vec<SharedString>) -> Shortcut {
    Shortcut {
        action: action.into(),
        keys,
    }
}

/// Every shortcut the app binds, grouped for display. Kept in lockstep
/// with the bindings in `workspace::run` and `search_input::bind_keys`.
pub fn catalog() -> Vec<ShortcutGroup> {
    // The nav and delete keys diverge by platform (Cmd vs. Alt for
    // history, Cmd-Backspace vs. Ctrl-Delete for trash).
    let (up, back, fwd) = if cfg!(target_os = "macos") {
        (
            vec![m(), "↑".into()],
            vec![m(), "[".into()],
            vec![m(), "]".into()],
        )
    } else {
        (
            vec!["Alt".into(), "↑".into()],
            vec!["Alt".into(), "←".into()],
            vec!["Alt".into(), "→".into()],
        )
    };
    let del = if cfg!(target_os = "macos") {
        vec![m(), "⌫".into()]
    } else {
        vec!["Ctrl".into(), "Del".into()]
    };

    vec![
        ShortcutGroup {
            title: "Search",
            shortcuts: vec![
                sc("Focus search", vec!["/".into()]),
                sc("Clear / leave search", vec!["Esc".into()]),
            ],
        },
        ShortcutGroup {
            title: "Navigation",
            shortcuts: vec![
                sc("Go up a folder", up),
                sc("Back", back),
                sc("Forward", fwd),
                sc("Refresh", vec![m(), "R".into()]),
            ],
        },
        ShortcutGroup {
            title: "Selection & files",
            shortcuts: vec![
                sc("Move selection", vec!["↑".into(), "↓".into()]),
                sc("Extend selection", vec!["⇧".into(), "↑".into()]),
                sc("Open", vec!["↵".into()]),
                sc("Select all", vec![m(), "A".into()]),
                sc("Copy", vec![m(), "C".into()]),
                sc("Cut", vec![m(), "X".into()]),
                sc("Paste", vec![m(), "V".into()]),
                sc("Rename", vec!["F2".into()]),
                sc("Move to trash", del),
                sc("Undo", vec![m(), "Z".into()]),
            ],
        },
        ShortcutGroup {
            title: "View",
            shortcuts: vec![
                sc("List / grid", vec!["V".into()]),
                sc("Toggle preview", vec![m(), "I".into()]),
                sc("Settings", vec![m(), ",".into()]),
            ],
        },
        ShortcutGroup {
            title: "Tabs",
            shortcuts: vec![
                sc("New tab", vec![m(), "T".into()]),
                sc("Close tab", vec![m(), "W".into()]),
                sc("Next tab", vec!["Ctrl".into(), "Tab".into()]),
                sc("Previous tab", vec!["Ctrl".into(), "⇧".into(), "Tab".into()]),
            ],
        },
        ShortcutGroup {
            title: "Help",
            shortcuts: vec![sc("Keyboard shortcuts", vec!["?".into()])],
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::catalog;

    #[test]
    fn catalog_entries_are_well_formed() {
        let groups = catalog();
        assert!(!groups.is_empty());
        for group in &groups {
            assert!(!group.shortcuts.is_empty(), "{} is empty", group.title);
            for sc in &group.shortcuts {
                assert!(!sc.action.is_empty());
                assert!(!sc.keys.is_empty(), "{} has no keys", sc.action);
                assert!(sc.keys.iter().all(|k| !k.is_empty()));
            }
        }
    }
}

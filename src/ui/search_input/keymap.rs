//! Text-editing key bindings for the `SearchInput` key context.

use super::*;

/// Key bindings for the input's key context. Call once at app startup.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys(key_bindings());
}

/// Standard text editing bindings shared by all editable fields.
pub fn key_bindings() -> Vec<gpui::KeyBinding> {
    const CTX: Option<&str> = Some("SearchInput");
    vec![
        gpui::KeyBinding::new("backspace", Backspace, CTX),
        gpui::KeyBinding::new("delete", Delete, CTX),
        gpui::KeyBinding::new(
            "escape",
            ClearInput,
            Some("SearchInput && !PathInput && !Settings"),
        ),
        gpui::KeyBinding::new("left", Left, CTX),
        gpui::KeyBinding::new("right", Right, CTX),
        gpui::KeyBinding::new("shift-left", SelectLeft, CTX),
        gpui::KeyBinding::new("shift-right", SelectRight, CTX),
        gpui::KeyBinding::new("home", Home, CTX),
        gpui::KeyBinding::new("end", End, CTX),
    ]
    .into_iter()
    .chain(platform_key_bindings(CTX))
    .collect()
}

/// Word- and line-granular editing plus clipboard. macOS uses Option for
/// word and Cmd for line/clipboard; elsewhere Ctrl covers both.
#[cfg(target_os = "macos")]
fn platform_key_bindings(ctx: Option<&str>) -> Vec<gpui::KeyBinding> {
    vec![
        gpui::KeyBinding::new("alt-backspace", DeleteToPreviousWord, ctx),
        gpui::KeyBinding::new("alt-delete", DeleteToNextWord, ctx),
        gpui::KeyBinding::new("cmd-backspace", DeleteToBeginningOfLine, ctx),
        gpui::KeyBinding::new("alt-left", WordLeft, ctx),
        gpui::KeyBinding::new("alt-right", WordRight, ctx),
        gpui::KeyBinding::new("alt-shift-left", SelectWordLeft, ctx),
        gpui::KeyBinding::new("alt-shift-right", SelectWordRight, ctx),
        gpui::KeyBinding::new("cmd-left", Home, ctx),
        gpui::KeyBinding::new("cmd-right", End, ctx),
        gpui::KeyBinding::new("cmd-a", SelectAll, ctx),
        gpui::KeyBinding::new("cmd-v", Paste, ctx),
        gpui::KeyBinding::new("cmd-c", Copy, ctx),
        gpui::KeyBinding::new("cmd-x", Cut, ctx),
        gpui::KeyBinding::new("ctrl-cmd-space", ShowCharacterPalette, ctx),
    ]
}

/// See the macOS variant.
#[cfg(not(target_os = "macos"))]
fn platform_key_bindings(ctx: Option<&str>) -> Vec<gpui::KeyBinding> {
    vec![
        gpui::KeyBinding::new("ctrl-backspace", DeleteToPreviousWord, ctx),
        gpui::KeyBinding::new("ctrl-left", WordLeft, ctx),
        gpui::KeyBinding::new("ctrl-right", WordRight, ctx),
        gpui::KeyBinding::new("ctrl-shift-left", SelectWordLeft, ctx),
        gpui::KeyBinding::new("ctrl-shift-right", SelectWordRight, ctx),
        gpui::KeyBinding::new("ctrl-a", SelectAll, ctx),
        gpui::KeyBinding::new("ctrl-v", Paste, ctx),
        gpui::KeyBinding::new("ctrl-c", Copy, ctx),
        gpui::KeyBinding::new("ctrl-x", Cut, ctx),
    ]
}

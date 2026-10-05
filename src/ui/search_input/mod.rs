//! Single-line search input with real cursor, selection, clipboard, and
//! IME support (dead keys, CJK composition) via `EntityInputHandler`.
//!
//! Adapted from gpui's canonical `input.rs` example — the custom element
//! (layout/paint/hit-testing) and the UTF-16 bridging are the example's
//! design; filex adds change events, palette styling, and the
//! backspace-when-empty navigation hook.

use std::ops::Range;

use gpui::{
    App, Bounds, ClipboardItem, Context, CursorStyle, ElementId, ElementInputHandler, Entity,
    EntityInputHandler, EventEmitter, FocusHandle, Focusable, GlobalElementId, LayoutId,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point,
    ShapedLine, SharedString, Style, TextRun, UTF16Selection, UnderlineStyle, Window, actions, div,
    fill, point, prelude::*, px, relative, size,
};
use unicode_segmentation::UnicodeSegmentation as _;

mod element;
pub mod keymap;

use element::TextElement;

actions!(
    search_input,
    [
        Backspace,
        Delete,
        DeleteToPreviousWord,
        DeleteToNextWord,
        DeleteToBeginningOfLine,
        Left,
        Right,
        WordLeft,
        WordRight,
        SelectLeft,
        SelectRight,
        SelectWordLeft,
        SelectWordRight,
        SelectAll,
        Home,
        End,
        ShowCharacterPalette,
        Paste,
        Cut,
        Copy,
        ClearInput,
    ]
);

use super::theme::ActiveTheme as _;

pub enum SearchInputEvent {
    /// The text changed (typing, paste, cut, IME commit, clear).
    Changed(String),
    /// Backspace pressed while empty — the workspace navigates up.
    BackspaceWhenEmpty,
    /// Escape pressed (the content is also cleared). Consumers that
    /// use the input as a transient editor (rename-in-place) treat
    /// this as cancel; the search box ignores it.
    Dismissed,
}

pub struct SearchInput {
    focus_handle: FocusHandle,
    propagate_empty: bool,
    content: SharedString,
    placeholder: SharedString,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    last_layout: Option<ShapedLine>,
    last_bounds: Option<Bounds<Pixels>>,
    is_selecting: bool,
    /// Horizontal scroll so the caret stays visible in a fixed-width box
    /// once the text is longer than it. Persisted between frames so the
    /// view doesn't jump; recomputed each prepaint from the caret.
    scroll_offset: Pixels,
}

impl EventEmitter<SearchInputEvent> for SearchInput {}

impl SearchInput {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            propagate_empty: true,
            content: "".into(),
            placeholder: "type to search".into(),
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            last_layout: None,
            last_bounds: None,
            is_selecting: false,
            scroll_offset: px(0.),
        }
    }

    /// Ordinary text fields must not turn empty clipboard actions into file operations.
    pub fn set_propagate_empty(&mut self, enabled: bool) {
        self.propagate_empty = enabled;
    }

    pub fn is_empty(&self) -> bool {
        self.content.is_empty()
    }

    /// Current content. Used by non-search consumers (rename-in-place)
    /// that read the value on commit instead of subscribing to Changed.
    pub fn text(&self) -> &str {
        &self.content
    }

    pub fn set_placeholder(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.placeholder = text.into();
        cx.notify();
    }

    /// Select the whole content, e.g. so a prefilled rename replaces on
    /// first keystroke. (The action-handler `select_all` needs a window;
    /// this programmatic variant doesn't.)
    pub fn select_all_text(&mut self, cx: &mut Context<Self>) {
        self.selected_range = 0..self.content.len();
        self.selection_reversed = false;
        cx.notify();
    }

    /// Replace the whole content programmatically (e.g. clearing after a
    /// result is activated). Emits Changed like any edit.
    pub fn set_text(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.content = text.into();
        let end = self.content.len();
        self.selected_range = end..end;
        self.selection_reversed = false;
        self.marked_range = None;
        self.emit_changed(cx);
        cx.notify();
    }

    fn emit_changed(&mut self, cx: &mut Context<Self>) {
        cx.emit(SearchInputEvent::Changed(self.content.to_string()));
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.content.is_empty() {
            if self.propagate_empty {
                cx.emit(SearchInputEvent::BackspaceWhenEmpty);
            }
            return;
        }
        if self.selected_range.is_empty() {
            self.select_to(self.previous_boundary(self.cursor_offset()), cx)
        }
        self.replace_text_in_range(None, "", window, cx)
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.select_to(self.next_boundary(self.cursor_offset()), cx)
        }
        self.replace_text_in_range(None, "", window, cx)
    }

    /// Alt/Ctrl-Backspace: delete the whitespace-delimited word before
    /// the cursor (or the current selection, if any).
    fn delete_to_previous_word(
        &mut self,
        _: &DeleteToPreviousWord,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.content.is_empty() {
            if self.propagate_empty {
                cx.emit(SearchInputEvent::BackspaceWhenEmpty);
            }
            return;
        }
        if self.selected_range.is_empty() {
            self.select_to(self.previous_word_boundary(self.cursor_offset()), cx);
        }
        self.replace_text_in_range(None, "", window, cx)
    }

    /// Alt/Fn-Delete: delete the word after the cursor.
    fn delete_to_next_word(
        &mut self,
        _: &DeleteToNextWord,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selected_range.is_empty() {
            self.select_to(self.next_word_boundary(self.cursor_offset()), cx);
        }
        self.replace_text_in_range(None, "", window, cx)
    }

    /// Cmd-Backspace: delete from the cursor back to the start of the
    /// line. While empty it bubbles, so the workspace's Finder-style
    /// "delete selected file" binding on the same key still fires.
    fn delete_to_beginning_of_line(
        &mut self,
        _: &DeleteToBeginningOfLine,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.content.is_empty() && self.propagate_empty {
            cx.propagate();
            return;
        }
        if self.selected_range.is_empty() {
            self.select_to(0, cx);
        }
        self.replace_text_in_range(None, "", window, cx)
    }

    fn word_left(&mut self, _: &WordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.previous_word_boundary(self.cursor_offset()), cx);
    }

    fn word_right(&mut self, _: &WordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.next_word_boundary(self.cursor_offset()), cx);
    }

    fn select_word_left(&mut self, _: &SelectWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_word_boundary(self.cursor_offset()), cx);
    }

    fn select_word_right(&mut self, _: &SelectWordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_word_boundary(self.cursor_offset()), cx);
    }

    fn clear(&mut self, _: &ClearInput, _: &mut Window, cx: &mut Context<Self>) {
        if self.content.is_empty() {
            // Nothing to clear: let escape bubble (the workspace closes
            // modals/panes with it), while still telling transient-
            // editor consumers to dismiss.
            cx.propagate();
            cx.emit(SearchInputEvent::Dismissed);
            return;
        }
        self.set_text("", cx);
        cx.emit(SearchInputEvent::Dismissed);
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.previous_boundary(self.cursor_offset()), cx);
        } else {
            self.move_to(self.selected_range.start, cx)
        }
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.next_boundary(self.selected_range.end), cx);
        } else {
            self.move_to(self.selected_range.end, cx)
        }
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_boundary(self.cursor_offset()), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_boundary(self.cursor_offset()), cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        // Empty input: bubble to the workspace so cmd-a selects all rows
        // (mirrors the clipboard keys).
        if self.content.is_empty() && self.propagate_empty {
            cx.propagate();
            return;
        }
        self.move_to(0, cx);
        self.select_to(self.content.len(), cx)
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.content.len(), cx);
    }

    fn show_character_palette(
        &mut self,
        _: &ShowCharacterPalette,
        window: &mut Window,
        _: &mut Context<Self>,
    ) {
        window.show_character_palette();
    }

    // Clipboard actions bubble to the workspace while the input is
    // empty: with no text to operate on, cmd-c/x/v become file
    // operations on the selected row (the workspace's paste falls back
    // to inserting clipboard text here, so paste-to-search still
    // works).

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if self.content.is_empty() && self.propagate_empty {
            cx.propagate();
            return;
        }
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.replace_text_in_range(None, &text.replace('\n', " "), window, cx);
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if self.content.is_empty() && self.propagate_empty {
            cx.propagate();
            return;
        }
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if self.content.is_empty() && self.propagate_empty {
            cx.propagate();
            return;
        }
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
            self.replace_text_in_range(None, "", window, cx)
        }
    }

    fn on_mouse_down(&mut self, event: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.is_selecting = true;
        if event.modifiers.shift {
            self.select_to(self.index_for_mouse_position(event.position), cx);
        } else {
            self.move_to(self.index_for_mouse_position(event.position), cx)
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_selecting {
            self.select_to(self.index_for_mouse_position(event.position), cx);
        }
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.selected_range = offset..offset;
        cx.notify()
    }

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        if self.content.is_empty() {
            return 0;
        }
        let (Some(bounds), Some(line)) = (self.last_bounds.as_ref(), self.last_layout.as_ref())
        else {
            return 0;
        };
        if position.y < bounds.top() {
            return 0;
        }
        if position.y > bounds.bottom() {
            return self.content.len();
        }
        // Undo the horizontal scroll so the click maps to the character
        // actually under the cursor.
        line.closest_index_for_x(position.x - bounds.left() + self.scroll_offset)
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        if self.selection_reversed {
            self.selected_range.start = offset
        } else {
            self.selected_range.end = offset
        };
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        cx.notify()
    }

    fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf8_offset = 0;
        let mut utf16_count = 0;
        for ch in self.content.chars() {
            if utf16_count >= offset {
                break;
            }
            utf16_count += ch.len_utf16();
            utf8_offset += ch.len_utf8();
        }
        utf8_offset
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut utf16_offset = 0;
        let mut utf8_count = 0;
        for ch in self.content.chars() {
            if utf8_count >= offset {
                break;
            }
            utf8_count += ch.len_utf8();
            utf16_offset += ch.len_utf16();
        }
        utf16_offset
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range_utf16: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range_utf16.start)..self.offset_from_utf16(range_utf16.end)
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .rev()
            .find_map(|(idx, _)| (idx < offset).then_some(idx))
            .unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .find_map(|(idx, _)| (idx > offset).then_some(idx))
            .unwrap_or(self.content.len())
    }

    /// Byte offset one word to the left of `offset` in the content.
    fn previous_word_boundary(&self, offset: usize) -> usize {
        prev_word_boundary(&self.content, offset)
    }

    /// Byte offset one word to the right of `offset` in the content.
    fn next_word_boundary(&self, offset: usize) -> usize {
        next_word_boundary(&self.content, offset)
    }
}

/// Byte offset one word to the left of `offset`: skip any whitespace
/// immediately before the cursor, then the run of non-whitespace before
/// that. Whitespace-delimited to stay predictable on the short queries
/// this input sees. `offset` must fall on a char boundary.
fn prev_word_boundary(text: &str, offset: usize) -> usize {
    let mut idx = offset;
    let mut chars = text[..offset].char_indices().rev().peekable();
    while let Some(&(i, c)) = chars.peek() {
        if c.is_whitespace() {
            idx = i;
            chars.next();
        } else {
            break;
        }
    }
    while let Some(&(i, c)) = chars.peek() {
        if c.is_whitespace() {
            break;
        }
        idx = i;
        chars.next();
    }
    idx
}

/// Byte offset one word to the right of `offset`: skip leading
/// whitespace, then the following run of non-whitespace. `offset` must
/// fall on a char boundary.
fn next_word_boundary(text: &str, offset: usize) -> usize {
    let mut idx = offset;
    let mut chars = text[offset..].char_indices().peekable();
    while let Some(&(i, c)) = chars.peek() {
        if c.is_whitespace() {
            idx = offset + i + c.len_utf8();
            chars.next();
        } else {
            break;
        }
    }
    while let Some(&(i, c)) = chars.peek() {
        if c.is_whitespace() {
            break;
        }
        idx = offset + i + c.len_utf8();
        chars.next();
    }
    idx
}

impl EntityInputHandler for SearchInput {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.content[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range),
            reversed: self.selection_reversed,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked_range
            .as_ref()
            .map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());

        self.content =
            (self.content[0..range.start].to_owned() + new_text + &self.content[range.end..])
                .into();
        self.selected_range = range.start + new_text.len()..range.start + new_text.len();
        self.marked_range.take();
        self.emit_changed(cx);
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());

        self.content =
            (self.content[0..range.start].to_owned() + new_text + &self.content[range.end..])
                .into();
        if !new_text.is_empty() {
            self.marked_range = Some(range.start..range.start + new_text.len());
        } else {
            self.marked_range = None;
        }
        self.selected_range = new_selected_range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .map(|new_range| new_range.start + range.start..new_range.end + range.end)
            .unwrap_or_else(|| range.start + new_text.len()..range.start + new_text.len());
        // Composition in progress still filters results live.
        self.emit_changed(cx);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let last_layout = self.last_layout.as_ref()?;
        let range = self.range_from_utf16(&range_utf16);
        Some(Bounds::from_corners(
            point(
                bounds.left() + last_layout.x_for_index(range.start),
                bounds.top(),
            ),
            point(
                bounds.left() + last_layout.x_for_index(range.end),
                bounds.bottom(),
            ),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: gpui::Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        let line_point = self.last_bounds?.localize(&point)?;
        let last_layout = self.last_layout.as_ref()?;
        // When empty, the shaped line holds the placeholder, not content.
        if last_layout.text != self.content {
            return None;
        }
        let utf8_index = last_layout.index_for_x(point.x - line_point.x)?;
        Some(self.offset_to_utf16(utf8_index))
    }
}

impl Render for SearchInput {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .key_context("SearchInput")
            .track_focus(&self.focus_handle(cx))
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::delete_to_previous_word))
            .on_action(cx.listener(Self::delete_to_next_word))
            .on_action(cx.listener(Self::delete_to_beginning_of_line))
            .on_action(cx.listener(Self::clear))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::word_left))
            .on_action(cx.listener(Self::word_right))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_word_left))
            .on_action(cx.listener(Self::select_word_right))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::show_character_palette))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::copy))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .w_full()
            .child(TextElement { input: cx.entity() })
    }
}

impl Focusable for SearchInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::{next_word_boundary, prev_word_boundary};

    #[test]
    fn prev_word_deletes_trailing_word_then_leading() {
        // "foo bar|" -> "foo |" -> "|"
        assert_eq!(prev_word_boundary("foo bar", 7), 4);
        assert_eq!(prev_word_boundary("foo ", 4), 0);
    }

    #[test]
    fn prev_word_skips_run_of_whitespace() {
        // Multiple spaces before the word collapse in one hop.
        assert_eq!(prev_word_boundary("foo   bar", 9), 6);
        assert_eq!(prev_word_boundary("foo   ", 6), 0);
    }

    #[test]
    fn prev_word_at_start_is_a_noop() {
        assert_eq!(prev_word_boundary("foo", 0), 0);
        assert_eq!(prev_word_boundary("", 0), 0);
    }

    #[test]
    fn prev_word_from_mid_word() {
        // Cursor inside "bar" (after 'b') deletes back to the space.
        assert_eq!(prev_word_boundary("foo bar", 5), 4);
    }

    #[test]
    fn next_word_skips_whitespace_then_word() {
        assert_eq!(next_word_boundary("foo bar", 0), 3);
        assert_eq!(next_word_boundary("foo bar", 3), 7);
        assert_eq!(next_word_boundary("foo   bar", 3), 9);
    }

    #[test]
    fn next_word_at_end_is_a_noop() {
        assert_eq!(next_word_boundary("foo", 3), 3);
        assert_eq!(next_word_boundary("", 0), 0);
    }

    #[test]
    fn word_boundaries_respect_multibyte_chars() {
        // "café x" — 'é' is two bytes, so "café" spans bytes 0..5.
        let s = "café x";
        assert_eq!(next_word_boundary(s, 0), 5);
        assert_eq!(prev_word_boundary(s, 5), 0);
        // From the end, delete just the word "x" (byte 6), leaving the
        // space intact — same rule as the ASCII cases above.
        assert_eq!(prev_word_boundary(s, s.len()), 6);
    }
}

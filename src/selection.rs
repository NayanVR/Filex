//! Multi-item list selection model.
//!
//! Index-based and list-agnostic: the workspace maps indices to browse
//! entries or search rows. A selection is a set of selected indices plus
//! two cursors — the **anchor** (the fixed end of a shift-range) and the
//! **lead** (the most recently touched item, and the origin of keyboard
//! movement). Kept in the lib, free of GPUI, so the gesture logic is
//! unit-tested in isolation (CLAUDE.md).
//!
//! Gesture mapping (wired in the workspace): plain click = [`select_one`],
//! cmd/ctrl-click = [`toggle`], shift-click = [`range_to`], cmd-a =
//! [`select_all`], arrow keys = [`move_lead`], shift-arrow =
//! [`extend_lead`].
//!
//! [`select_one`]: Selection::select_one
//! [`toggle`]: Selection::toggle
//! [`range_to`]: Selection::range_to
//! [`select_all`]: Selection::select_all
//! [`move_lead`]: Selection::move_lead
//! [`extend_lead`]: Selection::extend_lead

use std::collections::BTreeSet;

/// A set of selected list indices with an anchor and a lead cursor.
/// Indices are only meaningful against the list they were made in — the
/// workspace [`clear`](Selection::clear)s it whenever the active list
/// changes (navigation, a new search, a reload).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Selection {
    indices: BTreeSet<usize>,
    anchor: Option<usize>,
    lead: Option<usize>,
}

impl Selection {
    pub fn is_empty(&self) -> bool {
        self.indices.is_empty()
    }

    pub fn len(&self) -> usize {
        self.indices.len()
    }

    pub fn contains(&self, ix: usize) -> bool {
        self.indices.contains(&ix)
    }

    /// The lead cursor: the item keyboard movement starts from and the
    /// natural single target (activation, rename) when the caller wants
    /// "the current item" regardless of how many are selected.
    pub fn lead(&self) -> Option<usize> {
        self.lead
    }

    /// The selected indices, ascending.
    pub fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.indices.iter().copied()
    }

    /// Empty the selection and both cursors.
    pub fn clear(&mut self) {
        self.indices.clear();
        self.anchor = None;
        self.lead = None;
    }

    /// Plain click: select exactly `ix`, and make it both anchor and
    /// lead.
    pub fn select_one(&mut self, ix: usize) {
        self.indices.clear();
        self.indices.insert(ix);
        self.anchor = Some(ix);
        self.lead = Some(ix);
    }

    /// Cmd/ctrl-click: flip `ix` in or out of the set, and re-anchor
    /// there so a following shift-click ranges from it.
    pub fn toggle(&mut self, ix: usize) {
        if !self.indices.remove(&ix) {
            self.indices.insert(ix);
        }
        self.anchor = Some(ix);
        self.lead = Some(ix);
    }

    /// Shift-click: replace the set with the contiguous run from the
    /// anchor to `ix` (inclusive); the anchor stays put so the range can
    /// be resized by shift-clicking again. With no anchor yet, this is a
    /// plain [`select_one`](Self::select_one).
    pub fn range_to(&mut self, ix: usize) {
        let Some(anchor) = self.anchor else {
            self.select_one(ix);
            return;
        };
        let (lo, hi) = if anchor <= ix {
            (anchor, ix)
        } else {
            (ix, anchor)
        };
        self.indices = (lo..=hi).collect();
        self.lead = Some(ix);
    }

    /// Cmd-a: select every index in `0..len`. An empty list clears.
    pub fn select_all(&mut self, len: usize) {
        self.indices = (0..len).collect();
        self.anchor = if len == 0 { None } else { Some(0) };
        self.lead = len.checked_sub(1);
    }

    /// Arrow key: move the lead by `delta` (clamped into `0..len`) and
    /// select only it. Returns the new lead so the caller can scroll it
    /// into view. An empty list clears and returns `None`.
    pub fn move_lead(&mut self, delta: isize, len: usize) -> Option<usize> {
        if len == 0 {
            self.clear();
            return None;
        }
        let next = self.stepped(delta, len);
        self.select_one(next);
        Some(next)
    }

    /// Shift-arrow: move the lead by `delta` and extend the range from
    /// the anchor to the new lead (Finder-style range growth/shrink).
    /// Returns the new lead. An empty list clears and returns `None`.
    pub fn extend_lead(&mut self, delta: isize, len: usize) -> Option<usize> {
        if len == 0 {
            self.clear();
            return None;
        }
        if self.anchor.is_none() {
            return self.move_lead(delta, len);
        }
        let next = self.stepped(delta, len);
        self.range_to(next);
        Some(next)
    }

    /// The lead moved by `delta`, clamped to `0..len`. With no lead yet,
    /// a downward step starts at the top and an upward one at the bottom
    /// (matching the old single-selection behavior).
    fn stepped(&self, delta: isize, len: usize) -> usize {
        match self.lead {
            Some(ix) => ix.saturating_add_signed(delta).min(len - 1),
            None if delta > 0 => 0,
            None => len - 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selected(sel: &Selection) -> Vec<usize> {
        sel.iter().collect()
    }

    #[test]
    fn select_one_replaces_and_sets_cursors() {
        let mut sel = Selection::default();
        sel.select_one(3);
        assert_eq!(selected(&sel), [3]);
        sel.select_one(5);
        assert_eq!(selected(&sel), [5]);
        assert_eq!(sel.lead(), Some(5));
    }

    #[test]
    fn toggle_adds_and_removes() {
        let mut sel = Selection::default();
        sel.select_one(1);
        sel.toggle(3);
        sel.toggle(5);
        assert_eq!(selected(&sel), [1, 3, 5]);
        sel.toggle(3);
        assert_eq!(selected(&sel), [1, 5]);
        // The toggled item becomes the anchor/lead even when deselected,
        // so a following shift-click ranges from it (Finder behavior).
        assert_eq!(sel.lead(), Some(3));
    }

    #[test]
    fn range_to_spans_from_anchor_inclusive_both_directions() {
        let mut sel = Selection::default();
        sel.select_one(2); // anchor = 2
        sel.range_to(5);
        assert_eq!(selected(&sel), [2, 3, 4, 5]);
        // Anchor stays at 2, so a smaller range resizes rather than grows.
        sel.range_to(0);
        assert_eq!(selected(&sel), [0, 1, 2]);
    }

    #[test]
    fn range_to_without_anchor_is_single() {
        let mut sel = Selection::default();
        sel.range_to(4);
        assert_eq!(selected(&sel), [4]);
    }

    #[test]
    fn select_all_and_empty() {
        let mut sel = Selection::default();
        sel.select_all(4);
        assert_eq!(selected(&sel), [0, 1, 2, 3]);
        assert_eq!(sel.lead(), Some(3));
        sel.select_all(0);
        assert!(sel.is_empty());
        assert_eq!(sel.lead(), None);
    }

    #[test]
    fn move_lead_clamps_and_single_selects() {
        let mut sel = Selection::default();
        assert_eq!(sel.move_lead(1, 3), Some(0)); // no lead + down => top
        assert_eq!(sel.move_lead(1, 3), Some(1));
        assert_eq!(sel.move_lead(5, 3), Some(2)); // clamp to len-1
        assert_eq!(selected(&sel), [2]);
        assert_eq!(sel.move_lead(-9, 3), Some(0)); // clamp to 0
    }

    #[test]
    fn move_lead_on_empty_clears() {
        let mut sel = Selection::default();
        sel.select_one(0);
        assert_eq!(sel.move_lead(1, 0), None);
        assert!(sel.is_empty());
    }

    #[test]
    fn extend_lead_grows_range_from_anchor() {
        let mut sel = Selection::default();
        sel.select_one(1); // anchor = lead = 1
        assert_eq!(sel.extend_lead(1, 5), Some(2));
        assert_eq!(selected(&sel), [1, 2]);
        assert_eq!(sel.extend_lead(1, 5), Some(3));
        assert_eq!(selected(&sel), [1, 2, 3]);
        // Reversing past the anchor flips the range.
        assert_eq!(sel.extend_lead(-9, 5), Some(0));
        assert_eq!(selected(&sel), [0, 1]);
    }

    #[test]
    fn clear_resets_everything() {
        let mut sel = Selection::default();
        sel.select_all(3);
        sel.clear();
        assert!(sel.is_empty());
        assert_eq!(sel.lead(), None);
        // With the anchor gone, the next range is a single select.
        sel.range_to(2);
        assert_eq!(selected(&sel), [2]);
    }
}

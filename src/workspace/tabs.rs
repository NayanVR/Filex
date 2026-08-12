//! Tab lifecycle: snapshot/restore of per-tab state and open/close/switch.

use super::*;

impl Workspace {
    /// Save the live browse state into the active tab's slot.
    pub(super) fn snapshot_active(&mut self) -> TabSnapshot {
        TabSnapshot {
            cwd: std::mem::take(&mut self.cwd),
            entries: std::mem::take(&mut self.entries),
            load_error: self.load_error.take(),
            selection: std::mem::take(&mut self.selection),
            scroll: std::mem::replace(&mut self.browse_scroll, UniformListScrollHandle::new()),
            history_back: std::mem::take(&mut self.history_back),
            history_forward: std::mem::take(&mut self.history_forward),
        }
    }

    /// Load a saved tab into the live browse fields. Transient per-tab UI
    /// (rename, armed delete) does not survive the switch.
    pub(super) fn restore_tab(&mut self, snap: TabSnapshot) {
        self.cwd = snap.cwd;
        self.entries = snap.entries;
        self.load_error = snap.load_error;
        self.selection = snap.selection;
        self.browse_scroll = snap.scroll;
        self.history_back = snap.history_back;
        self.history_forward = snap.history_forward;
        self.renaming = None;
        self.pending_delete = None;
    }

    /// Snapshot the current tab and restore tab `i`. Returns whether the
    /// switch happened (`false` = already active or out of range). Leaves
    /// the MRU order untouched: a click commits the new tab to the front,
    /// while a Ctrl-Tab cycle keeps the order frozen.
    fn set_active_tab(&mut self, i: usize, cx: &mut Context<Self>) -> bool {
        if i == self.active_tab || i >= self.tabs.len() {
            return false;
        }
        self.tabs[self.active_tab] = self.snapshot_active();
        let target = std::mem::replace(&mut self.tabs[i], TabSnapshot::placeholder());
        self.restore_tab(target);
        self.active_tab = i;
        self.refresh_preview(cx);
        cx.notify();
        true
    }

    /// End any Ctrl-Tab cycle and move the active tab to the front of the
    /// MRU order — the "user settled here" signal.
    fn commit_mru(&mut self) {
        self.tab_cycle = None;
        self.tab_mru.retain(|&t| t != self.active_tab);
        self.tab_mru.insert(0, self.active_tab);
    }

    /// Switch to tab `i` from a click, committing it as most-recently-used.
    pub(super) fn activate_tab(&mut self, i: usize, cx: &mut Context<Self>) {
        if self.set_active_tab(i, cx) {
            self.commit_mru();
        }
    }

    /// Switch to the next (`delta > 0`) or previous tab in
    /// most-recently-used order. Consecutive presses keep walking the
    /// frozen `tab_mru` order; it only refreshes once the user settles on
    /// a tab (opens, closes, or clicks one), so Ctrl-Tab reads like an app
    /// switcher rather than reshuffling after every hop.
    pub(super) fn cycle_tab(&mut self, delta: isize, cx: &mut Context<Self>) {
        let n = self.tab_mru.len();
        if n <= 1 {
            return;
        }
        let start = self.tab_cycle.unwrap_or_else(|| {
            self.tab_mru
                .iter()
                .position(|&t| t == self.active_tab)
                .unwrap_or(0)
        });
        let pos = ring_step(start, delta, n);
        let target = self.tab_mru[pos];
        self.set_active_tab(target, cx);
        self.tab_cycle = Some(pos);
    }

    /// Open a new tab at `path` and make it active.
    pub(super) fn open_tab(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.tabs[self.active_tab] = self.snapshot_active();
        self.tabs.push(TabSnapshot::placeholder());
        self.active_tab = self.tabs.len() - 1;
        self.restore_tab(TabSnapshot::placeholder());
        self.load_dir(&path, cx);
        self.refresh_preview(cx);
        // The new index is fresh; committing puts it at the MRU front.
        self.commit_mru();
        cx.notify();
    }

    /// Close tab `i`, activating a neighbor if it was the active one.
    /// Closing the last tab is handled by the caller (it closes the
    /// window) — this assumes more than one tab remains.
    pub(super) fn close_tab(&mut self, i: usize, cx: &mut Context<Self>) {
        if self.tabs.len() <= 1 || i >= self.tabs.len() {
            return;
        }
        if i == self.active_tab {
            self.tabs.remove(i);
            let new_active = i.min(self.tabs.len() - 1);
            let target = std::mem::replace(&mut self.tabs[new_active], TabSnapshot::placeholder());
            self.restore_tab(target);
            self.active_tab = new_active;
            self.refresh_preview(cx);
        } else {
            self.tabs.remove(i);
            if i < self.active_tab {
                self.active_tab -= 1;
            }
        }
        // Drop the closed index from the MRU (rewriting the higher indices
        // that `Vec::remove` shifted down), then re-seat the active tab at
        // the front.
        mru_remove_index(&mut self.tab_mru, i);
        self.commit_mru();
        cx.notify();
    }

    /// The folder name shown on tab `i` (the drive/root shows as "/").
    pub(super) fn tab_title(&self, i: usize) -> SharedString {
        let cwd = if i == self.active_tab {
            &self.cwd
        } else {
            &self.tabs[i].cwd
        };
        cwd.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| cwd.display().to_string())
            .into()
    }
}

/// Remove a closed tab's index from an MRU list, shifting every higher
/// index down by one to match `Vec::remove` on the tab list.
fn mru_remove_index(mru: &mut Vec<usize>, closed: usize) {
    mru.retain(|&t| t != closed);
    for t in mru.iter_mut() {
        if *t > closed {
            *t -= 1;
        }
    }
}

/// The position `delta` steps around a ring of `len` items from `start`
/// (`rem_euclid` so a negative delta wraps to the end). `len` must be > 0.
fn ring_step(start: usize, delta: isize, len: usize) -> usize {
    (start as isize + delta).rem_euclid(len as isize) as usize
}

#[cfg(test)]
mod tests {
    use super::{mru_remove_index, ring_step};

    #[test]
    fn ring_step_wraps_both_ways() {
        assert_eq!(ring_step(0, 1, 3), 1);
        assert_eq!(ring_step(2, 1, 3), 0); // forward past the end wraps
        assert_eq!(ring_step(0, -1, 3), 2); // backward past the start wraps
        assert_eq!(ring_step(1, -1, 3), 0);
    }

    #[test]
    fn mru_remove_shifts_higher_indices_down() {
        // Closing tab 1 drops it and renumbers 2->1, 3->2 to match the
        // post-remove tab list; lower indices are untouched.
        let mut mru = vec![3, 0, 1, 2];
        mru_remove_index(&mut mru, 1);
        assert_eq!(mru, vec![2, 0, 1]);
    }

    #[test]
    fn mru_remove_of_absent_index_still_shifts() {
        let mut mru = vec![2, 0];
        mru_remove_index(&mut mru, 1);
        assert_eq!(mru, vec![1, 0]);
    }
}

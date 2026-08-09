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

    /// Switch to tab `i` (no-op if it's already active or out of range).
    pub(super) fn activate_tab(&mut self, i: usize, cx: &mut Context<Self>) {
        if i == self.active_tab || i >= self.tabs.len() {
            return;
        }
        self.tabs[self.active_tab] = self.snapshot_active();
        let target = std::mem::replace(&mut self.tabs[i], TabSnapshot::placeholder());
        self.restore_tab(target);
        self.active_tab = i;
        self.refresh_preview(cx);
        cx.notify();
    }

    /// Open a new tab at `path` and make it active.
    pub(super) fn open_tab(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.tabs[self.active_tab] = self.snapshot_active();
        self.tabs.push(TabSnapshot::placeholder());
        self.active_tab = self.tabs.len() - 1;
        self.restore_tab(TabSnapshot::placeholder());
        self.load_dir(&path, cx);
        self.refresh_preview(cx);
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

//! Auto-refresh of the browse view when other apps change the open
//! folder. The watcher thread lives in `filex::listing::watch`; this wires
//! its events through a debounce into a quiet reload of the active tab.

use super::*;
use filex::listing::watch::{Debounce, FolderWatch};
use futures::StreamExt as _;
use std::time::{Duration, Instant};

/// How often a deferred refresh re-checks whether it's still blocked
/// (settings closed, rename finished). Only runs while one is pending.
const DEFERRED_POLL: Duration = Duration::from_millis(300);

/// What a due refresh should do.
#[derive(Debug, PartialEq, Eq)]
enum Plan {
    /// Nothing to reload: a load that began after the change already shows
    /// it, or the folder on screen is not the watched one (a navigation is
    /// in flight and will read fresh rows anyway).
    Covered,
    /// Something on screen would be disturbed by a reload; try again later.
    Defer,
    Reload,
}

/// What the watcher task does after asking for a refresh.
enum Step {
    Done,
    Wait,
    Loading(gpui::Task<()>),
}

fn plan(on_screen: bool, loaded_since: bool, blocked: bool) -> Plan {
    if !on_screen || loaded_since {
        Plan::Covered
    } else if blocked {
        Plan::Defer
    } else {
        Plan::Reload
    }
}

impl Workspace {
    /// Start the folder watcher and the task that turns its events into
    /// reloads. A watcher that can't start leaves auto-refresh off.
    pub(super) fn spawn_folder_watch(&mut self, cx: &mut Context<Self>) {
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<()>();
        match FolderWatch::new(move || {
            let _ = tx.unbounded_send(());
        }) {
            Ok(watch) => self.folder_watch = Some(watch),
            Err(err) => {
                tracing::warn!("folder auto-refresh unavailable: {err:#}");
                return;
            }
        }
        cx.spawn(async move |this, cx| {
            let executor = cx.background_executor().clone();
            let mut debounce = Debounce::default();
            while rx.next().await.is_some() {
                debounce.event(Instant::now());
                while let Some(due) = debounce.due() {
                    let now = Instant::now();
                    if due > now {
                        executor.timer(due - now).await;
                        // Events that arrived meanwhile push the deadline.
                        while rx.try_recv().is_ok() {
                            debounce.event(Instant::now());
                        }
                        continue;
                    }
                    let Some(seen) = debounce.last_event() else {
                        break;
                    };
                    let Ok(step) = this.update(cx, |this, cx| this.auto_refresh(seen, cx)) else {
                        return; // workspace gone
                    };
                    match step {
                        Step::Done => debounce.cancel(),
                        Step::Wait => executor.timer(DEFERRED_POLL).await,
                        Step::Loading(load) => {
                            let start = Instant::now();
                            load.await;
                            debounce.reloaded(Instant::now(), start.elapsed());
                        }
                    }
                }
            }
        })
        .detach();
    }

    /// Point the watcher at the folder now on screen (none for This PC or
    /// before the first listing). Cheap: only queues the path.
    pub(super) fn sync_folder_watch(&mut self) {
        let target = Some(self.listed_dir.clone())
            .filter(|dir| !dir.as_os_str().is_empty() && !filex::drives::is_this_pc(dir));
        if target == self.watched_dir {
            return;
        }
        if let Some(watch) = &self.folder_watch {
            watch.retarget(target.clone());
        }
        self.watched_dir = target;
    }

    /// A reload would yank something the user is in the middle of: search
    /// results, a full-screen view, a rename editor, an armed delete, or a
    /// context menu (its Rename acts on a row index).
    fn refresh_blocked(&self) -> bool {
        !self.query.is_empty() || self.briefly_blocked()
    }

    /// The short-lived blockers, which have no close hook here and so are
    /// polled. A search isn't one: emptying the box calls
    /// [`refresh_if_stale`](Self::refresh_if_stale) itself, so a long
    /// search session costs no wakeups.
    fn briefly_blocked(&self) -> bool {
        self.settings_open
            || self.context_menu.is_some()
            || self.renaming.is_some()
            || self.pending_rename.is_some()
            || self.delete_prompt.is_some()
    }

    /// The watched folder changed (last seen at `seen`): reload it
    /// quietly, or mark it stale while something is in the way.
    fn auto_refresh(&mut self, seen: Instant, cx: &mut Context<Self>) -> Step {
        let on_screen = self.listed_dir == self.cwd
            && self.watched_dir.as_deref() == Some(self.listed_dir.as_path());
        match plan(
            on_screen,
            self.last_load_start >= seen,
            self.refresh_blocked(),
        ) {
            Plan::Covered => Step::Done,
            Plan::Defer => {
                self.folder_stale = true;
                if self.briefly_blocked() {
                    Step::Wait
                } else {
                    Step::Done
                }
            }
            Plan::Reload => Step::Loading(self.reload_quietly(cx)),
        }
    }

    /// Reload a folder marked stale, once nothing is in the way (called as
    /// the search box empties, and on returning to a tab). Never while a
    /// navigation is in flight: that load reads fresh rows itself.
    pub(super) fn refresh_if_stale(&mut self, cx: &mut Context<Self>) {
        if self.folder_stale && !self.refresh_blocked() && self.listed_dir == self.cwd {
            self.reload_quietly(cx).detach();
        }
    }

    /// Re-read the folder on screen without a notice, keeping selection
    /// (by path), scroll position and Quick Look.
    fn reload_quietly(&mut self, cx: &mut Context<Self>) -> gpui::Task<()> {
        self.folder_stale = false;
        let dir = self.listed_dir.clone();
        self.load_listing(&dir, None, true, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::{Plan, plan};

    /// Constructing a real `Workspace` needs the settings store, daemon
    /// probe and platform services, so the decision is tested on its own.
    #[test]
    fn auto_refresh_defers_while_blocked() {
        assert_eq!(plan(true, false, false), Plan::Reload);
        // A search is showing: mark stale and retry once it clears.
        assert_eq!(plan(true, false, true), Plan::Defer);
        // The app's own op already reloaded after the change.
        assert_eq!(plan(true, true, false), Plan::Covered);
        assert_eq!(plan(true, true, true), Plan::Covered);
        // Mid-navigation: the new folder's load reads fresh rows.
        assert_eq!(plan(false, false, false), Plan::Covered);
    }
}

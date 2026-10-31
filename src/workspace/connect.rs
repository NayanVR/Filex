//! "Connect to Server…" (FIL-59): a dialog for a share address that
//! connects through [`filex::connect`] on a thread of its own (the OS may
//! sit in a sign-in prompt for minutes) and opens the share once mounted.
//! Opened from the sidebar's Drives section, the This PC page, and the
//! location field when a typed share isn't connected.
//!
//! Disconnect / Eject (FIL-60) is a network drive's right-click action in
//! the same two places, also on its own thread.

use super::*;
use filex::connect::{ConnectError, ConnectRequest};
use filex::network::ShareAddress;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub(super) struct ConnectDialog {
    input: gpui::Entity<SearchInput>,
    state: ConnectState,
    _subscription: gpui::Subscription,
}

/// The dialog's state apart from its widgets, so its rules are testable
/// without a window.
#[derive(Default)]
struct ConnectState {
    /// Why the dialog opened (a typed share that isn't connected yet), or
    /// why the last attempt failed.
    message: Option<SharedString>,
    failed: bool,
    /// The running attempt's cancel flag; `Some` while connecting.
    pending: Option<Arc<AtomicBool>>,
    /// Windows: map to a drive letter rather than open by UNC path.
    map_drive_letter: bool,
}

impl ConnectState {
    /// Start an attempt for `text`: the request and its cancel flag, or
    /// `None` (with the hint shown) when it isn't a share address. Nothing
    /// starts while an attempt is already running.
    fn begin(&mut self, text: &str) -> Option<(ConnectRequest, Arc<AtomicBool>)> {
        if self.pending.is_some() {
            return None;
        }
        let Some(address) = ShareAddress::parse(text) else {
            self.message = Some(ADDRESS_HINT.into());
            self.failed = true;
            return None;
        };
        let cancel = Arc::new(AtomicBool::new(false));
        self.pending = Some(cancel.clone());
        self.message = None;
        self.failed = false;
        let request = ConnectRequest {
            address,
            map_drive_letter: self.map_drive_letter,
        };
        Some((request, cancel))
    }

    /// Take `attempt`'s result: the folder to open on success. A result
    /// from an attempt that isn't the running one is ignored.
    fn finish(
        &mut self,
        attempt: &Arc<AtomicBool>,
        result: Result<PathBuf, ConnectError>,
    ) -> Option<PathBuf> {
        if !self
            .pending
            .as_ref()
            .is_some_and(|p| Arc::ptr_eq(p, attempt))
        {
            return None;
        }
        self.pending = None;
        match result {
            Ok(path) => return Some(path),
            Err(ConnectError::Cancelled) => {}
            Err(ConnectError::Failed(message)) => {
                self.message = Some(message.into());
                self.failed = true;
            }
        }
        None
    }

    /// Abandon the running attempt, if any.
    fn cancel(&mut self) {
        if let Some(cancel) = self.pending.take() {
            cancel.store(true, Ordering::Relaxed);
        }
    }
}

const ADDRESS_HINT: &str = if cfg!(windows) {
    "Enter a share address, like \\\\server\\share or smb://server/share."
} else {
    "Enter a share address, like smb://server/share or nfs://server/export."
};

impl Workspace {
    /// Open the dialog, optionally filled in with an address and a note
    /// saying why (the location field's "isn't connected").
    pub(super) fn open_connect(
        &mut self,
        address: Option<String>,
        note: Option<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.connect.is_some() {
            return;
        }
        let input = cx.new(SearchInput::new);
        input.update(cx, |input, cx| {
            input.set_propagate_empty(false);
            input.set_placeholder(
                if cfg!(windows) {
                    "\\\\server\\share"
                } else {
                    "smb://server/share"
                },
                cx,
            );
            if let Some(address) = address {
                input.set_text(address, cx);
                input.select_all_text(cx);
            }
        });
        let subscription = cx.subscribe_in(&input, window, |this, _, event, window, cx| {
            if matches!(event, SearchInputEvent::Dismissed) {
                this.cancel_connect(window, cx);
            }
        });
        window.focus(&input.focus_handle(cx), cx);
        self.connect = Some(ConnectDialog {
            input,
            state: ConnectState {
                message: note,
                ..Default::default()
            },
            _subscription: subscription,
        });
        cx.notify();
    }

    /// Close the dialog, abandoning any attempt still running.
    pub(super) fn cancel_connect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(mut dialog) = self.connect.take() else {
            return;
        };
        dialog.state.cancel();
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    pub(super) fn submit_connect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = self.connect.as_mut() else {
            return;
        };
        let text = dialog.input.read(cx).text().to_owned();
        let Some((request, cancel)) = dialog.state.begin(&text) else {
            cx.notify();
            return;
        };
        let (tx, rx) = futures::channel::oneshot::channel();
        filex::connect::connect_on_thread(
            filex::connect::connector(),
            request,
            cancel.clone(),
            move |result| {
                let _ = tx.send(result);
            },
        );
        cx.spawn_in(window, async move |this, cx| {
            let Ok(result) = rx.await else {
                return;
            };
            this.update_in(cx, |this, window, cx| {
                this.finish_connect(&cancel, result, window, cx);
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn finish_connect(
        &mut self,
        attempt: &Arc<AtomicBool>,
        result: Result<PathBuf, ConnectError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(dialog) = self.connect.as_mut() else {
            return;
        };
        if let Some(path) = dialog.state.finish(attempt, result) {
            self.connect = None;
            window.focus(&self.focus_handle, cx);
            self.refresh_drives(cx);
            self.navigate(path, cx);
        }
        cx.notify();
    }

    /// Disconnect the network drive at `mount` off the UI thread, unless a
    /// running file operation is using it. Leaves the share's folders for
    /// This PC (Windows) or home, since they're about to vanish.
    pub(super) fn disconnect_drive(&mut self, mount: PathBuf, cx: &mut Context<Self>) {
        let label = filex::listing::location_label(&mount);
        if jobs_touch(&self.jobs, &mount) {
            self.notice = Some(
                format!("Can’t disconnect {label} while files are being copied or moved there")
                    .into(),
            );
            cx.notify();
            return;
        }
        self.notice = Some(format!("Disconnecting {label}…").into());
        cx.notify();
        let (tx, rx) = futures::channel::oneshot::channel();
        filex::connect::disconnect_on_thread(
            filex::connect::connector(),
            mount.clone(),
            move |result| {
                let _ = tx.send(result);
            },
        );
        cx.spawn(async move |this, cx| {
            let Ok(result) = rx.await else {
                return;
            };
            this.update(cx, |this, cx| {
                this.notice = Some(match result {
                    Ok(()) => {
                        if this.cwd.starts_with(&mount) {
                            this.leave_disconnected_share(cx);
                        }
                        format!("Disconnected {label}").into()
                    }
                    Err(error) => error.to_string().into(),
                });
                this.refresh_drives(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Go somewhere that still exists once the share on screen is gone.
    fn leave_disconnected_share(&mut self, cx: &mut Context<Self>) {
        let away = if cfg!(windows) {
            Some(filex::drives::this_pc())
        } else {
            self.user_dirs.get("home").map(Path::to_path_buf)
        };
        if let Some(away) = away {
            self.navigate(away, cx);
        }
    }

    pub(super) fn render_connect_modal(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let dialog = self.connect.as_ref()?;
        let theme = *cx.theme();
        let busy = dialog.state.pending.is_some();
        let field = div()
            .w_full()
            .px_2()
            .py_1p5()
            .rounded_md()
            .border_1()
            .border_color(if dialog.state.failed {
                theme.warn
            } else {
                theme.border
            })
            .text_sm()
            .child(dialog.input.clone());
        let message = dialog.state.message.clone().map(|message| {
            div()
                .w_full()
                .text_sm()
                .text_color(if dialog.state.failed {
                    theme.warn
                } else {
                    theme.text_dim
                })
                .child(message)
        });
        let connect = if busy {
            ui::button::disabled_button(
                &theme,
                "connect-go",
                "Connecting…",
                ui::button::Size::Regular,
            )
            .min_w(px(84.))
        } else {
            ui::modal::button(&theme, "connect-go", "Connect", true).on_click(
                cx.listener(|this, _: &ClickEvent, window, cx| this.submit_connect(window, cx)),
            )
        };
        Some(ui::modal::present(
            "connect-backdrop-in",
            ui::modal::backdrop("connect-backdrop")
                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                    this.cancel_connect(window, cx);
                }))
                .child(
                    ui::modal::panel(&theme, "connect-panel")
                        .on_click(|_, _, cx| cx.stop_propagation())
                        .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                            if event.keystroke.key == "enter" {
                                cx.stop_propagation();
                                this.submit_connect(window, cx);
                            }
                        }))
                        .child(ui::modal::title(&theme, "Connect to Server"))
                        .child(ui::modal::message(&theme, ADDRESS_HINT))
                        .child(field)
                        .children(message)
                        .when(cfg!(windows), |panel| {
                            panel.child(
                                ui::settings_pane::toggle_row(
                                    &theme,
                                    "connect-drive-letter",
                                    "Map to a drive letter",
                                    "Give the share the next free letter, like Z:, until you sign out.",
                                    dialog.state.map_drive_letter,
                                )
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    if let Some(dialog) = this.connect.as_mut() {
                                        dialog.state.map_drive_letter = !dialog.state.map_drive_letter;
                                        cx.notify();
                                    }
                                })),
                            )
                        })
                        .child(
                            ui::modal::buttons()
                                .child(
                                    ui::modal::button(&theme, "connect-cancel", "Cancel", false)
                                        .on_click(cx.listener(
                                            |this, _: &ClickEvent, window, cx| {
                                                this.cancel_connect(window, cx);
                                            },
                                        )),
                                )
                                .child(connect),
                        ),
                ),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(paths: &[&str]) -> Job {
        Job {
            id: 1,
            label: "copying".into(),
            progress: Default::default(),
            paths: paths.iter().map(PathBuf::from).collect(),
        }
    }

    /// FIL-60: a share is busy while a job reads from or writes to it.
    #[test]
    fn a_share_in_use_by_a_job_is_not_disconnected() {
        let mount = Path::new("/mnt/nas");
        let jobs = [
            job(&["/home/me/a.txt", "/home/me/backup"]),
            job(&["/mnt/nas/films/x.mkv", "/home/me/Videos"]),
        ];
        assert!(jobs_touch(&jobs, mount));
        assert!(!jobs_touch(&jobs[..1], mount));
        assert!(
            !jobs_touch(&[job(&["/mnt/nas-old/x"])], mount),
            "a sibling with the same prefix"
        );
        assert!(!jobs_touch(&[], mount));
    }

    fn failed(state: &ConnectState) -> Option<&str> {
        state.failed.then_some(state.message.as_deref()).flatten()
    }

    #[test]
    fn an_address_that_isnt_a_share_shows_the_hint_and_starts_nothing() {
        let mut state = ConnectState::default();
        assert!(state.begin("not a share").is_none());
        assert_eq!(failed(&state), Some(ADDRESS_HINT));
        assert!(state.pending.is_none());
    }

    #[test]
    fn one_attempt_runs_at_a_time_and_success_hands_back_the_folder() {
        let mut state = ConnectState {
            map_drive_letter: true,
            ..Default::default()
        };
        let (request, attempt) = state.begin("smb://nas/media").unwrap();
        assert!(request.map_drive_letter);
        assert_eq!(request.address.to_url(), "smb://nas/media");
        assert!(
            state.begin("smb://nas/other").is_none(),
            "already connecting"
        );
        assert_eq!(
            state.finish(&attempt, Ok(PathBuf::from("/mnt/media"))),
            Some(PathBuf::from("/mnt/media"))
        );
        assert!(state.pending.is_none());
    }

    #[test]
    fn a_failure_is_shown_and_a_retry_clears_it() {
        let mut state = ConnectState::default();
        let (_, attempt) = state.begin("smb://nas/media").unwrap();
        let error = ConnectError::Failed("Couldn’t reach the server “nas”.".into());
        assert_eq!(state.finish(&attempt, Err(error)), None);
        assert_eq!(failed(&state), Some("Couldn’t reach the server “nas”."));
        assert!(state.begin("smb://nas/media").is_some());
        assert_eq!(failed(&state), None);
    }

    #[test]
    fn results_from_abandoned_attempts_are_ignored() {
        let mut state = ConnectState::default();
        let (_, first) = state.begin("smb://nas/media").unwrap();
        state.cancel();
        assert!(first.load(Ordering::Relaxed), "the OS call is told to stop");
        let (_, second) = state.begin("smb://nas/media").unwrap();
        assert_eq!(state.finish(&first, Ok(PathBuf::from("/stale"))), None);
        assert!(
            state.pending.is_some(),
            "the newer attempt is still running"
        );
        assert_eq!(state.finish(&second, Err(ConnectError::Cancelled)), None);
        assert_eq!(failed(&state), None, "a dismissed prompt isn't an error");
    }
}

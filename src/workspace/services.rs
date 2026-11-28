//! Background tasks: resource sampling, drive refresh, the indexing
//! service probe/poll, app-update checks, crash upload, and tag pruning.

use super::*;
use std::time::Duration;

impl Workspace {
    /// Sample UI resident memory on a slow timer (`resource_sample`). The UI
    /// owns zero catalog bytes, and daemon resources are separate. Only
    /// spawned when analytics is on. The RSS query is a cheap OS call.
    pub(super) fn spawn_resource_sampling(&self, cx: &mut Context<Self>) {
        if !fa::is_enabled() {
            return;
        }
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(15 * 60))
                    .await;
                let Ok((roots, tabs)) =
                    this.update(cx, |this, _| (this.roots.len(), this.tabs.len()))
                else {
                    break;
                };
                fa::track(ev::ResourceSample {
                    rss_mb: fa::process_rss_bytes().map(|b| b / (1024 * 1024)),
                    index_roots: roots,
                    tabs,
                });
            }
        })
        .detach();
    }

    /// Refresh the mounted-volume list on a slow timer (drives change
    /// rarely; enumerating them hits the disk, so never per-frame). The
    /// blocking enumeration runs on the background executor.
    pub(super) fn spawn_drive_refresh(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                let drives = cx
                    .background_executor()
                    .spawn(async { filex::drives::list_drives() })
                    .await;
                let updated = this
                    .update(cx, |this, cx| this.set_drives(drives, cx))
                    .is_ok();
                if !updated {
                    break; // workspace dropped
                }
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(30))
                    .await;
            }
        })
        .detach();
    }

    /// Check for an update once on launch (distribution decision 7: no
    /// timer). Windows asks the `filex-updater` service, which downloads and
    /// verifies in the background; elsewhere — or when the service isn't
    /// installed — it's the notice-only manifest check.
    #[cfg(feature = "updater")]
    pub(super) fn spawn_update_check(&self, cx: &mut Context<Self>) {
        #[cfg(target_os = "windows")]
        self.drive_update_service(filex::updater::protocol::Command::Check, cx);
        #[cfg(not(target_os = "windows"))]
        self.spawn_notice_update_check(cx);
    }

    /// Send `command` to the Windows update service, then follow its phase
    /// into the banner until it settles. `Applying` means the install has
    /// started: stop the daemon and quit (the service reopens Filex).
    #[cfg(all(target_os = "windows", feature = "updater"))]
    fn drive_update_service(
        &self,
        command: filex::updater::protocol::Command,
        cx: &mut Context<Self>,
    ) {
        use filex::updater::protocol::{Command, Phase};
        use filex::updater::windows::client::{self, Unavailable};

        let applying_requested = command == Command::Apply;
        cx.spawn(async move |this, cx| {
            let mut next = Some(command);
            loop {
                let command = next.take().unwrap_or(Command::Status);
                let result = cx
                    .background_executor()
                    .spawn(async move { client::phase(command) })
                    .await;
                let phase = match result {
                    Ok(phase) => phase,
                    Err(error) => {
                        if matches!(error, Unavailable::NotInstalled) {
                            tracing::info!("{error}; using the release-page notice");
                        } else {
                            tracing::warn!("{error}");
                        }
                        this.update(cx, |this, cx| {
                            if applying_requested {
                                fa::track(ev::UpdateStep {
                                    stage: ev::UpdateStage::Failed,
                                });
                                this.update_status = filex::update::UpdateStatus::Failed;
                                this.notice = Some("Couldn't reach the Filex updater".into());
                                cx.notify();
                            } else {
                                // Dev builds, installs that predate the
                                // service, or a service that won't start
                                // still learn about new releases.
                                this.spawn_notice_update_check(cx);
                            }
                        })
                        .ok();
                        return;
                    }
                };
                let applying = matches!(phase, Phase::Applying { .. });
                let failed_apply = applying_requested && matches!(phase, Phase::Failed { .. });
                let alive = this
                    .update(cx, |this, cx| {
                        this.update_status = filex::updater::ui_status(&phase);
                        if failed_apply {
                            this.notice = Some("The update couldn't be installed".into());
                        }
                        cx.notify();
                    })
                    .is_ok();
                if !alive {
                    return;
                }
                if applying {
                    Self::exit_for_update(this, cx).await;
                    return;
                }
                if !phase.is_busy() {
                    return;
                }
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(1))
                    .await;
            }
        })
        .detach();
    }

    /// The install has started: stop the per-user daemon cleanly (the
    /// updater would otherwise terminate it) and quit.
    #[cfg(all(target_os = "windows", feature = "updater"))]
    async fn exit_for_update(this: gpui::WeakEntity<Self>, cx: &mut gpui::AsyncApp) {
        let Ok(service) = this.update(cx, |this, _| {
            this.exiting_for_update = true;
            this.service.take()
        }) else {
            return;
        };
        if let Some(service) = service {
            let stopped = cx
                .background_executor()
                .spawn(async move { service.shutdown(std::time::Duration::from_secs(5)) })
                .await;
            if let Err(error) = stopped {
                tracing::warn!("index daemon did not stop before the update: {error:#}");
            }
        }
        this.update(cx, |_, cx| cx.quit()).ok();
    }

    /// Notice-only manifest check: surface the banner if a newer version
    /// exists. Installation stays with the platform package workflow; this
    /// never downloads or verifies an artifact. Off-thread; a failed check is
    /// silent (retried next launch).
    #[cfg(feature = "updater")]
    fn spawn_notice_update_check(&self, cx: &mut Context<Self>) {
        let url = platform::UPDATE_MANIFEST_URL;
        if url.is_empty() {
            return;
        }
        cx.spawn(async move |this, cx| {
            let found = cx
                .background_executor()
                .spawn(async move {
                    let cancel = filex::update::CancelFlag::new();
                    filex::update::check_for_newer_version(
                        filex::update::http_fetch,
                        url,
                        filex::update::CURRENT_VERSION,
                        &cancel,
                    )
                })
                .await;
            if let Ok(Some(version)) = found {
                fa::track(ev::UpdateStep {
                    stage: ev::UpdateStage::Available,
                });
                this.update(cx, |this, cx| {
                    this.update_status = filex::update::UpdateStatus::Available {
                        version,
                        affordance: platform::update_affordance(),
                    };
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    /// Act on the update banner's primary button per the current
    /// affordance: copy the command, open the releases page, or (Windows)
    /// have the update service install and reopen Filex.
    pub(super) fn apply_update_action(&mut self, cx: &mut Context<Self>) {
        let filex::update::UpdateStatus::Available { affordance, .. } = &self.update_status else {
            return;
        };
        fa::track(ev::UpdateStep {
            stage: match affordance {
                filex::update::UpdateAffordance::RunCommand(_) => ev::UpdateStage::CopyCommand,
                filex::update::UpdateAffordance::OpenUrl(_) => ev::UpdateStage::OpenUrl,
                filex::update::UpdateAffordance::Restart => ev::UpdateStage::Restart,
            },
        });
        match affordance.clone() {
            filex::update::UpdateAffordance::RunCommand(cmd) => {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(cmd));
                self.notice = Some("Update command copied — run it in your terminal".into());
            }
            filex::update::UpdateAffordance::OpenUrl(url) => {
                let _ = open_with_default_app(std::path::Path::new(&url));
            }
            filex::update::UpdateAffordance::Restart => {
                #[cfg(all(target_os = "windows", feature = "updater"))]
                self.drive_update_service(filex::updater::protocol::Command::Apply, cx);
            }
        }
        cx.notify();
    }

    /// Hide the update banner (the ✕). The check runs again next launch.
    pub(super) fn dismiss_update(&mut self, cx: &mut Context<Self>) {
        fa::track(ev::UpdateStep {
            stage: ev::UpdateStage::Dismissed,
        });
        self.update_status = filex::update::UpdateStatus::Idle;
        cx.notify();
    }

    /// Connect or start the per-user daemon; reconnect without an in-process index.
    pub(super) fn spawn_service_probe(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let mut backoff = StartBackoff::default();
            loop {
                let existing = this.update(cx, |this, _| {
                    (!this.exiting_for_update).then(|| this.service.clone())
                });
                // Gone, or quitting for an update: don't (re)start the daemon.
                let Ok(Some(existing)) = existing else {
                    break;
                };
                let outcome = cx
                    .background_executor()
                    .spawn(async move {
                        let client = match existing {
                            Some(client) => client,
                            None => std::sync::Arc::new(filex::daemon::ipc::Client::start()?),
                        };
                        let status = client.status()?;
                        Ok::<_, anyhow::Error>((client, status))
                    })
                    .await;
                let delay = match &outcome {
                    Ok(_) => {
                        backoff.succeeded(std::time::Instant::now());
                        POLL_INTERVAL
                    }
                    Err(e) => match backoff.failed_with(e) {
                        Some(delay) => delay,
                        // Busy: keep the client and status, no repaint, no
                        // search re-run; just poll again.
                        None => {
                            cx.background_executor().timer(POLL_INTERVAL).await;
                            continue;
                        }
                    },
                };
                let crash_looping = backoff.crash_looping();
                if this
                    .update(cx, |this, cx| {
                        // Polled at 10 Hz; repainting on every poll kept the
                        // idle window awake (FIL-27 §2).
                        let repaint = match outcome {
                            Ok((client, status)) => {
                                // A Magic plan keeps its captured epoch while the
                                // filesystem changes. Refresh it on initial recovery,
                                // not on every ordinary catalog publication.
                                let refresh = this.service.is_none()
                                    || !this.in_magic_view()
                                    || (this.daemon_status.building && !status.building);
                                let changed = this.service.is_none()
                                    || status.epoch != this.daemon_status.epoch
                                    || status.building != this.daemon_status.building;
                                let repaint = needs_repaint(
                                    this.service.is_some(),
                                    &this.daemon_status,
                                    true,
                                    &status,
                                );
                                this.service = Some(client);
                                this.roots =
                                    status.roots.iter().map(IndexedRoot::from_status).collect();
                                if let Some(onboarding) = &mut this.onboarding {
                                    let now = std::time::Instant::now();
                                    onboarding.observe_index(status.building, now);
                                    onboarding.observe_loading(&status, now);
                                }
                                analytics::index_transition(
                                    this.service_seen,
                                    &this.daemon_status,
                                    &status,
                                );
                                this.service_seen = true;
                                this.daemon_status = status;
                                if changed && refresh && !this.query.is_empty() {
                                    this.update_search(cx);
                                }
                                repaint
                            }
                            Err(_) => {
                                // Once per lost connection, not per retry.
                                if this.service.is_some() {
                                    fa::track(ev::IndexUnavailable { crash_looping });
                                }
                                let mut next = this.daemon_status.clone();
                                next.error = Some(if crash_looping {
                                    "Index service keeps crashing — search unavailable, retrying"
                                        .into()
                                } else {
                                    "Search unavailable — reconnecting to filex-indexd".into()
                                });
                                let repaint = needs_repaint(
                                    this.service.take().is_some(),
                                    &this.daemon_status,
                                    false,
                                    &next,
                                );
                                this.daemon_status = next;
                                repaint
                            }
                        };
                        if repaint {
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
                cx.background_executor().timer(delay).await;
            }
        })
        .detach();
    }

    pub(super) fn spawn_fda_check(&self, cx: &mut Context<Self>) {
        #[cfg(target_os = "macos")]
        cx.spawn(async move |this, cx| {
            let has_access = cx
                .background_executor()
                .spawn(async { filex::ingest::has_full_disk_access() })
                .await;
            this.update(cx, |this, cx| {
                this.fda_missing = !has_access;
                cx.notify();
            })
            .ok();
        })
        .detach();
        #[cfg(not(target_os = "macos"))]
        let _ = cx;
    }

    /// Deliver queued crash reports (the UI's and the daemon's) to PostHog at
    /// launch, only with the user's consent (`share_diagnostics`, which is
    /// what started analytics). Runs off-thread. A report is deleted once
    /// PostHog accepts it and stays queued for next launch on a network
    /// failure (Phase 2c). Without the `analytics` feature this is a no-op
    /// and the durable queue simply caps at
    /// [`filex::diagnostics::telemetry::QUEUE_CAP`].
    pub(super) fn spawn_crash_upload(&self, cx: &mut Context<Self>) {
        if !fa::is_enabled() {
            return;
        }
        let Some(dir) = filex::diagnostics::telemetry::default_queue_dir() else {
            return; // no data dir
        };
        cx.background_executor()
            .spawn(async move {
                let sent = fa::drain_crashes(&dir);
                if sent > 0 {
                    tracing::info!("sent {sent} crash report(s)");
                }
            })
            .detach();
    }

    /// Drop sidecar tag keys whose file no longer exists — lazy cleanup
    /// (design-tags.md) for files moved/deleted outside filex, where we
    /// never saw the `from→to` pairing. Runs once at startup, off-thread.
    pub(super) fn spawn_tag_prune(&self, cx: &mut Context<Self>) {
        let tags = self.tags.clone();
        cx.background_executor()
            .spawn(async move {
                match tags.prune(|path| path.exists()) {
                    Ok(n) if n > 0 => tracing::debug!("pruned {n} stale tag entries"),
                    Ok(_) => {}
                    Err(err) => tracing::error!("failed to prune tags: {err:#}"),
                }
            })
            .detach();
    }
}

/// How often a healthy daemon is polled.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Spacing between daemon (re)start attempts. The daemon exits on any
/// panic, so one that dies at startup would otherwise be respawned every
/// couple of seconds forever, writing a crash report each time. Failures
/// back off 1 s, 2 s, 4 s … capped at 60 s. The count resets only once a
/// connection has stayed up for [`STABLE`](Self::STABLE), so a daemon that
/// connects and then dies a few seconds later still backs off.
#[derive(Debug, Default)]
struct StartBackoff {
    failures: u32,
    up_since: Option<std::time::Instant>,
}

impl StartBackoff {
    /// After this many failures in a row the status says the service is
    /// crashing rather than reconnecting.
    const CRASH_LOOP_AFTER: u32 = 5;
    const MAX_DELAY: Duration = Duration::from_secs(60);
    const STABLE: Duration = Duration::from_secs(10);

    fn succeeded(&mut self, now: std::time::Instant) {
        let since = *self.up_since.get_or_insert(now);
        if now.duration_since(since) >= Self::STABLE {
            self.failures = 0;
        }
    }

    /// Record a failed start/connect; returns the wait before the next try.
    fn failed(&mut self) -> Duration {
        self.up_since = None;
        self.failures = self.failures.saturating_add(1);
        let exp = (self.failures - 1).min(6); // 2^6 s already exceeds the cap
        Duration::from_secs(1 << exp).min(Self::MAX_DELAY)
    }

    /// [`failed`](Self::failed), except that a daemon refusing as busy is
    /// up: `None`, meaning keep the client and status untouched and poll
    /// again at the normal cadence. It counts nothing, so a local process
    /// flooding connections can't make it read as crashing.
    fn failed_with(&mut self, error: &anyhow::Error) -> Option<Duration> {
        (!filex::daemon::ipc::is_busy(error)).then(|| self.failed())
    }

    fn crash_looping(&self) -> bool {
        self.failures >= Self::CRASH_LOOP_AFTER
    }
}

/// Whether a daemon poll changed anything the window shows: connecting,
/// disconnecting, or a different status. The probe polls at 10 Hz, so
/// repainting unconditionally kept an idle window awake (FIL-27 §2).
fn needs_repaint(
    was_connected: bool,
    prev: &filex::daemon::ipc::Status,
    connected: bool,
    next: &filex::daemon::ipc::Status,
) -> bool {
    was_connected != connected || prev != next
}

#[cfg(test)]
mod tests {
    use super::{StartBackoff, needs_repaint};
    use filex::daemon::ipc::Status;
    use std::time::Duration;

    /// Regression: a daemon that dies on startup was respawned every ~2 s
    /// forever. Failures back off exponentially to 60 s, flag a crash loop
    /// after five, and one good poll restores the normal cadence.
    #[test]
    fn start_failures_back_off_and_reset() {
        let mut b = StartBackoff::default();
        let delays: Vec<u64> = (0..9).map(|_| b.failed().as_secs()).collect();
        assert_eq!(delays, [1, 2, 4, 8, 16, 32, 60, 60, 60]);
        assert!(b.crash_looping());
        // Connecting then dying within STABLE keeps the count.
        let t0 = std::time::Instant::now();
        b.succeeded(t0);
        b.succeeded(t0 + Duration::from_secs(5));
        assert!(b.crash_looping());
        assert_eq!(b.failed(), Duration::from_secs(60));
        // Up for STABLE resets it.
        b.succeeded(t0);
        b.succeeded(t0 + StartBackoff::STABLE);
        assert!(!b.crash_looping());
        assert_eq!(b.failed(), Duration::from_secs(1));
        for _ in 0..3 {
            b.failed();
        }
        assert!(!b.crash_looping()); // four in a row
        b.failed();
        assert!(b.crash_looping()); // five
        for _ in 0..100 {
            assert!(b.failed() <= Duration::from_secs(60));
        }
    }

    /// Regression: a "busy" refusal (the daemon's connection cap) is not a
    /// failure: the probe keeps its client and status (`None`), with no
    /// backoff or crash-loop text, and earlier failures keep their count.
    #[test]
    fn busy_replies_do_not_back_off() {
        let busy = anyhow::anyhow!(filex::daemon::ipc::BUSY);
        let mut b = StartBackoff::default();
        for _ in 0..20 {
            assert_eq!(b.failed_with(&busy), None);
        }
        assert!(!b.crash_looping());
        let gone = anyhow::anyhow!("connection refused");
        assert_eq!(b.failed_with(&gone), Some(Duration::from_secs(1)));
        assert_eq!(b.failed_with(&busy), None);
        assert_eq!(b.failed_with(&gone), Some(Duration::from_secs(2)));
    }

    /// Regression (FIL-27 §2): an unchanged poll must not repaint.
    #[test]
    fn repaints_only_on_change() {
        let idle = Status::default();
        let building = Status {
            building: true,
            ..Status::default()
        };
        let down = Status {
            error: Some("reconnecting".into()),
            ..Status::default()
        };
        assert!(!needs_repaint(true, &idle, true, &idle));
        assert!(needs_repaint(true, &idle, true, &building));
        assert!(needs_repaint(false, &idle, true, &idle)); // connected
        assert!(needs_repaint(true, &idle, false, &down)); // dropped
        assert!(!needs_repaint(false, &down, false, &down)); // still down
    }
}

//! Background tasks: resource sampling, drive refresh, the indexing
//! service probe/poll, app-update checks, crash upload, and tag pruning.

use super::*;

impl Workspace {
    /// Sample UI RSS; the UI owns zero catalog bytes. Daemon resources are separate.
    #[cfg(feature = "observability")]
    pub(super) fn spawn_resource_sampling(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(60))
                    .await;
                let Ok(roots) = this.update(cx, |this, _| this.roots.len()) else {
                    break;
                };
                filex::diagnostics::observability::record_resource_sample(roots);
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
                    .update(cx, |this, cx| {
                        if this.drives != drives {
                            this.drives = drives;
                            cx.notify();
                        }
                    })
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

    /// Check the manifest once on launch (distribution decision 7: no
    /// timer) and surface the banner if a newer version exists. Notice-
    /// only — installation remains with the platform package workflow; this never
    /// downloads or verifies an artifact. Off-thread; a failed check is
    /// silent (retried next launch).
    #[cfg(feature = "updater")]
    pub(super) fn spawn_update_check(&self, cx: &mut Context<Self>) {
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
    /// affordance: copy the command, open the releases page, or (Windows,
    /// unused here) restart.
    pub(super) fn apply_update_action(&mut self, cx: &mut Context<Self>) {
        let filex::update::UpdateStatus::Available { affordance, .. } = &self.update_status else {
            return;
        };
        match affordance.clone() {
            filex::update::UpdateAffordance::RunCommand(cmd) => {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(cmd));
                self.notice = Some("Update command copied — run it in your terminal".into());
            }
            filex::update::UpdateAffordance::OpenUrl(url) => {
                let _ = open_with_default_app(std::path::Path::new(&url));
            }
            filex::update::UpdateAffordance::Restart => {}
        }
        cx.notify();
    }

    /// Hide the update banner (the ✕). The check runs again next launch.
    pub(super) fn dismiss_update(&mut self, cx: &mut Context<Self>) {
        self.update_status = filex::update::UpdateStatus::Idle;
        cx.notify();
    }

    /// Connect or start the per-user daemon; reconnect without an in-process index.
    pub(super) fn spawn_service_probe(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                let existing = this.update(cx, |this, _| this.service.clone());
                let Ok(existing) = existing else {
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
                if this
                    .update(cx, |this, cx| {
                        match outcome {
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
                                this.service = Some(client);
                                this.roots =
                                    status.roots.iter().map(IndexedRoot::from_status).collect();
                                this.daemon_status = status;
                                if changed && refresh && !this.query.is_empty() {
                                    this.update_search(cx);
                                }
                            }
                            Err(_) => {
                                this.service = None;
                                this.daemon_status.error = Some(
                                    "Search unavailable — reconnecting to filex-indexd".into(),
                                );
                            }
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(100))
                    .await;
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

    /// Drain any queued crash reports to Sentry at launch — only with the
    /// user's consent (`crash_reports`, on-by-default/opt-out). Runs
    /// off-thread; each scrubbed report is captured and deleted on success,
    /// failures stay queued for next launch (Phase 2c). Sentry is the only
    /// transport, so without the `observability` feature this is a no-op and
    /// the durable queue simply caps at [`filex::diagnostics::telemetry::QUEUE_CAP`].
    pub(super) fn spawn_crash_upload(&self, cx: &mut Context<Self>) {
        if !self.settings.read(cx).settings().crash_reports {
            return;
        }
        #[cfg(feature = "observability")]
        {
            let Some(dir) = filex::diagnostics::telemetry::default_queue_dir() else {
                return; // no data dir
            };
            cx.background_executor()
                .spawn(async move {
                    let sent = filex::diagnostics::observability::drain_crashes_to_sentry(&dir);
                    if sent > 0 {
                        tracing::info!("sent {sent} crash report(s) to Sentry");
                    }
                })
                .detach();
        }
        #[cfg(not(feature = "observability"))]
        let _ = cx;
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

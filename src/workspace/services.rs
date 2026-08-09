//! Background tasks: resource sampling, drive refresh, the indexing
//! service probe/poll, app-update checks, crash upload, and tag pruning.

use super::*;

impl Workspace {
    /// Sample memory use on a slow timer for the observability backend: the
    /// index arena bytes (the filex-controlled figure behind the memory
    /// question) plus process RSS. Ready-root index handles are cloned on
    /// the UI thread; the `approx_bytes` walk and the send run off it.
    #[cfg(feature = "observability")]
    pub(super) fn spawn_resource_sampling(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(60))
                    .await;
                let handles = this.update(cx, |this, _cx| {
                    this.roots
                        .iter()
                        .filter_map(|slot| slot.ready_index())
                        .collect::<Vec<_>>()
                });
                let Ok(handles) = handles else {
                    break; // workspace dropped
                };
                let roots = handles.len();
                let arena_bytes = cx
                    .background_executor()
                    .spawn(async move {
                        handles
                            .iter()
                            .map(|index| read_index(index).approx_bytes() as u64)
                            .sum::<u64>()
                    })
                    .await;
                filex::observability::record_resource_sample(arena_bytes, roots);
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
    /// only — macOS/Linux install via their package manager, so this never
    /// downloads or verifies an artifact. Off-thread; a failed check is
    /// silent (retried next launch).
    #[cfg(all(not(target_os = "windows"), feature = "updater"))]
    pub(super) fn spawn_update_check(&self, cx: &mut Context<Self>) {
        let url = Self::UPDATE_MANIFEST_URL;
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
                        affordance: platform_affordance(),
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

    /// Probe for filex-indexd; on success run in service mode, otherwise
    /// start local indexing. Runs off-thread — the UI shows "indexing
    /// 0/0" briefly while probing.
    #[cfg(target_os = "windows")]
    pub(super) fn spawn_service_probe(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let client = cx
                .background_executor()
                .spawn(async { filex::index::ipc::ServiceClient::try_connect().ok() })
                .await;
            this.update(cx, |this, cx| {
                match client {
                    Some(client) => {
                        this.service = Some(std::sync::Arc::new(client));
                        this.spawn_service_status_poll(cx);
                    }
                    None => {
                        for path in this.configured_roots(cx) {
                            this.add_root_slot(path, cx);
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Keep the service's root/file counts fresh; on IPC failure fall
    /// back to local indexing so search keeps working.
    #[cfg(target_os = "windows")]
    pub(super) fn spawn_service_status_poll(&self, cx: &mut Context<Self>) {
        let Some(client) = self.service.clone() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            loop {
                let status = cx
                    .background_executor()
                    .spawn({
                        let client = client.clone();
                        async move { client.status() }
                    })
                    .await;
                let keep_polling = this.update(cx, |this, cx| match status {
                    Ok(status) => {
                        this.service_status = status.roots;
                        cx.notify();
                        true
                    }
                    Err(err) => {
                        tracing::warn!("index service lost ({err:#}); indexing locally");
                        this.service_disconnected(cx);
                        false
                    }
                });
                if !matches!(keep_polling, Ok(true)) {
                    break;
                }
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(5))
                    .await;
            }
        })
        .detach();
    }

    #[cfg(target_os = "windows")]
    pub(super) fn service_disconnected(&mut self, cx: &mut Context<Self>) {
        self.service = None;
        self.service_status.clear();
        if self.roots.is_empty() {
            for path in self.configured_roots(cx) {
                self.add_root_slot(path, cx);
            }
        }
        self.update_search(cx);
        cx.notify();
    }

    #[cfg(target_os = "windows")]
    pub(super) fn service_mode(&self) -> bool {
        self.service.is_some()
    }

    #[cfg(not(target_os = "windows"))]
    pub(super) fn service_mode(&self) -> bool {
        false
    }

    pub(super) fn spawn_fda_check(&self, cx: &mut Context<Self>) {
        #[cfg(target_os = "macos")]
        cx.spawn(async move |this, cx| {
            let has_access = cx
                .background_executor()
                .spawn(async { filex::index::macos::has_full_disk_access() })
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
    /// the durable queue simply caps at [`filex::telemetry::QUEUE_CAP`].
    pub(super) fn spawn_crash_upload(&self, cx: &mut Context<Self>) {
        if !self.settings.read(cx).settings().crash_reports {
            return;
        }
        #[cfg(feature = "observability")]
        {
            let Some(dir) = filex::telemetry::default_queue_dir() else {
                return; // no data dir
            };
            cx.background_executor()
                .spawn(async move {
                    let sent = filex::observability::drain_crashes_to_sentry(&dir);
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

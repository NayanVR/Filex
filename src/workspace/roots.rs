//! Root-management requests; the UI owns only daemon status rows.
use super::*;
impl Workspace {
    pub(super) fn add_current_folder(&mut self, cx: &mut Context<Self>) {
        self.add_root(self.cwd.clone(), cx);
    }
    pub(super) fn add_root(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.change_root(path, true, cx);
    }
    pub(super) fn remove_root(&mut self, path: &Path, cx: &mut Context<Self>) {
        self.change_root(path.to_path_buf(), false, cx);
    }
    fn change_root(&mut self, path: PathBuf, add: bool, cx: &mut Context<Self>) {
        let path = filex::ingest::canonical_event_path(&path);
        let Some(client) = self.service.clone() else {
            self.notice = Some("Search daemon unavailable — browsing remains available".into());
            cx.notify();
            return;
        };
        cx.spawn(async move |this, cx| {
            let configured = path.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    client.call(if add {
                        filex::daemon::ipc::Command::AddRoot(path)
                    } else {
                        filex::daemon::ipc::Command::RemoveRoot(path)
                    })
                })
                .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(_) => {
                        this.settings.update(cx, |store, cx| {
                            store.update(cx, |s| {
                                if add {
                                    if !s.roots.contains(&configured) {
                                        s.roots.push(configured);
                                    }
                                } else {
                                    s.roots.retain(|p| p != &configured);
                                }
                            })
                        });
                        this.notice = None;
                    }
                    Err(e) => this.notice = Some(e.to_string().into()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

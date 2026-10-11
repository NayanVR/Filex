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
        // What the daemon indexes right now — before this change lands.
        let live: Vec<PathBuf> = self.roots.iter().map(|r| r.path.clone()).collect();
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
                                s.roots = edited_roots(&s.roots, &live, configured, add);
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

/// The saved root list after adding or removing `path`. An empty saved
/// list means "the platform defaults", which the daemon is indexing
/// (`live`) but nothing has written down. The first edit must write them
/// out, or adding one folder replaces them on the next daemon start —
/// adding `/Users/Shared` silently dropped the home folder from the index.
fn edited_roots(saved: &[PathBuf], live: &[PathBuf], path: PathBuf, add: bool) -> Vec<PathBuf> {
    let mut roots = if !saved.is_empty() {
        saved.to_vec()
    } else if !live.is_empty() {
        live.to_vec()
    } else {
        filex::drives::default_index_roots()
    };
    roots.retain(|p| p != &path);
    if add {
        roots.push(path);
    }
    roots
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(list: &[&str]) -> Vec<PathBuf> {
        list.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn the_first_added_root_keeps_the_defaults() {
        // Regression: settings saved only the added folder, so the next
        // daemon start indexed it alone and lost the home folder.
        let roots = edited_roots(&[], &paths(&["/Users/me"]), "/Users/Shared".into(), true);
        assert_eq!(roots, paths(&["/Users/me", "/Users/Shared"]));
    }

    #[test]
    fn removing_a_default_root_writes_the_rest_out() {
        let live = paths(&["/Users/me", "/Volumes/Data"]);
        let roots = edited_roots(&[], &live, "/Volumes/Data".into(), false);
        assert_eq!(roots, paths(&["/Users/me"]));
    }

    #[test]
    fn a_saved_list_is_edited_in_place() {
        let saved = paths(&["/a", "/b"]);
        assert_eq!(
            edited_roots(&saved, &paths(&["/x"]), "/b".into(), true),
            paths(&["/a", "/b"])
        );
        assert_eq!(
            edited_roots(&saved, &paths(&["/x"]), "/a".into(), false),
            paths(&["/b"])
        );
    }
}

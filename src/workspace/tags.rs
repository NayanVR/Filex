//! The tag editor: loading, editing, and persisting a file's tags.

use super::*;

impl Workspace {
    /// Drop the details panel's cached tags and any open tag editor
    /// (selection moved, or the panel closed).
    pub(super) fn clear_tag_state(&mut self) {
        self.preview_tags.clear();
        self.tag_editor = None;
    }

    /// Read the lead item's tags into `preview_tags` off-thread — the
    /// store read is an xattr syscall on macOS, so it never runs on the UI
    /// thread. Applied only if the selection hasn't moved on.
    pub(super) fn spawn_refresh_tags(&self, path: PathBuf, cx: &mut Context<Self>) {
        let store = self.tags.clone();
        cx.spawn(async move |this, cx| {
            let fetched = cx
                .background_executor()
                .spawn({
                    let store = store.clone();
                    let path = path.clone();
                    async move { store.tags(&path) }
                })
                .await;
            this.update(cx, |this, cx| {
                if this.lead_item().map(|(p, _, _)| p).as_deref() == Some(path.as_path()) {
                    this.preview_tags = fetched;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Persist `tags` for `path` through the store off-thread (xattr +
    /// sidecar), then mirror them into `preview_tags` if the item is still
    /// selected. A failure surfaces as a notice, never a crash.
    pub(super) fn spawn_set_tags(&mut self, path: PathBuf, tags: Vec<Tag>, cx: &mut Context<Self>) {
        let store = self.tags.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn({
                    let store = store.clone();
                    let path = path.clone();
                    let tags = tags.clone();
                    async move { store.set_tags(&path, &tags) }
                })
                .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        if this.lead_item().map(|(p, _, _)| p).as_deref() == Some(path.as_path()) {
                            this.preview_tags = tags;
                        }
                        this.refresh_sidebar_tags(cx);
                    }
                    Err(err) => this.notice = Some(format!("{err:#}").into()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Open the details-panel tag editor. `existing` prefills to
    /// recolor/rename an existing chip; `None` starts a fresh tag.
    pub(super) fn open_tag_editor(
        &mut self,
        existing: Option<Tag>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((path, _, _)) = self.lead_item() else {
            return;
        };
        let input = cx.new(SearchInput::new);
        let (seed, color, original) = match existing {
            Some(tag) => (tag.name.clone(), tag.color, Some(tag.name)),
            None => (String::new(), None, None),
        };
        input.update(cx, |input, cx| {
            input.set_placeholder("tag name", cx);
            if !seed.is_empty() {
                input.set_text(seed, cx);
                input.select_all_text(cx);
            }
        });
        let subscription = cx.subscribe_in(&input, window, |this, _input, event, window, cx| {
            if matches!(event, SearchInputEvent::Dismissed) {
                this.cancel_tag_editor(window, cx);
            }
        });
        window.focus(&input.focus_handle(cx));
        self.tag_editor = Some(TagEditor {
            path,
            input,
            color,
            existing: original,
            _subscription: subscription,
        });
        cx.notify();
    }

    pub(super) fn cancel_tag_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.tag_editor.take().is_some() {
            window.focus(&self.search_input.focus_handle(cx));
            cx.notify();
        }
    }

    /// Set the pending color in the open editor (clicking a swatch).
    pub(super) fn set_editor_color(&mut self, color: Option<TagColor>, cx: &mut Context<Self>) {
        if let Some(editor) = self.tag_editor.as_mut() {
            editor.color = color;
            cx.notify();
        }
    }

    /// Commit the tag editor: fold the typed name + chosen color into the
    /// item's tag set (adding, or replacing the edited tag in place) and
    /// persist it. An empty name is treated as cancel.
    pub(super) fn commit_tag_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.tag_editor.take() else {
            return;
        };
        window.focus(&self.search_input.focus_handle(cx));
        let name = editor.input.read(cx).text().trim().to_string();
        if name.is_empty() {
            cx.notify();
            return;
        }
        let new_tag = Tag {
            name,
            color: editor.color,
        };
        let tags = filex::tags::upsert_tag(&self.preview_tags, editor.existing.as_deref(), new_tag);
        self.spawn_set_tags(editor.path, tags, cx);
        cx.notify();
    }

    /// Remove the tag currently being edited (the editor's Remove button).
    pub(super) fn remove_editing_tag(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.tag_editor.take() else {
            return;
        };
        window.focus(&self.search_input.focus_handle(cx));
        if let Some(original) = editor.existing {
            let mut tags = self.preview_tags.clone();
            tags.retain(|t| t.name != original);
            self.spawn_set_tags(editor.path, tags, cx);
        }
        cx.notify();
    }
}

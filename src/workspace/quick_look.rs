//! Space-to-preview, shared by browse and search selections.

use super::*;

impl Workspace {
    fn quick_look_items(&self) -> (Vec<PathBuf>, usize) {
        let lead = self.lead_item().map(|(path, _, _)| path);
        let paths: Vec<_> = self
            .selected_paths()
            .into_iter()
            .map(|(path, _)| path)
            .collect();
        let selected = lead
            .and_then(|lead| paths.iter().position(|path| *path == lead))
            .unwrap_or(0);
        (paths, selected)
    }

    pub(super) fn toggle_quick_look(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings_open
            || self.renaming.is_some()
            || self.tag_editor.is_some()
            || self.conflict.is_some()
            || self.in_magic_view()
        {
            return;
        }
        if self
            .quick_look
            .as_ref()
            .is_some_and(|viewer| viewer.is_visible())
        {
            self.close_quick_look();
            return;
        }
        let (paths, selected) = self.quick_look_items();
        if paths.is_empty() {
            return;
        }
        self.context_menu = None;
        let result = (|| -> anyhow::Result<()> {
            if self.quick_look.is_none() {
                let workspace = cx.weak_entity();
                let app = cx.to_async();
                self.quick_look = Some(crate::quick_look::Viewer::new(
                    window,
                    cx.foreground_executor().clone(),
                    move |err| {
                        let mut app = app.clone();
                        let _ = workspace.update(&mut app, |this, cx| {
                            this.notice = Some(format!("Couldn’t open Quick Look: {err}").into());
                            cx.notify();
                        });
                    },
                )?);
            }
            if let Some(viewer) = self.quick_look.as_ref() {
                viewer.show(paths, selected);
            }
            Ok(())
        })();
        if let Err(err) = result {
            self.notice = Some(format!("Couldn’t open Quick Look: {err}").into());
        }
        cx.notify();
    }

    pub(super) fn sync_quick_look(&self) {
        if let Some(viewer) = self
            .quick_look
            .as_ref()
            .filter(|viewer| viewer.is_visible())
        {
            let (paths, selected) = self.quick_look_items();
            viewer.update(paths, selected);
        }
    }

    pub(super) fn close_quick_look(&self) {
        if let Some(viewer) = &self.quick_look {
            viewer.close();
        }
    }
}

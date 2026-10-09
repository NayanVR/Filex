//! Editable location field and background path navigation.
use super::*;

/// Expand a location without touching disk. Relative paths use the active folder.
fn resolve(text: &str, cwd: &Path, home: Option<&Path>) -> Result<PathBuf, String> {
    let text = text.trim();
    let text = text
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(text);
    if text.is_empty() {
        return Err("Enter a folder path.".into());
    }
    if text == "~" {
        return home
            .map(Path::to_path_buf)
            .ok_or_else(|| "Home folder is unavailable.".into());
    }
    if let Some(tail) = text.strip_prefix("~/").or_else(|| text.strip_prefix("~\\")) {
        return home
            .map(|p| p.join(tail))
            .ok_or_else(|| "Home folder is unavailable.".into());
    }
    let path = PathBuf::from(text);
    Ok(if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    })
}

/// Read and validate the destination before changing any active-tab state.
fn read_location(
    target: &Path,
    sort: &filex::settings::SortSettings,
) -> Result<(PathBuf, Vec<Entry>), String> {
    let path = std::fs::canonicalize(target)
        .map_err(|e| format!("Couldn’t open {}: {e}", target.display()))?;
    let entries = read_dir_sorted(&path, sort)
        .map_err(|e| format!("Couldn’t open {}: {e}", target.display()))?;
    Ok((path, entries))
}

impl Workspace {
    pub(super) fn sync_path(&mut self, cx: &mut Context<Self>) {
        let path = self.cwd.to_string_lossy().into_owned();
        if self.path_input.read(cx).text() != path {
            self.path_input
                .update(cx, |input, cx| input.set_text(path, cx));
        }
        self.path_error = None;
    }

    pub(super) fn focus_path(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.path_input.focus_handle(cx), cx);
        self.path_input
            .update(cx, |input, cx| input.select_all_text(cx));
        cx.notify();
    }

    pub(super) fn cancel_path(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.path_request = self.path_request.wrapping_add(1);
        self.path_loading = false;
        self.sync_path(cx);
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    pub(super) fn submit_path(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let target = match resolve(
            self.path_input.read(cx).text(),
            &self.cwd,
            self.user_dirs.get("home"),
        ) {
            Ok(path) => path,
            Err(error) => {
                self.path_error = Some(error.into());
                cx.notify();
                return;
            }
        };
        self.path_request = self.path_request.wrapping_add(1);
        let request = self.path_request;
        let sort = self.settings.read(cx).settings().sort;
        self.path_error = None;
        self.path_loading = true;
        window.focus(&self.focus_handle, cx);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { read_location(&target, &sort) })
                .await;
            this.update(cx, |this, cx| {
                if this.path_request != request {
                    return;
                }
                this.path_loading = false;
                match result {
                    Ok((path, entries)) => {
                        this.clear_search(cx);
                        if this.cwd != path {
                            this.history_back.push(this.cwd.clone());
                            this.history_forward.clear();
                        }
                        this.apply_directory(&path, entries, cx);
                        this.record_recent(path, cx);
                        this.refresh_preview(cx);
                    }
                    Err(error) => this.path_error = Some(error.into()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn render_location(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let theme = *cx.theme();
        let focused = self.path_input.focus_handle(cx).is_focused(window);
        div()
            .id("location-field")
            .key_context("PathInput")
            .flex()
            .flex_1()
            .min_w_0()
            .items_center()
            .gap_2()
            .h(px(36.))
            .px_3()
            .rounded_lg()
            .border_1()
            .border_color(if self.path_error.is_some() {
                theme.warn
            } else if focused {
                theme.accent
            } else {
                theme.border
            })
            .bg(theme.bg)
            .text_sm()
            .child(if self.path_loading {
                ui::icon::spinner(
                    "icons/loader-circle.svg",
                    theme.text_dim,
                    15.,
                    "path-loading",
                )
            } else {
                ui::icon::ui_icon("icons/folder.svg", theme.text_dim)
                    .size(px(15.))
                    .into_any_element()
            })
            .child(div().flex_1().min_w_0().child(self.path_input.clone()))
            .tooltip(ui::tooltip::text_tooltip(
                "Edit the path, then press Enter to go",
                theme,
            ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_readable_folders_are_accepted_as_destinations() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("年度 reports");
        std::fs::create_dir(&folder).unwrap();
        let file = folder.join("notes.txt");
        std::fs::write(&file, "notes").unwrap();
        let sort = filex::settings::SortSettings::default();
        let (path, entries) = read_location(&folder, &sort).unwrap();
        assert_eq!(path, std::fs::canonicalize(&folder).unwrap());
        assert_eq!(entries.len(), 1);
        assert!(read_location(&file, &sort).is_err());
        assert!(read_location(&folder.join("missing"), &sort).is_err());
    }

    #[test]
    fn relative_paths_and_home_are_expanded_without_losing_spaces() {
        let cwd = Path::new("/work/project");
        let home = Path::new("/users/person");
        assert_eq!(
            resolve("../Other Folder", cwd, Some(home)).unwrap(),
            cwd.join("../Other Folder")
        );
        assert_eq!(
            resolve("~/My Files", cwd, Some(home)).unwrap(),
            home.join("My Files")
        );
        assert_eq!(resolve("~", cwd, Some(home)).unwrap(), home);
        assert!(resolve(" ", cwd, Some(home)).is_err());
    }
    #[test]
    fn quoted_paths_preserve_unicode_and_spaces() {
        let cwd = Path::new("/work");
        assert_eq!(
            resolve("\"年度 reports\"", cwd, None).unwrap(),
            cwd.join("年度 reports")
        );
        assert!(resolve("~/files", cwd, None).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn absolute_paths_are_not_joined_to_cwd() {
        assert_eq!(
            resolve("/tmp/Files", Path::new("/work"), None).unwrap(),
            Path::new("/tmp/Files")
        );
    }
}

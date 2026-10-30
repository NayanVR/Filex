//! Editable location field and background path navigation.
use super::*;

/// Expand a location without touching disk. Relative paths use the active
/// folder; share addresses (`smb://…`, `\\server\share`) resolve against
/// the mounted `drives`.
fn resolve(
    text: &str,
    cwd: &Path,
    home: Option<&Path>,
    drives: &[Drive],
) -> Result<PathBuf, String> {
    let text = text.trim();
    let text = text
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(text);
    if text.is_empty() {
        return Err("Enter a folder path.".into());
    }
    if let Some(address) = filex::network::ShareAddress::parse(text) {
        return resolve_share(&address, drives, cfg!(windows));
    }
    if cfg!(windows)
        && (text.eq_ignore_ascii_case(filex::drives::THIS_PC_LABEL)
            || filex::drives::is_this_pc(Path::new(text)))
    {
        return Ok(filex::drives::this_pc());
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
    if path.is_absolute() {
        Ok(path)
    } else if filex::drives::is_this_pc(cwd) {
        // This PC isn't a folder, so there is nothing to be relative to.
        Err("Enter a full path, like C:\\Users.".into())
    } else {
        Ok(cwd.join(path))
    }
}

/// The folder for a share address. Windows opens SMB shares by UNC path
/// directly (`native_unc`); elsewhere a share has to be mounted first, so
/// the address maps into the mount that covers it. Connecting from here
/// is the next step (docs/design-network.md).
fn resolve_share(
    address: &filex::network::ShareAddress,
    drives: &[Drive],
    native_unc: bool,
) -> Result<PathBuf, String> {
    if native_unc && let Some(unc) = address.to_unc() {
        return Ok(PathBuf::from(unc));
    }
    filex::drives::locate_share(address, drives).ok_or_else(|| {
        format!(
            "{} isn’t connected. Mount it first, then open it here.",
            address.display_name()
        )
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
        let path = display_path(&self.cwd);
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
            &self.drives,
        ) {
            Ok(path) => path,
            Err(error) => {
                self.path_error = Some(error.into());
                cx.notify();
                return;
            }
        };
        if filex::drives::is_this_pc(&target) {
            window.focus(&self.focus_handle, cx);
            self.clear_search(cx);
            self.navigate(target, cx);
            return;
        }
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
        ui::top_bar::field(&theme, focused, self.path_error.is_some())
            .id("location-field")
            .key_context("PathInput")
            .flex_1()
            .min_w_0()
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
            .tooltip(self.action_tip("Edit the path, then press Enter", "location", cx))
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
            resolve("../Other Folder", cwd, Some(home), &[]).unwrap(),
            cwd.join("../Other Folder")
        );
        assert_eq!(
            resolve("~/My Files", cwd, Some(home), &[]).unwrap(),
            home.join("My Files")
        );
        assert_eq!(resolve("~", cwd, Some(home), &[]).unwrap(), home);
        assert!(resolve(" ", cwd, Some(home), &[]).is_err());
    }
    #[test]
    fn quoted_paths_preserve_unicode_and_spaces() {
        let cwd = Path::new("/work");
        assert_eq!(
            resolve("\"年度 reports\"", cwd, None, &[]).unwrap(),
            cwd.join("年度 reports")
        );
        assert!(resolve("~/files", cwd, None, &[]).is_err());
    }
    /// Regression guard: joining onto This PC would resolve against the
    /// process's working directory.
    #[test]
    fn relative_paths_from_this_pc_are_refused() {
        let this_pc = filex::drives::this_pc();
        assert!(resolve("Users", &this_pc, None, &[]).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn this_pc_can_be_typed_by_name() {
        let cwd = Path::new(r"C:\work");
        let this_pc = filex::drives::this_pc();
        assert_eq!(resolve("This PC", cwd, None, &[]).unwrap(), this_pc);
        assert_eq!(resolve("this pc", cwd, None, &[]).unwrap(), this_pc);
        assert_eq!(
            resolve(filex::drives::THIS_PC, cwd, None, &[]).unwrap(),
            this_pc
        );
        assert_eq!(
            resolve(r"D:\Data", &this_pc, None, &[]).unwrap(),
            Path::new(r"D:\Data")
        );
    }

    #[cfg(unix)]
    #[test]
    fn absolute_paths_are_not_joined_to_cwd() {
        assert_eq!(
            resolve("/tmp/Files", Path::new("/work"), None, &[]).unwrap(),
            Path::new("/tmp/Files")
        );
    }

    fn mounted(source: &str, fstype: &str, path: &str) -> Drive {
        Drive {
            name: "share".into(),
            path: path.into(),
            kind: filex::drives::DriveKind::Network,
            capacity: None,
            share: filex::network::ShareAddress::from_mount_source(source, fstype),
        }
    }

    #[test]
    fn typed_shares_open_their_mount_point() {
        let drives = [mounted("//nas/Media", "cifs", "/mnt/media")];
        let share = |text| filex::network::ShareAddress::parse(text).unwrap();
        assert_eq!(
            resolve_share(&share("smb://nas/Media/Movies"), &drives, false).unwrap(),
            Path::new("/mnt/media/Movies")
        );
        assert_eq!(
            resolve_share(&share(r"\\nas\media"), &drives, false).unwrap(),
            Path::new("/mnt/media")
        );
        let error = resolve_share(&share("smb://nas/Photos"), &drives, false).unwrap_err();
        assert!(error.starts_with("Photos (nas) isn’t connected"), "{error}");
    }

    #[test]
    fn windows_opens_smb_shares_by_unc_path() {
        let share = |text| filex::network::ShareAddress::parse(text).unwrap();
        assert_eq!(
            resolve_share(&share("smb://nas/Media/Movies"), &[], true).unwrap(),
            PathBuf::from(r"\\nas\Media\Movies")
        );
        // NFS has no UNC spelling: it still needs a mount.
        let drives = [mounted("fs1:/srv", "nfs", r"N:\")];
        assert_eq!(
            resolve_share(&share("nfs://fs1/srv/x"), &drives, true).unwrap(),
            Path::new(r"N:\").join("x")
        );
        assert!(resolve_share(&share("nfs://fs2/srv"), &drives, true).is_err());
    }

    #[test]
    fn share_addresses_skip_the_filesystem_path_rules() {
        let drives = [mounted("//nas/Media", "cifs", "/mnt/media")];
        let resolved = resolve("smb://nas/Media", Path::new("/work"), None, &drives);
        if cfg!(windows) {
            assert_eq!(resolved.unwrap(), PathBuf::from(r"\\nas\Media"));
        } else {
            assert_eq!(resolved.unwrap(), Path::new("/mnt/media"));
        }
    }
}

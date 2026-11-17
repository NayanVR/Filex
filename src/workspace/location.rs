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
/// the address maps into the mount that covers it. When it isn't, the
/// caller offers "Connect to Server" (FIL-59) with this message.
fn resolve_share(
    address: &filex::network::ShareAddress,
    drives: &[Drive],
    native_unc: bool,
) -> Result<PathBuf, String> {
    if native_unc && let Some(unc) = address.to_unc() {
        return Ok(PathBuf::from(unc));
    }
    filex::drives::locate_share(address, drives)
        .ok_or_else(|| format!("{} isn’t connected yet.", address.display_name()))
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
        // As every other surface writes it; `resolve` reads `~` back.
        let path = tilde(display_path(&self.cwd), self.user_dirs.get("home"));
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
        let text = self.path_input.read(cx).text().to_owned();
        // A share address that can't be opened gets "Connect to Server"
        // rather than a dead-end error.
        let typed_share = filex::network::ShareAddress::parse(&text).is_some();
        let target = match resolve(&text, &self.cwd, self.user_dirs.get("home"), &self.drives) {
            Ok(path) => path,
            Err(error) if typed_share => {
                self.sync_path(cx);
                self.open_connect(Some(text), Some(error.into()), window, cx);
                return;
            }
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
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { read_location(&target, &sort) })
                .await;
            this.update_in(cx, |this, window, cx| {
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
                    // Windows opens shares by UNC path, which fails when the
                    // share needs a sign-in the OS doesn't have yet.
                    Err(error) if typed_share => {
                        this.sync_path(cx);
                        this.open_connect(Some(text), Some(error.into()), window, cx);
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
            .child(if focused || self.path_error.is_some() {
                div()
                    .flex_1()
                    .min_w_0()
                    .child(self.path_input.clone())
                    .into_any_element()
            } else {
                // At rest, breadcrumbs: each folder on the way is a click
                // back to it, and the empty run after them edits the path
                // as text. Ancestors shrink first; the folder you're in
                // never does.
                let trail = collapse(crumbs(&self.cwd, self.user_dirs.get("home")));
                let last = trail.len().saturating_sub(1);
                let mut row = div().flex_1().min_w_0().flex().items_center().gap_0p5();
                for (i, (label, path)) in trail.into_iter().enumerate() {
                    if i > 0 {
                        row = row.child(
                            ui::icon::ui_icon("icons/chevron-right.svg", theme.text_dim)
                                .size(px(11.))
                                .flex_none(),
                        );
                    }
                    let crumb = div()
                        .id(("crumb", i))
                        .px_1()
                        .rounded_sm()
                        .cursor_pointer()
                        .hover(|s| s.bg(theme.hover))
                        .child(label);
                    // Whole names, never cut inside: the folded "…" is
                    // what absorbs depth. Only the parent may shrink, and
                    // only when the last two alone overflow.
                    row = row.child(if i == last {
                        crumb.flex_none().font_weight(gpui::FontWeight::MEDIUM)
                    } else {
                        crumb
                            .when(i + 1 == last, |c| c.min_w(px(28.)).truncate())
                            .when(i + 1 != last, |c| c.flex_none())
                            .text_color(theme.text_dim)
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                // The folded middle ("…") opens the path
                                // as text, where all of it can be read.
                                if path.as_os_str().is_empty() {
                                    this.focus_path(window, cx)
                                } else {
                                    this.navigate(path.clone(), cx)
                                }
                            }))
                    });
                }
                row.child(
                    div()
                        .id("location-edit")
                        .flex_1()
                        .h_full()
                        .min_w(px(24.))
                        .cursor_text()
                        .tooltip(self.action_tip("Type a path", "location", cx))
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.focus_path(window, cx)
                        })),
                )
                .into_any_element()
            })
    }
}

/// A deep trail keeps its start and the last three folders, with the
/// rest folded into one "…" crumb (an empty path): eight squeezed crumbs
/// read as noise, where four full ones read as a path.
fn collapse(mut trail: Vec<(String, PathBuf)>) -> Vec<(String, PathBuf)> {
    const KEEP: usize = 2;
    if trail.len() > KEEP + 2 {
        trail.splice(1..trail.len() - KEEP, [("…".to_owned(), PathBuf::new())]);
    }
    trail
}

/// The breadcrumb trail for `cwd`: each folder from the top down with the
/// path it opens. A folder inside the home folder starts at "Home".
fn crumbs(cwd: &Path, home: Option<&Path>) -> Vec<(String, PathBuf)> {
    if filex::drives::is_this_pc(cwd) {
        return vec![(display_path(cwd), cwd.to_path_buf())];
    }
    let mut trail: Vec<(String, PathBuf)> = cwd
        .ancestors()
        .map(|path| {
            let label = match path.file_name() {
                Some(name) => name.to_string_lossy().into_owned(),
                // The top of the tree reads as the sidebar names it, not
                // as a lone "/" beside the › separators.
                None if path == Path::new("/") => platform::machine_place().1.to_owned(),
                None => display_path(path),
            };
            (label, path.to_path_buf())
        })
        .collect();
    trail.reverse();
    if let Some(home) = home.filter(|_| !cfg!(windows))
        && let Some(at) = trail.iter().position(|(_, path)| path == home)
    {
        trail.drain(..at);
        trail[0].0 = "Home".into();
    }
    trail
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn the_breadcrumbs_start_at_home_or_the_root() {
        let labels = |cwd: &str, home: Option<&str>| {
            crumbs(Path::new(cwd), home.map(Path::new))
                .into_iter()
                .map(|(label, _)| label)
                .collect::<Vec<_>>()
        };
        if cfg!(windows) {
            return;
        }
        assert_eq!(
            labels("/Users/me/code/x", Some("/Users/me")),
            ["Home", "code", "x"]
        );
        assert_eq!(labels("/Users/me", Some("/Users/me")), ["Home"]);
        assert_eq!(
            labels("/tmp/a", Some("/Users/me")),
            ["Computer", "tmp", "a"]
        );
        // Each crumb opens its own folder.
        let trail = crumbs(Path::new("/tmp/a"), None);
        assert_eq!(trail[1].1, Path::new("/tmp"));
        // A deep one folds its middle.
        let deep = collapse(crumbs(Path::new("/a/b/c/d/e/f"), None));
        let deep: Vec<_> = deep.iter().map(|(label, _)| label.as_str()).collect();
        assert_eq!(deep, ["Computer", "…", "e", "f"]);
        assert_eq!(collapse(crumbs(Path::new("/a/b/c"), None)).len(), 4);
    }

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

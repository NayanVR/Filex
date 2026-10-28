//! Picker mode (`filex --pick`): the normal browser and search in a window
//! that answers another app's Open / Save / Select Folder request.
//!
//! The request arrives as JSON on stdin and exactly one answer leaves on
//! stdout (see `filex::picker`). Exiting without writing one means "use
//! the system dialog instead" to the caller, so every user-visible way
//! out — Cancel, Escape, closing the window — writes `Cancelled` first.

use super::*;
use filex::picker::{PickMode, PickRequest, PickResponse, SaveNameError, resolve_save_path};
use std::io::{Read as _, Write as _};
use std::sync::atomic::{AtomicBool, Ordering};

/// Marks the process as a picker before its window exists, so
/// `Workspace::new` skips app-lifetime chores (see there).
pub(super) struct PickerProcess;
impl gpui::Global for PickerProcess {}

/// Set once an answer has been written, so quitting after a pick does not
/// also write `Cancelled`.
static RESPONDED: AtomicBool = AtomicBool::new(false);

pub(super) struct PickerState {
    pub(super) request: PickRequest,
    pub(super) filter_index: usize,
    /// The file-name box (Save only).
    pub(super) name_input: Option<gpui::Entity<SearchInput>>,
    pub(super) error: Option<SharedString>,
    /// A Save target that exists and awaits "Replace?" confirmation.
    pub(super) replace: Option<PathBuf>,
    /// The type dropdown, anchored where it was clicked.
    pub(super) filter_menu: Option<Point<Pixels>>,
}

/// Entry point for `filex --pick`. Reads the request before any window
/// exists; a request this build cannot serve exits with status 2 and no
/// output, which tells the caller to show the system dialog.
pub fn run_picker() {
    let _logging_guard = filex::diagnostics::logging::init("filex-picker");
    let request = match read_request() {
        Ok(request) => request,
        Err(err) => {
            tracing::warn!("picker request rejected: {err:#}");
            // `exit` skips destructors: flush the log first.
            drop(_logging_guard);
            std::process::exit(2);
        }
    };
    tracing::info!(mode = ?request.mode, "picker starting");
    gpui_platform::application()
        .with_assets(ui::assets::Assets)
        .run(move |cx: &mut App| {
            app::init_app(cx);
            cx.set_global(PickerProcess);
            cx.on_app_quit(|_| async {
                respond(&PickResponse::Cancelled);
            })
            .detach();
            let title: SharedString = request.window_title().to_string().into();
            let owner = request.owner;
            let window = app::open_workspace_window(
                size(px(960.), px(620.)),
                Some(title),
                cx,
                move |ws, cx| ws.enter_picker(request, cx),
            );
            // Read the handle inside the update, move the window outside
            // it (see `owner_attacher`).
            let attach = owner.and_then(|owner| {
                window
                    .update(cx, |_, window, _| platform::owner_attacher(window, owner))
                    .ok()
                    .flatten()
            });
            if let Some(attach) = attach {
                cx.foreground_executor()
                    .spawn(async move { attach() })
                    .detach();
            }
        });
}

fn read_request() -> anyhow::Result<PickRequest> {
    let mut json = String::new();
    std::io::stdin().read_to_string(&mut json)?;
    let request: PickRequest = serde_json::from_str(&json)?;
    anyhow::ensure!(
        request.version == filex::picker::PROTOCOL_VERSION,
        "protocol version {} (expected {})",
        request.version,
        filex::picker::PROTOCOL_VERSION
    );
    Ok(request)
}

/// Write the one answer. Later calls are ignored.
fn respond(response: &PickResponse) {
    if RESPONDED.swap(true, Ordering::SeqCst) {
        return;
    }
    let mut out = std::io::stdout().lock();
    if let Ok(line) = serde_json::to_string(response) {
        let _ = writeln!(out, "{line}");
        let _ = out.flush();
    }
}

impl Workspace {
    pub(super) fn enter_picker(&mut self, request: PickRequest, cx: &mut Context<Self>) {
        let name_input = (request.mode == PickMode::SaveFile).then(|| {
            let input = cx.new(SearchInput::new);
            input.update(cx, |input, cx| {
                input.set_propagate_empty(false);
                input.set_placeholder("File name", cx);
                if let Some(name) = &request.file_name {
                    input.set_text(name.clone(), cx);
                    // As in the system dialog: the stem is selected, so
                    // typing renames without losing the extension.
                    input.select_range(0..filex::picker::stem_len(name), cx);
                }
            });
            input
        });
        let filter_index = request
            .filter_index
            .min(request.filters.len().saturating_sub(1));
        let start = request.folder.clone().unwrap_or_else(|| self.cwd.clone());
        self.picker = Some(PickerState {
            request,
            filter_index,
            name_input,
            error: None,
            replace: None,
            filter_menu: None,
        });
        self.load_dir(&start, cx);
    }

    /// Where keyboard focus starts: the file-name box for Save (Enter
    /// saves at once, as in the system dialog), else the file list.
    pub(super) fn initial_focus(&self, cx: &App) -> gpui::FocusHandle {
        self.picker
            .as_ref()
            .and_then(|p| p.name_input.as_ref())
            .map_or_else(|| self.focus_handle.clone(), |input| input.focus_handle(cx))
    }

    /// Whether a row belongs in the picker's lists: folders always (to
    /// navigate, or to pick in folder mode); files only outside folder mode
    /// and when the active type filter accepts their name. Everything
    /// shows when not picking.
    pub(super) fn picker_shows(&self, name: &str, is_dir: bool) -> bool {
        let Some(picker) = &self.picker else {
            return true;
        };
        if is_dir {
            return true;
        }
        picker.request.mode != PickMode::PickFolder
            && picker
                .request
                .filter(picker.filter_index)
                .is_none_or(|filter| filter.matches(name))
    }

    /// Apply [`Self::picker_shows`] to a folder listing.
    pub(super) fn picker_filter_entries(&self, entries: &mut Vec<Entry>) {
        if self.picker.is_some() {
            entries.retain(|e| self.picker_shows(&e.name, e.is_dir));
        }
    }

    /// Enter / double-click on a file while picking. Returns `false` when
    /// not in picker mode, so the caller opens the file normally.
    pub(super) fn picker_activate_file(
        &mut self,
        path: &Path,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(picker) = &self.picker else {
            return false;
        };
        match picker.request.mode {
            PickMode::OpenFile => self.picker_confirm(window, cx),
            PickMode::SaveFile => {
                self.picker_set_name(path, cx);
                self.picker_confirm(window, cx);
            }
            PickMode::PickFolder => {}
        }
        true
    }

    /// A single click on a file in Save mode copies its name into the box,
    /// as the system dialog does.
    pub(super) fn picker_set_name(&mut self, path: &Path, cx: &mut Context<Self>) {
        let Some(input) = self.picker.as_ref().and_then(|p| p.name_input.clone()) else {
            return;
        };
        if let Some(name) = path.file_name() {
            let name = name.to_string_lossy().into_owned();
            input.update(cx, |input, cx| input.set_text(name, cx));
        }
    }

    /// The file at row `ix` of the active list, when picking a Save name.
    pub(super) fn picker_clicked_file(&self, ix: usize) -> Option<PathBuf> {
        let picker = self.picker.as_ref()?;
        if picker.request.mode != PickMode::SaveFile {
            return None;
        }
        if self.query.is_empty() {
            let entry = self.entries.get(ix)?;
            (!entry.is_dir).then(|| entry.path.clone())
        } else {
            let row = self.results.get(ix)?;
            (!row.is_dir).then(|| row.target.clone())
        }
    }

    /// The selected rows of the active list as (path, is_dir).
    fn picker_selection(&self) -> Vec<(PathBuf, bool)> {
        let selection = self.active_selection();
        if self.query.is_empty() {
            selection
                .iter()
                .filter_map(|ix| self.entries.get(ix))
                .map(|e| (e.path.clone(), e.is_dir))
                .collect()
        } else {
            selection
                .iter()
                .filter_map(|ix| self.results.get(ix))
                .map(|r| (r.target.clone(), r.is_dir))
                .collect()
        }
    }

    /// The confirm button / Enter in the name box.
    pub(super) fn picker_confirm(&mut self, _window: Option<&mut Window>, cx: &mut Context<Self>) {
        let Some(picker) = &self.picker else {
            return;
        };
        // Enter while "Replace?" is up answers it (focus stays in the name
        // box under the modal, so Enter arrives here).
        if picker.replace.is_some() {
            self.picker_resolve_replace(true, cx);
            return;
        }
        let outcome = match picker.request.mode {
            PickMode::OpenFile => self.confirm_open(),
            PickMode::PickFolder => self.confirm_folder(),
            PickMode::SaveFile => self.confirm_save(cx),
        };
        match outcome {
            Confirm::Done(paths) => self.picker_finish(paths, cx),
            Confirm::Navigate(dir) => {
                self.clear_search(cx);
                self.navigate(dir, cx);
            }
            Confirm::Check(path) => self.check_save_target(path, cx),
            Confirm::Error(message) => {
                if let Some(picker) = &mut self.picker {
                    picker.error = Some(message.into());
                }
            }
            Confirm::Nothing => {}
        }
        cx.notify();
    }

    fn confirm_open(&self) -> Confirm {
        let multiple = self.picker.as_ref().is_some_and(|p| p.request.multiple);
        let selected = self.picker_selection();
        // One folder selected: go into it, as Enter does in the system dialog.
        if let [(dir, true)] = selected.as_slice() {
            return Confirm::Navigate(dir.clone());
        }
        let files: Vec<PathBuf> = selected
            .into_iter()
            .filter(|(_, is_dir)| !is_dir)
            .map(|(path, _)| path)
            .collect();
        match files.len() {
            0 => Confirm::Nothing,
            n if n > 1 && !multiple => Confirm::Error("Select one file.".into()),
            _ => Confirm::Done(files),
        }
    }

    fn confirm_folder(&self) -> Confirm {
        let selected = self.picker_selection();
        let dir = match selected.as_slice() {
            [(dir, true)] => dir.clone(),
            _ => self.cwd.clone(),
        };
        if filex::drives::is_this_pc(&dir) {
            return Confirm::Error("Choose a folder or drive.".into());
        }
        Confirm::Done(vec![dir])
    }

    fn confirm_save(&self, cx: &App) -> Confirm {
        let Some(picker) = &self.picker else {
            return Confirm::Nothing;
        };
        let typed = picker
            .name_input
            .as_ref()
            .map(|input| input.read(cx).text().to_string())
            .unwrap_or_default();
        if filex::drives::is_this_pc(&self.cwd) && !Path::new(typed.trim()).is_absolute() {
            return Confirm::Error("Open a folder to save into.".into());
        }
        let path = match resolve_save_path(&self.cwd, &typed, &picker.request, picker.filter_index)
        {
            Ok(path) => path,
            Err(SaveNameError::Empty) => return Confirm::Error("Type a file name.".into()),
            Err(SaveNameError::InvalidCharacter(c)) => {
                return Confirm::Error(format!("File names can't contain “{c}”."));
            }
        };
        Confirm::Check(path)
    }

    /// Whether a Save target is a folder (go there), an existing file (ask
    /// before replacing) or new (done). The stat runs off the UI thread:
    /// the typed path may be on a slow network share.
    fn check_save_target(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let confirm_overwrite = self
            .picker
            .as_ref()
            .is_some_and(|p| p.request.confirm_overwrite);
        cx.spawn(async move |this, cx| {
            let probe = path.clone();
            let kind = cx
                .background_executor()
                .spawn(async move { std::fs::metadata(&probe).map(|m| m.is_dir()).ok() })
                .await;
            this.update(cx, |this, cx| match kind {
                Some(true) => {
                    this.clear_search(cx);
                    this.navigate(path, cx);
                }
                Some(false) if confirm_overwrite => {
                    if let Some(picker) = &mut this.picker {
                        picker.replace = Some(path);
                    }
                    cx.notify();
                }
                _ => this.picker_finish(vec![path], cx),
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn picker_resolve_replace(&mut self, replace: bool, cx: &mut Context<Self>) {
        let Some(path) = self.picker.as_mut().and_then(|p| p.replace.take()) else {
            return;
        };
        if replace {
            self.picker_finish(vec![path], cx);
        }
        cx.notify();
    }

    fn picker_finish(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let filter_index = self.picker.as_ref().map_or(0, |p| p.filter_index);
        tracing::info!(count = paths.len(), "picker confirmed");
        respond(&PickResponse::Picked {
            paths: filex::picker::host_paths(paths),
            filter_index,
        });
        cx.quit();
    }

    pub(super) fn picker_cancel(&mut self, cx: &mut Context<Self>) {
        respond(&PickResponse::Cancelled);
        cx.quit();
    }

    pub(super) fn set_picker_filter(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(picker) = &mut self.picker else {
            return;
        };
        picker.filter_menu = None;
        if picker.filter_index != index {
            picker.filter_index = index;
            let cwd = self.cwd.clone();
            self.load_dir(&cwd, cx);
            if !self.query.is_empty() {
                self.update_search(cx);
            }
        }
        cx.notify();
    }

    /// The bottom bar: type filter, file name (Save), Cancel and confirm.
    pub(super) fn render_picker_bar(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let picker = self.picker.as_ref()?;
        let theme = *cx.theme();
        // File types mean nothing when choosing a folder.
        let filter = picker
            .request
            .filter(picker.filter_index)
            .filter(|_| picker.request.mode != PickMode::PickFolder)
            .map(|f| {
                ui::button::button(
                    &theme,
                    "picker-filter",
                    format!("{}  ▾", f.name),
                    ui::button::Variant::Secondary,
                    ui::button::Size::Regular,
                )
                .max_w(px(260.))
                .on_click(cx.listener(|this, event: &ClickEvent, window, cx| {
                    let position = Self::clamped_menu_position(event.position(), 120., window);
                    if let Some(picker) = &mut this.picker {
                        picker.filter_menu = Some(position);
                    }
                    cx.notify();
                }))
            });
        let name = picker.name_input.as_ref().map(|input| {
            let focused = input.focus_handle(cx).is_focused(window);
            ui::top_bar::field(&theme, focused, picker.error.is_some())
                .id("picker-name")
                // Reuses the path box's bindings: Enter submits, Escape
                // cancels, and single-key shortcuts stay typeable.
                .key_context("PathInput")
                .on_action(cx.listener(|this, _: &SubmitPath, window, cx| {
                    this.picker_confirm(Some(window), cx);
                }))
                .on_action(cx.listener(|this, _: &CancelPath, _window, cx| {
                    // Escape backs out of "Replace?" first, then cancels.
                    if this.picker.as_ref().is_some_and(|p| p.replace.is_some()) {
                        this.picker_resolve_replace(false, cx);
                    } else {
                        this.picker_cancel(cx);
                    }
                }))
                .flex_1()
                .min_w_0()
                .child(div().flex_1().min_w_0().child(input.clone()))
        });
        Some(
            div()
                .flex()
                .flex_none()
                .flex_col()
                .gap_1()
                .px_4()
                .py_3()
                .border_t_1()
                .border_color(theme.border)
                .children(
                    picker
                        .error
                        .clone()
                        .map(|error| div().text_xs().text_color(theme.warn).child(error)),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        // The asking app's own words ("Export report"):
                        // the custom title bar does not show the window
                        // title, so the bar carries it.
                        .child(
                            div()
                                .flex_none()
                                .max_w(px(240.))
                                .truncate()
                                .text_sm()
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .child(picker.request.window_title().to_string()),
                        )
                        .children(name)
                        .when(picker.name_input.is_none(), |row| row.child(div().flex_1()))
                        .children(filter)
                        .child(
                            ui::button::button(
                                &theme,
                                "picker-cancel",
                                "Cancel",
                                ui::button::Variant::Secondary,
                                ui::button::Size::Regular,
                            )
                            .on_click(cx.listener(
                                |this, _: &ClickEvent, _window, cx| {
                                    this.picker_cancel(cx);
                                },
                            )),
                        )
                        .child(
                            ui::button::button(
                                &theme,
                                "picker-confirm",
                                picker.request.confirm_label().to_string(),
                                ui::button::Variant::Primary,
                                ui::button::Size::Regular,
                            )
                            .on_click(cx.listener(
                                |this, _: &ClickEvent, window, cx| {
                                    this.picker_confirm(Some(window), cx);
                                },
                            )),
                        ),
                )
                .into_any_element(),
        )
    }

    pub(super) fn render_picker_filter_menu(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let picker = self.picker.as_ref()?;
        let position = picker.filter_menu?;
        let theme = *cx.theme();
        let items = picker
            .request
            .filters
            .iter()
            .enumerate()
            .map(|(i, filter)| {
                let mark = if i == picker.filter_index { "✓" } else { " " };
                let label = format!("{mark}  {}", filter.name);
                ui::menu::item(&theme, ("picker-filter-item", i), label, false)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                        this.set_picker_filter(i, cx);
                    }))
                    .into_any_element()
            })
            .collect();
        Some(
            ui::menu::overlay("picker-filter-overlay")
                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                    if let Some(picker) = &mut this.picker {
                        picker.filter_menu = None;
                    }
                    cx.notify();
                }))
                .child(ui::menu::panel(&theme, position, items))
                .into_any_element(),
        )
    }

    pub(super) fn render_picker_replace_modal(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let path = self.picker.as_ref()?.replace.as_ref()?;
        let theme = *cx.theme();
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        Some(ui::modal::present(
            "picker-replace-in",
            ui::modal::backdrop("picker-replace-backdrop")
                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                    this.picker_resolve_replace(false, cx);
                }))
                .child(
                    ui::modal::panel(&theme, "picker-replace-panel")
                        .on_click(|_, _, cx| cx.stop_propagation())
                        .child(ui::modal::title(&theme, format!("“{name}” already exists")))
                        .child(ui::modal::message(&theme, "Do you want to replace it?"))
                        .child(
                            ui::modal::buttons()
                                .child(
                                    ui::modal::button(&theme, "picker-replace-no", "No", false)
                                        .on_click(cx.listener(
                                            |this, _: &ClickEvent, _window, cx| {
                                                this.picker_resolve_replace(false, cx);
                                            },
                                        )),
                                )
                                .child(
                                    ui::modal::button(
                                        &theme,
                                        "picker-replace-yes",
                                        "Replace",
                                        true,
                                    )
                                    .on_click(cx.listener(
                                        |this, _: &ClickEvent, _window, cx| {
                                            this.picker_resolve_replace(true, cx);
                                        },
                                    )),
                                ),
                        ),
                ),
        ))
    }
}

/// What the confirm button does with the current state.
enum Confirm {
    Done(Vec<PathBuf>),
    Navigate(PathBuf),
    /// A Save path to classify before finishing ([`Workspace::check_save_target`]).
    Check(PathBuf),
    Error(String),
    Nothing,
}

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{
    App, Application, Bounds, ClickEvent, Context, FocusHandle, KeyBinding, KeyDownEvent,
    SharedString, TitlebarOptions, Window, WindowBounds, WindowOptions, actions, div, prelude::*,
    px, rgb, size, uniform_list,
};

use filex::index::VolumeIndex;
use filex::index::walker::{FsWalkSource, IndexSource};
use filex::listing::{Entry, format_size, read_dir_sorted};

actions!(filex, [Quit, CloseWindow, GoUp]);

// Palette (dark). Placeholder until a real theme system lands.
const BG: u32 = 0x1e2227;
const BG_PANEL: u32 = 0x23272e;
const BG_HOVER: u32 = 0x2f343c;
const BORDER: u32 = 0x363c45;
const TEXT: u32 = 0xd7dae0;
const TEXT_DIM: u32 = 0x8b929e;
const ACCENT: u32 = 0x5ac8fa;

const SEARCH_RESULT_LIMIT: usize = 500;

enum IndexStatus {
    Building,
    Ready { files: usize },
    Failed(SharedString),
}

/// A search hit prepared for display (paths pre-materialized off-thread).
struct SearchRow {
    name: SharedString,
    path_label: SharedString,
    target: PathBuf,
    is_dir: bool,
}

struct Workspace {
    focus_handle: FocusHandle,
    cwd: PathBuf,
    entries: Vec<Entry>,
    load_error: Option<SharedString>,
    index: Option<Arc<VolumeIndex>>,
    index_status: IndexStatus,
    query: String,
    results: Vec<SearchRow>,
    search_generation: u64,
}

impl Workspace {
    fn new(cx: &mut Context<Self>) -> Self {
        let cwd = std::env::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            cwd: cwd.clone(),
            entries: Vec::new(),
            load_error: None,
            index: None,
            index_status: IndexStatus::Building,
            query: String::new(),
            results: Vec::new(),
            search_generation: 0,
        };
        this.load_dir(&cwd);
        this.spawn_bootstrap_index(cwd, cx);
        this
    }

    /// Build the volume index on the background executor; the UI thread only
    /// receives the finished result.
    fn spawn_bootstrap_index(&self, root: PathBuf, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { FsWalkSource::default().bootstrap(&root) })
                .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(index) => {
                        this.index_status = IndexStatus::Ready { files: index.len() };
                        this.index = Some(Arc::new(index));
                        // A query typed while indexing can now be answered.
                        this.update_search(cx);
                    }
                    Err(err) => {
                        this.index_status = IndexStatus::Failed(format!("{err:#}").into());
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn load_dir(&mut self, path: &Path) {
        match read_dir_sorted(path) {
            Ok(entries) => {
                self.cwd = path.to_path_buf();
                self.entries = entries;
                self.load_error = None;
            }
            Err(err) => {
                self.load_error = Some(format!("{err:#}").into());
            }
        }
    }

    fn navigate(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.load_dir(&path);
        cx.notify();
    }

    fn go_up(&mut self, cx: &mut Context<Self>) {
        if let Some(parent) = self.cwd.parent().map(Path::to_path_buf) {
            self.navigate(parent, cx);
        }
    }

    fn clear_search(&mut self, cx: &mut Context<Self>) {
        self.query.clear();
        self.update_search(cx);
    }

    /// Interim search input: characters are captured from raw key events.
    /// Replace with a real text input view (gpui-component's Input or an
    /// EntityInputHandler) once the shell grows edit affordances.
    fn handle_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        if keystroke.modifiers.platform || keystroke.modifiers.control {
            return; // shortcuts are handled by actions
        }
        match keystroke.key.as_str() {
            "backspace" => {
                if self.query.is_empty() {
                    self.go_up(cx);
                } else {
                    self.query.pop();
                    self.update_search(cx);
                }
            }
            "escape" => self.clear_search(cx),
            _ => {
                if let Some(text) = &keystroke.key_char
                    && !text.chars().any(char::is_control)
                {
                    self.query.push_str(text);
                    self.update_search(cx);
                }
            }
        }
    }

    /// Kick off an index query on the background executor. Stale completions
    /// (an older keystroke finishing after a newer one) are dropped by
    /// generation check, keeping search-as-you-type strictly ordered.
    fn update_search(&mut self, cx: &mut Context<Self>) {
        self.search_generation += 1;
        let generation = self.search_generation;
        cx.notify();

        if self.query.is_empty() {
            self.results.clear();
            return;
        }
        let Some(index) = self.index.clone() else {
            return; // still building; bootstrap completion re-runs the query
        };
        let query = self.query.clone();

        cx.spawn(async move |this, cx| {
            let rows = cx
                .background_executor()
                .spawn(async move {
                    index
                        .search(&query, SEARCH_RESULT_LIMIT)
                        .into_iter()
                        .filter_map(|hit| {
                            let path = index.path_of(hit.id)?;
                            Some(SearchRow {
                                name: index.name_of(hit.id)?.to_string().into(),
                                path_label: path.display().to_string().into(),
                                is_dir: index.is_dir(hit.id)?,
                                target: path,
                            })
                        })
                        .collect::<Vec<_>>()
                })
                .await;
            this.update(cx, |this, cx| {
                if this.search_generation == generation {
                    this.results = rows;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn open_search_result(&mut self, row_target: PathBuf, is_dir: bool, cx: &mut Context<Self>) {
        let destination = if is_dir {
            Some(row_target)
        } else {
            row_target.parent().map(Path::to_path_buf)
        };
        if let Some(destination) = destination {
            self.navigate(destination, cx);
        }
        self.clear_search(cx);
    }

    fn render_top_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let searching = !self.query.is_empty();
        div()
            .flex()
            .items_center()
            .gap_2()
            .h(px(40.))
            .px_3()
            .border_b_1()
            .border_color(rgb(BORDER))
            .bg(rgb(BG_PANEL))
            .child(
                div()
                    .id("up")
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(BG_HOVER)))
                    .text_color(rgb(TEXT_DIM))
                    .child("↑")
                    .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                        this.go_up(cx);
                    })),
            )
            .child(
                div()
                    .flex_1()
                    .text_sm()
                    .text_color(rgb(TEXT_DIM))
                    .overflow_hidden()
                    .child(self.cwd.display().to_string()),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(if searching { ACCENT } else { BORDER }))
                    .text_sm()
                    .child(if searching {
                        div().text_color(rgb(TEXT)).child(self.query.clone())
                    } else {
                        div().text_color(rgb(TEXT_DIM)).child("type to search")
                    }),
            )
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let places: Vec<(&str, PathBuf)> = [
            ("Home", std::env::home_dir()),
            ("Root", Some(PathBuf::from("/"))),
        ]
        .into_iter()
        .filter_map(|(label, path)| Some((label, path?)))
        .collect();

        div()
            .flex()
            .flex_col()
            .w(px(180.))
            .h_full()
            .py_2()
            .border_r_1()
            .border_color(rgb(BORDER))
            .bg(rgb(BG_PANEL))
            .child(
                div()
                    .px_3()
                    .pb_1()
                    .text_xs()
                    .text_color(rgb(TEXT_DIM))
                    .child("PLACES"),
            )
            .children(places.into_iter().enumerate().map(|(ix, (label, path))| {
                div()
                    .id(ix)
                    .mx_2()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .text_sm()
                    .hover(|s| s.bg(rgb(BG_HOVER)))
                    .child(label)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                        this.navigate(path.clone(), cx);
                    }))
            }))
    }

    fn render_file_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        uniform_list(
            "entries",
            self.entries.len(),
            cx.processor(|this, range: Range<usize>, _window, cx| {
                range
                    .filter_map(|ix| {
                        let entry = this.entries.get(ix)?;
                        let path = entry.path.clone();
                        let is_dir = entry.is_dir;
                        Some(
                            div()
                                .id(ix)
                                .flex()
                                .items_center()
                                .gap_2()
                                .h(px(28.))
                                .px_3()
                                .cursor_pointer()
                                .hover(|s| s.bg(rgb(BG_HOVER)))
                                .child(
                                    div()
                                        .w(px(16.))
                                        .text_color(rgb(if is_dir { ACCENT } else { TEXT_DIM }))
                                        .child(if is_dir { "▸" } else { "·" }),
                                )
                                .child(div().flex_1().text_sm().child(entry.name.clone()))
                                .child(div().text_xs().text_color(rgb(TEXT_DIM)).child(
                                    if is_dir {
                                        "—".to_string()
                                    } else {
                                        format_size(entry.size)
                                    },
                                ))
                                .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                                    if is_dir {
                                        this.navigate(path.clone(), cx);
                                    }
                                })),
                        )
                    })
                    .collect()
            }),
        )
        .flex_1()
    }

    fn render_search_results(&self, cx: &mut Context<Self>) -> impl IntoElement {
        uniform_list(
            "results",
            self.results.len(),
            cx.processor(|this, range: Range<usize>, _window, cx| {
                range
                    .filter_map(|ix| {
                        let row = this.results.get(ix)?;
                        let target = row.target.clone();
                        let is_dir = row.is_dir;
                        Some(
                            div()
                                .id(ix)
                                .flex()
                                .items_center()
                                .gap_2()
                                .h(px(28.))
                                .px_3()
                                .cursor_pointer()
                                .hover(|s| s.bg(rgb(BG_HOVER)))
                                .child(
                                    div()
                                        .w(px(16.))
                                        .text_color(rgb(if is_dir { ACCENT } else { TEXT_DIM }))
                                        .child(if is_dir { "▸" } else { "·" }),
                                )
                                .child(div().text_sm().child(row.name.clone()))
                                .child(
                                    div()
                                        .flex_1()
                                        .text_xs()
                                        .text_color(rgb(TEXT_DIM))
                                        .overflow_hidden()
                                        .child(row.path_label.clone()),
                                )
                                .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                                    this.open_search_result(target.clone(), is_dir, cx);
                                })),
                        )
                    })
                    .collect()
            }),
        )
        .flex_1()
    }

    fn render_search_pane(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        if let IndexStatus::Building = self.index_status {
            return div()
                .flex_1()
                .p_4()
                .text_sm()
                .text_color(rgb(TEXT_DIM))
                .child("still indexing — results will appear when ready…")
                .into_any_element();
        }
        if self.results.is_empty() {
            return div()
                .flex_1()
                .p_4()
                .text_sm()
                .text_color(rgb(TEXT_DIM))
                .child(format!("no matches for “{}”", self.query))
                .into_any_element();
        }
        self.render_search_results(cx).into_any_element()
    }

    fn render_status_bar(&self) -> impl IntoElement {
        let left: SharedString = match &self.load_error {
            Some(err) => format!("error — {err}").into(),
            None if self.query.is_empty() => format!("{} items", self.entries.len()).into(),
            None => format!(
                "{} result{}",
                self.results.len(),
                if self.results.len() == 1 { "" } else { "s" }
            )
            .into(),
        };
        let right: SharedString = match &self.index_status {
            IndexStatus::Building => "indexing…".into(),
            IndexStatus::Ready { files } => format!("{files} files indexed").into(),
            IndexStatus::Failed(err) => format!("index failed — {err}").into(),
        };
        div()
            .flex()
            .items_center()
            .justify_between()
            .h(px(26.))
            .px_3()
            .border_t_1()
            .border_color(rgb(BORDER))
            .bg(rgb(BG_PANEL))
            .text_xs()
            .text_color(rgb(TEXT_DIM))
            .child(left)
            .child(right)
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let searching = !self.query.is_empty();
        div()
            .track_focus(&self.focus_handle)
            .on_action(|_: &CloseWindow, window, _| window.remove_window())
            .on_action(cx.listener(|this, _: &GoUp, _window, cx| this.go_up(cx)))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _window, cx| {
                this.handle_key(event, cx);
            }))
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
            .child(self.render_top_bar(cx))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_sidebar(cx))
                    .child(if searching {
                        self.render_search_pane(cx)
                    } else {
                        self.render_file_list(cx).into_any_element()
                    }),
            )
            .child(self.render_status_bar())
    }
}

fn main() {
    Application::new().run(|cx: &mut App| {
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.bind_keys([
            #[cfg(target_os = "macos")]
            KeyBinding::new("cmd-q", Quit, None),
            #[cfg(target_os = "macos")]
            KeyBinding::new("cmd-w", CloseWindow, None),
            #[cfg(target_os = "macos")]
            KeyBinding::new("cmd-up", GoUp, None),
            #[cfg(not(target_os = "macos"))]
            KeyBinding::new("ctrl-q", Quit, None),
            #[cfg(not(target_os = "macos"))]
            KeyBinding::new("ctrl-w", CloseWindow, None),
            #[cfg(not(target_os = "macos"))]
            KeyBinding::new("alt-up", GoUp, None),
        ]);
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        let bounds = Bounds::centered(None, size(px(1000.), px(700.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("filex".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |window, cx| {
                cx.activate(true);
                cx.new(|cx| {
                    let workspace = Workspace::new(cx);
                    workspace.focus_handle.focus(window);
                    workspace
                })
            },
        )
        .expect("failed to open the main window");
    });
}

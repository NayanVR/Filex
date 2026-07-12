mod listing;

use std::ops::Range;
use std::path::{Path, PathBuf};

use gpui::{
    App, Application, Bounds, ClickEvent, Context, FocusHandle, KeyBinding, SharedString,
    TitlebarOptions, Window, WindowBounds, WindowOptions, actions, div, prelude::*, px, rgb, size,
    uniform_list,
};

use listing::{Entry, format_size, read_dir_sorted};

actions!(filex, [Quit, CloseWindow, GoUp]);

// Palette (dark). Placeholder until a real theme system lands.
const BG: u32 = 0x1e2227;
const BG_PANEL: u32 = 0x23272e;
const BG_HOVER: u32 = 0x2f343c;
const BORDER: u32 = 0x363c45;
const TEXT: u32 = 0xd7dae0;
const TEXT_DIM: u32 = 0x8b929e;
const ACCENT: u32 = 0x5ac8fa;

struct Workspace {
    focus_handle: FocusHandle,
    cwd: PathBuf,
    entries: Vec<Entry>,
    load_error: Option<SharedString>,
}

impl Workspace {
    fn new(cx: &mut Context<Self>) -> Self {
        let cwd = std::env::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            cwd: cwd.clone(),
            entries: Vec::new(),
            load_error: None,
        };
        this.load_dir(&cwd);
        this
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

    fn render_top_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
                    .child(self.cwd.display().to_string()),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(TEXT_DIM))
                    .child("search: soon"),
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

    fn render_status_bar(&self) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .h(px(26.))
            .px_3()
            .border_t_1()
            .border_color(rgb(BORDER))
            .bg(rgb(BG_PANEL))
            .text_xs()
            .text_color(rgb(TEXT_DIM))
            .child(match &self.load_error {
                Some(err) => SharedString::from(format!("error — {err}")),
                None => format!("{} items", self.entries.len()).into(),
            })
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .track_focus(&self.focus_handle)
            .on_action(|_: &CloseWindow, window, _| window.remove_window())
            .on_action(cx.listener(|this, _: &GoUp, _window, cx| this.go_up(cx)))
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
                    .child(self.render_file_list(cx)),
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
            #[cfg(not(target_os = "macos"))]
            KeyBinding::new("ctrl-q", Quit, None),
            #[cfg(not(target_os = "macos"))]
            KeyBinding::new("ctrl-w", CloseWindow, None),
            KeyBinding::new("backspace", GoUp, None),
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

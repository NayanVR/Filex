//! The `Render` impl and top-level chrome: top bar, editable location,
//! sidebar, panes, status bar, tab bar, and view toggles.

use super::*;

/// What a top-bar navigation button does. An enum rather than four
/// near-identical `.child(toolbar_button(..).on_click(..))` blocks, so the
/// buttons are a table and the wiring is written once.
#[derive(Clone, Copy)]
enum NavAction {
    Reload,
    Back,
    Forward,
    Up,
}

impl NavAction {
    fn run(self, workspace: &mut Workspace, cx: &mut Context<Workspace>) {
        match self {
            Self::Reload => workspace.reload_dir(cx),
            Self::Back => workspace.go_back(cx),
            Self::Forward => workspace.go_forward(cx),
            Self::Up => workspace.go_up(cx),
        }
    }
}

impl Workspace {
    /// Flip the browse layout between list and grid (persisted).
    pub(super) fn toggle_view(&mut self, cx: &mut Context<Self>) {
        self.settings.update(cx, |store, cx| {
            store.update(cx, |settings| {
                settings.view = match settings.view {
                    ViewMode::List => ViewMode::Grid,
                    ViewMode::Grid => ViewMode::List,
                };
            });
        });
    }

    /// Step the grid card size, clamped to the valid range (persisted).
    pub(super) fn zoom_grid(&mut self, delta: i8, cx: &mut Context<Self>) {
        self.settings.update(cx, |store, cx| {
            store.update(cx, |settings| {
                let max = ui::grid::max_zoom() as i16;
                settings.grid_zoom =
                    ((settings.grid_zoom as i16 + delta as i16).clamp(0, max)) as u8;
            });
        });
    }

    /// The navigation bar (second row): refresh/back/forward/up, the path
    /// field, then the search box pinned to the right.
    pub(super) fn render_top_bar(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = *cx.theme();
        let (can_back, can_forward) = (
            !self.history_back.is_empty(),
            !self.history_forward.is_empty(),
        );
        let dim = |enabled: bool| {
            if enabled {
                theme.text_dim
            } else {
                theme.border
            }
        };
        // Left: the navigation controls followed by the path, taking the
        // flexible share so a long path stays within the available space
        // before the search box (which keeps its fixed width on the right).
        let left = div()
            .flex_1()
            .min_w_0()
            .flex()
            .items_center()
            .gap_1()
            .overflow_hidden()
            .children(
                [
                    (
                        "back",
                        "icons/chevron-left.svg",
                        dim(can_back),
                        NavAction::Back,
                    ),
                    (
                        "forward",
                        "icons/chevron-right.svg",
                        dim(can_forward),
                        NavAction::Forward,
                    ),
                    ("up", "icons/arrow-up.svg", theme.text_dim, NavAction::Up),
                    (
                        "refresh",
                        "icons/refresh-cw.svg",
                        theme.text_dim,
                        NavAction::Reload,
                    ),
                ]
                .map(|(id, icon, color, action)| {
                    let label = match action {
                        NavAction::Back => "Go back",
                        NavAction::Forward => "Go forward",
                        NavAction::Up => "Parent folder",
                        NavAction::Reload => "Refresh folder",
                    };
                    ui::top_bar::toolbar_button(&theme, id, icon, color)
                        .tooltip(ui::tooltip::text_tooltip(label, theme))
                        .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                            action.run(this, cx)
                        }))
                }),
            )
            .child(self.render_location(window, cx));

        // The magic toggle lights for the *effective* mode, not just the
        // explicit flag: a command typed in normal mode auto-switches the
        // app into magic view, and the icon must say so — otherwise the
        // results list vanishes with no visible reason. `in_magic_view`
        // is the same predicate the render path branches on.
        // A definite width so the search box's `w_full` resolves (a
        // `flex_none` parent would otherwise shrink-wrap to nothing); the
        // box's own `max_w` still caps it. The nav controls to the left
        // take the remaining space and shrink first on a narrow window.
        let search = div().flex_none().w(px(340.)).flex().justify_end().child(
            ui::top_bar::search_box(&theme, !self.query.is_empty() || self.in_magic_view())
                .child(
                    ui::top_bar::scope_selector(
                        &theme,
                        self.search_scope.label(),
                        self.scope_menu.is_some(),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseDownEvent, window, cx| {
                            this.open_scope_menu(event.position, window, cx);
                        }),
                    ),
                )
                .child(div().flex_1().min_w_0().child(self.search_input.clone()))
                // The active search shortcut. Hidden once a
                // query is present so it never crowds the typed text.
                .children(self.query.is_empty().then(|| {
                    ui::kbd::keycap(
                        &theme,
                        shortcuts::hint(
                            "search",
                            &self.settings.read(cx).settings().keyboard_shortcuts,
                        ),
                    )
                }))
                .children((!self.query.is_empty()).then(|| {
                    ui::top_bar::toolbar_button(
                        &theme,
                        "clear-search",
                        "icons/x.svg",
                        theme.text_dim,
                    )
                    .size(px(22.))
                    .tooltip(ui::tooltip::text_tooltip("Clear search", theme))
                    .on_click(cx.listener(
                        |this, _: &ClickEvent, window, cx| {
                            this.clear_search(cx);
                            this.focus_search(window, cx);
                        },
                    ))
                }))
                .child(
                    ui::top_bar::magic_toggle(&theme, self.in_magic_view()).on_click(cx.listener(
                        |this, _: &ClickEvent, window, cx| {
                            this.toggle_magic_mode(window, cx);
                        },
                    )),
                ),
        );

        ui::top_bar::top_bar(&theme).child(left).child(search)
    }

    /// A clickable disclosure header; toggling it persists into settings
    /// and the Changed event re-renders the sidebar.
    pub(super) fn section_header(
        &self,
        theme: &Theme,
        id: &'static str,
        label: &'static str,
        cx: &mut Context<Self>,
    ) -> (gpui::Stateful<gpui::Div>, bool) {
        let collapsed = self.is_section_collapsed(id, cx);
        let header = ui::sidebar::collapsible_header(theme, id, label, collapsed).on_click(
            cx.listener(move |this, _: &ClickEvent, _window, cx| {
                this.toggle_section(id, cx);
            }),
        );
        (header, collapsed)
    }

    pub(super) fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *cx.theme();
        let favorites = self.settings.read(cx).settings().favorites.clone();

        let mut content = div()
            .id("sidebar-scroll")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll();

        // PLACES.
        let (header, collapsed) = self.section_header(&theme, "places", "Places", cx);
        content = content.child(header);
        if !collapsed {
            // Reuse the startup cache: OS folder resolution can read XDG files.
            let place = |name: &str| self.user_dirs.get(name).map(Path::to_path_buf);
            let places: Vec<(&'static str, &str, PathBuf)> = [
                ("icons/house.svg", "Home", place("home")),
                ("icons/folder.svg", "Desktop", place("desktop")),
                ("icons/file-text.svg", "Documents", place("documents")),
                ("icons/download.svg", "Downloads", place("downloads")),
                ("icons/image.svg", "Pictures", place("pictures")),
                ("icons/hard-drive.svg", "Root", Some(PathBuf::from("/"))),
            ]
            .into_iter()
            .filter_map(|(icon, label, path)| Some((icon, label, path?)))
            .collect();
            content = content.children(places.into_iter().enumerate().map(
                |(ix, (icon, label, path))| {
                    ui::sidebar::sidebar_row(&theme, ("place", ix))
                        .when(self.query.is_empty() && self.cwd == path, |s| {
                            s.bg(theme.selected).text_color(theme.accent)
                        })
                        .child(ui::icon::ui_icon(icon, theme.text_dim).size(px(16.)))
                        .child(ui::sidebar::sidebar_label(label))
                        .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                            this.navigate(path.clone(), cx);
                        }))
                },
            ));
        }

        // FAVORITES (only shown once something is pinned).
        if !favorites.is_empty() {
            let (header, collapsed) = self.section_header(&theme, "favorites", "Favorites", cx);
            content = content.child(header);
            if !collapsed {
                content = content.children(favorites.into_iter().enumerate().map(|(ix, path)| {
                    let label = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.display().to_string());
                    let (nav, menu) = (path.clone(), path.clone());
                    let tip = path.display().to_string();
                    ui::sidebar::sidebar_row(&theme, ("favorite", ix))
                        .when(self.query.is_empty() && self.cwd == path, |s| {
                            s.bg(theme.selected).text_color(theme.accent)
                        })
                        .tooltip(ui::tooltip::text_tooltip(tip, theme))
                        .child(ui::icon::ui_icon("icons/star.svg", theme.accent).size(px(14.)))
                        .child(ui::sidebar::sidebar_label(label))
                        .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                            this.navigate(nav.clone(), cx);
                        }))
                        .on_mouse_down(
                            MouseButton::Right,
                            cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                                this.open_favorite_menu(menu.clone(), event.position, window, cx);
                            }),
                        )
                }));
            }
        }

        // RECENTS (only shown once something's been opened).
        if !self.recents.is_empty() {
            let (header, collapsed) = self.section_header(&theme, "recents", "Recent", cx);
            content = content.child(header);
            if !collapsed {
                content =
                    content.children(self.recents.recent_paths().enumerate().map(|(ix, path)| {
                        let path = path.to_path_buf();
                        let label = path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| path.display().to_string());
                        let tip = path.display().to_string();
                        ui::sidebar::sidebar_row(&theme, ("recent", ix))
                            .when(self.query.is_empty() && self.cwd == path, |s| {
                                s.bg(theme.selected).text_color(theme.accent)
                            })
                            .tooltip(ui::tooltip::text_tooltip(tip, theme))
                            .child(
                                ui::icon::ui_icon("icons/clock.svg", theme.text_dim).size(px(14.)),
                            )
                            .child(ui::sidebar::sidebar_label(label))
                            .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                                this.open_recent(path.clone(), cx);
                            }))
                    }));
                content = content.child(
                    ui::sidebar::sidebar_row(&theme, "recents-clear")
                        .text_color(theme.text_dim)
                        .child(ui::icon::ui_icon("icons/trash-2.svg", theme.text_dim).size(px(13.)))
                        .child("Clear")
                        .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                            this.clear_recents(cx);
                        })),
                );
            }
        }

        // TAGS (only once something's been tagged). Clicking a tag runs a
        // `tag:NAME` search.
        if !self.sidebar_tags.is_empty() {
            let (header, collapsed) = self.section_header(&theme, "tags", "Tags", cx);
            content = content.child(header);
            if !collapsed {
                content =
                    content.children(self.sidebar_tags.iter().enumerate().map(|(ix, tag)| {
                        let name = tag.name.clone();
                        let dot = ui::details::tag_dot_color(&theme, tag.color);
                        ui::sidebar::sidebar_row(&theme, ("tag", ix))
                            .tooltip(ui::tooltip::text_tooltip(name.clone(), theme))
                            .child(div().flex_none().size(px(8.)).rounded_full().bg(dot))
                            .child(ui::sidebar::sidebar_label(SharedString::from(name.clone())))
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                this.search_tag(name.clone(), window, cx);
                            }))
                    }));
            }
        }

        // INDEXED.
        let (header, collapsed) = self.section_header(&theme, "indexed", "Search locations", cx);
        content = content.child(header);
        if !collapsed {
            let root_rows: Vec<gpui::AnyElement> = (0..self.roots.len())
                .map(|ix| self.render_root_row(ix, cx).into_any_element())
                .collect();
            content = content.children(root_rows).child(
                ui::sidebar::sidebar_row(&theme, "add-root")
                    .text_color(theme.text_dim)
                    .child(ui::icon::ui_icon("icons/plus.svg", theme.text_dim).size(px(14.)))
                    .child("Index this folder")
                    .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                        this.add_current_folder(cx);
                    })),
            );
        }

        // DRIVES (only once the background refresh has found any).
        if !self.drives.is_empty() {
            let (header, collapsed) = self.section_header(&theme, "drives", "Drives", cx);
            content = content.child(header);
            if !collapsed {
                content = content.children(self.drives.iter().enumerate().map(|(ix, drive)| {
                    let path = drive.path.clone();
                    let free_line: SharedString = format!(
                        "{} free of {}",
                        format_size(drive.free_bytes),
                        format_size(drive.total_bytes)
                    )
                    .into();
                    ui::sidebar::drive_row(&theme, ("drive", ix))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    ui::icon::ui_icon("icons/hard-drive.svg", theme.text_dim)
                                        .size(px(14.)),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .text_sm()
                                        .overflow_hidden()
                                        .child(SharedString::from(drive.name.clone())),
                                ),
                        )
                        .child(ui::sidebar::capacity_bar(&theme, drive.used_fraction()))
                        .child(div().text_xs().text_color(theme.text_dim).child(free_line))
                        .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                            this.navigate(path.clone(), cx);
                        }))
                }));
            }
        }

        let sidebar = ui::sidebar::sidebar_panel(&theme)
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap_2()
                    .px_4()
                    .h(px(42.))
                    .child(ui::icon::ui_icon("icons/folder.svg", theme.accent).size(px(22.)))
                    .child(
                        div()
                            .text_base()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child("filex"),
                    ),
            )
            .child(content);

        // The FDA banner stays pinned below the scrollable sections.
        #[cfg(target_os = "macos")]
        let sidebar = if self.fda_missing {
            sidebar.child(
                ui::sidebar::sidebar_row(&theme, "fda-banner")
                    .text_xs()
                    .text_color(theme.warn)
                    .child(ui::icon::ui_icon("icons/triangle-alert.svg", theme.warn).size(px(14.)))
                    .child("Grant Full Disk Access")
                    .on_click(|_: &ClickEvent, _window, _cx| {
                        filex::ingest::open_full_disk_access_settings();
                    }),
            )
        } else {
            sidebar
        };

        sidebar
    }

    /// Give the current location and its layout controls one stable home.
    fn render_content_header(&self, cx: &mut Context<Self>) -> gpui::Div {
        let theme = *cx.theme();
        let settings = self.settings.read(cx).settings();
        let browsing = self.query.is_empty() && !self.in_magic_view();
        let title: SharedString = if self.in_magic_view() {
            "File actions".into()
        } else if !self.query.is_empty() {
            "Search results".into()
        } else {
            self.tab_title(self.active_tab)
        };
        let subtitle: SharedString = if self.in_magic_view() {
            format!(
                "Review files before applying · {}",
                self.search_scope.label()
            )
            .into()
        } else if browsing {
            format!("{} items", self.entries.len()).into()
        } else {
            format!(
                "{} results · {}",
                self.results.len(),
                self.search_scope.label()
            )
            .into()
        };
        let mut controls = div().flex().flex_none().items_center().gap_2();
        if browsing {
            let mut layouts = div()
                .flex()
                .items_center()
                .gap_0p5()
                .p_0p5()
                .rounded_lg()
                .bg(theme.panel)
                .border_1()
                .border_color(theme.border);
            for (id, icon, label, view) in [
                ("view-list", "icons/list.svg", "List view", ViewMode::List),
                (
                    "view-grid",
                    "icons/layout-grid.svg",
                    "Grid view",
                    ViewMode::Grid,
                ),
            ] {
                let active = settings.view == view;
                layouts = layouts.child(
                    ui::top_bar::toolbar_button(
                        &theme,
                        id,
                        icon,
                        if active { theme.accent } else { theme.text_dim },
                    )
                    .when(active, |s| s.bg(theme.bg))
                    .tooltip(ui::tooltip::text_tooltip(label, theme))
                    .on_click(cx.listener(
                        move |this, _: &ClickEvent, _window, cx| {
                            this.settings
                                .update(cx, |store, cx| store.update(cx, |s| s.view = view));
                        },
                    )),
                );
            }
            if settings.view == ViewMode::Grid {
                for (id, icon, label, delta) in [
                    ("zoom-out", "icons/minus.svg", "Smaller icons", -1),
                    ("zoom-in", "icons/plus.svg", "Larger icons", 1),
                ] {
                    controls = controls.child(
                        ui::top_bar::toolbar_button(&theme, id, icon, theme.text_dim)
                            .tooltip(ui::tooltip::text_tooltip(label, theme))
                            .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                                this.zoom_grid(delta, cx)
                            })),
                    );
                }
            }
            controls = controls.child(layouts);
        }
        if !self.in_magic_view() {
            controls = controls.child(
                ui::top_bar::toolbar_button(
                    &theme,
                    "preview-toggle",
                    "icons/panel-right.svg",
                    if settings.preview_open {
                        theme.accent
                    } else {
                        theme.text_dim
                    },
                )
                .when(settings.preview_open, |s| s.bg(theme.selected))
                .tooltip(ui::tooltip::text_tooltip("Toggle details", theme))
                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| this.toggle_preview(cx))),
            );
        }
        ui::pane::content_header(&theme, title, subtitle, controls)
    }

    /// Header click: select the column, or flip the direction when it's
    /// already the sort key. Goes through the settings store, so it
    /// persists and the Changed event re-sorts the listing.
    pub(super) fn set_sort(&mut self, by: SortBy, cx: &mut Context<Self>) {
        self.settings.update(cx, |store, cx| {
            store.update(cx, |settings| {
                if settings.sort.by == by {
                    settings.sort.ascending = !settings.sort.ascending;
                } else {
                    settings.sort.by = by;
                    settings.sort.ascending = true;
                }
            });
        });
    }

    pub(super) fn render_column_headers(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *cx.theme();
        let sort = self.settings.read(cx).settings().sort;
        let active = |by: SortBy| (sort.by == by).then_some(sort.ascending);
        let on_sort = |by: SortBy| {
            cx.listener(move |this: &mut Self, _: &ClickEvent, _window, cx| {
                this.set_sort(by, cx);
            })
        };
        ui::list_row::header_row(&theme, theme.icon_size)
            .child(
                ui::list_row::header_cell(&theme, "sort-name", "Name", active(SortBy::Name))
                    .flex_1()
                    .on_click(on_sort(SortBy::Name)),
            )
            .child(
                ui::list_row::header_cell(
                    &theme,
                    "sort-modified",
                    "Modified",
                    active(SortBy::Modified),
                )
                .w(px(ui::list_row::MODIFIED_COL_WIDTH))
                .flex_none()
                .justify_end()
                .on_click(on_sort(SortBy::Modified)),
            )
            .child(
                ui::list_row::header_cell(&theme, "sort-size", "Size", active(SortBy::Size))
                    .w(px(ui::list_row::SIZE_COL_WIDTH))
                    .flex_none()
                    .justify_end()
                    .on_click(on_sort(SortBy::Size)),
            )
    }

    pub(super) fn render_browse_pane(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let inner = if let Some(err) = &self.load_error {
            ui::pane::message_state(
                cx.theme(),
                "icons/triangle-alert.svg",
                "Unable to open this folder",
                err.clone(),
            )
        } else if self.entries.is_empty() {
            ui::pane::message_state(
                cx.theme(),
                "icons/folder.svg",
                "This folder is empty",
                "Files you add here will appear in this view.",
            )
        } else {
            match self.settings.read(cx).settings().view {
                // A small horizontal inset so the header and the rounded row
                // selection float off the pane edges, Finder-style (the header
                // and list share it, so their columns stay aligned).
                ViewMode::List => div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .px_3()
                    .child(self.render_column_headers(cx))
                    .child(self.render_file_list(cx))
                    .into_any_element(),
                ViewMode::Grid => self.render_grid(window, cx),
            }
        };
        // The whole pane accepts an OS-file drop into the current folder
        // (a copy). Folder rows/cards register their own handlers and win
        // when the drop lands on them; this catches drops on empty space
        // or on non-folder rows. Internal drags are not handled here — the
        // items are already in this folder.
        let dest = self.cwd.clone();
        div()
            .id("browse-pane")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(inner)
            .on_drop(cx.listener(move |this, ext: &ExternalPaths, _window, cx| {
                this.drop_onto(dest.clone(), ext.paths().to_vec(), ClipMode::Copy, cx);
            }))
            .into_any_element()
    }

    pub(super) fn render_search_pane(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = *cx.theme();
        if !self.any_root_ready() {
            return ui::pane::empty_state(&theme, self.index_status_text());
        }
        if self.results.is_empty() {
            if let Some(notice) = &self.notice {
                return ui::pane::empty_state(&theme, notice.clone());
            }
            return ui::pane::message_state(
                &theme,
                "icons/search.svg",
                "No matching files",
                format!(
                    "No matches for “{}”. Try another name or remove a filter.",
                    self.query
                ),
            );
        }
        // Match the browse list's inset so search rows get the same
        // floating rounded selection.
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .px_3()
            .child(self.render_search_results(cx))
            .children(self.search_more.then(|| {
                ui::modal::button(
                    &theme,
                    "load-more-search",
                    if self.search_paging {
                        "Loading…"
                    } else {
                        "Load more results"
                    },
                    false,
                )
                .flex_none()
                .mx_3()
                .my_2()
                .text_xs()
                .on_click(cx.listener(|this, _, _, cx| this.load_more_results(cx)))
            }))
            .into_any_element()
    }

    pub(super) fn render_status_bar(&self, theme: &Theme) -> impl IntoElement {
        let left: SharedString = if self.in_magic_view() {
            self.magic
                .as_ref()
                .map(|s| s.status())
                .unwrap_or_else(|| "Magic mode · type a file action".into())
                .into()
        } else if let Some(notice) = &self.notice {
            notice.clone()
        } else if let Some(err) = &self.load_error {
            format!("error — {err}").into()
        } else if !self.active_selection().is_empty() {
            let (n, total) = (self.active_selection().len(), self.active_list_len());
            // Combined size only in browse — search rows carry no size.
            if self.query.is_empty() {
                let bytes: u64 = self
                    .selection
                    .iter()
                    .filter_map(|ix| self.entries.get(ix))
                    .filter(|entry| !entry.is_dir)
                    .map(|entry| entry.size)
                    .sum();
                format!("{n} of {total} selected · {}", format_size(bytes)).into()
            } else {
                format!("{n} of {total} selected").into()
            }
        } else if self.query.is_empty() {
            format!("{} items", self.entries.len()).into()
        } else {
            format!(
                "{} result{}",
                self.results.len(),
                if self.results.len() == 1 { "" } else { "s" }
            )
            .into()
        };
        ui::status_bar::status_bar(theme, left, self.index_status_text())
    }

    /// The topmost bar: location tabs, new tab, shortcuts, and settings.
    /// Layout and preview controls live beside the content heading.
    pub(super) fn render_tab_bar(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = *cx.theme();
        // Only offer a close control when closing is meaningful (more than
        // one tab); a lone tab reads as the window's title, not a chip to
        // dismiss.
        let closable = self.tabs.len() > 1;
        let mut tabs = ui::tabs::tab_group();
        for i in 0..self.tabs.len() {
            let mut chip = ui::tabs::tab(&theme, ("tab", i), i == self.active_tab)
                .child(
                    ui::icon::ui_icon(
                        "icons/folder.svg",
                        if i == self.active_tab {
                            theme.accent
                        } else {
                            theme.text_dim
                        },
                    )
                    .size(px(14.)),
                )
                .child(ui::tabs::tab_label(self.tab_title(i)));
            if closable {
                chip = chip.child(ui::tabs::tab_close(&theme, ("tab-close", i)).on_click(
                    cx.listener(move |this, _: &ClickEvent, _window, cx| {
                        cx.stop_propagation();
                        this.close_tab(i, cx);
                    }),
                ));
            }
            tabs = tabs.child(
                chip.on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                    this.activate_tab(i, cx);
                }))
                .on_mouse_down(
                    MouseButton::Middle,
                    cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                        this.close_tab(i, cx);
                    }),
                ),
            );
        }
        let cwd = self.cwd.clone();
        tabs = tabs.child(
            ui::tabs::new_tab_button(&theme, "new-tab").on_click(cx.listener(
                move |this, _: &ClickEvent, _window, cx| {
                    this.open_tab(cwd.clone(), cx);
                },
            )),
        );

        let icons = div().flex().flex_none().items_center().gap_1().child(
            ui::top_bar::toolbar_button(
                &theme,
                "settings",
                "icons/settings.svg",
                if self.settings_open {
                    theme.accent
                } else {
                    theme.text_dim
                },
            )
            .on_click(
                cx.listener(|this, _: &ClickEvent, window, cx| this.toggle_settings(window, cx)),
            ),
        );

        ui::tabs::tab_bar(&theme)
            .child(tabs)
            .child(icons)
            .into_any_element()
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let searching = !self.query.is_empty();
        let theme = *cx.theme();
        div()
            .track_focus(&self.focus_handle)
            // Anchors the `!SearchInput` key bindings: single-key
            // shortcuts fire here (list focused) but not while the search
            // box is focused, so those keys stay typeable as text.
            .key_context("Workspace")
            .on_action(
                cx.listener(|this, _: &FocusSearch, window, cx| this.focus_search(window, cx)),
            )
            .on_action(cx.listener(|this, _: &FocusPath, window, cx| this.focus_path(window, cx)))
            .on_action(cx.listener(|this, _: &SubmitPath, window, cx| this.submit_path(window, cx)))
            .on_action(cx.listener(|this, _: &CancelPath, window, cx| this.cancel_path(window, cx)))
            .on_action(cx.listener(|this, _: &ToggleShortcuts, window, cx| {
                this.open_keyboard_settings(window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &SelectPrevious, _, cx| this.move_selection(-1, false, cx)),
            )
            .on_action(cx.listener(|this, _: &SelectNext, _, cx| this.move_selection(1, false, cx)))
            .on_action(
                cx.listener(|this, _: &ExtendPrevious, _, cx| this.move_selection(-1, true, cx)),
            )
            .on_action(cx.listener(|this, _: &ExtendNext, _, cx| this.move_selection(1, true, cx)))
            .on_action(cx.listener(|this, _: &OpenSelected, window, cx| {
                if this.conflict.is_some() {
                    this.resolve_conflict(true, cx);
                } else if this.renaming.is_some() {
                    this.commit_rename(window, cx);
                } else if this.tag_editor.is_some() {
                    this.commit_tag_editor(window, cx);
                } else {
                    this.activate_selected(cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleView, _window, cx| this.toggle_view(cx)))
            .on_action(cx.listener(|this, _: &GoUp, _window, cx| this.go_up(cx)))
            .on_action(cx.listener(|this, _: &GoBack, _window, cx| this.go_back(cx)))
            .on_action(cx.listener(|this, _: &GoForward, _window, cx| this.go_forward(cx)))
            .on_action(cx.listener(|this, _: &Refresh, _window, cx| this.reload_dir(cx)))
            .on_action(cx.listener(|this, _: &NewTab, _window, cx| {
                let cwd = this.cwd.clone();
                this.open_tab(cwd, cx);
            }))
            // cmd-w closes the active tab; the last tab closes the window.
            .on_action(cx.listener(|this, _: &CloseTab, window, cx| {
                if this.tabs.len() <= 1 {
                    window.remove_window();
                } else {
                    this.close_tab(this.active_tab, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &NextTab, _window, cx| this.cycle_tab(1, cx)))
            .on_action(cx.listener(|this, _: &PrevTab, _window, cx| this.cycle_tab(-1, cx)))
            .on_action(cx.listener(|this, _: &ToggleSettings, window, cx| {
                this.toggle_settings(window, cx);
            }))
            .on_action(cx.listener(|this, _: &TogglePreview, _window, cx| {
                this.toggle_preview(cx);
            }))
            .on_action(cx.listener(|this, _: &RenameSelected, window, cx| {
                this.start_rename(window, cx);
            }))
            .on_action(cx.listener(|this, _: &DeleteSelected, _window, cx| {
                this.delete_selected(cx);
            }))
            // Bubbled up from the (empty) search input: file-level
            // clipboard operations on the selected row.
            .on_action(cx.listener(|this, _: &search_input::Copy, _window, cx| {
                this.clip_selected(ClipMode::Copy, cx);
            }))
            .on_action(cx.listener(|this, _: &search_input::Cut, _window, cx| {
                this.clip_selected(ClipMode::Cut, cx);
            }))
            .on_action(cx.listener(|this, _: &search_input::Paste, _window, cx| {
                this.paste_clipboard(cx);
            }))
            // cmd-a while the input is empty selects every row.
            .on_action(
                cx.listener(|this, _: &search_input::SelectAll, _window, cx| {
                    this.select_all(cx);
                }),
            )
            // Escape, bubbled by the empty input: close whatever is
            // topmost, and failing that blur the search box back to the
            // list so single-key shortcuts work again.
            .on_action(
                cx.listener(|this, _: &search_input::ClearInput, window, cx| {
                    if this.context_menu.is_some() {
                        this.close_menu(cx);
                    } else if this.scope_menu.is_some() {
                        this.close_scope_menu(cx);
                    } else if this.conflict.is_some() {
                        this.resolve_conflict(false, cx);
                    } else if this.settings_open {
                        this.toggle_settings(window, cx);
                    } else {
                        window.focus(&this.focus_handle);
                    }
                }),
            )
            .on_action(cx.listener(|this, _: &Undo, _window, cx| {
                this.undo_last(cx);
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.handle_key(event, window, cx);
            }))
            .flex()
            .flex_col()
            .size_full()
            .font_family(ui::fonts::UI_FONT_FAMILY)
            .bg(theme.bg)
            .text_color(theme.text)
            .child(self.render_tab_bar(cx))
            .child(self.render_top_bar(window, cx))
            .children(self.path_error.as_ref().map(|error| {
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap_2()
                    .px_4()
                    .py_2()
                    .bg(theme.panel)
                    .text_xs()
                    .text_color(theme.warn)
                    .child(ui::icon::ui_icon("icons/triangle-alert.svg", theme.warn).size(px(14.)))
                    .child(div().flex_1().min_w_0().truncate().child(error.clone()))
            }))
            .children(self.render_filter_chips(cx))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_sidebar(cx))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .min_h_0()
                            .child(self.render_content_header(cx))
                            .child(if self.in_magic_view() {
                                self.render_magic_pane(cx)
                            } else if searching {
                                self.render_search_pane(cx)
                            } else {
                                self.render_browse_pane(window, cx)
                            }),
                    )
                    .children(
                        (!self.in_magic_view())
                            .then(|| self.render_details_panel(window, cx))
                            .flatten(),
                    ),
            )
            .children(self.render_jobs(cx))
            .children(self.render_update_banner(cx))
            .child(self.render_status_bar(&theme))
            .children(self.render_scope_menu(cx))
            .children(self.render_context_menu(cx))
            .children(self.render_settings_modal(window, cx))
            .children(self.render_conflict_modal(cx))
    }
}

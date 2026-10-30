//! Sidebar: favorites, collapsible sections, recents, and its rendering.

use super::*;

impl Workspace {
    pub(super) fn is_favorite(&self, path: &Path, cx: &App) -> bool {
        self.settings
            .read(cx)
            .settings()
            .favorites
            .iter()
            .any(|p| p == path)
    }

    /// Pin a folder to the sidebar's Favorites (idempotent).
    pub(super) fn pin_favorite(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.settings.update(cx, |store, cx| {
            store.update(cx, |settings| {
                if !settings.favorites.iter().any(|p| p == &path) {
                    settings.favorites.push(path.clone());
                }
            });
        });
    }

    pub(super) fn unpin_favorite(&mut self, path: &Path, cx: &mut Context<Self>) {
        let path = path.to_path_buf();
        self.settings.update(cx, |store, cx| {
            store.update(cx, |settings| settings.favorites.retain(|p| p != &path));
        });
    }

    /// Move a favorite up (`delta < 0`) or down in the list.
    pub(super) fn move_favorite(&mut self, path: &Path, delta: isize, cx: &mut Context<Self>) {
        let path = path.to_path_buf();
        self.settings.update(cx, |store, cx| {
            store.update(cx, |settings| {
                let favorites = &mut settings.favorites;
                if let Some(i) = favorites.iter().position(|p| p == &path) {
                    let j = (i as isize + delta).clamp(0, favorites.len() as isize - 1) as usize;
                    if i != j {
                        let item = favorites.remove(i);
                        favorites.insert(j, item);
                    }
                }
            });
        });
    }

    pub(super) fn is_section_collapsed(&self, id: &str, cx: &App) -> bool {
        self.settings
            .read(cx)
            .settings()
            .collapsed_sections
            .iter()
            .any(|s| s == id)
    }

    pub(super) fn toggle_section(&mut self, id: &'static str, cx: &mut Context<Self>) {
        self.settings.update(cx, |store, cx| {
            store.update(cx, |settings| {
                let sections = &mut settings.collapsed_sections;
                if let Some(i) = sections.iter().position(|s| s == id) {
                    sections.remove(i);
                } else {
                    sections.push(id.to_string());
                }
            });
        });
    }

    /// Note `path` as recently opened and persist the log off-thread.
    pub(super) fn record_recent(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        // A page, not a file the index or the recents list can hold.
        if filex::drives::is_this_pc(&path) {
            return;
        }
        if let Some(client) = self.service.clone() {
            let touched = path.clone();
            cx.background_executor()
                .spawn(async move {
                    let _ = client.call(filex::daemon::ipc::Command::Touch { path: touched });
                })
                .detach();
        }
        self.recents.record(path);
        self.persist_recents(cx);
    }

    pub(super) fn persist_recents(&self, cx: &Context<Self>) {
        let Some(file) = filex::recents::default_recents_file() else {
            return;
        };
        let recents = self.recents.clone();
        cx.background_executor()
            .spawn(async move {
                if let Err(err) = recents.save(&file) {
                    tracing::error!("failed to save recents: {err:#}");
                }
            })
            .detach();
    }

    pub(super) fn clear_recents(&mut self, cx: &mut Context<Self>) {
        self.recents.clear();
        self.persist_recents(cx);
        cx.notify();
    }

    /// Recompute the sidebar's distinct-tag list off-thread (the store
    /// scan/clone must not run on the UI thread) and cache it. Called at
    /// startup and after any change to the store.
    pub(super) fn refresh_sidebar_tags(&self, cx: &mut Context<Self>) {
        let store = self.tags.clone();
        cx.spawn(async move |this, cx| {
            let distinct = cx
                .background_executor()
                .spawn(async move {
                    let all = store.all();
                    filex::tags::distinct_tags(all.iter().flat_map(|(_, tags)| tags.iter()))
                })
                .await;
            this.update(cx, |this, cx| {
                this.sidebar_tags = distinct;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Navigate a recent folder, or open a recent file with its default
    /// app (a stat on click decides which).
    pub(super) fn open_recent(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if path.is_dir() {
            self.navigate(path, cx);
        } else {
            self.open_target(path, false, false, cx);
        }
    }

    /// Open the feedback dialog (the sidebar footer button), focusing the
    /// message.
    pub(super) fn open_feedback(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.feedback.is_some() {
            return;
        }
        let message = cx.new(SearchInput::new);
        let email = cx.new(SearchInput::new);
        message.update(cx, |input, cx| {
            input.set_placeholder("What's working, what's broken, what's missing?", cx)
        });
        email.update(cx, |input, cx| {
            input.set_placeholder("Email, if you'd like a reply (optional)", cx)
        });
        let dismiss = |this: &mut Self,
                       _: &gpui::Entity<SearchInput>,
                       event: &SearchInputEvent,
                       window: &mut Window,
                       cx: &mut Context<Self>| {
            if matches!(event, SearchInputEvent::Dismissed) {
                this.cancel_feedback(window, cx);
            }
        };
        let subscriptions = [
            cx.subscribe_in(&message, window, dismiss),
            cx.subscribe_in(&email, window, dismiss),
        ];
        window.focus(&message.focus_handle(cx), cx);
        self.feedback = Some(FeedbackDraft {
            message,
            email,
            _subscriptions: subscriptions,
        });
        cx.notify();
    }

    pub(super) fn cancel_feedback(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.feedback.take().is_some() {
            window.focus(&self.search_input.focus_handle(cx), cx);
            cx.notify();
        }
    }

    /// Send the draft to Sentry off-thread; the status-bar notice reports
    /// the outcome. An empty message keeps the dialog open.
    pub(super) fn submit_feedback(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(draft) = self.feedback.as_ref() else {
            return;
        };
        let message = draft.message.read(cx).text().trim().to_string();
        if message.is_empty() {
            return;
        }
        let email = draft.email.read(cx).text().trim().to_string();
        self.cancel_feedback(window, cx);
        self.notice = Some("Sending feedback…".into());
        cx.spawn(async move |this, cx| {
            let sent = cx
                .background_executor()
                .spawn(async move { send_feedback(&message, &email) })
                .await;
            this.update(cx, |this, cx| {
                this.notice = Some(if sent {
                    "Thanks — feedback sent".into()
                } else {
                    "Couldn't send feedback — check your connection and try again".into()
                });
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

/// Deliver feedback via Sentry; blocking. Builds without the
/// `observability` feature have no transport, so they report failure.
fn send_feedback(message: &str, email: &str) -> bool {
    #[cfg(feature = "observability")]
    {
        let email = (!email.is_empty()).then_some(email);
        filex::diagnostics::observability::send_feedback(message, email, env!("CARGO_PKG_VERSION"))
    }
    #[cfg(not(feature = "observability"))]
    {
        let _ = (message, email);
        false
    }
}

impl Workspace {
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

    pub(super) fn render_feedback_modal(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let draft = self.feedback.as_ref()?;
        let theme = *cx.theme();
        let field = |input: &gpui::Entity<SearchInput>| {
            div()
                .w_full()
                .px_2()
                .py_1p5()
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .text_sm()
                .child(input.clone())
        };
        Some(ui::modal::present(
            "feedback-backdrop-in",
            ui::modal::backdrop("feedback-backdrop")
                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                    this.cancel_feedback(window, cx);
                }))
                .child(
                    ui::modal::panel(&theme, "feedback-panel")
                        .on_click(|_, _, cx| cx.stop_propagation())
                        .child(ui::modal::title(&theme, "Send feedback"))
                        .child(ui::modal::message(
                            &theme,
                            "Report a bug or suggest an idea. Your message, the app \
                             version and your OS are sent; file paths are removed.",
                        ))
                        .child(field(&draft.message))
                        .child(field(&draft.email))
                        .child(
                            ui::modal::buttons()
                                .child(
                                    ui::modal::button(&theme, "feedback-cancel", "Cancel", false)
                                        .on_click(cx.listener(
                                            |this, _: &ClickEvent, window, cx| {
                                                this.cancel_feedback(window, cx);
                                            },
                                        )),
                                )
                                .child(
                                    ui::modal::button(&theme, "feedback-send", "Send", true)
                                        .on_click(cx.listener(
                                            |this, _: &ClickEvent, window, cx| {
                                                this.submit_feedback(window, cx);
                                            },
                                        )),
                                ),
                        ),
                ),
        ))
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
            let (machine_icon, machine_label, machine) = platform::machine_place();
            let places: Vec<(&'static str, &str, PathBuf)> = [
                ("icons/house.svg", "Home", place("home")),
                ("icons/folder.svg", "Desktop", place("desktop")),
                ("icons/file-text.svg", "Documents", place("documents")),
                ("icons/download.svg", "Downloads", place("downloads")),
                ("icons/image.svg", "Pictures", place("pictures")),
                (machine_icon, machine_label, Some(machine)),
            ]
            .into_iter()
            .filter_map(|(icon, label, path)| Some((icon, label, path?)))
            .collect();
            content = content.children(places.into_iter().enumerate().map(
                |(ix, (icon, label, path))| {
                    let active = self.query.is_empty() && self.cwd == path;
                    ui::sidebar::sidebar_item(&theme, ("place", ix), icon, label, active).on_click(
                        cx.listener(move |this, _: &ClickEvent, _window, cx| {
                            this.navigate(path.clone(), cx);
                        }),
                    )
                },
            ));
        }

        // FAVORITES (only shown once something is pinned).
        if !favorites.is_empty() {
            let (header, collapsed) = self.section_header(&theme, "favorites", "Favorites", cx);
            content = content.child(header);
            if !collapsed {
                content = content.children(favorites.into_iter().enumerate().map(|(ix, path)| {
                    let label = filex::listing::location_label(&path);
                    let (nav, menu) = (path.clone(), path.clone());
                    let tip = display_path(&path);
                    let active = self.query.is_empty() && self.cwd == path;
                    ui::sidebar::sidebar_item(
                        &theme,
                        ("favorite", ix),
                        "icons/star.svg",
                        label,
                        active,
                    )
                    .tooltip(ui::tooltip::text_tooltip(tip, theme))
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
                        let label = filex::listing::location_label(&path);
                        let tip = display_path(&path);
                        let active = self.query.is_empty() && self.cwd == path;
                        ui::sidebar::sidebar_item(
                            &theme,
                            ("recent", ix),
                            "icons/clock.svg",
                            label,
                            active,
                        )
                        .tooltip(ui::tooltip::text_tooltip(tip, theme))
                        .on_click(cx.listener(
                            move |this, _: &ClickEvent, _window, cx| {
                                this.open_recent(path.clone(), cx);
                            },
                        ))
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

        // DRIVES (only once the background refresh has found any).
        if !self.drives.is_empty() {
            let (header, collapsed) = self.section_header(&theme, "drives", "Drives", cx);
            content = content.child(header);
            if !collapsed {
                content = content.children(self.drives.iter().enumerate().map(|(ix, drive)| {
                    let path = drive.path.clone();
                    let free_line: SharedString = this_pc::drive_detail(drive).into();
                    ui::sidebar::drive_row(&theme, ("drive", ix))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    ui::icon::ui_icon(
                                        this_pc::drive_icon(drive.kind),
                                        theme.text_dim,
                                    )
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
                        .when(drive.capacity.is_some(), |s| {
                            s.child(ui::sidebar::capacity_bar(&theme, drive.used_fraction()))
                        })
                        .child(div().text_xs().text_color(theme.text_dim).child(free_line))
                        .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                            this.navigate(path.clone(), cx);
                        }))
                }));
            }
        }

        let sidebar = ui::sidebar::sidebar_panel().child(content);

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
}

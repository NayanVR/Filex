//! Menus, modals, side panels, filter chips, and row builders.

use super::*;

impl Workspace {
    /// The slim update banner above the status bar, or nothing when there's
    /// no update to show. Text/labels come from `filex::update`'s tested
    /// pure functions; this only renders and wires the buttons.
    pub(super) fn render_update_banner(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let content = filex::update::banner_content(&self.update_status)?;
        let theme = *cx.theme();
        let mut bar = ui::update_banner::update_banner(&theme)
            .child(ui::update_banner::message(&theme, content.message));
        if let Some(label) = content.action_label {
            bar = bar.child(
                ui::update_banner::action_button(&theme, "update-action", label).on_click(
                    cx.listener(|this, _: &ClickEvent, _window, cx| {
                        this.apply_update_action(cx);
                    }),
                ),
            );
        }
        bar = bar.child(
            ui::update_banner::dismiss_button(&theme, "update-dismiss").on_click(cx.listener(
                |this, _: &ClickEvent, _window, cx| {
                    this.dismiss_update(cx);
                },
            )),
        );
        Some(bar.into_any_element())
    }

    /// The removable filter chips shown under the top bar — one pill per
    /// recognized `key:value` token in the query (`tag:` pills carry the
    /// tag's color dot). `None` when the query has no filter tokens.
    pub(super) fn render_filter_chips(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if self.query.is_empty() {
            return None;
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        // Chips must describe the search that actually ran. For a command
        // query that is *not* the raw text: `magic::parse` reads only the
        // command's target ("pdfs from downloads", not the verb or the
        // destination) and expands it with `expand_as_description`, whose
        // lone-word rule differs from `expand`. Deriving chips from the
        // whole query instead showed filters the plan does not apply —
        // "move all pdfs from downloads to documents" grew a `kind:document`
        // chip off the destination word, while the command carried only
        // `ext:pdf`.
        let source = match &self.magic {
            Some(state) => state.command.selection.source.clone(),
            None => self.query.clone(),
        };
        let tokens = filex::search_filter::filter_tokens(&source, now);
        let residual = filex::search_filter::parse_query(&source, now).text;
        let phrases = match &self.magic {
            Some(_) => filex::phrases::expand_as_description(&residual, now).phrases,
            None => filex::phrases::expand(&residual, now).phrases,
        };
        if tokens.is_empty() && phrases.is_empty() {
            return None;
        }
        let theme = *cx.theme();
        let mut strip = ui::top_bar::filter_chip_strip(&theme);
        for (i, (token, filter)) in tokens.into_iter().enumerate() {
            // `tag:` pills show the tag name with its color dot; the rest
            // show the raw token (`kind:image`, `size:>2mb`, …).
            let (label, dot) = match &filter {
                Filter::Tag(name) => {
                    let color = self
                        .sidebar_tags
                        .iter()
                        .find(|t| t.name.eq_ignore_ascii_case(name))
                        .and_then(|t| t.color);
                    (
                        name.clone(),
                        Some(ui::details::tag_dot_color(&theme, color)),
                    )
                }
                _ => (token.clone(), None),
            };
            strip = strip.child(
                ui::top_bar::filter_chip(&theme, ("filter-chip", i), label, dot).on_click(
                    cx.listener(move |this, _: &ClickEvent, _window, cx| {
                        this.remove_filter_token(&token, cx);
                    }),
                ),
            );
        }
        // Inferred phrase chips, after the explicit ones. Each is labelled
        // with what it became (`kind:image`) or, for sizes and dates, the
        // words the user typed — and clicking removes those words.
        for (i, phrase) in phrases.into_iter().enumerate() {
            for (j, filter) in phrase.filters.iter().enumerate() {
                let label = filex::phrases::label_for(filter, &phrase.source);
                let source = phrase.source.clone();
                strip = strip.child(
                    ui::top_bar::filter_chip(&theme, ("phrase-chip", i * 8 + j), label, None)
                        .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                            this.remove_phrase(&source, cx);
                        })),
                );
            }
        }
        Some(strip.into_any_element())
    }

    pub(super) fn render_jobs(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if self.jobs.is_empty() {
            return None;
        }
        let theme = *cx.theme();
        let mut bar = ui::job::jobs_bar(&theme);
        for job in &self.jobs {
            let id = job.id;
            bar = bar.child(
                ui::job::job_row(
                    &theme,
                    ("job", id as usize),
                    job.label.clone(),
                    job.progress.fraction(),
                )
                .child(
                    ui::job::cancel_button(&theme, ("job-cancel", id as usize)).on_click(
                        cx.listener(move |this, _: &ClickEvent, _window, cx| {
                            this.cancel_job(id, cx);
                        }),
                    ),
                ),
            );
        }
        Some(bar.into_any_element())
    }

    /// Settings live in a centred modal over the browse view (rather
    /// than replacing it): a dimmed backdrop closes on an outside click,
    /// Escape closes it too (see the ClearInput handler). `None` while
    /// closed.
    pub(super) fn render_settings_modal(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if !self.settings_open {
            return None;
        }
        let theme = *cx.theme();
        let settings = self.settings.read(cx).settings().clone();
        let file_note: SharedString = match filex::settings::default_settings_file() {
            Some(path) => format!("saved to {}", path.display()).into(),
            None => "no config directory found — settings won't persist".into(),
        };
        let close =
            ui::top_bar::toolbar_button(&theme, "settings-close", "icons/x.svg", theme.text_dim)
                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| this.toggle_settings(cx)))
                .into_any_element();
        let card = ui::settings_pane::settings_card(&theme, "settings-panel")
            .on_click(|_, _, cx| cx.stop_propagation())
            .child(ui::settings_pane::card_header(&theme, "Settings", close))
            .child(ui::settings_pane::choice_row(
                &theme,
                "Appearance",
                "Match the system, or force a light or dark palette",
                self.render_theme_selector(&theme, settings.theme, cx),
            ))
            .child(
                ui::settings_pane::toggle_row(
                    &theme,
                    "show-hidden",
                    "Show hidden files",
                    "Dotfiles and OS-hidden entries in the browse list",
                    settings.show_hidden_files,
                )
                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                    this.settings.update(cx, |store, cx| {
                        store.update(cx, |s| s.show_hidden_files = !s.show_hidden_files);
                    });
                })),
            )
            .child(
                ui::settings_pane::toggle_row(
                    &theme,
                    "index-system-files",
                    "Index system folders",
                    "Include C:\\Windows, Program Files, and the like in search. Off saves memory; takes effect on next rebuild. Folders stay browsable either way.",
                    settings.index_system_files,
                )
                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                    this.settings.update(cx, |store, cx| {
                        store.update(cx, |s| s.index_system_files = !s.index_system_files);
                    });
                })),
            )
            .child(
                ui::settings_pane::toggle_row(
                    &theme,
                    "confirm-delete",
                    "Confirm before deleting",
                    "First press arms; a second press moves the file to the trash",
                    settings.confirm_delete,
                )
                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                    this.settings.update(cx, |store, cx| {
                        store.update(cx, |s| s.confirm_delete = !s.confirm_delete);
                    });
                })),
            )
            .child(
                ui::settings_pane::toggle_row(
                    &theme,
                    "dirs-first",
                    "Directories first",
                    "Group folders above files whatever the sort order",
                    settings.sort.directories_first,
                )
                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                    this.settings.update(cx, |store, cx| {
                        store.update(cx, |s| {
                            s.sort.directories_first = !s.sort.directories_first;
                        });
                    });
                })),
            )
            .child(
                ui::settings_pane::toggle_row(
                    &theme,
                    "thumbnails",
                    "Image thumbnails",
                    "Decode small previews for image files in the list",
                    settings.thumbnails_enabled,
                )
                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                    this.settings.update(cx, |store, cx| {
                        store.update(cx, |s| s.thumbnails_enabled = !s.thumbnails_enabled);
                    });
                })),
            )
            .child(
                ui::settings_pane::toggle_row(
                    &theme,
                    "crash-reports",
                    "Share anonymous diagnostics",
                    "Scrubbed crashes + performance only — never file names, paths, or queries",
                    settings.crash_reports,
                )
                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                    this.settings.update(cx, |store, cx| {
                        store.update(cx, |s| s.crash_reports = !s.crash_reports);
                    });
                    // Turning it on drains anything already queued.
                    this.spawn_crash_upload(cx);
                })),
            )
            .child(ui::settings_pane::footnote(&theme, file_note));
        Some(
            ui::modal::backdrop("settings-backdrop")
                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| this.toggle_settings(cx)))
                .child(card)
                .into_any_element(),
        )
    }

    /// The three-way light/dark/system segmented control for the
    /// Appearance setting. Each segment writes the chosen mode straight
    /// to the store; the Changed event re-resolves and reinstalls the
    /// theme, restyling the whole app live.
    pub(super) fn render_theme_selector(
        &self,
        theme: &Theme,
        current: ThemeMode,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let segment = |id: &'static str, label: &'static str, mode: ThemeMode| {
            ui::settings_pane::segment(theme, id, label, current == mode).on_click(cx.listener(
                move |this, _: &ClickEvent, _window, cx| {
                    this.settings.update(cx, |store, cx| {
                        store.update(cx, |s| s.theme = mode);
                    });
                },
            ))
        };
        ui::settings_pane::segmented(theme)
            .child(segment("theme-system", "System", ThemeMode::System))
            .child(segment("theme-light", "Light", ThemeMode::Light))
            .child(segment("theme-dark", "Dark", ThemeMode::Dark))
            .into_any_element()
    }

    /// A row for one service-managed root (service mode has no local
    /// slots; state comes from the status poll).
    #[cfg(target_os = "windows")]
    pub(super) fn render_service_root_row(
        &self,
        ix: usize,
        root: &filex::index::ipc::RootStatus,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let path = PathBuf::from(&root.path);
        let label: SharedString = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| root.path.clone())
            .into();
        let theme = *cx.theme();
        ui::sidebar::sidebar_row(&theme, ("service-root", ix))
            .child(ui::icon::ui_icon("icons/dot.svg", theme.accent).size(px(14.)))
            .child(div().flex_1().overflow_hidden().child(label))
            .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                this.navigate(path.clone(), cx);
            }))
            .into_any_element()
    }

    // `use<>`: the built element owns its data; without opting out of
    // lifetime capture it couldn't be collected across loop iterations.
    pub(super) fn render_root_row(
        &self,
        ix: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = *cx.theme();
        let slot = &self.roots[ix];
        let path = slot.path.clone();
        // Clicking a healthy root navigates to it; clicking a failed one
        // surfaces why it failed in the status bar.
        let failure: Option<SharedString> = match &slot.state {
            RootState::Failed(err) => Some(err.clone()),
            _ => None,
        };
        // Building spins (indexing is live); ready/failed are static.
        // Sidebar rows aren't virtualized, so animating here is fine.
        let marker = match &slot.state {
            RootState::Building => ui::icon::spinner(
                "icons/loader-circle.svg",
                theme.text_dim,
                14.,
                ("root-spin", ix),
            ),
            RootState::Ready { .. } => ui::icon::ui_icon("icons/dot.svg", theme.accent)
                .size(px(14.))
                .into_any_element(),
            RootState::Failed(_) => ui::icon::ui_icon("icons/triangle-alert.svg", theme.warn)
                .size(px(14.))
                .into_any_element(),
        };
        let menu_path = slot.path.clone();
        let tip = slot.path.display().to_string();
        ui::sidebar::sidebar_row(&theme, ("root", ix))
            .tooltip(ui::tooltip::text_tooltip(tip, theme))
            .child(marker)
            .child(ui::sidebar::sidebar_label(slot.label.clone()))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.open_root_menu(menu_path.clone(), event.position, window, cx);
                }),
            )
            .on_click(
                cx.listener(move |this, _: &ClickEvent, _window, cx| match &failure {
                    Some(err) => {
                        this.notice = Some(err.clone());
                        cx.notify();
                    }
                    None => this.navigate(path.clone(), cx),
                }),
            )
    }

    /// The search-scope dropdown (Anywhere / Current Dir), when open.
    pub(super) fn render_scope_menu(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let position = *self.scope_menu.as_ref()?;
        let theme = *cx.theme();
        let current = self.search_scope;
        let items = [SearchScope::Anywhere, SearchScope::CurrentDir]
            .into_iter()
            .enumerate()
            .map(|(i, scope)| {
                let label = if scope == current {
                    format!("✓ {}", scope.label())
                } else {
                    format!("   {}", scope.label())
                };
                ui::menu::item(&theme, ("scope-item", i), label, false)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                        this.set_scope(scope, cx);
                    }))
                    .into_any_element()
            })
            .collect();
        Some(
            ui::menu::overlay("scope-menu-overlay")
                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                    this.close_scope_menu(cx);
                }))
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(|this, _: &MouseDownEvent, _window, cx| {
                        this.close_scope_menu(cx);
                    }),
                )
                .child(ui::menu::panel(&theme, position, items))
                .into_any_element(),
        )
    }

    pub(super) fn render_context_menu(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let menu = self.context_menu.as_ref()?;
        let theme = *cx.theme();
        let mut items: Vec<gpui::AnyElement> = Vec::new();
        match &menu.target {
            MenuTarget::Entry {
                ix,
                path,
                name,
                is_dir,
                from_search,
            } => {
                let (ix, is_dir, from_search) = (*ix, *is_dir, *from_search);
                // The whole selection is the target; single-item-only
                // actions (open, rename, reveal, index) drop out when
                // more than one row is selected.
                let count = self.active_selection().len();
                let heading =
                    describe_items(&self.selected_paths()).unwrap_or_else(|| name.clone());
                items.push(ui::menu::heading(&theme, heading).into_any_element());

                if count <= 1 {
                    let p = path.clone();
                    items.push(
                        ui::menu::item(&theme, "menu-open", "Open", false)
                            .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                                this.close_menu(cx);
                                this.open_target(p.clone(), is_dir, from_search, cx);
                            }))
                            .into_any_element(),
                    );
                    // "Open With…" only applies to files, and only where the
                    // platform can actually show a chooser (see
                    // `open_with_supported`).
                    if !is_dir && open_with_supported() {
                        let p = path.clone();
                        items.push(
                            ui::menu::item(&theme, "menu-open-with", "Open With…", false)
                                .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                                    this.close_menu(cx);
                                    this.open_with(p.clone(), cx);
                                }))
                                .into_any_element(),
                        );
                    }
                    if from_search {
                        let p = path.clone();
                        items.push(
                            ui::menu::item(&theme, "menu-reveal", "Reveal in Folder", false)
                                .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                                    this.close_menu(cx);
                                    this.reveal(p.clone(), cx);
                                }))
                                .into_any_element(),
                        );
                    } else {
                        items.push(
                            ui::menu::item(&theme, "menu-rename", "Rename", false)
                                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                    this.close_menu(cx);
                                    this.active_selection_mut().select_one(ix);
                                    this.start_rename(window, cx);
                                }))
                                .into_any_element(),
                        );
                    }
                    items.push(ui::menu::separator(&theme).into_any_element());
                }

                items.push(
                    ui::menu::item(&theme, "menu-copy", "Copy", false)
                        .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                            this.close_menu(cx);
                            this.clip_selected(ClipMode::Copy, cx);
                        }))
                        .into_any_element(),
                );
                items.push(
                    ui::menu::item(&theme, "menu-cut", "Cut", false)
                        .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                            this.close_menu(cx);
                            this.clip_selected(ClipMode::Cut, cx);
                        }))
                        .into_any_element(),
                );
                let copy_path_label = if count > 1 { "Copy Paths" } else { "Copy Path" };
                items.push(
                    ui::menu::item(&theme, "menu-copy-path", copy_path_label, false)
                        .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                            this.close_menu(cx);
                            this.copy_selected_paths(cx);
                        }))
                        .into_any_element(),
                );
                if count <= 1 && is_dir && !self.service_mode() {
                    let p = path.clone();
                    items.push(
                        ui::menu::item(&theme, "menu-index", "Index This Folder", false)
                            .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                                this.close_menu(cx);
                                this.add_root(p.clone(), cx);
                            }))
                            .into_any_element(),
                    );
                }
                if count <= 1 && is_dir {
                    let pinned = self.is_favorite(path, cx);
                    let p = path.clone();
                    let label = if pinned {
                        "Unpin from Sidebar"
                    } else {
                        "Pin to Sidebar"
                    };
                    items.push(
                        ui::menu::item(&theme, "menu-pin", label, false)
                            .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                                this.close_menu(cx);
                                if pinned {
                                    this.unpin_favorite(&p, cx);
                                } else {
                                    this.pin_favorite(p.clone(), cx);
                                }
                            }))
                            .into_any_element(),
                    );
                }
                items.push(ui::menu::separator(&theme).into_any_element());
                items.push(
                    ui::menu::item(&theme, "menu-trash", "Move to Trash", true)
                        .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                            this.close_menu(cx);
                            this.trash_selected(cx);
                        }))
                        .into_any_element(),
                );
            }
            MenuTarget::Root { path } => {
                let label = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                items.push(ui::menu::heading(&theme, label).into_any_element());
                let p = path.clone();
                items.push(
                    ui::menu::item(&theme, "menu-remove-root", "Remove from Index", true)
                        .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                            this.close_menu(cx);
                            this.remove_root(&p.clone(), cx);
                        }))
                        .into_any_element(),
                );
            }
            MenuTarget::Favorite { path } => {
                let label = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                items.push(ui::menu::heading(&theme, label).into_any_element());
                let p = path.clone();
                items.push(
                    ui::menu::item(&theme, "fav-up", "Move Up", false)
                        .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                            this.close_menu(cx);
                            this.move_favorite(&p, -1, cx);
                        }))
                        .into_any_element(),
                );
                let p = path.clone();
                items.push(
                    ui::menu::item(&theme, "fav-down", "Move Down", false)
                        .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                            this.close_menu(cx);
                            this.move_favorite(&p, 1, cx);
                        }))
                        .into_any_element(),
                );
                items.push(ui::menu::separator(&theme).into_any_element());
                let p = path.clone();
                items.push(
                    ui::menu::item(&theme, "fav-unpin", "Unpin from Sidebar", true)
                        .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                            this.close_menu(cx);
                            this.unpin_favorite(&p, cx);
                        }))
                        .into_any_element(),
                );
            }
        }
        Some(
            ui::menu::overlay("context-menu-overlay")
                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                    this.close_menu(cx);
                }))
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(|this, _: &MouseDownEvent, _window, cx| {
                        this.close_menu(cx);
                    }),
                )
                .child(ui::menu::panel(&theme, menu.position, items))
                .into_any_element(),
        )
    }

    pub(super) fn render_conflict_modal(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let conflict = self.conflict.as_ref()?;
        let theme = *cx.theme();
        let name = conflict
            .dest
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        Some(
            ui::modal::backdrop("conflict-backdrop")
                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                    this.resolve_conflict(false, cx);
                }))
                .child(
                    ui::modal::panel(&theme, "conflict-panel")
                        .on_click(|_, _, cx| cx.stop_propagation())
                        .child(ui::modal::title(
                            &theme,
                            format!("“{name}” already exists here"),
                        ))
                        .child(ui::modal::message(
                            &theme,
                            "Nothing is overwritten: keep both renames the new one to a \
                             free “name 2” variant.",
                        ))
                        .child(
                            ui::modal::buttons()
                                .child(
                                    ui::modal::button(&theme, "conflict-cancel", "Cancel", false)
                                        .on_click(cx.listener(
                                            |this, _: &ClickEvent, _window, cx| {
                                                this.resolve_conflict(false, cx);
                                            },
                                        )),
                                )
                                .child(
                                    ui::modal::button(&theme, "conflict-keep", "Keep Both", true)
                                        .on_click(cx.listener(
                                            |this, _: &ClickEvent, _window, cx| {
                                                this.resolve_conflict(true, cx);
                                            },
                                        )),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }

    /// The right-hand details/preview panel for the lead item. `None`
    /// when the panel is closed. (Settings now float in a modal over the
    /// browse view rather than replacing it, so the panel stays put
    /// underneath.) Uses `&mut self` because the preview image may
    /// schedule a thumbnail decode, exactly like a list row.
    pub(super) fn render_details_panel(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let settings = self.settings.read(cx).settings();
        if !settings.preview_open {
            return None;
        }
        let width = settings.preview_width;
        let theme = *cx.theme();
        let Some((path, name, is_dir)) = self.lead_item() else {
            return Some(
                ui::details::panel(&theme, width)
                    .child(ui::details::empty(&theme, "No selection"))
                    .into_any_element(),
            );
        };
        let kind = FileKind::of(&name, is_dir);
        let preview = self.render_icon_cell(&name, &path, is_dir, 160., cx);
        let meta = self.preview_meta.as_ref().filter(|m| m.path == path);
        let now = std::time::SystemTime::now();

        let size_value: SharedString = if is_dir {
            "—".into()
        } else {
            match meta {
                Some(m) => format_size(m.size).into(),
                None => "…".into(),
            }
        };
        let modified_value: SharedString = match meta {
            Some(m) => format_modified(m.modified, now).into(),
            None => "…".into(),
        };
        let created_value: SharedString = match meta {
            Some(m) => format_modified(m.created, now).into(),
            None => "…".into(),
        };
        let where_value: SharedString = path
            .parent()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
            .into();

        let mut panel = ui::details::panel(&theme, width)
            .child(ui::details::preview_box(&theme).child(preview))
            .child(ui::details::title(&theme, name))
            .child(ui::details::divider(&theme))
            .child(ui::details::meta_row(&theme, "Kind", kind.label()))
            .child(ui::details::meta_row(&theme, "Size", size_value))
            .child(ui::details::meta_row(&theme, "Modified", modified_value))
            .child(ui::details::meta_row(&theme, "Created", created_value));
        if let Some((w, h)) = meta.and_then(|m| m.dimensions) {
            panel = panel.child(ui::details::meta_row(
                &theme,
                "Dimensions",
                format!("{w} × {h}"),
            ));
        }
        let panel = panel
            .child(ui::details::divider(&theme))
            .child(ui::details::meta_row(&theme, "Where", where_value))
            .child(ui::details::divider(&theme))
            .child(self.render_tags_section(&path, &theme, cx));
        Some(panel.into_any_element())
    }

    /// The details-panel "Tags" section: the item's chips (each opens the
    /// editor), an add affordance, and the inline editor when open.
    pub(super) fn render_tags_section(
        &self,
        path: &Path,
        theme: &ui::theme::Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let mut chips = ui::details::wrap_row();
        for (i, tag) in self.preview_tags.iter().enumerate() {
            let for_edit = tag.clone();
            chips = chips.child(
                ui::details::tag_chip(theme, ("tag-chip", i), tag.name.clone(), tag.color)
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.open_tag_editor(Some(for_edit.clone()), window, cx);
                    })),
            );
        }
        chips = chips.child(ui::details::add_tag_chip(theme, "tag-add").on_click(
            cx.listener(|this, _: &ClickEvent, window, cx| this.open_tag_editor(None, window, cx)),
        ));

        let mut section = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(ui::details::section_label(theme, "Tags"))
            .child(chips);

        if let Some(editor) = self.tag_editor.as_ref().filter(|e| e.path == path) {
            section = section.child(self.render_tag_editor(editor, theme, cx));
        }
        section.into_any_element()
    }

    /// The inline tag editor card: name input, color swatches, and the
    /// Save / Remove / Cancel buttons.
    pub(super) fn render_tag_editor(
        &self,
        editor: &TagEditor,
        theme: &ui::theme::Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let selected = editor.color;
        let is_existing = editor.existing.is_some();

        let input_box = div()
            .px_2()
            .py_1()
            .rounded_md()
            .border_1()
            .border_color(theme.accent)
            .text_sm()
            .child(editor.input.clone());

        let mut swatches = ui::details::wrap_row().child(
            ui::details::tag_swatch(theme, "tag-sw-none", None, selected.is_none()).on_click(
                cx.listener(|this, _: &ClickEvent, _window, cx| this.set_editor_color(None, cx)),
            ),
        );
        for color in TagColor::all() {
            swatches = swatches.child(
                ui::details::tag_swatch(
                    theme,
                    ("tag-sw", color.finder_index() as usize),
                    Some(color),
                    selected == Some(color),
                )
                .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                    this.set_editor_color(Some(color), cx);
                })),
            );
        }

        let mut footer = div().flex().items_center().gap_2().child(
            ui::details::tag_button(
                theme,
                "tag-save",
                if is_existing { "Save" } else { "Add" },
                false,
            )
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.commit_tag_editor(window, cx);
            })),
        );
        if is_existing {
            footer = footer.child(
                ui::details::tag_button(theme, "tag-remove", "Remove", true).on_click(cx.listener(
                    |this, _: &ClickEvent, window, cx| {
                        this.remove_editing_tag(window, cx);
                    },
                )),
            );
        }
        footer = footer.child(
            ui::details::tag_button(theme, "tag-cancel", "Cancel", false).on_click(
                cx.listener(|this, _: &ClickEvent, window, cx| this.cancel_tag_editor(window, cx)),
            ),
        );

        ui::details::tag_editor_box(theme)
            .child(input_box)
            .child(swatches)
            .child(footer)
            .into_any_element()
    }
}

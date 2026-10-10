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
                ui::button::button(
                    &theme,
                    "update-action",
                    label,
                    ui::button::Variant::Primary,
                    ui::button::Size::Small,
                )
                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                    this.apply_update_action(cx);
                })),
            );
        }
        bar = bar.child(
            ui::button::icon_button(
                &theme,
                "update-dismiss",
                "icons/x.svg",
                theme.text_dim,
                ui::button::IconSize::Small,
            )
            .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                this.dismiss_update(cx);
            })),
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
        let tokens = filex::search::filter::filter_tokens(&source, now);
        let residual = filex::search::filter::parse_query(&source, now).text;
        let phrases = match &self.magic {
            Some(_) => filex::search::phrases::expand_as_description(&residual, now).phrases,
            None => filex::search::phrases::expand(&residual, now).phrases,
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
                let label = filex::search::phrases::label_for(filter, &phrase.source);
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
                    ui::button::icon_button(
                        &theme,
                        ("job-cancel", id as usize),
                        "icons/x.svg",
                        theme.text_dim,
                        ui::button::IconSize::Small,
                    )
                    .on_click(cx.listener(
                        move |this, _: &ClickEvent, _window, cx| {
                            this.cancel_job(id, cx);
                        },
                    )),
                ),
            );
        }
        Some(bar.into_any_element())
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
                if count <= 1 && is_dir {
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
                if count <= 1 && is_dir {
                    let p = path.clone();
                    items.push(
                        ui::menu::item(&theme, "menu-folder-icon", "Folder Icon…", false)
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                cx.stop_propagation();
                                this.open_folder_icon_menu(p.clone(), window, cx);
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
                    ui::menu::item(&theme, "fav-folder-icon", "Folder Icon…", false)
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            cx.stop_propagation();
                            this.open_folder_icon_menu(p.clone(), window, cx);
                        }))
                        .into_any_element(),
                );
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
            MenuTarget::FolderIcon { path } => {
                items.push(ui::menu::heading(&theme, "Folder Icon").into_any_element());
                let current = self
                    .settings
                    .read(cx)
                    .settings()
                    .folder_icons
                    .get(path)
                    .copied();
                let choices = [
                    (None, "Automatic"),
                    (Some(FolderIcon::Plain), "Plain"),
                    (Some(FolderIcon::Pictures), "Pictures"),
                    (Some(FolderIcon::Music), "Music"),
                    (Some(FolderIcon::Videos), "Videos"),
                    (Some(FolderIcon::Documents), "Documents"),
                    (Some(FolderIcon::Downloads), "Downloads"),
                    (Some(FolderIcon::Code), "Code"),
                    (Some(FolderIcon::Archives), "Archives"),
                    (Some(FolderIcon::Desktop), "Desktop"),
                ];
                for (ix, (choice, label)) in choices.into_iter().enumerate() {
                    let p = path.clone();
                    let label = if current == choice {
                        format!("✓ {label}")
                    } else {
                        format!("   {label}")
                    };
                    items.push(
                        ui::menu::item(&theme, ("folder-icon-choice", ix), label, false)
                            .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                                this.close_menu(cx);
                                this.settings.update(cx, |store, cx| {
                                    store.update(cx, |settings| {
                                        if let Some(icon) = choice {
                                            settings.folder_icons.insert(p.clone(), icon);
                                        } else {
                                            settings.folder_icons.remove(&p);
                                        }
                                    });
                                });
                            }))
                            .into_any_element(),
                    );
                }
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
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let settings = self.settings.read(cx).settings();
        if !settings.preview_open {
            return None;
        }
        let width = ui::details::fitted_width(
            settings.preview_width,
            f32::from(window.viewport_size().width) - ui::sidebar::SIDEBAR_WIDTH,
        );
        let theme = *cx.theme();
        let Some((path, name, is_dir)) = self.lead_item() else {
            return Some(
                ui::details::panel(&theme, width)
                    .child(ui::details::empty(
                        &theme,
                        "Select a file to see its preview and details.",
                    ))
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
            ui::button::button(
                theme,
                "tag-save",
                if is_existing { "Save" } else { "Add" },
                ui::button::Variant::Primary,
                ui::button::Size::Small,
            )
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.commit_tag_editor(window, cx);
            })),
        );
        if is_existing {
            footer = footer.child(
                ui::button::button(
                    theme,
                    "tag-remove",
                    "Remove",
                    ui::button::Variant::Danger,
                    ui::button::Size::Small,
                )
                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                    this.remove_editing_tag(window, cx);
                })),
            );
        }
        footer = footer.child(
            ui::button::button(
                theme,
                "tag-cancel",
                "Cancel",
                ui::button::Variant::Secondary,
                ui::button::Size::Small,
            )
            .on_click(
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

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
        // words the user typed — and clicking removes those words. When
        // file names matched the query as typed, the search ran as a plain
        // filename search instead, and the chips offer the filters.
        let suggested = self.magic.is_none() && self.phrases_literal.as_ref() == Some(&self.query);
        for (i, phrase) in phrases.into_iter().enumerate() {
            for (j, filter) in phrase.filters.iter().enumerate() {
                let label = filex::search::phrases::label_for(filter, &phrase.source);
                let id = ("phrase-chip", i * 8 + j);
                if suggested {
                    strip = strip.child(ui::top_bar::suggestion_chip(&theme, id, label).on_click(
                        cx.listener(|this, _: &ClickEvent, _window, cx| {
                            this.use_phrases_as_filters(cx);
                        }),
                    ));
                    continue;
                }
                let source = phrase.source.clone();
                strip = strip.child(ui::top_bar::filter_chip(&theme, id, label, None).on_click(
                    cx.listener(move |this, _: &ClickEvent, _window, cx| {
                        this.remove_phrase(&source, cx);
                    }),
                ));
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

    /// The open context menu: a full-window overlay (click/right-click
    /// outside closes) holding the main panel and, beside it, any open
    /// submenu. The overlay owns keyboard focus while it's up, so the
    /// `ContextMenu` key context routes arrows/Enter/Escape to the menu.
    pub(super) fn render_context_menu(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let menu = self.context_menu.as_ref()?;
        let theme = *cx.theme();
        let main = self.menu_rows(MenuPanel::Main, &menu.rows, menu.focus, &theme, cx);
        let submenu = menu.submenu.as_ref().map(|sub| {
            let rows = self.menu_rows(MenuPanel::Sub, &sub.rows, sub.focus, &theme, cx);
            ui::menu::scroll_panel(
                &theme,
                "context-submenu",
                sub.position,
                sub.max_height,
                rows,
            )
        });
        Some(
            ui::menu::overlay("context-menu-overlay")
                .track_focus(&menu.focus_handle)
                .key_context("ContextMenu")
                .on_action(cx.listener(|this, _: &MenuPrev, window, cx| {
                    this.menu_key(menus::MenuKey::Prev, window, cx)
                }))
                .on_action(cx.listener(|this, _: &MenuNext, window, cx| {
                    this.menu_key(menus::MenuKey::Next, window, cx)
                }))
                .on_action(cx.listener(|this, _: &MenuLeft, window, cx| {
                    this.menu_key(menus::MenuKey::Left, window, cx)
                }))
                .on_action(cx.listener(|this, _: &MenuRight, window, cx| {
                    this.menu_key(menus::MenuKey::Right, window, cx)
                }))
                .on_action(cx.listener(|this, _: &MenuConfirm, window, cx| {
                    this.menu_key(menus::MenuKey::Confirm, window, cx)
                }))
                .on_action(cx.listener(|this, _: &MenuDismiss, window, cx| {
                    this.menu_key(menus::MenuKey::Dismiss, window, cx)
                }))
                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                    this.close_menu(cx);
                }))
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(|this, _: &MouseDownEvent, _window, cx| {
                        this.close_menu(cx);
                    }),
                )
                .child(ui::menu::panel(&theme, menu.position, main))
                .children(submenu)
                .into_any_element(),
        )
    }

    /// Elements for one panel's rows, each wired to hover (highlight +
    /// submenu intent) and click.
    fn menu_rows(
        &self,
        panel: MenuPanel,
        rows: &[menu_model::Row],
        focus: Option<menu_model::Focus>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let overrides = self.settings.read(cx).settings().keyboard_shortcuts.clone();
        let catalog = shortcuts::catalog();
        let shortcut_label = |id: &str| -> Option<SharedString> {
            let spec = catalog.iter().find(|spec| spec.id == id)?;
            let key = shortcuts::active_keys(spec, &overrides)
                .into_iter()
                .next()?;
            Some(shortcuts::label(&key).into())
        };
        let tag = match panel {
            MenuPanel::Main => "menu-main",
            MenuPanel::Sub => "menu-sub",
        };
        let wire =
            |element: gpui::Stateful<gpui::Div>, at: menu_model::Focus, cx: &mut Context<Self>| {
                element
                    .on_hover(cx.listener(move |this, hovered: &bool, window, cx| {
                        if *hovered {
                            this.menu_hover(panel, at, window, cx);
                        }
                    }))
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.menu_click(panel, at, window, cx);
                    }))
            };
        let mut out = Vec::with_capacity(rows.len());
        for (row_ix, row) in rows.iter().enumerate() {
            let element = match row {
                menu_model::Row::Heading(text) => {
                    ui::menu::heading(theme, text.clone()).into_any_element()
                }
                menu_model::Row::Separator => ui::menu::separator(theme).into_any_element(),
                menu_model::Row::Item(item) => {
                    let at = menu_model::Focus::row(row_ix);
                    let shortcut = item.shortcut.and_then(shortcut_label);
                    let element = ui::menu::model_row(
                        theme,
                        (tag, row_ix),
                        item,
                        shortcut,
                        focus == Some(at),
                    );
                    wire(element, at, cx).into_any_element()
                }
                menu_model::Row::Icons(items) => {
                    let buttons = items
                        .iter()
                        .enumerate()
                        .map(|(col, item)| {
                            let at = menu_model::Focus { row: row_ix, col };
                            let id = gpui::ElementId::from((tag, row_ix * 16 + col));
                            let button = ui::menu::icon_button(theme, id, item, focus == Some(at));
                            wire(button, at, cx).into_any_element()
                        })
                        .collect();
                    ui::menu::icon_bar(buttons).into_any_element()
                }
            };
            out.push(element);
        }
        out
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
        Some(ui::modal::present(
            "conflict-backdrop-in",
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
                ),
        ))
    }

    /// The right-hand details/preview panel for the lead item. `None`
    /// when the panel is closed. (Settings now float in a modal over the
    /// browse view rather than replacing it, so the panel stays put
    /// underneath.) Uses `&mut self` because the preview image may
    /// schedule a thumbnail decode, exactly like a list row.
    /// The details panel, clipped to its animated width: it slides open
    /// and shut while its contents keep their full-width layout (no
    /// reflow mid-slide).
    pub(super) fn render_details_panel(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let frac = self.preview_frac;
        if frac <= 0. {
            return None;
        }
        let width = ui::details::fitted_width(
            self.settings.read(cx).settings().preview_width,
            f32::from(window.viewport_size().width) - ui::sidebar::SIDEBAR_WIDTH,
        );
        let panel = self.render_details_content(window, cx);
        Some(
            div()
                .flex_none()
                .h_full()
                .w(px(width * frac))
                .overflow_hidden()
                .opacity(frac)
                .child(panel)
                .into_any_element(),
        )
    }

    fn render_details_content(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let settings = self.settings.read(cx).settings();
        let width = ui::details::fitted_width(
            settings.preview_width,
            f32::from(window.viewport_size().width) - ui::sidebar::SIDEBAR_WIDTH,
        );
        let theme = *cx.theme();
        let Some((path, name, is_dir)) = self.lead_item() else {
            return ui::details::panel(&theme, width)
                .child(ui::details::empty(
                    &theme,
                    "Select a file to see its preview and details.",
                ))
                .into_any_element();
        };
        let kind = FileKind::of(&name, is_dir);
        let preview = self.render_icon_cell(&name, &path, is_dir, 160., cx);
        let meta = self.preview_meta.as_ref().filter(|m| m.path == path);

        let size_value: SharedString = if is_dir {
            "—".into()
        } else {
            match meta {
                Some(m) => format_size(m.size).into(),
                None => "…".into(),
            }
        };
        let modified_value: SharedString = match meta {
            Some(m) => format_timestamp(m.modified).into(),
            None => "…".into(),
        };
        let created_value: SharedString = match meta {
            Some(m) => format_timestamp(m.created).into(),
            None => "…".into(),
        };
        let where_value: SharedString = path.parent().map(display_path).unwrap_or_default().into();

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
            .child(ui::details::meta_block(&theme, "Where", where_value))
            .child(ui::details::divider(&theme))
            .child(self.render_tags_section(&path, &theme, cx));
        panel.into_any_element()
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

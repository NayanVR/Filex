//! The virtualized list, grid, and Magic-plan bodies, plus the icon cell.

use super::*;

impl Workspace {
    /// The icon cell for a row: a decoded thumbnail for image files when
    /// ready, otherwise the kind glyph. May schedule a decode as a side
    /// effect — only rows the virtualized list renders get here.
    pub(super) fn render_icon_cell(
        &mut self,
        name: &str,
        path: &Path,
        is_dir: bool,
        edge: f32,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let kind = FileKind::of(name, is_dir);
        if kind == FileKind::Image && self.settings.read(cx).settings().thumbnails_enabled {
            match self.thumbnails.get(path) {
                Some(ThumbnailState::Ready(imagery)) => {
                    return ui::icon::thumbnail_icon(imagery.clone(), edge);
                }
                Some(_) => {}
                None => self.request_thumbnail(path.to_path_buf(), cx),
            }
        }
        ui::icon::file_icon(cx.theme(), kind, edge)
    }

    /// The full-pane magic view (`docs/design-magic-mode.md` v2): the plan
    /// *replaces* the results list rather than floating above it. Shown
    /// whenever [`in_magic_view`](Self::in_magic_view) holds.
    ///
    /// Three shapes: a hint (magic mode on, nothing typed yet), a resolved
    /// plan (the reviewable op list with confirm/cancel), or a refusal
    /// (the command parsed but couldn't build a plan — say why).
    pub(super) fn render_magic_pane(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = *cx.theme();
        let Some(state) = self.magic.as_ref() else {
            // Explicit magic mode, no command yet.
            return ui::magic_card::pane()
                .child(ui::magic_card::pane_hint(
                    &theme,
                    "Type a command",
                    &[
                        "move all pdfs to Documents",
                        "delete screenshots older than 30 days",
                        "rename * to invoice-{n}.{ext}",
                    ],
                ))
                .into_any_element();
        };

        let verb = state.command.verb;
        let destructive = verb == filex::magic::Verb::Delete;
        let echo = format!("matching “{}”", state.command.selection.source);

        let mut header = ui::magic_card::pane_header(&theme);
        let mut body: Option<gpui::AnyElement> = None;
        let mut action_bar: Option<gpui::Div> = None;

        match &state.outcome {
            None => {
                header = header
                    .child(ui::magic_card::heading(&theme, verb.label()))
                    .child(ui::magic_card::subtitle(
                        &theme,
                        if self.any_root_ready() {
                            "finding matches…"
                        } else {
                            "still indexing — matches will appear when ready…"
                        },
                    ));
                header = header.child(ui::magic_card::subtitle(&theme, echo));
            }
            Some(Ok(plan)) => {
                let count = state.checked.iter().filter(|c| **c).count();
                header = header
                    .child(ui::magic_card::heading(
                        &theme,
                        format!("{} {} {}", verb.label(), count, plural_items(count)),
                    ))
                    .child(ui::magic_card::subtitle(&theme, echo));
                if plan.skipped > 0 {
                    header = header.child(ui::magic_card::subtitle(
                        &theme,
                        format!(
                            "{} already {} — nothing to do for {}",
                            plan.skipped,
                            if verb == filex::magic::Verb::Rename {
                                "named that"
                            } else {
                                "there"
                            },
                            if plan.skipped == 1 { "it" } else { "them" },
                        ),
                    ));
                }

                // Virtualized: a 342-op plan renders only the visible rows.
                // Building every row as a child (the old approach) is what
                // made the plan view sluggish to scroll on a large batch.
                let list = uniform_list(
                    "magic-plan",
                    plan.ops.len(),
                    cx.processor(|this, range: Range<usize>, _window, cx| {
                        let theme = *cx.theme();
                        let Some(state) = this.magic.as_ref() else {
                            return Vec::new();
                        };
                        let Some(Ok(plan)) = state.outcome.as_ref() else {
                            return Vec::new();
                        };
                        range
                            .filter_map(|ix| {
                                let op = plan.ops.get(ix)?;
                                let checked = state.checked.get(ix).copied().unwrap_or(false);
                                let PlanRow {
                                    name,
                                    location,
                                    dest,
                                    tooltip,
                                } = describe_op(op);
                                Some(
                                    ui::magic_card::op_row(
                                        &theme,
                                        ("magic-op", ix),
                                        checked,
                                        name,
                                        location,
                                        dest,
                                    )
                                    .tooltip(ui::tooltip::text_tooltip(tooltip, theme))
                                    .on_click(cx.listener(
                                        move |this, _: &ClickEvent, _window, cx| {
                                            this.toggle_magic_op(ix, cx);
                                        },
                                    )),
                                )
                            })
                            .collect()
                    }),
                )
                .track_scroll(self.magic_scroll.clone())
                .with_decoration(ui::scrollbar::scrollbar(
                    self.magic_scroll.clone(),
                    self.magic_scrollbar.clone(),
                    cx.theme(),
                ))
                .flex_1();
                body = Some(list.into_any_element());

                // Select all / Deselect all: labelled by the current state
                // so one click always flips the whole plan the other way.
                let all_checked = !plan.ops.is_empty() && count == plan.ops.len();
                let toggle_label = if all_checked {
                    "Deselect all"
                } else {
                    "Select all"
                };
                action_bar = Some(
                    ui::magic_card::pane_actions(&theme)
                        .justify_between()
                        .child(
                            ui::magic_card::secondary_button(
                                &theme,
                                "magic-select-all",
                                toggle_label,
                            )
                            .on_click(cx.listener(
                                move |this, _: &ClickEvent, _window, cx| {
                                    this.set_all_magic_ops(!all_checked, cx);
                                },
                            )),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    ui::magic_card::secondary_button(
                                        &theme,
                                        "magic-cancel",
                                        "Cancel",
                                    )
                                    .on_click(cx.listener(
                                        |this, _: &ClickEvent, _window, cx| {
                                            this.clear_search(cx);
                                        },
                                    )),
                                )
                                .child(
                                    ui::magic_card::confirm_button(
                                        &theme,
                                        "magic-confirm",
                                        verb.label(),
                                        count > 0,
                                        destructive,
                                    )
                                    .on_click(cx.listener(
                                        |this, _: &ClickEvent, _window, cx| {
                                            this.confirm_magic(cx);
                                        },
                                    )),
                                ),
                        ),
                );
            }
            Some(Err(err)) => {
                header = header
                    .child(ui::magic_card::heading(&theme, verb.label()))
                    .child(ui::magic_card::subtitle(&theme, echo))
                    .child(ui::magic_card::refusal(&theme, format!("{err}")));
            }
        }

        ui::magic_card::pane()
            .child(header)
            .children(body)
            .children(action_bar)
            .into_any_element()
    }

    /// The card grid: a `uniform_list` whose rows are strips of N cards,
    /// N derived from the pane width so it reflows on resize. Each row
    /// builds only its own cards, so the grid is as virtualized as the
    /// list.
    pub(super) fn render_grid(&self, window: &Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let settings = self.settings.read(cx).settings();
        let size = ui::grid::card_size(settings.grid_zoom);
        let cell = ui::grid::cell_width(size);
        let preview_w = if settings.preview_open {
            ui::details::clamp_width(settings.preview_width)
        } else {
            0.
        };
        // Approximate content width: the window minus the sidebar, the
        // open details panel, and the grid's own inset. Off-by-one on the
        // odd frame just reflows.
        let content_w = (f32::from(window.viewport_size().width)
            - ui::sidebar::SIDEBAR_WIDTH
            - preview_w
            - ui::grid::CARD_GAP * 2.)
            .max(cell);
        let cols = ui::grid::columns_for(content_w, cell);
        let rows = self.entries.len().div_ceil(cols);
        uniform_list(
            "grid",
            rows,
            cx.processor(move |this, range: Range<usize>, _window, cx| {
                let theme = *cx.theme();
                range
                    .map(|row| {
                        let start = row * cols;
                        let end = (start + cols).min(this.entries.len());
                        let mut strip = ui::grid::grid_row(size);
                        for ix in start..end {
                            strip = strip.child(this.render_card(ix, size, &theme, cx));
                        }
                        strip
                    })
                    .collect()
            }),
        )
        .track_scroll(self.browse_scroll.clone())
        .with_decoration(ui::scrollbar::scrollbar(
            self.browse_scroll.clone(),
            self.browse_scrollbar.clone(),
            cx.theme(),
        ))
        .flex_1()
        .into_any_element()
    }

    /// One browse card for entry `ix`. Copies the row data out before
    /// asking for the icon (which needs `&mut self`), mirroring the list
    /// processor.
    pub(super) fn render_card(
        &mut self,
        ix: usize,
        size: f32,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(entry) = self.entries.get(ix) else {
            return div().into_any_element();
        };
        let is_dir = entry.is_dir;
        let is_selected = self.active_selection().contains(ix);
        let (name, path) = (entry.name.clone(), entry.path.clone());
        let detail: SharedString = if is_dir {
            format_modified(entry.modified, std::time::SystemTime::now()).into()
        } else {
            format_size(entry.size).into()
        };
        let name_tip: SharedString = name.clone().into();
        let icon = self.render_icon_cell(&name, &path, is_dir, size, cx);
        let drag = self.drag_items(ix, *theme);
        let card = ui::grid::card(theme, ("card", ix), size, is_selected)
            .tooltip(ui::tooltip::text_tooltip(name_tip, *theme))
            .child(ui::grid::card_icon_area(size).child(icon))
            .child(ui::grid::card_name(theme, size).child(name))
            .child(
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(theme.text_dim)
                    .child(detail),
            )
            .on_click(cx.listener(move |this, event: &ClickEvent, _window, cx| {
                if event.click_count() >= 2 {
                    this.activate(ix, cx);
                } else {
                    this.select_click(ix, event.modifiers(), cx);
                }
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.open_entry_menu(ix, false, event.position, window, cx);
                }),
            );
        let card = card.when_some(drag, |card, items| {
            card.on_drag(items, |items, position, _window, cx| {
                cx.new(|_| items.clone().at(position))
            })
        });
        let card = if is_dir {
            self.folder_drop_target(card, path, *theme, cx)
        } else {
            card
        };
        card.into_any_element()
    }

    pub(super) fn render_file_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        uniform_list(
            "entries",
            self.entries.len(),
            cx.processor(|this, range: Range<usize>, _window, cx| {
                let theme = *cx.theme();
                range
                    .filter_map(|ix| {
                        // Copy row data out first: the icon cell needs &mut self (it may
                        // schedule a thumbnail decode) while `entry` borrows self.
                        let entry = this.entries.get(ix)?;
                        let is_dir = entry.is_dir;
                        let size = entry.size;
                        let modified = entry.modified;
                        let is_selected = this.active_selection().contains(ix);
                        let (name, path) = (entry.name.clone(), entry.path.clone());
                        let name_tip: SharedString = name.clone().into();
                        let icon =
                            this.render_icon_cell(&name, &path, is_dir, ui::icon::ICON_SIZE, cx);
                        let rename_input = this
                            .renaming
                            .as_ref()
                            .filter(|rename| rename.ix == ix)
                            .map(|rename| rename.input.clone());
                        let is_renaming = rename_input.is_some();
                        let name_cell = match rename_input {
                            Some(input) => div()
                                .flex_1()
                                .px_1()
                                .rounded_sm()
                                .border_1()
                                .border_color(theme.accent)
                                .text_sm()
                                .child(input)
                                .into_any_element(),
                            None => div()
                                .flex_1()
                                .min_w_0()
                                .child(div().w_full().text_sm().truncate().child(name))
                                .into_any_element(),
                        };
                        let drag = (!is_renaming).then(|| this.drag_items(ix, theme)).flatten();
                        let row = ui::list_row::list_row(&theme, ix, is_selected)
                            // Full name on hover (helps when it's truncated);
                            // suppressed mid-rename so it doesn't cover the field.
                            .when(!is_renaming, |row| {
                                row.tooltip(ui::tooltip::text_tooltip(name_tip, theme))
                            })
                            .child(icon)
                            .child(name_cell)
                            .child(ui::list_row::detail_cell(
                                &theme,
                                ui::list_row::MODIFIED_COL_WIDTH,
                                format_modified(modified, std::time::SystemTime::now()),
                            ))
                            .child(ui::list_row::detail_cell(
                                &theme,
                                ui::list_row::SIZE_COL_WIDTH,
                                if is_dir {
                                    "—".to_string()
                                } else {
                                    format_size(size)
                                },
                            ))
                            .on_click(cx.listener(move |this, event: &ClickEvent, _window, cx| {
                                if event.click_count() >= 2 {
                                    this.activate(ix, cx);
                                } else {
                                    this.select_click(ix, event.modifiers(), cx);
                                }
                            }))
                            .on_mouse_down(
                                MouseButton::Right,
                                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                                    this.open_entry_menu(ix, false, event.position, window, cx);
                                }),
                            );
                        // Any row can be a drag source; only folders accept a drop.
                        let row = row.when_some(drag, |row, items| {
                            row.on_drag(items, |items, position, _window, cx| {
                                cx.new(|_| items.clone().at(position))
                            })
                        });
                        let row = row.when(is_dir, |row| {
                            this.folder_drop_target(row, path.clone(), theme, cx)
                        });
                        Some(row)
                    })
                    .collect()
            }),
        )
        .track_scroll(self.browse_scroll.clone())
        .with_decoration(ui::scrollbar::scrollbar(
            self.browse_scroll.clone(),
            self.browse_scrollbar.clone(),
            cx.theme(),
        ))
        .flex_1()
    }

    pub(super) fn render_search_results(&self, cx: &mut Context<Self>) -> impl IntoElement {
        uniform_list(
            "results",
            self.results.len(),
            cx.processor(|this, range: Range<usize>, _window, cx| {
                let theme = *cx.theme();
                range
                    .filter_map(|ix| {
                        let row = this.results.get(ix)?;
                        let is_dir = row.is_dir;
                        let is_selected = this.active_selection().contains(ix);
                        let (name, path) = (row.name.clone(), row.target.clone());
                        let path_label = row.path_label.clone();
                        let icon =
                            this.render_icon_cell(&name, &path, is_dir, ui::icon::ICON_SIZE, cx);
                        Some(
                            ui::list_row::list_row(&theme, ix, is_selected)
                                // Full path on hover — the row truncates it below.
                                .tooltip(ui::tooltip::text_tooltip(path_label.clone(), theme))
                                .child(icon)
                                .child(div().flex_none().text_sm().whitespace_nowrap().child(name))
                                // flex_1 + min_w_0 lets the cell shrink; the inner
                                // w_full gives the text a *definite* width, which
                                // gpui needs to actually paint the … ellipsis
                                // (a bare flex child never truncates).
                                .child(
                                    div().flex_1().min_w_0().child(
                                        div()
                                            .w_full()
                                            .text_xs()
                                            .text_color(theme.text_dim)
                                            .truncate()
                                            .child(path_label),
                                    ),
                                )
                                .on_click(cx.listener(
                                    move |this, event: &ClickEvent, _window, cx| {
                                        if event.click_count() >= 2 {
                                            this.activate(ix, cx);
                                        } else {
                                            this.select_click(ix, event.modifiers(), cx);
                                        }
                                    },
                                ))
                                .on_mouse_down(
                                    MouseButton::Right,
                                    cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                                        this.open_entry_menu(ix, true, event.position, window, cx);
                                    }),
                                ),
                        )
                    })
                    .collect()
            }),
        )
        .track_scroll(self.results_scroll.clone())
        .with_decoration(ui::scrollbar::scrollbar(
            self.results_scroll.clone(),
            self.results_scrollbar.clone(),
            cx.theme(),
        ))
        .flex_1()
    }
}

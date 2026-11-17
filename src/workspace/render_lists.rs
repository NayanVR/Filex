//! The virtualized list, grid, and Magic-plan bodies, plus the icon cell.

use super::*;

impl Workspace {
    /// The icon cell for a row: a decoded thumbnail for image files when
    /// ready, otherwise folder or document artwork. May schedule a decode as a side
    /// effect — only rows the virtualized list renders get here.
    ///
    /// A thumbnail fills `edge`; artwork is drawn at `edge * art_scale`, so a
    /// grid can give previews the whole card while icons stay balanced —
    /// including an image's icon shown until (or instead of) its preview.
    pub(super) fn render_icon_cell(
        &mut self,
        name: &str,
        path: &Path,
        is_dir: bool,
        edge: f32,
        art_scale: f32,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let kind = FileKind::of(name, is_dir);
        let thumbnail_candidate = kind == FileKind::Image
            || (cfg!(windows) && !is_dir && filex::platform_preview::policy::shell_candidate(path));
        if thumbnail_candidate && self.settings.read(cx).settings().thumbnails_enabled {
            match self.thumbnails.get(path) {
                Some(ThumbnailState::Ready(imagery)) => {
                    return ui::icon::thumbnail_icon(imagery.clone(), edge);
                }
                Some(_) => {}
                None => self.request_thumbnail(path.to_path_buf(), cx),
            }
        }
        let edge = art_edge(edge, art_scale);
        if is_dir {
            let icon = self
                .settings
                .read(cx)
                .settings()
                .folder_icons
                .get(path)
                .copied()
                .unwrap_or_else(|| ui::icon::automatic_folder_icon(path));
            return ui::icon::folder_icon(cx.theme(), icon, edge);
        }
        ui::icon::file_icon(cx.theme(), name, edge)
    }

    /// The full-pane magic view (`docs/design-magic-mode.md` v2): the plan
    /// *replaces* the results list rather than floating above it. Shown
    /// whenever [`in_magic_view`](Self::in_magic_view) holds.
    ///
    /// Without a plan it is the idle pane (suggestions, a go/app command
    /// awaiting Enter, an answer). With one: the reviewable list with
    /// confirm/cancel, or a refusal that says why there is no plan.
    pub(super) fn render_magic_pane(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = *cx.theme();
        let Some(state) = self.magic.as_ref() else {
            return self.render_magic_idle(cx);
        };

        let verb = state.command.verb;
        let destructive = verb == filex::magic::Verb::Delete;
        let cancel = |id: &'static str, cx: &mut Context<Self>| {
            ui::button::button(
                &theme,
                id,
                "Cancel",
                ui::button::Variant::Secondary,
                ui::button::Size::Regular,
            )
            .on_click(cx.listener(|this, _, _, cx| this.clear_search(cx)))
        };
        if let Some(error) = &state.error {
            return ui::magic_card::pane()
                .child(ui::pane::message_state(
                    &theme,
                    "icons/search.svg",
                    "Couldn’t prepare this action",
                    error.clone(),
                ))
                .child(
                    ui::magic_card::pane_actions(&theme)
                        .child(cancel("magic-error-cancel", cx))
                        .child(
                            ui::button::button(
                                &theme,
                                "magic-retry",
                                "Try again",
                                ui::button::Variant::Primary,
                                ui::button::Size::Regular,
                            )
                            .on_click(cx.listener(|this, _, _, cx| this.update_search(cx))),
                        ),
                )
                .into_any_element();
        }
        // What the plan is (verb, count, what it matched) is the pane's
        // title — `magic_header`. Under it go only the things worth a
        // second look. A paraphrase or typo fix is shown in every state,
        // never applied invisibly.
        let mut notes: Vec<gpui::Div> = Vec::new();
        let read_note = || {
            self.magic_read_as.as_ref().map(|read_as| {
                ui::magic_card::note(&theme, "icons/sparkles.svg", format!("Read as: {read_as}"))
            })
        };
        let mut body: Option<gpui::AnyElement> = None;
        let mut action_bar: Option<gpui::Div> = None;

        match &state.outcome {
            None => {
                // Echo second in every state. It used to come third
                // while loading and second once resolved, so the lines
                // reshuffled under the user the moment results landed.
                notes.extend(read_note());
            }
            Some(Ok(plan)) => {
                let count = state.selection_count();
                notes.extend(read_note());
                for (icon, note) in plan_notes(&state.command, plan) {
                    notes.push(ui::magic_card::note(&theme, icon, note));
                }
                // A broad search reaches into caches and app data nobody
                // was thinking of. Those rows start unticked
                // (`install_plan`); say so, with the way to mean it.
                if !state.layout.hidden.is_empty() {
                    let hidden = &state.layout.hidden;
                    let included = hidden
                        .iter()
                        .filter(|&&ix| state.checked.get(ix).copied().unwrap_or(false))
                        .count();
                    let (text, action, include) = if included == 0 {
                        (
                            format!("{} in hidden or app folders left out", hidden.len()),
                            "Include",
                            true,
                        )
                    } else {
                        (
                            format!("{included} in hidden or app folders — usually app data"),
                            "Leave out",
                            false,
                        )
                    };
                    notes.push(
                        div()
                            .flex()
                            .items_center()
                            .gap_1p5()
                            .child(
                                // Information, not a warning: these were
                                // left out on purpose, and Include is here.
                                ui::icon::ui_icon("icons/info.svg", theme.text_dim)
                                    .size(px(12.))
                                    .flex_none(),
                            )
                            .child(ui::magic_card::subtitle(&theme, text))
                            .child(
                                ui::button::button(
                                    &theme,
                                    "magic-hidden",
                                    action,
                                    ui::button::Variant::Subtle,
                                    ui::button::Size::Small,
                                )
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        if let Some(state) = this.magic.as_mut() {
                                            state.set_hidden(include);
                                            cx.notify();
                                        }
                                    },
                                )),
                            ),
                    );
                }

                // Virtualized: a 342-op plan renders only the visible rows.
                // Building every row as a child (the old approach) is what
                // made the plan view sluggish to scroll on a large batch.
                let list = uniform_list(
                    "magic-plan",
                    state.layout.lines.len(),
                    cx.processor(|this, range: Range<usize>, _window, cx| {
                        let theme = *cx.theme();
                        // Pictures get their thumbnail (lazy, like the file
                        // list): approving the deletion of 80 screenshots by
                        // file name alone is guesswork. Drawn first, since
                        // drawing one needs the workspace mutably.
                        let pictures: Vec<(usize, String, PathBuf)> = match this.magic.as_ref() {
                            Some(state) => match &state.outcome {
                                Some(Ok(plan)) => range
                                    .clone()
                                    .filter_map(|line| match state.layout.lines.get(line)? {
                                        PlanLine::Op(ix) => {
                                            let source = plan.ops.get(*ix)?.source();
                                            let name = leaf(source);
                                            (FileKind::of(&name, false) == FileKind::Image).then(
                                                || (*ix, name.to_string(), source.to_path_buf()),
                                            )
                                        }
                                        PlanLine::Group(_) => None,
                                    })
                                    .collect(),
                                _ => Vec::new(),
                            },
                            None => Vec::new(),
                        };
                        let mut thumbs: std::collections::HashMap<usize, gpui::AnyElement> =
                            pictures
                                .into_iter()
                                .map(|(ix, name, source)| {
                                    let thumb =
                                        this.render_icon_cell(&name, &source, false, 24., 1., cx);
                                    // A framed square, so a wide screenshot
                                    // reads as a picture, not a stray sliver.
                                    let tile = div()
                                        .size(px(24.))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .rounded_sm()
                                        .overflow_hidden()
                                        .bg(theme.field)
                                        .border_1()
                                        .border_color(theme.border)
                                        .child(thumb);
                                    (ix, tile.into_any_element())
                                })
                                .collect();
                        let Some(state) = this.magic.as_ref() else {
                            return Vec::new();
                        };
                        let Some(Ok(plan)) = state.outcome.as_ref() else {
                            return Vec::new();
                        };
                        let grouped = state.layout.groups.len() > 1;
                        let one_dest = state.layout.dest.is_some();
                        let home = this.user_dirs.get("home");
                        range
                            .filter_map(|line| match *state.layout.lines.get(line)? {
                                PlanLine::Group(gix) => {
                                    let (folder, _) = state.layout.groups.get(gix)?;
                                    let (checked, total) = state.group_count(gix);
                                    // How many of this folder's files had
                                    // to take a new name — what the
                                    // conflict count in the title is
                                    // made of, findable while scrolling.
                                    let (_, rows) = state.layout.groups.get(gix)?;
                                    let renamed = rows
                                        .iter()
                                        .filter(|&&ix| {
                                            matches!(plan.ops.get(ix), Some(
                                                FileOp::Move { from, to }
                                                | FileOp::Copy { from, to },
                                            ) if from.file_name() != to.file_name())
                                        })
                                        .count();
                                    let parent = folder
                                        .parent()
                                        .map(|parent| {
                                            let mut shown = tilde(display_path(parent), home);
                                            if !shown.ends_with(std::path::MAIN_SEPARATOR) {
                                                shown.push(std::path::MAIN_SEPARATOR);
                                            }
                                            shown
                                        })
                                        .unwrap_or_default();
                                    let mixed = checked > 0 && checked < total;
                                    let check = ui::magic_card::group_check(
                                        &theme,
                                        gix,
                                        (!mixed).then_some(checked == total),
                                    )
                                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                        // The checkbox, not the fold.
                                        cx.stop_propagation();
                                        if let Some(state) = this.magic.as_mut() {
                                            state.toggle_group(gix);
                                            cx.notify();
                                        }
                                    }));
                                    Some(
                                        ui::magic_card::group_row(
                                            &theme,
                                            gix,
                                            state.layout.collapsed.contains(folder),
                                            parent,
                                            leaf(folder),
                                            {
                                                let mut count = if checked == total {
                                                    format!("{total} {}", plural_items(total))
                                                } else {
                                                    format!("{checked} of {total}")
                                                };
                                                if renamed > 0 {
                                                    count = format!("{renamed} renamed · {count}");
                                                }
                                                count
                                            },
                                            check,
                                        )
                                        .on_click(
                                            cx.listener(move |this, _: &ClickEvent, _, cx| {
                                                if let Some(state) = this.magic.as_mut() {
                                                    state.layout.toggle_collapsed(gix);
                                                    cx.notify();
                                                }
                                            }),
                                        ),
                                    )
                                }
                                PlanLine::Op(ix) => {
                                    let checked = state.checked.get(ix).copied().unwrap_or(false);
                                    let PlanRow {
                                        name,
                                        dest,
                                        tooltip,
                                    } = describe_row(plan, ix)?;
                                    // One destination for all: it is in
                                    // the title. A row then shows only a
                                    // name it had to change to fit there.
                                    let dest = if one_dest {
                                        match plan.ops.get(ix) {
                                            Some(
                                                FileOp::Move { from, to }
                                                | FileOp::Copy { from, to },
                                            ) if from.file_name() != to.file_name() => {
                                                Some(changed_tail(&leaf(from), &leaf(to)).into())
                                            }
                                            // No column for the other rows:
                                            // a rename is the exception, and
                                            // a mostly-empty column only
                                            // cut every name short.
                                            _ => None,
                                        }
                                    } else {
                                        dest.map(|dest| tilde(dest.into(), home).into())
                                    };
                                    let icon = thumbs.remove(&ix).unwrap_or_else(|| {
                                        ui::icon::file_icon(&theme, &name, 16.).into_any_element()
                                    });
                                    let facts = plan
                                        .ops
                                        .get(ix)
                                        .and_then(|op| state.meta.get(op.source()))
                                        .map(|&(size, modified)| {
                                            (
                                                SharedString::from(format_modified(
                                                    modified,
                                                    std::time::SystemTime::now(),
                                                )),
                                                SharedString::from(format_size(size)),
                                            )
                                        });
                                    Some(
                                        ui::magic_card::op_row(
                                            &theme,
                                            ix,
                                            checked,
                                            icon,
                                            name,
                                            // Secondary text: a new name or
                                            // folder is a preview, and accent
                                            // read as a link.
                                            dest.map(|dest: SharedString| (dest, theme.text_dim)),
                                            grouped,
                                            facts,
                                        )
                                        .tooltip(ui::tooltip::text_tooltip(tooltip, theme))
                                        .on_click(
                                            cx.listener(move |this, _: &ClickEvent, window, cx| {
                                                window.focus(&this.focus_handle, cx);
                                                this.toggle_magic_op(ix, cx);
                                            }),
                                        ),
                                    )
                                }
                            })
                            .collect()
                    }),
                )
                .track_scroll(&self.magic_scroll)
                .pr(px(ui::scrollbar::GUTTER))
                .pb(px(24.))
                .with_decoration(
                    ui::scrollbar::scrollbar(
                        self.magic_scroll.clone(),
                        self.magic_scrollbar.clone(),
                        cx.theme(),
                    )
                    .outset(ui::scrollbar::GUTTER),
                )
                .flex_1();
                // The destination column (and so its label) is absent for
                // verbs whose rows have no per-file target.
                let dest_label = match verb {
                    filex::magic::Verb::Rename => Some("New name"),
                    filex::magic::Verb::Move | filex::magic::Verb::Copy
                        if state.layout.dest.is_some() =>
                    {
                        None
                    }
                    filex::magic::Verb::Move
                    | filex::magic::Verb::Copy
                    | filex::magic::Verb::Organize => Some("Goes to"),
                    _ => None,
                };
                // Select all / none lives where a table keeps it: a
                // checkbox over the rows' own.
                let total = plan.len();
                // Counted over the rows the plan offers, not the app-data
                // rows it left out on purpose: those have their own line
                // and Include. Otherwise a plan with every offered row
                // checked showed "−".
                let left_out = |ix: &&usize| !state.checked.get(**ix).copied().unwrap_or(false);
                let shelved = state.layout.hidden.iter().filter(left_out).count();
                let all = count == total - shelved;
                let master = ui::magic_card::group_check(
                    &theme,
                    usize::MAX,
                    match count {
                        0 => Some(false),
                        _ if all => Some(true),
                        _ => None,
                    },
                )
                .cursor_pointer()
                .tooltip(ui::tooltip::text_tooltip(
                    if all { "Uncheck all" } else { "Check all" },
                    theme,
                ))
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    if all {
                        this.set_all_magic_ops(false, cx);
                    } else if let Some(state) = this.magic.as_mut() {
                        // Everything offered; the left-out rows stay out.
                        let shelved: std::collections::HashSet<usize> =
                            state.layout.hidden.iter().copied().collect();
                        for (ix, slot) in state.checked.iter_mut().enumerate() {
                            *slot = *slot || !shelved.contains(&ix);
                        }
                        cx.notify();
                    }
                    window.focus(&this.focus_handle, cx);
                }));
                body = Some(
                    ui::magic_card::pane_list()
                        .child(ui::magic_card::plan_header(
                            &theme,
                            master,
                            dest_label,
                            state.layout.groups.len() > 1,
                            !state.meta.is_empty(),
                        ))
                        .child(list)
                        .child(ui::pane::bottom_fade(&theme))
                        .into_any_element(),
                );

                let apply_key = shortcuts::hint(
                    "apply_plan",
                    &self.settings.read(cx).settings().keyboard_shortcuts,
                );
                action_bar = Some(
                    ui::magic_card::pane_actions(&theme)
                        .justify_between()
                        // The plan's count lives here, beside what applying
                        // it means, not again in the status bar below.
                        .child(ui::magic_card::subtitle(
                            &theme,
                            if count == 0 {
                                "Check files to continue".to_owned()
                            } else {
                                let what = if verb == filex::magic::Verb::Tag
                                    || verb == filex::magic::Verb::Untag
                                {
                                    "only checked files change"
                                } else if destructive {
                                    if cfg!(windows) {
                                        "checked files go to the Recycle Bin · undo puts them back"
                                    } else {
                                        "checked files go to the Trash · undo puts them back"
                                    }
                                } else {
                                    "only checked files change · undo puts them back"
                                };
                                format!("{} · {what}", state.status())
                            },
                        ))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(cancel("magic-cancel", cx))
                                .child(
                                    {
                                        // Says what the title says, destination
                                        // included, so the two never differ.
                                        let mut label = magic::plan_action(verb, count);
                                        if let Some(dest) = &state.layout.dest {
                                            label = format!("{label} to {}", self.dest_name(dest));
                                        }
                                        let size = ui::button::Size::Regular;
                                        if count > 0 && !state.loading {
                                            let variant = if destructive {
                                                ui::button::Variant::Danger
                                            } else {
                                                ui::button::Variant::Primary
                                            };
                                            let ink = if destructive {
                                                theme.on_danger
                                            } else {
                                                theme.on_accent
                                            };
                                            // The shortcut is part of the
                                            // button it presses.
                                            ui::button::button(
                                                &theme,
                                                "magic-confirm",
                                                label,
                                                variant,
                                                size,
                                            )
                                            .gap_2()
                                            .children(
                                                (!apply_key.is_empty())
                                                    .then(|| ui::kbd::keycap_on(ink, apply_key)),
                                            )
                                        } else {
                                            ui::button::disabled_button(
                                                &theme,
                                                "magic-confirm",
                                                label,
                                                size,
                                            )
                                        }
                                    }
                                    .on_click(cx.listener(
                                        |this, _: &ClickEvent, window, cx| {
                                            this.confirm_magic(cx);
                                            // Out of the text box, so undo
                                            // means the batch, not typing.
                                            window.focus(&this.focus_handle, cx);
                                        },
                                    )),
                                ),
                        ),
                );
            }
            Some(Err(err)) => {
                notes.extend(read_note());
                // The reason is the pane's one message, not a footnote
                // under a generic one.
                // The reason and the fix are the pane's title and
                // subtitle; under them, folders it may have meant.
                let mut near = ui::magic_card::pane_list();
                if let filex::magic::PlanError::UnknownDestination(name) = err {
                    // Near misses first; failing those, the folders right
                    // here, so the way forward is a click, not a retype.
                    let (close, heading) = self.refusal_folders(name);
                    if !close.is_empty() {
                        near = near.child(ui::magic_card::section_row(&theme, heading));
                    }
                    for (ix, folder) in close.into_iter().enumerate() {
                        let wrong = name.clone();
                        near = near.child(
                            ui::magic_card::suggestion_row(
                                &theme,
                                ix,
                                ix == 0,
                                "icons/folder.svg",
                                folder.clone(),
                                if ix == 0 { "Use this folder" } else { "" },
                                ix == 0,
                            )
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.use_folder_instead(&wrong, &folder, cx);
                                },
                            )),
                        );
                    }
                }
                body = Some(near.into_any_element());
                if matches!(err, filex::magic::PlanError::NoMatches)
                    && let Some(button) = self.did_you_mean_button(&theme, cx)
                {
                    action_bar = Some(
                        ui::magic_card::pane_actions(&theme)
                            .child(cancel("magic-stop", cx))
                            .child(button),
                    );
                }
                // A folder that isn't there is one click (or the apply
                // shortcut) from being made, and the plan picks up again.
                if let Some((name, _)) = self.missing_destination_fix() {
                    let apply_key = shortcuts::hint(
                        "apply_plan",
                        &self.settings.read(cx).settings().keyboard_shortcuts,
                    );
                    action_bar = Some(
                        ui::magic_card::pane_actions(&theme)
                            // Same footer as a plan: what the keys do,
                            // and that nothing has moved yet.
                            .child(
                                div()
                                    .flex_1()
                                    .text_xs()
                                    .text_color(theme.text_dim)
                                    .child("↩ uses the highlighted folder · you review the plan before anything moves"),
                            )
                            .child(cancel("magic-stop", cx))
                            .child(
                                ui::button::button(
                                    &theme,
                                    "magic-create-destination",
                                    format!("Create “{name}” and continue"),
                                    // Secondary: Return belongs to the
                                    // highlighted folder row, so only one
                                    // thing looks like the default.
                                    ui::button::Variant::Secondary,
                                    ui::button::Size::Regular,
                                )
                                .gap_2()
                                .children(
                                    (!apply_key.is_empty())
                                        .then(|| ui::kbd::keycap(&theme, apply_key)),
                                )
                                .on_click(cx.listener(
                                    |this, _, _, cx| {
                                        this.create_missing_destination(cx);
                                    },
                                )),
                            ),
                    );
                }
            }
        }

        if body.is_none() {
            body = Some(if state.loading {
                div()
                    .flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .gap_3()
                    .child(ui::icon::spinner(
                        "icons/loader-circle.svg",
                        theme.accent,
                        22.,
                        "magic-loading",
                    ))
                    .child(ui::magic_card::subtitle(&theme, state.status()))
                    .into_any_element()
            } else {
                // Not loading, no outcome: the search ended early (a
                // newer query took over). Nothing to say.
                div().flex_1().into_any_element()
            });
        }
        let action_bar = action_bar.unwrap_or_else(|| {
            ui::magic_card::pane_actions(&theme).child(cancel("magic-stop", cx))
        });
        ui::magic_card::pane()
            .children((!notes.is_empty()).then(|| ui::magic_card::notes(&theme).children(notes)))
            .children(body)
            .child(action_bar)
            .into_any_element()
    }

    /// The card grid: a `uniform_list` whose rows are strips of N cards,
    /// N derived from the pane width so it reflows on resize. Each row
    /// builds only its own cards, so the grid is as virtualized as the
    /// list.
    pub(super) fn render_grid(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let settings = self.settings.read(cx).settings();
        let size = ui::grid::card_size(settings.grid_zoom);
        let cell = ui::grid::cell_width(size);
        // The panel's *current* width, so columns reflow in step with its
        // slide rather than jumping at the start or end of it.
        let preview_w = ui::details::fitted_width(
            settings.preview_width,
            f32::from(window.viewport_size().width) - ui::sidebar::SIDEBAR_WIDTH,
        ) * self.preview_frac;
        // Approximate content width: the window minus the sidebar, the
        // open details panel, and the grid's own inset. Off-by-one on the
        // odd frame just reflows.
        let content_w = (f32::from(window.viewport_size().width)
            - ui::sidebar::SIDEBAR_WIDTH
            - preview_w
            - ui::grid::CARD_GAP * 2.
            - ui::scrollbar::GUTTER)
            .max(cell);
        let cols = ui::grid::columns_for(content_w, cell);
        self.browse_cols = cols;
        let column_width = ui::grid::column_width(content_w, cols);
        let rows = self.entries.len().div_ceil(cols);
        uniform_list(
            "grid",
            rows,
            cx.processor(move |this, range: Range<usize>, window, cx| {
                let theme = *cx.theme();
                // An open context menu is about the selection: keep it lit,
                // as Finder does, rather than greying it like lost focus.
                let focused = this.focus_handle.is_focused(window) || this.context_menu.is_some();
                range
                    .map(|row| {
                        let start = row * cols;
                        let end = (start + cols).min(this.entries.len());
                        let mut strip = ui::grid::grid_row(size);
                        for ix in start..end {
                            strip = strip.child(this.render_card(
                                ix,
                                size,
                                column_width,
                                focused,
                                &theme,
                                cx,
                            ));
                        }
                        strip
                    })
                    .collect()
            }),
        )
        .track_scroll(&self.browse_scroll)
        // Rows end before the scrollbar, so its thumb never sits on a
        // selected row's highlight; room at the foot so the last row is
        // never cut through its label.
        .pr(px(ui::scrollbar::GUTTER))
        .pb(px(24.))
        .with_decoration(
            ui::scrollbar::scrollbar(
                self.browse_scroll.clone(),
                self.browse_scrollbar.clone(),
                cx.theme(),
            )
            .outset(ui::scrollbar::GUTTER),
        )
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
        width: f32,
        // Whether the file list has focus: a selection elsewhere-focused
        // is drawn grey, as Finder does.
        focused: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(entry) = self.entries.get(ix) else {
            return div().into_any_element();
        };
        let is_dir = entry.is_dir;
        let is_selected = self.active_selection().contains(ix);
        let (name, path) = (entry.name.clone(), entry.path.clone());
        // No date or size under a card: a grey line under every tile was
        // noise at a glance, as Finder's default grid has it. The list
        // view, details pane and status bar carry them.
        let name_tip: SharedString = name.clone().into();
        // Previews take the whole card; icons sit at 80% so they stay
        // balanced beside them.
        // The area is shorter than the icon edge; the art stays 80% of
        // the edge, and a preview fills the (shorter) area.
        let area = ui::grid::icon_area_height(size);
        let icon = self.render_icon_cell(&name, &path, is_dir, area, 0.8 * size / area, cx);
        let (label, truncated) =
            ui::grid::card_name(theme, width, &name, is_selected.then_some(focused), cx);
        let card = ui::grid::card(("card", ix), size, width)
            // The full name only when the label had to cut it.
            .when(truncated && self.context_menu.is_none(), |card| {
                card.tooltip(ui::tooltip::text_tooltip(name_tip, *theme))
            })
            .child(
                ui::grid::card_icon_area(theme, size, is_selected)
                    .when(is_selected && !focused, |area| area.bg(theme.control))
                    .child(icon),
            )
            .child(label)
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
        let card = self.drag_source(card, ix, *theme);
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
            cx.processor(|this, range: Range<usize>, window, cx| {
                let theme = *cx.theme();
                // An open context menu is about the selection: keep it lit,
                // as Finder does, rather than greying it like lost focus.
                let focused = this.focus_handle.is_focused(window) || this.context_menu.is_some();
                range
                    .filter_map(|ix| {
                        // Copy row data out first: the icon cell needs &mut self (it may
                        // schedule a thumbnail decode) while `entry` borrows self.
                        let entry = this.entries.get(ix)?;
                        let is_dir = entry.is_dir;
                        let size = entry.size;
                        let modified = entry.modified;
                        let pending = this.meta_pending;
                        let is_selected = this.active_selection().contains(ix);
                        let (name, path) = (entry.name.clone(), entry.path.clone());
                        let kind = FileKind::describe(&name, is_dir);
                        let name_tip: SharedString = name.clone().into();
                        let icon =
                            this.render_icon_cell(&name, &path, is_dir, theme.icon_size, 1., cx);
                        let rename_input = this
                            .renaming
                            .as_ref()
                            .filter(|rename| rename.ix == ix)
                            .map(|rename| rename.input.clone());
                        let is_renaming = rename_input.is_some();
                        let menu_closed = this.context_menu.is_none();
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
                        let row = ui::list_row::list_row(&theme, ix, is_selected)
                            // Grey while focus is elsewhere (the path bar,
                            // a menu), as Finder does.
                            .when(is_selected && !focused, |row| row.bg(theme.control))
                            // Full name on hover (helps when it's truncated);
                            // suppressed mid-rename so it doesn't cover the
                            // field, and while a menu is up so it doesn't
                            // draw over the menu.
                            .when(!is_renaming && menu_closed, |row| {
                                row.tooltip(ui::tooltip::text_tooltip(name_tip, theme))
                            })
                            .child(icon)
                            .child(name_cell)
                            .child(ui::list_row::kind_cell(&theme, kind))
                            .child(ui::list_row::detail_cell(
                                &theme,
                                ui::list_row::MODIFIED_COL_WIDTH,
                                // Blank until backfilled (FIL-25).
                                if pending {
                                    String::new()
                                } else {
                                    format_modified(modified, std::time::SystemTime::now())
                                },
                            ))
                            .child(ui::list_row::detail_cell(
                                &theme,
                                ui::list_row::SIZE_COL_WIDTH,
                                // Folders have no size of their own: "—",
                                // as in the details pane.
                                if pending {
                                    String::new()
                                } else if is_dir {
                                    "—".into()
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
                        // Any row can be a drag source (not mid-rename); only
                        // folders accept a drop.
                        let row = if is_renaming {
                            row
                        } else {
                            this.drag_source(row, ix, theme)
                        };
                        let row = row.when(is_dir, |row| {
                            this.folder_drop_target(row, path.clone(), theme, cx)
                        });
                        Some(row)
                    })
                    .collect()
            }),
        )
        .track_scroll(&self.browse_scroll)
        // Rows end before the scrollbar, so its thumb never sits on a
        // selected row's highlight; room at the foot so the last row is
        // never cut through its label.
        .pr(px(ui::scrollbar::GUTTER))
        .pb(px(24.))
        .with_decoration(
            ui::scrollbar::scrollbar(
                self.browse_scroll.clone(),
                self.browse_scrollbar.clone(),
                cx.theme(),
            )
            .outset(ui::scrollbar::GUTTER),
        )
        .flex_1()
    }

    pub(super) fn render_search_results(&self, cx: &mut Context<Self>) -> impl IntoElement {
        uniform_list(
            "results",
            self.results.len(),
            cx.processor(|this, range: Range<usize>, window, cx| {
                let theme = *cx.theme();
                // The plain words of the query, to light up in each name.
                let words: Vec<String> = this
                    .query
                    .split_whitespace()
                    .filter(|word| !word.contains(':'))
                    .map(|word| word.trim_matches('"').to_ascii_lowercase())
                    .collect();
                // Fetch the next page while the user is still a screen away
                // from the end, so scrolling never hits a wall. Deferred: the
                // list is mid-render here.
                if this.search_more
                    && !this.search_paging
                    && range.end + RESULTS_PREFETCH_ROWS >= this.results.len()
                {
                    cx.defer_in(window, |this, _window, cx| this.load_more_results(cx));
                }
                range
                    .filter_map(|ix| {
                        let row = this.results.get(ix)?;
                        let is_dir = row.is_dir;
                        let is_selected = this.active_selection().contains(ix);
                        let (name, path) = (row.name.clone(), row.target.clone());
                        let path_label = row.path_label.clone();
                        let full_path: SharedString = display_path(&path).into();
                        let facts = this.search_meta.get(&path).copied();
                        let matches = ui::list_row::match_ranges(&name, &words);
                        let icon =
                            this.render_icon_cell(&name, &path, is_dir, theme.icon_size, 1., cx);
                        let menu_closed = this.context_menu.is_none();
                        let row = ui::list_row::list_row(&theme, ix, is_selected)
                            .when(menu_closed, |row| {
                                row.tooltip(ui::tooltip::text_tooltip(full_path, theme))
                            })
                            .child(icon)
                            .child(ui::list_row::search_label(
                                &theme,
                                name,
                                path_label,
                                matches,
                                is_selected,
                            ))
                            .child(ui::list_row::detail_cell(
                                &theme,
                                ui::list_row::MODIFIED_COL_WIDTH,
                                facts
                                    .map(|(_, modified)| {
                                        format_modified(modified, std::time::SystemTime::now())
                                    })
                                    .unwrap_or_default(),
                            ))
                            .child(ui::list_row::detail_cell(
                                &theme,
                                ui::list_row::SIZE_COL_WIDTH,
                                facts
                                    .filter(|_| !is_dir)
                                    .map(|(size, _)| format_size(size))
                                    .unwrap_or_default(),
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
                                    this.open_entry_menu(ix, true, event.position, window, cx);
                                }),
                            );
                        // Results drag like browse rows (into folders, other
                        // apps); folder results accept drops.
                        let row = this.drag_source(row, ix, theme);
                        Some(if is_dir {
                            this.folder_drop_target(row, path, theme, cx)
                        } else {
                            row
                        })
                    })
                    .collect()
            }),
        )
        .track_scroll(&self.results_scroll)
        // Rows end before the scrollbar, so its thumb never sits on a
        // selected row's highlight; the foot clears the fade.
        .pr(px(ui::scrollbar::GUTTER))
        .pb(px(24.))
        .with_decoration(
            ui::scrollbar::scrollbar(
                self.results_scroll.clone(),
                self.results_scrollbar.clone(),
                cx.theme(),
            )
            .outset(ui::scrollbar::GUTTER),
        )
        .flex_1()
    }
}

/// The edge icon artwork is drawn at in a cell of `edge` px. Only previews
/// fill the whole cell; the file's kind never changes the artwork's size,
/// so an image whose preview is missing matches its neighbours.
fn art_edge(edge: f32, art_scale: f32) -> f32 {
    edge * art_scale
}

#[cfg(test)]
mod icon_cell_tests {
    use super::art_edge;

    /// Regression: grid cards gave every image a full-size cell for its
    /// preview, so an image without one drew an oversized icon.
    #[test]
    fn artwork_size_is_independent_of_kind() {
        assert_eq!(art_edge(104., 0.8), 104. * 0.8);
        assert_eq!(art_edge(20., 1.), 20.);
    }
}

/// A new name as only the part that changed, after the part it shares
/// with the old one: "Report_main.pdf" → "Report_main 2.pdf" shows
/// "… 2.pdf". The whole name when they share too little to be worth it.
fn changed_tail(old: &str, new: &str) -> String {
    let shared = old
        .char_indices()
        .zip(new.chars())
        .take_while(|((_, a), b)| a == b)
        .last()
        .map_or(0, |((at, a), _)| at + a.len_utf8());
    // Back to a word boundary, so the tail starts on a whole word.
    let cut = new[..shared]
        .rfind([' ', '_', '-', '.'])
        .filter(|at| *at >= 4)
        .unwrap_or(shared);
    if cut < 6 {
        return new.to_owned();
    }
    format!("…{}", &new[cut..])
}

#[cfg(test)]
mod tail_tests {
    #[test]
    fn only_the_changed_part_of_a_rename_is_shown() {
        assert_eq!(
            super::changed_tail("A_Smart_Meeting_main.pdf", "A_Smart_Meeting_main 2.pdf"),
            "…_main 2.pdf"
        );
        assert_eq!(super::changed_tail("a.pdf", "a 2.pdf"), "a 2.pdf");
        assert_eq!(super::changed_tail("photo.png", "trip-1.png"), "trip-1.png");
    }
}

//! Categorized preferences, their setting widgets, and an in-place shortcut recorder.
use super::*;
use filex::settings::Settings;

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum SettingsSection {
    #[default]
    Appearance,
    Browsing,
    Search,
    Keyboard,
    Privacy,
}
impl SettingsSection {
    const ALL: [Self; 5] = [
        Self::Appearance,
        Self::Browsing,
        Self::Search,
        Self::Keyboard,
        Self::Privacy,
    ];
    fn label(self) -> &'static str {
        match self {
            Self::Appearance => "Appearance",
            Self::Browsing => "Files & folders",
            Self::Search => "Search",
            Self::Keyboard => "Keyboard",
            Self::Privacy => "Privacy",
        }
    }
    fn icon(self) -> &'static str {
        match self {
            Self::Appearance => "icons/layout-grid.svg",
            Self::Browsing => "icons/folder.svg",
            Self::Search => "icons/search.svg",
            Self::Keyboard => "icons/keyboard.svg",
            Self::Privacy => "icons/shield.svg",
        }
    }
    fn description(self) -> &'static str {
        match self {
            Self::Appearance => "Make filex feel at home on your desktop.",
            Self::Browsing => "Choose how your files are displayed and handled.",
            Self::Search => "Control which locations appear in search.",
            Self::Keyboard => "Click a shortcut, then press your preferred combination.",
            Self::Privacy => "Choose what you share with filex.",
        }
    }
    fn contains(self, id: &str) -> bool {
        match self {
            Self::Browsing => matches!(
                id,
                "show-hidden" | "confirm-delete" | "dirs-first" | "thumbnails"
            ),
            Self::Search => id == "index-system-files",
            Self::Privacy => id == "crash-reports",
            _ => false,
        }
    }
}
/// One plain on/off row of the settings modal: element id, label,
/// explanation, how to read the value, how to flip it, and anything to do
/// afterwards. A table rather than six near-identical `.child(...)` blocks
/// — adding a boolean setting is now one entry.
type SettingToggle = (
    &'static str,
    &'static str,
    &'static str,
    fn(&Settings) -> bool,
    fn(&mut Settings),
    fn(&Workspace, &mut Context<Workspace>),
);

/// Nothing to do after flipping — the case for every toggle but one.
fn no_follow_up(_: &Workspace, _: &mut Context<Workspace>) {}

const SETTING_TOGGLES: &[SettingToggle] = &[
    (
        "show-hidden",
        "Show hidden files",
        "Dotfiles and OS-hidden entries in the browse list",
        |s| s.show_hidden_files,
        |s| s.show_hidden_files = !s.show_hidden_files,
        no_follow_up,
    ),
    (
        "index-system-files",
        "Index system folders",
        "Include operating system and application folders in search. Uses more memory and applies after rebuilding the index.",
        |s| s.index_system_files,
        |s| s.index_system_files = !s.index_system_files,
        no_follow_up,
    ),
    (
        "confirm-delete",
        "Confirm before deleting",
        "First press arms; a second press moves the file to the trash",
        |s| s.confirm_delete,
        |s| s.confirm_delete = !s.confirm_delete,
        no_follow_up,
    ),
    (
        "dirs-first",
        "Folders first",
        "Group folders above files whatever the sort order",
        |s| s.sort.directories_first,
        |s| s.sort.directories_first = !s.sort.directories_first,
        no_follow_up,
    ),
    (
        "thumbnails",
        "Image thumbnails",
        "Show image previews in list and grid views",
        |s| s.thumbnails_enabled,
        |s| s.thumbnails_enabled = !s.thumbnails_enabled,
        no_follow_up,
    ),
    (
        "crash-reports",
        "Share anonymous diagnostics",
        "Scrubbed crashes + performance only — never file names, paths, or queries",
        |s| s.crash_reports,
        |s| s.crash_reports = !s.crash_reports,
        // Turning it on drains anything already queued.
        Workspace::spawn_crash_upload,
    ),
];

impl Workspace {
    pub(super) fn toggle_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings_open = !self.settings_open;
        self.recording_shortcut = None;
        self.shortcut_error = None;
        let handle = if self.settings_open {
            &self.settings_focus
        } else {
            &self.focus_handle
        };
        window.focus(handle, cx);
        cx.notify();
    }

    pub(super) fn open_keyboard_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings_open = true;
        self.settings_section = SettingsSection::Keyboard;
        self.recording_shortcut = None;
        self.shortcut_error = None;
        window.focus(&self.settings_focus, cx);
        cx.notify();
    }

    fn handle_settings_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let stroke = &event.keystroke;
        if let Some(id) = self.recording_shortcut {
            cx.stop_propagation();
            if stroke.key == "escape" {
                self.recording_shortcut = None;
                self.shortcut_error = None;
            } else if !stroke.modifiers.modified()
                && matches!(stroke.key.as_str(), "backspace" | "delete")
            {
                self.settings.update(cx, |store, cx| {
                    store.update(cx, |s| {
                        s.keyboard_shortcuts.insert(id.into(), String::new());
                    })
                });
                self.recording_shortcut = None;
                self.shortcut_error = None;
            } else {
                match shortcuts::validate(
                    id,
                    &stroke.unparse(),
                    &self.settings.read(cx).settings().keyboard_shortcuts,
                ) {
                    Ok(chord) => {
                        self.settings.update(cx, |store, cx| {
                            store.update(cx, |s| {
                                s.keyboard_shortcuts.insert(id.into(), chord);
                            })
                        });
                        self.recording_shortcut = None;
                        self.shortcut_error = None;
                    }
                    Err(error) => self.shortcut_error = Some(error.into()),
                }
            }
            cx.notify();
            return;
        }
        if stroke.key == "escape" {
            cx.stop_propagation();
            self.toggle_settings(window, cx);
        }
    }

    fn reset_shortcut(&mut self, id: &'static str, cx: &mut Context<Self>) {
        let overrides = &self.settings.read(cx).settings().keyboard_shortcuts;
        let Some(spec) = shortcuts::catalog().into_iter().find(|s| s.id == id) else {
            return;
        };
        for key in std::iter::once(spec.default).chain(spec.aliases.iter().copied()) {
            if let Err(error) = shortcuts::validate(id, key, overrides) {
                self.shortcut_error = Some(error.into());
                cx.notify();
                return;
            }
        }
        self.recording_shortcut = None;
        self.shortcut_error = None;
        self.settings.update(cx, |store, cx| {
            store.update(cx, |s| {
                s.keyboard_shortcuts.remove(id);
            })
        });
    }

    pub(super) fn render_settings_modal(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        if !self.settings_open {
            return None;
        }
        let theme = *cx.theme();
        let section = self.settings_section;
        let settings = self.settings.read(cx).settings().clone();
        let close =
            ui::top_bar::toolbar_button(&theme, "settings-close", "icons/x.svg", theme.text_dim)
                .tooltip(ui::tooltip::text_tooltip("Close settings · Esc", theme))
                .on_click(cx.listener(|this, _, window, cx| this.toggle_settings(window, cx)));
        let nav = ui::settings_pane::navigation(&theme).children(
            SettingsSection::ALL.into_iter().map(|item| {
                ui::settings_pane::navigation_item(
                    &theme,
                    item.label(),
                    item.icon(),
                    item == section,
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.settings_section = item;
                    this.recording_shortcut = None;
                    this.shortcut_error = None;
                    window.focus(&this.settings_focus, cx);
                    cx.notify();
                }))
            }),
        );
        let mut page = ui::settings_pane::page(section.label()).child(
            ui::settings_pane::page_heading(&theme, section.label(), section.description()),
        );
        if section == SettingsSection::Appearance {
            page = page.child(
                ui::settings_pane::group(&theme)
                    .child(ui::settings_pane::choice_row(
                        &theme,
                        "Theme",
                        "Use your device’s appearance or choose a theme",
                        self.render_theme_selector(&theme, settings.theme, cx),
                    ))
                    .child(ui::settings_pane::choice_row(
                        &theme,
                        "Accent color",
                        "Color for folders, selections, and controls",
                        self.render_accent_picker(&theme, settings.accent, cx),
                    ))
                    .child(ui::settings_pane::choice_row(
                        &theme,
                        "Density",
                        "Give files more room or fit more on screen",
                        self.render_density_selector(&theme, settings.density, cx),
                    )),
            );
        } else if section == SettingsSection::Keyboard {
            page = page.child(self.render_keyboard_preferences(&theme, cx));
        } else {
            let mut group = ui::settings_pane::group(&theme);
            for &(id, label, description, read, toggle, follow_up) in
                SETTING_TOGGLES.iter().filter(|s| section.contains(s.0))
            {
                group = group.child(
                    ui::settings_pane::toggle_row(&theme, id, label, description, read(&settings))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.settings
                                .update(cx, |store, cx| store.update(cx, toggle));
                            follow_up(this, cx);
                        })),
                );
            }
            page = page.child(group);
            if section == SettingsSection::Search {
                page = page.child(ui::settings_pane::note(
                    &theme,
                    "Add search locations from the sidebar using “Index this folder”.",
                ));
            }
        }
        let card = ui::settings_pane::settings_card(&theme, "settings-panel")
            .h((window.viewport_size().height - px(64.)).min(px(650.)))
            .track_focus(&self.settings_focus)
            .key_context("Settings")
            .capture_key_down(cx.listener(Self::handle_settings_key))
            .on_key_down(|_, _, cx| cx.stop_propagation())
            .on_click(|_, _, cx| cx.stop_propagation())
            .child(ui::settings_pane::shell_header(&theme, "Settings", close))
            .child(div().flex().flex_1().min_h_0().child(nav).child(page));
        Some(
            ui::modal::backdrop("settings-backdrop")
                .on_click(cx.listener(|this, _, window, cx| this.toggle_settings(window, cx)))
                .child(card)
                .into_any_element(),
        )
    }

    fn render_keyboard_preferences(&self, theme: &Theme, cx: &mut Context<Self>) -> gpui::Div {
        let overrides = &self.settings.read(cx).settings().keyboard_shortcuts;
        let mut content = div().flex().flex_col().gap_4();
        content = content.child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap_2()
                .child(ui::settings_pane::note(
                    theme,
                    "Changes save automatically. Text editing keeps standard system shortcuts.",
                ))
                .child(
                    ui::modal::button(theme, "reset-shortcuts", "Reset all", false)
                        .flex_none()
                        .text_xs()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.recording_shortcut = None;
                            this.shortcut_error = None;
                            this.settings.update(cx, |store, cx| {
                                store.update(cx, |s| s.keyboard_shortcuts.clear())
                            });
                        })),
                ),
        );
        if let Some(error) = &self.shortcut_error {
            content = content.child(
                div()
                    .p_3()
                    .rounded_lg()
                    .bg(theme.panel)
                    .text_sm()
                    .text_color(theme.warn)
                    .child(error.clone()),
            );
        }
        if self.recording_shortcut.is_some() {
            content = content.child(ui::settings_pane::note(
                theme,
                "Press a combination. Backspace clears it; Escape cancels.",
            ));
        }
        for group_name in ["Navigation", "Selection & files", "View", "Tabs & window"] {
            let mut group = ui::settings_pane::group(theme);
            for spec in shortcuts::catalog()
                .into_iter()
                .filter(|s| s.group == group_name)
            {
                let id = spec.id;
                let recording = self.recording_shortcut == Some(id);
                let keys = shortcuts::active_keys(&spec, overrides);
                let key_label = if recording {
                    "Press keys…".to_owned()
                } else {
                    keys.first()
                        .map(|k| shortcuts::label(k))
                        .unwrap_or_else(|| "Unassigned".into())
                };
                let alternatives = keys
                    .iter()
                    .skip(1)
                    .map(|k| shortcuts::label(k))
                    .collect::<Vec<_>>()
                    .join(" · ");
                let row = ui::settings_pane::shortcut_row(theme, spec.label, alternatives)
                    .child(
                        ui::settings_pane::shortcut_button(theme, id, key_label, recording)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.recording_shortcut = Some(id);
                                this.shortcut_error = None;
                                window.focus(&this.settings_focus, cx);
                                cx.notify();
                            })),
                    )
                    .child(
                        ui::top_bar::toolbar_button(
                            theme,
                            SharedString::from(format!("reset-{id}")),
                            "icons/refresh-cw.svg",
                            theme.text_dim,
                        )
                        .when(!overrides.contains_key(id), |s| s.opacity(0.25))
                        .tooltip(ui::tooltip::text_tooltip(
                            "Restore default shortcut",
                            *theme,
                        ))
                        .on_click(cx.listener(move |this, _, _, cx| this.reset_shortcut(id, cx))),
                    );
                group = group.child(row);
            }
            content = content.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text_dim)
                            .child(group_name),
                    )
                    .child(group),
            );
        }
        content
    }
}

impl Workspace {
    /// Settings live in a centred modal over the browse view (rather
    /// than replacing it): a dimmed backdrop closes on an outside click,
    /// Escape closes it too (see the ClearInput handler). `None` while
    /// closed.
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
        self.segmented_setting(
            theme,
            current,
            &[
                ("theme-system", "System", ThemeMode::System),
                ("theme-light", "Light", ThemeMode::Light),
                ("theme-dark", "Dark", ThemeMode::Dark),
                ("theme-oled", "OLED", ThemeMode::Oled),
            ],
            |s, mode| s.theme = mode,
            cx,
        )
    }

    /// A segmented control bound to an enum setting: one segment per
    /// option, the current one filled, each writing straight to the store.
    /// Shared by the Appearance and Density rows.
    fn segmented_setting<T: PartialEq + Copy + 'static>(
        &self,
        theme: &Theme,
        current: T,
        options: &[(&'static str, &'static str, T)],
        set: fn(&mut Settings, T),
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let mut row = ui::settings_pane::segmented(theme);
        for &(id, label, value) in options {
            row = row.child(
                ui::settings_pane::segment(theme, id, label, current == value).on_click(
                    cx.listener(move |this, _: &ClickEvent, _window, cx| {
                        this.settings
                            .update(cx, |store, cx| store.update(cx, |s| set(s, value)));
                    }),
                ),
            );
        }
        row.into_any_element()
    }

    /// The Comfortable/Compact list-density control.
    pub(super) fn render_density_selector(
        &self,
        theme: &Theme,
        current: Density,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        self.segmented_setting(
            theme,
            current,
            &[
                ("density-comfortable", "Comfortable", Density::Comfortable),
                ("density-compact", "Compact", Density::Compact),
            ],
            |s, density| s.density = density,
            cx,
        )
    }

    /// The accent-color swatch row: `Default` (the palette's own accent)
    /// followed by the presets. Clicking one writes it to the store; the
    /// Changed event re-resolves the theme and recolors the app live.
    pub(super) fn render_accent_picker(
        &self,
        theme: &Theme,
        current: AccentColor,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let swatch = |ix: usize, accent: AccentColor, color: gpui::Rgba| {
            ui::settings_pane::swatch(theme, ("accent", ix), color, current == accent).on_click(
                cx.listener(move |this, _: &ClickEvent, _window, cx| {
                    this.settings.update(cx, |store, cx| {
                        store.update(cx, |s| s.accent = accent);
                    });
                }),
            )
        };
        // The Default swatch shows the palette's *own* accent (resolved
        // without any override), so it stays a stable target even while a
        // custom color is active.
        let base = self.settings.read(cx).settings().theme;
        let base_accent = Theme::resolve(base, self.appearance, AccentColor::Default).accent;
        let mut row =
            ui::settings_pane::swatch_row().child(swatch(0, AccentColor::Default, base_accent));
        for (ix, accent) in ui::theme::ACCENT_PRESETS.into_iter().enumerate() {
            let color = ui::theme::accent_rgb(accent).expect("presets are not Default");
            row = row.child(swatch(ix + 1, accent, color));
        }
        // The free hex field: typing a valid `#RRGGBB` sets a Custom
        // accent (see the accent_hex subscription). Its border lights when
        // a custom color is the active one.
        let custom_active = matches!(current, AccentColor::Custom(_));
        let hex_box = div()
            .flex()
            .items_center()
            .w(px(84.))
            .px_1p5()
            .py(px(2.))
            .rounded_md()
            .border_1()
            .border_color(if custom_active {
                theme.accent
            } else {
                theme.border
            })
            .bg(theme.bg)
            .text_xs()
            .text_color(theme.text)
            .child(div().flex_1().min_w_0().child(self.accent_hex.clone()));
        row.child(hex_box).into_any_element()
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
            IndexState::Failed(err) => Some(err.clone()),
            _ => None,
        };
        // Building spins (indexing is live); ready/failed are static.
        // Sidebar rows aren't virtualized, so animating here is fine.
        let marker = match &slot.state {
            IndexState::Building => ui::icon::spinner(
                "icons/loader-circle.svg",
                theme.text_dim,
                14.,
                ("root-spin", ix),
            ),
            IndexState::Ready => ui::icon::ui_icon("icons/dot.svg", theme.accent)
                .size(px(14.))
                .into_any_element(),
            IndexState::Failed(_) => ui::icon::ui_icon("icons/triangle-alert.svg", theme.warn)
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
}

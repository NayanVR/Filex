//! First-run onboarding: a full-window welcome flow rendered *instead of*
//! the app until finished or skipped, so no file shortcut can act on a
//! list the user can't see. Design record: `docs/design-onboarding.md`.

use std::time::{Duration, Instant};

use super::integration::Integration;
use super::*;

/// One screen of the flow, in order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Step {
    /// Before the welcome while the first index builds; not one of the
    /// counted steps (`ALL`).
    Loading,
    Welcome,
    Theme,
    Favorites,
    Search,
    Magic,
    /// "Open your folders here?": offered where the OS lets Filex be the
    /// default file manager and it isn't already.
    DefaultApp,
    Ready,
}

/// Which of the conditional steps are shown.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(super) struct Optional {
    /// Favorites: there is something to import.
    favorites: bool,
    /// DefaultApp: the platform supports it and Filex isn't the default.
    default_app: bool,
}

impl Step {
    const ALL: [Step; 7] = [
        Step::Welcome,
        Step::Theme,
        Step::Favorites,
        Step::Search,
        Step::Magic,
        Step::DefaultApp,
        Step::Ready,
    ];

    /// The steps actually shown: the conditional ones only when `optional`
    /// says so.
    fn visible(optional: Optional) -> impl Iterator<Item = Step> {
        Self::ALL.into_iter().filter(move |step| match step {
            Step::Favorites => optional.favorites,
            Step::DefaultApp => optional.default_app,
            _ => true,
        })
    }

    /// The step `offset` places away among the visible ones, or `None`
    /// past either end.
    fn shifted(self, offset: isize, optional: Optional) -> Option<Step> {
        let steps: Vec<Step> = Self::visible(optional).collect();
        // A step hidden after the fact (imports emptied) counts from the
        // step before it.
        let at = steps.iter().position(|s| *s == self).unwrap_or(1) as isize;
        steps.get(usize::try_from(at + offset).ok()?).copied()
    }
}

pub(super) struct Onboarding {
    step: Step,
    /// Folders found in the OS file manager, each with its checkbox.
    /// Empty until background discovery lands (usually within the
    /// Welcome step), and empty for good if there is nothing new.
    imports: Vec<(PathBuf, bool)>,
    opened: Instant,
    /// When the current step appeared: the clock its motion runs on.
    step_started: Instant,
    /// The Magic step has moved from its scripted demo to the user's own
    /// commands.
    magic_live: bool,
    /// Whether the index was seen building while onboarding was up. Only
    /// then is "indexed in N s" a true statement.
    saw_building: bool,
    indexed_in: Option<Duration>,
    loading: Loading,
    /// Offer the DefaultApp step. False until a background check finds the
    /// platform supports it and Filex isn't the default already.
    offer_default: bool,
    /// The DefaultApp step's tick. Starts on (decided 2026-10-28, like the
    /// favorites' ticks); unticking it or skipping the tour keeps File
    /// Explorer.
    make_default: bool,
}

/// The first index as the loading step follows it.
struct Loading {
    /// Names the walk passed, kept in arrival order so a lane's name stays
    /// put while it flies. Capped; the first ones are plenty to cycle.
    names: Vec<SharedString>,
    /// Entries found so far (or the finished index's size).
    scanned: u64,
    /// When the walk finished and the index started being written.
    indexing_since: Option<Instant>,
    /// When the index was first seen ready.
    ready_at: Option<Instant>,
    /// The count as drawn, easing toward `scanned`.
    shown: f64,
    shown_at: Instant,
}

/// Real names kept for the loading scene.
const LOADING_NAMES: usize = 96;
/// After this long the loading step offers to carry on without waiting.
const OFFER_CONTINUE_AFTER: Duration = Duration::from_secs(20);

/// The hand-off from onboarding into the app, while it plays.
pub(super) struct Reveal {
    started: Instant,
    /// Leaving from the last step: its screen lifts away over the app.
    from_ready: bool,
}

impl Onboarding {
    /// Opens on the loading step unless the index is already `ready`.
    fn new(now: Instant, ready: bool) -> Self {
        Self {
            step: if ready { Step::Welcome } else { Step::Loading },
            imports: Vec::new(),
            opened: now,
            step_started: now,
            magic_live: false,
            saw_building: false,
            indexed_in: None,
            loading: Loading {
                names: Vec::new(),
                scanned: 0,
                indexing_since: None,
                ready_at: None,
                shown: 0.,
                shown_at: now,
            },
            offer_default: false,
            make_default: true,
        }
    }

    fn optional(&self) -> Optional {
        Optional {
            favorites: !self.imports.is_empty(),
            default_app: self.offer_default,
        }
    }

    /// Whether leaving the flow should make Filex the default file
    /// manager: only when the user finished (not skipped) with the
    /// DefaultApp step offered and ticked.
    fn applies_default(&self, finished: bool) -> bool {
        finished && self.offer_default && self.make_default
    }

    /// Fold one daemon status in for the loading step: the walk's count,
    /// names and phase, and when the index came ready.
    pub(super) fn observe_loading(&mut self, status: &filex::daemon::ipc::Status, now: Instant) {
        let loading = &mut self.loading;
        if let Some(progress) = &status.progress {
            loading.scanned = loading.scanned.max(progress.scanned);
            if progress.indexing && loading.indexing_since.is_none() {
                loading.indexing_since = Some(now);
            }
            for name in &progress.recent {
                if loading.names.len() < LOADING_NAMES
                    && !loading.names.iter().any(|known| known == name)
                {
                    loading.names.push(name.clone().into());
                }
            }
        }
        if !status.building {
            let files: u64 = status.roots.iter().map(|r| r.files).sum();
            loading.scanned = loading.scanned.max(files);
            loading.ready_at.get_or_insert(now);
        }
    }

    /// When the loading step's completion beat starts: once the index is
    /// ready, but not before the scene has had its minimum time.
    fn outro_start(&self) -> Option<Instant> {
        let ready = self.loading.ready_at?;
        let earliest = self.step_started + Duration::from_secs_f32(ui::onboarding::LOADING_MIN);
        Some(ready.max(earliest))
    }

    /// Whether the loading step is over at `now`. An index that was ready
    /// before onboarding saw it build skips the scene outright.
    fn loading_done(&self, now: Instant) -> bool {
        if self.loading.ready_at.is_some() && !self.saw_building {
            return true;
        }
        self.outro_start().is_some_and(|start| {
            now >= start + Duration::from_secs_f32(ui::onboarding::LOADING_OUTRO)
        })
    }

    /// Move from loading to the welcome once loading is over.
    fn advance_loading(&mut self, now: Instant) {
        if self.step == Step::Loading && self.loading_done(now) {
            self.step = Step::Welcome;
            self.step_started = now;
        }
    }

    /// The count to draw at `now`, eased toward the latest one so it rolls
    /// rather than jumps between status polls.
    fn eased_count(&mut self, now: Instant) -> u64 {
        let loading = &mut self.loading;
        let dt = now
            .saturating_duration_since(loading.shown_at)
            .as_secs_f64();
        loading.shown_at = now;
        let target = loading.scanned as f64;
        loading.shown += (target - loading.shown) * (1. - (-dt * 6.).exp());
        if (target - loading.shown).abs() < 1. {
            loading.shown = target;
        }
        loading.shown as u64
    }

    /// Fold one daemon status poll in: time the first build that
    /// finishes while onboarding is open.
    pub(super) fn observe_index(&mut self, building: bool, now: Instant) {
        if building {
            self.saw_building = true;
        } else if self.saw_building && self.indexed_in.is_none() {
            self.indexed_in = Some(now - self.opened);
        }
    }

    /// Seconds into the current step's motion. Reduced motion pins every
    /// scene to its settled frame (`settled`).
    fn scene_time(&self, reduce_motion: bool, settled: f32) -> f32 {
        if reduce_motion {
            settled
        } else {
            self.step_started.elapsed().as_secs_f32()
        }
    }

    /// Whether the step still moves at `t` and needs another frame. The
    /// search demo holds still once settled so the backdrop never redraws
    /// under a live search.
    fn animating(&self, t: f32) -> bool {
        let live_input = self.step == Step::Search || (self.step == Step::Magic && self.magic_live);
        !live_input || t < ui::onboarding::STEP_IN
    }

    fn checked_imports(&self) -> Vec<PathBuf> {
        self.imports
            .iter()
            .filter(|(_, on)| *on)
            .map(|(path, _)| path.clone())
            .collect()
    }
}

/// What finishing writes: onboarding is done, and the imported folders
/// join the favorites (after any already there, no duplicates).
fn finish_settings(settings: &mut filex::settings::Settings, imports: Vec<PathBuf>) {
    settings.onboarding_pending = false;
    for path in imports {
        if !settings.favorites.contains(&path) {
            settings.favorites.push(path);
        }
    }
}

/// Rows shown under the demo search box; the full list lives in the app.
const DEMO_RESULTS: usize = 6;

/// The demo search box's placeholder: a query that finds something on
/// nearly every disk.
const DEMO_PLACEHOLDER: &str = "resume pdf";

/// The live Magic step's placeholder: a command most disks can act on.
const MAGIC_PLACEHOLDER: &str = "move all pdfs to Documents";

impl Workspace {
    /// Open the flow (first launch, or "Replay welcome" in Settings) and
    /// look for the OS file manager's pinned folders in the background.
    pub(super) fn start_onboarding(&mut self, cx: &mut Context<Self>) {
        self.settings_open = false;
        let ready = self.service.is_some() && !self.daemon_status.building;
        self.onboarding = Some(Onboarding::new(Instant::now(), ready));
        let mut known: Vec<PathBuf> = ["home", "desktop", "documents", "downloads", "pictures"]
            .iter()
            .filter_map(|name| self.user_dirs.get(name).map(Path::to_path_buf))
            .collect();
        known.extend(self.settings.read(cx).settings().favorites.iter().cloned());
        self.check_default_offer(cx);
        cx.spawn(async move |this, cx| {
            let found = cx
                .background_executor()
                .spawn(async { filex::favorites_import::discover() })
                .await;
            let imports = filex::favorites_import::new_folders(found, &known);
            this.update(cx, |this, cx| {
                if let Some(onboarding) = &mut this.onboarding {
                    onboarding.imports = imports.into_iter().map(|p| (p, true)).collect();
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn onboarding_go(&mut self, offset: isize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(onboarding) = &mut self.onboarding else {
            return;
        };
        if onboarding.step == Step::Loading {
            // Only forward, and only once carrying on is offered.
            if offset > 0 && onboarding.step_started.elapsed() >= OFFER_CONTINUE_AFTER {
                onboarding.step = Step::Welcome;
                onboarding.step_started = Instant::now();
                cx.notify();
            }
            return;
        }
        let Some(step) = onboarding.step.shifted(offset, onboarding.optional()) else {
            if offset > 0 {
                self.finish_onboarding(true, window, cx);
            }
            return;
        };
        onboarding.step = step;
        onboarding.step_started = Instant::now();
        let magic_live = onboarding.magic_live;
        // Each step starts from an empty box in plain search mode; the
        // search and live Magic steps then set up their own input.
        self.clear_search(cx);
        match step {
            Step::Search => {
                self.set_search_placeholder(DEMO_PLACEHOLDER, cx);
                self.focus_search(window, cx);
            }
            Step::Magic if magic_live => self.go_live_magic(window, cx),
            _ => {
                if step == Step::Magic {
                    self.schedule_magic_handover(window, cx);
                }
                self.set_search_placeholder(search_input::DEFAULT_PLACEHOLDER, cx);
                window.focus(&self.focus_handle, cx);
            }
        }
        cx.notify();
    }

    /// Leave the flow for the app. `import` pins the ticked folders;
    /// skipping doesn't.
    fn finish_onboarding(&mut self, import: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.leave_onboarding(import, cx);
        self.clear_search(cx);
        window.focus(&self.focus_handle, cx);
    }

    /// Close the flow and save what it decided, leaving the query alone
    /// (a Magic plan applied from onboarding carries on in the app).
    fn leave_onboarding(&mut self, import: bool, cx: &mut Context<Self>) {
        let Some(onboarding) = self.onboarding.take() else {
            return;
        };
        let imports = if import {
            onboarding.checked_imports()
        } else {
            Vec::new()
        };
        self.settings.update(cx, |store, cx| {
            store.update(cx, |settings| finish_settings(settings, imports));
        });
        if onboarding.applies_default(import) {
            self.set_integration(Integration::DefaultExplorer, true, cx);
        }
        self.set_search_placeholder(search_input::DEFAULT_PLACEHOLDER, cx);
        if !cx.reduce_motion() {
            self.reveal = Some(Reveal {
                started: Instant::now(),
                from_ready: onboarding.step == Step::Ready,
            });
        }
        cx.notify();
    }

    /// Seconds into the hand-off while it plays, asking for the next frame;
    /// `None` once the app has settled.
    pub(super) fn reveal_time(&mut self, window: &mut Window) -> Option<f32> {
        let t = self.reveal.as_ref()?.started.elapsed().as_secs_f32();
        if t >= ui::onboarding::REVEAL_LEN {
            self.reveal = None;
            return None;
        }
        window.request_animation_frame();
        Some(t)
    }

    /// Onboarding dissolving off the app, `t` seconds into the hand-off.
    pub(super) fn render_reveal(&self, t: f32, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let reveal = self.reveal.as_ref()?;
        let theme = *cx.theme();
        let body = reveal
            .from_ready
            .then(|| self.render_ready_step(&theme, ui::onboarding::INTRO_LEN, cx));
        Some(ui::onboarding::reveal_overlay(&theme, t, body).into_any_element())
    }

    /// Applying a plan from the live Magic step: finish onboarding first, so
    /// the plan runs in the app where its progress, any conflict dialog and
    /// undo are on screen. Returns whether onboarding was open.
    pub(super) fn hand_off_onboarding(&mut self, cx: &mut Context<Self>) -> bool {
        if self.onboarding.is_none() {
            return false;
        }
        self.leave_onboarding(true, cx);
        true
    }

    /// Once the demo has played and held for a breath, hand the Magic step to
    /// the user.
    fn schedule_magic_handover(&self, window: &mut Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_secs_f32(ui::onboarding::MAGIC_HANDOVER))
                .await;
            this.update_in(cx, |this, window, cx| {
                let waiting = this.onboarding.as_ref().is_some_and(|o| {
                    o.step == Step::Magic
                        && !o.magic_live
                        && o.step_started.elapsed().as_secs_f32() >= ui::onboarding::MAGIC_HANDOVER
                });
                if waiting {
                    this.go_live_magic(window, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Switch the Magic step to the real thing: the search input in forced
    /// Magic mode, with the app's own plan pane under it.
    fn go_live_magic(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(onboarding) = &mut self.onboarding else {
            return;
        };
        if !onboarding.magic_live {
            onboarding.magic_live = true;
            onboarding.step_started = Instant::now();
        }
        self.magic_mode = MagicMode::On;
        self.set_search_placeholder(MAGIC_PLACEHOLDER, cx);
        window.focus(&self.search_input.focus_handle(cx), cx);
        self.update_search(cx);
        cx.notify();
    }

    fn set_search_placeholder(&self, text: &'static str, cx: &mut Context<Self>) {
        self.search_input
            .update(cx, |input, cx| input.set_placeholder(text, cx));
    }

    fn toggle_import(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some((_, on)) = self
            .onboarding
            .as_mut()
            .and_then(|onboarding| onboarding.imports.get_mut(ix))
        {
            *on = !*on;
            cx.notify();
        }
    }

    /// Read, off the UI thread, whether to offer the DefaultApp step:
    /// supported here and not already the default. It lands long before
    /// the user reaches that step (it sits after Magic).
    fn check_default_offer(&mut self, cx: &mut Context<Self>) {
        if !filex::shell_integration::support().default_explorer {
            return;
        }
        cx.spawn(async move |this, cx| {
            let status = cx
                .background_executor()
                .spawn(async { filex::shell_integration::status() })
                .await;
            this.update(cx, |this, cx| {
                if let Some(onboarding) = &mut this.onboarding {
                    onboarding.offer_default = !status.default_explorer;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn toggle_make_default(&mut self, cx: &mut Context<Self>) {
        if let Some(onboarding) = &mut self.onboarding {
            onboarding.make_default = !onboarding.make_default;
            cx.notify();
        }
    }

    pub(super) fn render_onboarding(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = *cx.theme();
        if let Some(onboarding) = &mut self.onboarding {
            onboarding.advance_loading(Instant::now());
        }
        let Some(onboarding) = &self.onboarding else {
            return div().into_any_element();
        };
        let step = onboarding.step;
        if step == Step::Loading {
            return self.render_loading_screen(&theme, window, cx);
        }
        let settled = match step {
            Step::Welcome => ui::onboarding::INTRO_LEN,
            Step::Magic => ui::onboarding::MAGIC_STILL,
            _ => 2.,
        };
        let t = onboarding.scene_time(cx.reduce_motion(), settled);
        if !cx.reduce_motion() && onboarding.animating(t) {
            window.request_animation_frame();
        }
        let viewport = window.viewport_size();
        let body = match step {
            Step::Loading | Step::Welcome => self.render_welcome(&theme, viewport, t),
            Step::Theme => self.render_theme_step(&theme, cx),
            Step::Favorites => self.render_favorites_step(&theme, cx),
            Step::Search => self.render_search_step(&theme),
            Step::Magic => self.render_magic_step(&theme, t, cx),
            Step::DefaultApp => self.render_default_step(&theme, cx),
            Step::Ready => self.render_ready_step(&theme, t, cx),
        };
        let steps: Vec<Step> = Step::visible(onboarding.optional()).collect();
        let footer = ui::onboarding::footer((step == Step::Welcome).then_some(t))
            .child(ui::onboarding::progress_dots(
                &theme,
                steps.len(),
                steps.iter().position(|s| *s == step).unwrap_or(0),
            ))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .children((step != Step::Welcome).then(|| {
                        ui::button::button(
                            &theme,
                            "onboarding-back",
                            "Back",
                            ui::button::Variant::Secondary,
                            ui::button::Size::Regular,
                        )
                        .on_click(cx.listener(
                            |this, _: &ClickEvent, window, cx| {
                                this.onboarding_go(-1, window, cx);
                            },
                        ))
                    }))
                    .child(
                        ui::button::button(
                            &theme,
                            "onboarding-next",
                            match step {
                                Step::Welcome => "Get started  ↵",
                                Step::Ready => "Start exploring  ↵",
                                _ => "Continue  ↵",
                            },
                            ui::button::Variant::Primary,
                            ui::button::Size::Regular,
                        )
                        .on_click(cx.listener(
                            |this, _: &ClickEvent, window, cx| {
                                this.onboarding_go(1, window, cx);
                            },
                        )),
                    ),
            );
        ui::onboarding::screen(&theme)
            .track_focus(&self.focus_handle)
            .key_context("Onboarding")
            // Escape in the (empty) search box arrives as its bubbled
            // ClearInput; everywhere else as a plain key.
            .on_action(
                cx.listener(|this, _: &search_input::ClearInput, window, cx| {
                    this.onboarding_go(-1, window, cx);
                }),
            )
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                let modifiers = event.keystroke.modifiers;
                if modifiers.platform || modifiers.control || modifiers.alt {
                    return;
                }
                match event.keystroke.key.as_str() {
                    "enter" => this.onboarding_go(1, window, cx),
                    "escape" => this.onboarding_go(-1, window, cx),
                    _ => return,
                }
                cx.stop_propagation();
            }))
            .child(ui::onboarding::backdrop())
            .child(ui::onboarding::top_row().child(
                ui::onboarding::link(&theme, "onboarding-skip", "Skip").on_click(cx.listener(
                    |this, _: &ClickEvent, window, cx| {
                        this.finish_onboarding(false, window, cx);
                    },
                )),
            ))
            .child(ui::onboarding::stage(body, t))
            .child(footer)
            .into_any_element()
    }

    fn render_welcome(
        &self,
        theme: &Theme,
        viewport: gpui::Size<gpui::Pixels>,
        t: f32,
    ) -> gpui::AnyElement {
        // The intro's counter races to the real index size once known.
        let files = self
            .daemon_status
            .roots
            .iter()
            .map(|r| r.files)
            .sum::<u64>()
            .max(self.onboarding.as_ref().map_or(0, |o| o.loading.scanned));
        div()
            .flex()
            .flex_col()
            .items_center()
            .gap_8()
            .child(ui::onboarding::intro_mark(theme, viewport, t, Some(files)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_3()
                    .child(ui::onboarding::intro_title(theme, t))
                    .child(ui::onboarding::intro_lede(
                        theme,
                        "Filex is a file explorer built for speed. Every file name on \
                         your disk, searchable as fast as you can type.",
                        t,
                    )),
            )
            .into_any_element()
    }

    /// The loading step: the whole screen, as it has no step dots or Back.
    /// It hands over to the welcome by itself once the index is ready.
    fn render_loading_screen(
        &mut self,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let now = Instant::now();
        let reduce_motion = cx.reduce_motion();
        let connected = self.service.is_some();
        let overrides = self.settings.read(cx).settings().keyboard_shortcuts.clone();
        let Some(onboarding) = &mut self.onboarding else {
            return div().into_any_element();
        };
        let count = onboarding.eased_count(now);
        let open_for = now.saturating_duration_since(onboarding.step_started);
        let since = |at: Instant| now.saturating_duration_since(at).as_secs_f32();
        let outro_start = onboarding.outro_start().filter(|start| now >= *start);
        let phase = if outro_start.is_some() {
            ui::onboarding::LoadingPhase::Ready
        } else if onboarding.loading.indexing_since.is_some() {
            ui::onboarding::LoadingPhase::Indexing
        } else if connected {
            ui::onboarding::LoadingPhase::Scanning
        } else {
            ui::onboarding::LoadingPhase::Starting
        };
        let tips = [
            (
                Some(shortcuts::hint("search", &overrides)),
                "Search every file name, from anywhere",
            ),
            (
                Some(shortcuts::hint("quick_look", &overrides)),
                "Peek at a file without opening it",
            ),
            (None, "Matches appear as you type, in milliseconds"),
            (None, "Your index stays on this computer"),
            (
                Some(shortcuts::hint("keyboard", &overrides)),
                "See every shortcut",
            ),
        ];
        let offer_continue = open_for >= OFFER_CONTINUE_AFTER && outro_start.is_none();
        let scene = ui::onboarding::LoadingScene {
            t: if reduce_motion {
                1.
            } else {
                open_for.as_secs_f32()
            },
            phase,
            indexing_for: onboarding.loading.indexing_since.map(since),
            outro: outro_start.map_or(0., |start| {
                (since(start) / ui::onboarding::LOADING_OUTRO).min(1.)
            }),
            count,
            names: &onboarding.loading.names,
            tips: &tips,
            offer_continue,
        };
        if !reduce_motion {
            window.request_animation_frame();
        }
        let viewport = window.viewport_size();
        let body = div()
            .flex()
            .flex_col()
            .items_center()
            .gap_6()
            .child(ui::onboarding::loading_core(theme, viewport, &scene))
            .child(ui::onboarding::loading_status(theme, &scene))
            .into_any_element();
        let footer = ui::onboarding::footer(None)
            .justify_center()
            .h(px(32.))
            .child(ui::onboarding::loading_tip(theme, &scene))
            .children(scene.offer_continue.then(|| {
                ui::onboarding::link(theme, "onboarding-continue", "Continue while it finishes →")
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.onboarding_go(1, window, cx);
                    }))
            }));
        ui::onboarding::screen(theme)
            .track_focus(&self.focus_handle)
            .key_context("Onboarding")
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "enter" {
                    this.onboarding_go(1, window, cx);
                    cx.stop_propagation();
                }
            }))
            .child(ui::onboarding::backdrop())
            .child(ui::onboarding::top_row().child(
                ui::onboarding::link(theme, "onboarding-skip", "Skip").on_click(cx.listener(
                    |this, _: &ClickEvent, window, cx| {
                        this.finish_onboarding(false, window, cx);
                    },
                )),
            ))
            .child(ui::onboarding::stage(body, scene.t.min(1.)))
            .child(footer)
            .into_any_element()
    }

    fn render_theme_step(&self, theme: &Theme, cx: &mut Context<Self>) -> gpui::AnyElement {
        let settings = self.settings.read(cx).settings().clone();
        div()
            .flex()
            .flex_col()
            .gap_6()
            .child(ui::onboarding::hero(theme, "Make it yours."))
            .child(ui::onboarding::lede(
                theme,
                "Pick a look. You can change it any time in Settings.",
            ))
            .child(ui::onboarding::sample_rows(theme))
            .child(ui::onboarding::field(
                theme,
                "Theme",
                self.render_theme_selector(theme, settings.theme, cx),
            ))
            .child(ui::onboarding::field(
                theme,
                "Accent",
                self.render_accent_picker(theme, settings.accent, cx),
            ))
            .child(ui::onboarding::field(
                theme,
                "Density",
                self.render_density_selector(theme, settings.density, cx),
            ))
            .into_any_element()
    }

    fn render_favorites_step(&self, theme: &Theme, cx: &mut Context<Self>) -> gpui::AnyElement {
        let source = if cfg!(target_os = "macos") {
            "Finder"
        } else if cfg!(windows) {
            "Quick Access"
        } else {
            "your file manager"
        };
        let imports = self
            .onboarding
            .as_ref()
            .map(|o| o.imports.clone())
            .unwrap_or_default();
        div()
            .flex()
            .flex_col()
            .gap_6()
            .child(ui::onboarding::hero(theme, "Bring your favorites."))
            .child(ui::onboarding::lede(
                theme,
                format!(
                    "These folders are pinned in {source}. Ticked ones join the Filex sidebar."
                ),
            ))
            .child(
                div()
                    .id("onboarding-imports")
                    .flex()
                    .flex_col()
                    .gap_1()
                    .max_h(px(280.))
                    .overflow_y_scroll()
                    .children(imports.into_iter().enumerate().map(|(ix, (path, on))| {
                        let name = path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| display_path(&path));
                        ui::onboarding::check_row(
                            theme,
                            ("import", ix),
                            "icons/star.svg",
                            name,
                            display_path(&path),
                            on,
                        )
                        .on_click(cx.listener(
                            move |this, _: &ClickEvent, _window, cx| {
                                this.toggle_import(ix, cx);
                            },
                        ))
                    })),
            )
            .into_any_element()
    }

    fn render_default_step(&self, theme: &Theme, cx: &mut Context<Self>) -> gpui::AnyElement {
        let on = self.onboarding.as_ref().is_some_and(|o| o.make_default);
        div()
            .flex()
            .flex_col()
            .gap_6()
            .child(ui::onboarding::hero(theme, "Open your folders here?"))
            .child(ui::onboarding::lede(
                theme,
                "Make Filex your default file explorer: folders and drives you open \
                 anywhere in Windows land in Filex. Places it can’t show, like Control \
                 Panel, still open in File Explorer.",
            ))
            .child(
                ui::onboarding::check_row(
                    theme,
                    "onboarding-make-default",
                    "icons/folder.svg",
                    "Make Filex my default file explorer",
                    "Change it any time in Settings › Integration.",
                    on,
                )
                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                    this.toggle_make_default(cx);
                })),
            )
            .into_any_element()
    }

    fn render_search_step(&self, theme: &Theme) -> gpui::AnyElement {
        let files: u64 = self.daemon_status.roots.iter().map(|r| r.files).sum();
        let onboarding = self.onboarding.as_ref();
        let counter_caption: SharedString = if self.service.is_none() {
            "Starting the indexer…".into()
        } else if self.daemon_status.building {
            "files indexed so far".into()
        } else if let Some(took) = onboarding.and_then(|o| o.indexed_in) {
            format!("files indexed in {:.1} s", took.as_secs_f32()).into()
        } else {
            "files indexed".into()
        };
        let latency: Option<SharedString> = self
            .search_latency
            .as_ref()
            .filter(|(query, _)| !self.query.is_empty() && *query == self.query)
            .map(|(_, took)| {
                let ms = took.as_secs_f64() * 1000.;
                let count = self.results.len();
                let shown = if self.search_more {
                    format!("{count}+")
                } else {
                    count.to_string()
                };
                format!("{shown} matches in {ms:.1} ms").into()
            });
        div()
            .flex()
            .flex_col()
            .gap_5()
            .child(ui::onboarding::counter(
                theme,
                ui::onboarding::with_commas(files),
                counter_caption,
                self.daemon_status.building || self.service.is_none(),
            ))
            .child(ui::onboarding::lede(
                theme,
                "Now try it. Type part of any file name on your disk.",
            ))
            .child(
                ui::top_bar::search_box(theme, true)
                    .max_w_full()
                    .h(px(44.))
                    .text_base()
                    .child(div().flex_1().min_w_0().child(self.search_input.clone())),
            )
            .child(
                div().flex().flex_col().h(px(6. * 40.)).children(
                    self.results
                        .iter()
                        .take(DEMO_RESULTS)
                        .enumerate()
                        .map(|(ix, row)| {
                            ui::onboarding::result_row(
                                theme,
                                ix,
                                row.name.clone(),
                                row.path_label.clone(),
                                row.is_dir,
                            )
                        }),
                ),
            )
            .child(ui::onboarding::caption(
                theme,
                latency.unwrap_or_else(|| " ".into()),
            ))
            .into_any_element()
    }

    fn render_magic_step(&self, theme: &Theme, t: f32, cx: &mut Context<Self>) -> gpui::AnyElement {
        let live = self.onboarding.as_ref().is_some_and(|o| o.magic_live);
        if live {
            return div()
                .flex()
                .flex_col()
                .gap_4()
                .child(ui::onboarding::hero(theme, "Your turn."))
                .child(ui::onboarding::lede(
                    theme,
                    "Type a command about your own files. Filex shows the plan; \
                     Apply runs it in the main window, where you can undo it.",
                ))
                .child(
                    ui::onboarding::magic_box(theme)
                        .child(div().flex_1().min_w_0().child(self.search_input.clone())),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .h(px(300.))
                        .rounded_lg()
                        .border_1()
                        .border_color(theme.border)
                        .overflow_hidden()
                        .child(self.render_magic_pane(cx)),
                )
                .into_any_element();
        }
        div()
            .flex()
            .flex_col()
            .gap_5()
            .child(ui::onboarding::hero(theme, "Just say it."))
            .child(ui::onboarding::lede(
                theme,
                "Magic mode turns plain English into a plan. You review every file \
                 before anything moves.",
            ))
            .child(ui::onboarding::magic_demo(theme, t))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(ui::onboarding::caption(
                        theme,
                        "In the app: type a command in search, or click ✦ to switch modes.",
                    ))
                    .child(
                        ui::button::button(
                            theme,
                            "magic-try",
                            "Try it yourself",
                            ui::button::Variant::Secondary,
                            ui::button::Size::Small,
                        )
                        .on_click(cx.listener(
                            |this, _: &ClickEvent, window, cx| {
                                this.go_live_magic(window, cx);
                            },
                        )),
                    ),
            )
            .into_any_element()
    }

    fn render_ready_step(&self, theme: &Theme, t: f32, cx: &mut Context<Self>) -> gpui::AnyElement {
        let overrides = self.settings.read(cx).settings().keyboard_shortcuts.clone();
        div()
            .flex()
            .flex_col()
            .gap_6()
            .child(ui::onboarding::hero(theme, "You're all set."))
            .child(ui::onboarding::lede(
                theme,
                "Three keys get you most of the way.",
            ))
            .child(
                div().flex().flex_col().gap_3().children(
                    [
                        ("search", "Search everything, from anywhere"),
                        ("quick_look", "Peek at a file without opening it"),
                        ("keyboard", "See every shortcut"),
                    ]
                    .into_iter()
                    .enumerate()
                    .map(|(ix, (id, what))| {
                        ui::onboarding::shortcut_tip(
                            theme,
                            shortcuts::hint(id, &overrides),
                            what,
                            ix,
                            t,
                        )
                    }),
                ),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: Optional = Optional {
        favorites: false,
        default_app: false,
    };
    const FAVORITES: Optional = Optional {
        favorites: true,
        default_app: false,
    };
    const DEFAULT_APP: Optional = Optional {
        favorites: false,
        default_app: true,
    };

    #[test]
    fn favorites_step_only_appears_with_something_to_import() {
        assert_eq!(Step::Theme.shifted(1, FAVORITES), Some(Step::Favorites));
        assert_eq!(Step::Theme.shifted(1, NONE), Some(Step::Search));
        assert_eq!(Step::Search.shifted(-1, NONE), Some(Step::Theme));
        assert_eq!(Step::Search.shifted(1, NONE), Some(Step::Magic));
        assert_eq!(Step::Magic.shifted(1, NONE), Some(Step::Ready));
        assert_eq!(Step::Welcome.shifted(-1, FAVORITES), None);
        assert_eq!(Step::Ready.shifted(1, FAVORITES), None);
    }

    #[test]
    fn default_app_step_sits_before_ready_only_when_offered() {
        assert_eq!(Step::Magic.shifted(1, DEFAULT_APP), Some(Step::DefaultApp));
        assert_eq!(Step::DefaultApp.shifted(1, DEFAULT_APP), Some(Step::Ready));
        assert_eq!(Step::Ready.shifted(-1, DEFAULT_APP), Some(Step::DefaultApp));
        assert_eq!(Step::Ready.shifted(-1, NONE), Some(Step::Magic));
        assert_eq!(Step::visible(NONE).count(), 5);
        assert_eq!(
            Step::visible(Optional {
                favorites: true,
                default_app: true
            })
            .count(),
            7
        );
    }

    #[test]
    fn a_step_hidden_after_the_fact_still_moves() {
        // On Favorites when its imports vanish: forward goes on, not back.
        assert_eq!(Step::Favorites.shifted(1, NONE), Some(Step::Search));
    }

    #[test]
    fn default_app_is_applied_only_when_offered_ticked_and_finished() {
        let mut onboarding = Onboarding::new(Instant::now(), true);
        assert!(onboarding.make_default, "ticked by default");
        assert!(
            !onboarding.applies_default(true),
            "not offered (unsupported, or already the default)"
        );
        onboarding.offer_default = true;
        assert!(onboarding.applies_default(true));
        assert!(!onboarding.applies_default(false), "Skip changes nothing");
        onboarding.make_default = false;
        assert!(!onboarding.applies_default(true), "unticked");
    }

    #[test]
    fn index_time_is_only_claimed_for_a_build_we_watched() {
        let start = Instant::now();
        let mut already_built = Onboarding::new(start, true);
        already_built.observe_index(false, start + Duration::from_millis(100));
        assert_eq!(already_built.indexed_in, None);

        let mut fresh = Onboarding::new(start, true);
        fresh.observe_index(true, start + Duration::from_secs(1));
        fresh.observe_index(false, start + Duration::from_secs(4));
        fresh.observe_index(false, start + Duration::from_secs(9));
        assert_eq!(fresh.indexed_in, Some(Duration::from_secs(4)));
    }

    fn status(
        building: bool,
        scanned: u64,
        indexing: bool,
        recent: &[&str],
    ) -> filex::daemon::ipc::Status {
        filex::daemon::ipc::Status {
            building,
            progress: building.then(|| filex::daemon::ipc::Progress {
                scanned,
                indexing,
                recent: recent.iter().map(|n| n.to_string()).collect(),
            }),
            ..Default::default()
        }
    }

    /// The welcome waits for the index, then for the scene's minimum time,
    /// then for the completion beat.
    #[test]
    fn loading_hands_over_only_after_the_index_is_ready() {
        let start = Instant::now();
        let at = |secs: f32| start + Duration::from_secs_f32(secs);
        let mut onboarding = Onboarding::new(start, false);
        assert_eq!(onboarding.step, Step::Loading);
        onboarding.observe_index(true, at(0.1));
        onboarding.observe_loading(&status(true, 500, false, &["a.txt"]), at(0.1));
        onboarding.advance_loading(at(30.));
        assert_eq!(onboarding.step, Step::Loading, "never while building");

        // Ready at 0.5 s: the scene still gets its minimum, then the outro.
        onboarding.observe_index(false, at(0.5));
        onboarding.observe_loading(&status(false, 0, false, &[]), at(0.5));
        let handover = ui::onboarding::LOADING_MIN + ui::onboarding::LOADING_OUTRO;
        onboarding.advance_loading(at(handover - 0.05));
        assert_eq!(onboarding.step, Step::Loading);
        onboarding.advance_loading(at(handover + 0.05));
        assert_eq!(onboarding.step, Step::Welcome);
    }

    #[test]
    fn a_slow_index_plays_its_outro_when_it_finishes() {
        let start = Instant::now();
        let at = |secs: f32| start + Duration::from_secs_f32(secs);
        let mut onboarding = Onboarding::new(start, false);
        onboarding.observe_index(true, at(0.1));
        onboarding.observe_index(false, at(12.));
        onboarding.observe_loading(&status(false, 0, false, &[]), at(12.));
        assert_eq!(onboarding.outro_start(), Some(at(12.)));
        onboarding.advance_loading(at(12. + ui::onboarding::LOADING_OUTRO + 0.05));
        assert_eq!(onboarding.step, Step::Welcome);
    }

    /// An index that was already built (a reinstall) skips the scene: no
    /// staged wait for work that isn't happening.
    #[test]
    fn an_index_ready_before_onboarding_skips_loading() {
        let start = Instant::now();
        let mut onboarding = Onboarding::new(start, false);
        onboarding.observe_index(false, start);
        onboarding.observe_loading(&status(false, 0, false, &[]), start);
        onboarding.advance_loading(start);
        assert_eq!(onboarding.step, Step::Welcome);
        assert_eq!(Onboarding::new(start, true).step, Step::Welcome, "replay");
    }

    #[test]
    fn loading_keeps_walk_names_in_order_without_repeats() {
        let start = Instant::now();
        let mut onboarding = Onboarding::new(start, false);
        onboarding.observe_loading(&status(true, 10, false, &["a", "b"]), start);
        onboarding.observe_loading(&status(true, 30, true, &["b", "c"]), start);
        // A later walk restarting its count never rolls the number back.
        onboarding.observe_loading(&status(true, 4, true, &[]), start);
        let loading = &onboarding.loading;
        assert_eq!(loading.names, ["a", "b", "c"].map(SharedString::from));
        assert_eq!(loading.scanned, 30);
        assert!(loading.indexing_since.is_some());

        let many: Vec<String> = (0..LOADING_NAMES + 10).map(|i| format!("n{i}")).collect();
        let many: Vec<&str> = many.iter().map(String::as_str).collect();
        onboarding.observe_loading(&status(true, 40, true, &many), start);
        assert_eq!(onboarding.loading.names.len(), LOADING_NAMES);
        assert_eq!(
            onboarding.loading.names[0], "a",
            "earlier names keep their place"
        );
    }

    #[test]
    fn live_input_steps_stop_redrawing_once_settled() {
        let mut onboarding = Onboarding::new(Instant::now(), true);
        let settled = ui::onboarding::STEP_IN + 0.1;
        onboarding.step = Step::Search;
        assert!(!onboarding.animating(settled));
        onboarding.step = Step::Magic;
        assert!(
            onboarding.animating(settled),
            "the scripted demo keeps moving"
        );
        onboarding.magic_live = true;
        assert!(!onboarding.animating(settled));
        assert!(onboarding.animating(0.), "the step still eases in");
    }

    #[test]
    fn finishing_clears_the_flag_and_appends_new_favorites_once() {
        let mut settings = filex::settings::Settings {
            onboarding_pending: true,
            favorites: vec!["/kept".into()],
            ..Default::default()
        };
        finish_settings(&mut settings, vec!["/kept".into(), "/new".into()]);
        assert!(!settings.onboarding_pending);
        assert_eq!(
            settings.favorites,
            vec![PathBuf::from("/kept"), PathBuf::from("/new")]
        );
    }

    #[test]
    fn only_ticked_imports_are_pinned() {
        let mut onboarding = Onboarding::new(Instant::now(), true);
        onboarding.imports = vec![("/a".into(), true), ("/b".into(), false)];
        assert_eq!(onboarding.checked_imports(), vec![PathBuf::from("/a")]);
    }
}

//! Onboarding building blocks: the full-window screen and its aurora
//! backdrop, the welcome intro, the Magic mode demo, and the small rows the
//! steps use. State and wiring live in `workspace::onboarding`.
//!
//! Motion is a pure function of `t`, the seconds since the current step
//! appeared (the workspace re-renders every frame while a step moves). Each
//! scene is a 1:1 port of `docs/onboarding-motion.html`; keep the two in
//! step. Only opacity, position, size and two-stop gradients are animated —
//! what gpui-ce can draw.

use std::time::Duration;

use gpui::{
    Animation, AnimationExt as _, AnyElement, Div, ElementId, Rgba, SharedString, Stateful, div,
    ease_out_quint, img, linear_color_stop, linear_gradient, prelude::*, px, rgb,
};

use super::icon::{LOGO_ASPECT, LogoLayer, logo_layer, ui_icon};
use super::kbd::keycap;
use super::theme::{Theme, fixed};

/// Width of the centered content column.
const COLUMN: f32 = 560.;

/// How long a step takes to settle in.
pub const STEP_IN: f32 = 0.42;

/// After this the welcome only runs its idle loops (halo, float, aurora).
pub const INTRO_LEN: f32 = 3.4;

/// When the Magic demo hands over to the user: its last beat (Apply) lands
/// at 4.9 s, then the finished plan holds for 5 s so it can be read.
pub const MAGIC_HANDOVER: f32 = 4.9 + 5.;

/// A frame of the Magic demo with everything on screen, for reduced motion.
pub const MAGIC_STILL: f32 = 5.;

// ---- Timing ----------------------------------------------------------------

/// Progress through the beat `from..to` (seconds) at scene time `t`,
/// clamped to 0..1.
fn beat(t: f32, from: f32, to: f32) -> f32 {
    ((t - from) / (to - from)).clamp(0., 1.)
}

fn quint(t: f32) -> f32 {
    1. - (1. - t).powi(5)
}

fn cubic_in(t: f32) -> f32 {
    t * t * t
}

/// Ease-out that overshoots ~10% and settles: pops.
fn back_out(t: f32) -> f32 {
    let (c1, c3) = (1.70158, 2.70158);
    1. + c3 * (t - 1.).powi(3) + c1 * (t - 1.).powi(2)
}

/// Fade-and-lift for an element arriving `at` seconds into the scene.
fn rise<E: Styled>(element: E, t: f32, at: f32) -> E {
    let p = quint(beat(t, at, at + 0.6));
    element.opacity(p).mt(px(14. * (1. - p)))
}

/// `1234567` → `"1,234,567"`.
pub fn with_commas(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

fn white(alpha: f32) -> Rgba {
    fixed::with_alpha(rgb(0xffffff), alpha)
}

/// A two-stop gradient fill from `from` to `to` at `angle` degrees
/// (0 = upward, 90 = rightward).
fn gradient(angle: f32, from: Rgba, to: Rgba) -> gpui::Background {
    linear_gradient(
        angle,
        linear_color_stop(from, 0.),
        linear_color_stop(to, 1.),
    )
}

/// A soft horizontal band: two mirrored gradients, transparent at both
/// ends and `color` in the middle. gpui gradients have two stops, so a
/// band takes two halves.
fn band(color: Rgba, width: f32, height: f32) -> Div {
    let clear = fixed::with_alpha(color, 0.);
    div()
        .flex()
        .w(px(width))
        .h(px(height))
        .child(div().flex_1().bg(gradient(90., clear, color)))
        .child(div().flex_1().bg(gradient(90., color, clear)))
}

// ---- Screen chrome ---------------------------------------------------------

/// The whole window: a centered column between a top row (kept clear of
/// the macOS traffic lights) and the footer, over the aurora backdrop.
pub fn screen(theme: &Theme) -> Div {
    div()
        .relative()
        .size_full()
        .overflow_hidden()
        .flex()
        .flex_col()
        .items_center()
        .px_8()
        .pb_8()
        .bg(theme.bg)
        .text_color(theme.text)
        .font_family(super::fonts::UI_FONT_FAMILY)
}

/// A soft brand-blue wash from the top of the window, behind every step.
pub fn backdrop() -> Div {
    let deep = fixed::brand_deep();
    div().absolute().inset_0().bg(gradient(
        180.,
        fixed::with_alpha(deep, 0.18),
        fixed::with_alpha(deep, 0.),
    ))
}

/// Right-aligned strip at the top, tall enough to clear the traffic lights.
pub fn top_row() -> Div {
    div()
        .flex()
        .flex_none()
        .justify_end()
        .items_center()
        .w_full()
        .h(px(52.))
}

/// A quiet text link (Skip).
pub fn link(theme: &Theme, id: impl Into<ElementId>, label: &'static str) -> Stateful<Div> {
    let text = theme.text;
    div()
        .id(id)
        .px_2()
        .py_1()
        .rounded_md()
        .cursor_pointer()
        .text_sm()
        .text_color(theme.text_dim)
        .hover(move |s| s.text_color(text))
        .child(label)
}

/// Holds the current step, centered, easing it in: a fade plus a short rise.
pub fn stage(body: AnyElement, t: f32) -> Div {
    let p = quint(beat(t, 0., STEP_IN));
    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .justify_center()
        .w_full()
        .max_w(px(COLUMN))
        .opacity(p)
        .mt(px(18. * (1. - p)))
        .child(body)
}

/// Bottom row: progress dots left, buttons right. On the welcome it rises
/// in with the rest of the intro.
pub fn footer(t: Option<f32>) -> Div {
    let footer = div()
        .flex()
        .flex_none()
        .items_center()
        .justify_between()
        .w_full()
        .max_w(px(COLUMN));
    match t {
        Some(t) => rise(footer, t, 2.8),
        None => footer,
    }
}

/// One dot per step; the current one stretches into an accent pill as its
/// step arrives.
pub fn progress_dots(theme: &Theme, count: usize, current: usize) -> Div {
    div()
        .flex()
        .items_center()
        .gap_1p5()
        .children((0..count).map(|ix| {
            let dot = div().h(px(6.)).w(px(6.)).rounded_full();
            if ix != current {
                return dot.bg(theme.border).into_any_element();
            }
            dot.bg(theme.accent)
                .with_animation(
                    ("onboarding-dot", current),
                    Animation::new(Duration::from_millis(420)).with_easing(ease_out_quint()),
                    |dot, delta| dot.w(px(6. + 14. * delta)),
                )
                .into_any_element()
        }))
}

// ---- Hand-off to the app ---------------------------------------------------

/// How long leaving onboarding for the app takes.
pub const REVEAL_LEN: f32 = 0.8;

/// How far below its place the app's main area starts while onboarding
/// dissolves off it, `t` seconds into the hand-off; 0 once settled.
pub fn reveal_offset(t: f32) -> f32 {
    16. * (1. - quint(beat(t, 0.15, REVEAL_LEN)))
}

/// The departing onboarding, drawn over the app: its last screen (`body`,
/// laid out exactly where the step showed it) lifts away first, then the
/// sheet itself fades to reveal the app beneath. Not occluding, so the app
/// is usable the moment it shows.
pub fn reveal_overlay(theme: &Theme, t: f32, body: Option<AnyElement>) -> Div {
    let lift = quint(beat(t, 0., 0.3));
    let sheet = quint(beat(t, 0.2, REVEAL_LEN));
    let overlay = screen(theme)
        .absolute()
        .inset_0()
        .opacity(1. - sheet)
        .child(backdrop())
        .child(top_row());
    match body {
        Some(body) => overlay
            .child(stage(body, STEP_IN).opacity(1. - lift).mt(px(-16. * lift)))
            // Stands in for the footer so the body sits where it was.
            .child(div().flex_none().h(px(32.))),
        None => overlay,
    }
}

// ---- Welcome intro ---------------------------------------------------------

/// File names streaming through the intro: "your whole disk", acted out.
const NAMES: [&str; 24] = [
    "budget-2026.xlsx",
    "IMG_2041.jpg",
    "main.rs",
    "Resume.pdf",
    "notes.md",
    "invoice_0193.pdf",
    "holiday.mov",
    "design-v3.fig",
    "package.json",
    "DSC_8812.raw",
    "taxes",
    "README.md",
    "podcast.mp3",
    "Screenshot 14.02.png",
    "thesis.docx",
    "archive.zip",
    "index.html",
    "Projects",
    "report-q3.key",
    "song.flac",
    "lib.rs",
    "Passport scan.pdf",
    "wedding-01.heic",
    "kicad.sch",
];
/// Chips bursting outward, then rushing back in.
const OUTWARD: usize = 40;
const INWARD: usize = 24;
/// Logo width on the welcome.
const LOGO_W: f32 = 150.;
const LOGO_H: f32 = LOGO_W / LOGO_ASPECT;

/// Golden-angle direction for chip `ix`, so chips spread evenly.
fn angle(ix: usize) -> f32 {
    ix as f32 * 2.399_963
}

/// A floating file-name chip centred on `(x, y)` in its parent's space.
fn chip(theme: &Theme, name: impl Into<SharedString>, x: f32, y: f32, opacity: f32) -> Div {
    let name = name.into();
    // A width estimate is plenty to centre a short label.
    let half = name.chars().count() as f32 * 3.4 + 9.;
    div()
        .absolute()
        .left(px(x - half))
        .top(px(y - 11.))
        .whitespace_nowrap()
        .px_2()
        .py(px(3.))
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .bg(fixed::with_alpha(theme.panel, 0.85))
        .text_xs()
        .text_color(theme.text_dim)
        .opacity(opacity)
        .child(name)
}

/// A ring or disc of diameter `edge` centred on `(x, y)`.
fn round(x: f32, y: f32, edge: f32) -> Div {
    div()
        .absolute()
        .left(px(x - edge / 2.))
        .top(px(y - edge / 2.))
        .size(px(edge))
        .rounded_full()
}

/// One logo layer image, `w`×`h`, at `(left, top)` inside the logo box.
fn layer(layer: LogoLayer, left: f32, top: f32, w: f32, h: f32, opacity: f32) -> AnyElement {
    match logo_layer(layer) {
        Some(image) => img(image)
            .absolute()
            .left(px(left))
            .top(px(top))
            .w(px(w))
            .h(px(h))
            .opacity(opacity)
            .into_any_element(),
        None => div().into_any_element(),
    }
}

/// The intro's centrepiece, a `LOGO_W`×`LOGO_H` slot whose effects paint
/// far outside it: a warp of file names bursting out while a counter races,
/// names imploding back, a gradient burst and shockwaves, the logo
/// assembling from its layers with a shine, then a breathing halo and a
/// gentle float. `viewport` sizes the warp. The counter races to `files`,
/// the real index size, when it is known.
pub fn intro_mark(
    theme: &Theme,
    viewport: gpui::Size<gpui::Pixels>,
    t: f32,
    files: Option<u64>,
) -> Div {
    let (cx, cy) = (LOGO_W / 2., LOGO_H / 2.);
    let reach = f32::from(viewport.width).max(f32::from(viewport.height)) * 0.75;
    let sky = fixed::brand_sky();
    let mut slot = div().relative().flex_none().w(px(LOGO_W)).h(px(LOGO_H));

    // Halo: three soft discs that grow in, then breathe.
    let breathe = if t > 2. {
        0.5 + 0.5 * ((t - 2.) * 2.2).sin()
    } else {
        0.
    };
    let halo_in = quint(beat(t, 1.6, 2.4));
    for (edge, alpha) in [(460., 0.05), (330., 0.07), (230., 0.10)] {
        let edge = edge * (0.92 + 0.08 * breathe) * halo_in;
        slot = slot.child(round(cx, cy, edge).bg(fixed::with_alpha(sky, alpha)));
    }

    // Warp: names burst outward, accelerating past the window edge.
    for ix in 0..OUTWARD {
        let start = 0.05 + (ix % 8) as f32 * 0.04;
        let p = beat(t, start, start + 0.75);
        if p <= 0. || p >= 1. {
            continue;
        }
        let d = cubic_in(p) * reach + 30.;
        let a = angle(ix);
        let opacity = (p * 5.).min(1.) * (1. - p);
        slot = slot.child(chip(
            theme,
            NAMES[ix % NAMES.len()],
            cx + a.cos() * d,
            cy + a.sin() * d * 0.7,
            opacity,
        ));
    }
    // Implode: a second wave rushes in from the edges.
    for ix in 0..INWARD {
        let start = 0.95 + (ix % 6) as f32 * 0.03;
        let p = beat(t, start, start + 0.5);
        if p <= 0. || p >= 1. {
            continue;
        }
        let d = (1. - cubic_in(p)) * reach;
        let a = angle(ix) + 0.4;
        slot = slot.child(chip(
            theme,
            NAMES[(ix * 7) % NAMES.len()],
            cx + a.cos() * d,
            cy + a.sin() * d * 0.7,
            (p * 4.).min(1.),
        ));
    }

    // Counter racing while the warp runs.
    let counter = beat(t, 0.05, 0.3).min(1. - beat(t, 0.95, 1.2));
    if counter > 0. {
        let total = files.filter(|&n| n > 0).unwrap_or(2_481_337) as f64;
        let n = (total * f64::from(quint(beat(t, 0.1, 1.0)))) as u64;
        slot = slot.child(
            div()
                .absolute()
                .left(px(cx - 200.))
                .top(px(cy - 40.))
                .w(px(400.))
                .flex()
                .flex_col()
                .items_center()
                .opacity(counter)
                .child(
                    div()
                        .text_size(px(48.))
                        .line_height(px(56.))
                        .font_weight(gpui::FontWeight::EXTRA_BOLD)
                        .text_color(theme.text)
                        .child(with_commas(n)),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(theme.text_dim)
                        .child("file names"),
                ),
        );
    }

    // Burst and shockwaves as the logo lands.
    let burst = quint(beat(t, 1.5, 2.1));
    if burst > 0. && burst < 1. {
        slot = slot.child(
            round(cx, cy, 40. + 560. * burst)
                .bg(gradient(
                    180.,
                    fixed::with_alpha(sky, 0.55),
                    fixed::with_alpha(fixed::brand_deep(), 0.),
                ))
                .opacity(1. - burst),
        );
    }
    for ring in 0..3 {
        let at = 1.55 + ring as f32 * 0.15;
        let p = quint(beat(t, at, at + 0.75));
        if p <= 0. || p >= 1. {
            continue;
        }
        slot = slot.child(
            round(cx, cy, LOGO_W + 320. * p)
                .border_2()
                .border_color(sky)
                .opacity(0.8 * (1. - p)),
        );
    }

    // The logo assembles: back folder drops, front panel rises, "fx" pops.
    let float = if t > 2.6 {
        4. * ((t - 2.6) * 1.8).sin()
    } else {
        0.
    };
    let back = back_out(beat(t, 1.5, 2.0));
    let front = back_out(beat(t, 1.62, 2.12));
    let mark = back_out(beat(t, 1.8, 2.3)).max(0.);
    let shine = quint(beat(t, 2.25, 2.75));
    let mut logo = div()
        .absolute()
        .left_0()
        .top(px(float))
        .w(px(LOGO_W))
        .h(px(LOGO_H))
        .overflow_hidden()
        .rounded(px(14.))
        .child(layer(
            LogoLayer::Back,
            0.,
            -80. * (1. - back),
            LOGO_W,
            LOGO_H,
            (back * 3.).min(1.),
        ))
        .child(layer(
            LogoLayer::Front,
            0.,
            80. * (1. - front),
            LOGO_W,
            LOGO_H,
            (front * 3.).min(1.),
        ))
        .child(layer(
            LogoLayer::Mark,
            cx - LOGO_W * mark / 2.,
            cy - LOGO_H * mark / 2.,
            LOGO_W * mark,
            LOGO_H * mark,
            (mark * 3.).min(1.),
        ));
    if shine > 0. && shine < 1. {
        logo = logo.child(
            band(white(0.35), 50., LOGO_H)
                .absolute()
                .top_0()
                .left(px(-60. + (LOGO_W + 120.) * shine)),
        );
    }
    slot.child(logo)
}

/// The welcome headline with its last words in brand blue, rising in.
pub fn intro_title(theme: &Theme, t: f32) -> Div {
    rise(
        div()
            .flex()
            .flex_wrap()
            .justify_center()
            .gap(px(12.))
            .text_size(px(44.))
            .line_height(px(52.))
            .font_weight(gpui::FontWeight::EXTRA_BOLD)
            .child(div().text_color(theme.text).child("Find anything."))
            .child(div().text_color(fixed::brand_sky()).child("Instantly.")),
        t,
        2.3,
    )
}

/// The welcome's sentence, rising in after the title.
pub fn intro_lede(theme: &Theme, text: &'static str, t: f32) -> Div {
    rise(lede(theme, text).text_center(), t, 2.55)
}

// ---- Loading (before the welcome) ------------------------------------------

/// Where the first index stands, as the loading scene tells it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LoadingPhase {
    /// The index service isn't answering yet.
    Starting,
    /// The walk is reading the disk.
    Scanning,
    /// The walk is done; the index is being written.
    Indexing,
    /// Built: the completion beat plays, then the welcome.
    Ready,
}

/// Everything the loading scene draws from.
pub struct LoadingScene<'a> {
    /// Seconds since the scene opened.
    pub t: f32,
    pub phase: LoadingPhase,
    /// Seconds since the walk finished, while indexing or after.
    pub indexing_for: Option<f32>,
    /// Progress through the completion beat, 0..1.
    pub outro: f32,
    /// Entries found so far, already eased for display.
    pub count: u64,
    /// Real names from the walk; the stock names stand in until some arrive.
    pub names: &'a [SharedString],
    /// Tips cycled under the scene: keys (if any) and what they do.
    pub tips: &'a [(Option<String>, &'static str)],
    /// Offer to carry on while the index finishes.
    pub offer_continue: bool,
}

/// How long the completion beat runs before the welcome takes over.
pub const LOADING_OUTRO: f32 = 1.15;
/// The scene shows at least this long, so a fast build still reads as a
/// moment rather than a flash.
pub const LOADING_MIN: f32 = 2.4;
/// Seconds each tip stays up.
const TIP_EVERY: f32 = 5.;
/// File-name lanes spiralling into the core.
const LANES: usize = 22;
/// Diameter of the glowing core.
const CORE: f32 = 96.;
/// Core slot size; effects paint far outside it.
const CORE_SLOT: f32 = 300.;

/// The lane's name for its `cycle`-th flight: real names when there are
/// some, so a user watches their own disk pour in.
fn lane_name(names: &[SharedString], lane: usize, cycle: u64) -> SharedString {
    let pick = lane.wrapping_mul(7) as u64 + cycle.wrapping_mul(13);
    if names.is_empty() {
        NAMES[pick as usize % NAMES.len()].into()
    } else {
        names[pick as usize % names.len()].clone()
    }
}

/// The loading centrepiece: file names spiral in from beyond the window
/// edge and are absorbed into a breathing core, each landing with a small
/// ripple, while satellites circle three orbits (faster once the walk is
/// done and the index is being written). On completion the core flares,
/// shockwaves roll out and everything clears for the welcome.
pub fn loading_core(
    theme: &Theme,
    viewport: gpui::Size<gpui::Pixels>,
    scene: &LoadingScene,
) -> Div {
    let t = scene.t;
    let c = CORE_SLOT / 2.;
    let reach = f32::from(viewport.width).max(f32::from(viewport.height)) * 0.62;
    let sky = fixed::brand_sky();
    let deep = fixed::brand_deep();
    let outro = scene.outro;
    // Names stop arriving once the walk is done.
    let streaming = scene
        .indexing_for
        .map_or(1., |since| 1. - quint(beat(since, 0., 0.8)))
        * quint(beat(t, 0., 0.8));
    let clear = 1. - quint(beat(outro, 0.25, 0.85));
    let mut slot = div()
        .relative()
        .flex_none()
        .size(px(CORE_SLOT))
        .opacity(1. - quint(beat(outro, 0.7, 1.)));

    // Halo behind everything, breathing.
    let breathe = 0.5 + 0.5 * (t * 1.9).sin();
    for (edge, alpha) in [(420., 0.045), (300., 0.065), (200., 0.09)] {
        let edge = edge * (0.94 + 0.06 * breathe) * (1. + 0.5 * quint(beat(outro, 0., 0.5)));
        slot = slot.child(round(c, c, edge).bg(fixed::with_alpha(sky, alpha)));
    }

    // Orbits and their satellites.
    let spin = if scene.indexing_for.is_some() {
        2.6
    } else {
        1.
    };
    for (ring, (edge, speed, dots)) in [(150., 0.55, 2), (212., -0.38, 3), (276., 0.24, 4)]
        .into_iter()
        .enumerate()
    {
        slot = slot.child(
            round(c, c, edge)
                .border_1()
                .border_color(fixed::with_alpha(sky, 0.16 * clear)),
        );
        for dot in 0..dots {
            let a =
                t * speed * spin + dot as f32 * std::f32::consts::TAU / dots as f32 + ring as f32;
            let (x, y) = (c + a.cos() * edge / 2., c + a.sin() * edge / 2.);
            let size = 5. - ring as f32;
            slot = slot.child(round(x, y, size).bg(fixed::with_alpha(sky, 0.85 * clear)));
        }
    }

    // Names spiralling in; each lane reuses its slot on a fixed period.
    for lane in 0..LANES {
        let period = 2.4 + (lane % 5) as f32 * 0.33;
        let local = (t + lane as f32 * 0.41) / period;
        let cycle = local.floor() as u64;
        let p = local.fract();
        // Ripple where the previous flight landed.
        if cycle > 0 && p < 0.14 {
            let q = quint(p / 0.14);
            slot = slot.child(
                round(c, c, CORE + 70. * q)
                    .border_1()
                    .border_color(sky)
                    .opacity(0.35 * (1. - q) * streaming * clear),
            );
        }
        let a = angle(lane) + cycle as f32 * 0.9 + p * 1.3;
        let d = CORE / 2. + (reach - CORE / 2.) * (1. - p).powf(1.7);
        let fade_in = (p / 0.18).min(1.);
        let absorb = beat(p, 0.8, 1.);
        let opacity = fade_in * (1. - absorb) * streaming * clear;
        if opacity <= 0.01 {
            continue;
        }
        slot = slot.child(
            chip(
                theme,
                lane_name(scene.names, lane, cycle),
                c + a.cos() * d,
                c + a.sin() * d * 0.66,
                opacity,
            )
            .text_size(px(12. - 3. * absorb)),
        );
    }

    // Completion: a flare and three shockwaves.
    let flare = quint(beat(outro, 0., 0.55));
    if flare > 0. && flare < 1. {
        slot = slot.child(
            round(c, c, CORE + 520. * flare)
                .bg(gradient(
                    180.,
                    fixed::with_alpha(sky, 0.5),
                    fixed::with_alpha(deep, 0.),
                ))
                .opacity(1. - flare),
        );
    }
    for wave in 0..3 {
        let at = 0.05 + wave as f32 * 0.12;
        let p = quint(beat(outro, at, at + 0.6));
        if p <= 0. || p >= 1. {
            continue;
        }
        slot = slot.child(
            round(c, c, CORE + 360. * p)
                .border_2()
                .border_color(sky)
                .opacity(0.8 * (1. - p)),
        );
    }

    // The core: a brand disc that pulses, beats faster while indexing and
    // pops on completion, with the logo riding on it.
    let beat_rate = if scene.indexing_for.is_some() {
        5.5
    } else {
        2.4
    };
    let pulse = 1. + 0.035 * (t * beat_rate).sin() + 0.18 * back_out(beat(outro, 0., 0.35)).min(1.)
        - 0.18 * quint(beat(outro, 0.35, 0.7));
    let edge = CORE * pulse;
    slot = slot.child(
        round(c, c, edge)
            .bg(gradient(160., sky, deep))
            .border_1()
            .border_color(white(0.25)),
    );
    let logo_w = 58. * pulse;
    let logo_h = logo_w / LOGO_ASPECT;
    let mut logo = div()
        .absolute()
        .left(px(c - logo_w / 2.))
        .top(px(c - logo_h / 2.))
        .w(px(logo_w))
        .h(px(logo_h));
    for part in [LogoLayer::Back, LogoLayer::Front, LogoLayer::Mark] {
        logo = logo.child(layer(part, 0., 0., logo_w, logo_h, 1.));
    }
    slot.child(logo)
}

/// The live count and what is happening, with an indeterminate sweep that
/// fills on completion.
pub fn loading_status(theme: &Theme, scene: &LoadingScene) -> Div {
    let dots = ".".repeat((scene.t * 2.5) as usize % 4);
    let caption: SharedString = match scene.phase {
        LoadingPhase::Starting => format!("Starting the indexer{dots}").into(),
        LoadingPhase::Scanning => format!("Reading your disk{dots}").into(),
        LoadingPhase::Indexing => format!("Building your index{dots}").into(),
        LoadingPhase::Ready => "Ready.".into(),
    };
    let done = scene.phase == LoadingPhase::Ready;
    let track = 260.;
    let sweep = band(fixed::brand_sky(), 90., 3.)
        .absolute()
        .top_0()
        .left(px(-90. + (track + 180.) * ((scene.t / 1.5).fract())));
    let bar = div()
        .relative()
        .w(px(track))
        .h(px(3.))
        .rounded_full()
        .overflow_hidden()
        .bg(fixed::with_alpha(theme.border, 0.7))
        .child(if done {
            div()
                .absolute()
                .top_0()
                .left_0()
                .h_full()
                .w(px(track * quint(beat(scene.outro, 0., 0.3))))
                .bg(theme.success)
                .into_any_element()
        } else {
            sweep.into_any_element()
        });
    div()
        .flex()
        .flex_col()
        .items_center()
        .gap_2()
        .child(
            div()
                .flex()
                .items_baseline()
                .gap_2()
                .child(
                    div()
                        .text_size(px(44.))
                        .line_height(px(52.))
                        .font_weight(gpui::FontWeight::EXTRA_BOLD)
                        .text_color(if done { theme.success } else { theme.text })
                        .child(with_commas(scene.count)),
                )
                .child(div().text_sm().text_color(theme.text_dim).child("files")),
        )
        .child(
            div()
                .h(px(20.))
                .text_sm()
                .text_color(theme.text_dim)
                .child(caption),
        )
        .child(div().pt_2().child(bar))
}

/// One tip at a time under the scene, cross-fading every few seconds.
pub fn loading_tip(theme: &Theme, scene: &LoadingScene) -> Div {
    let slot = div().h(px(30.)).flex().items_center().justify_center();
    if scene.tips.is_empty() {
        return slot;
    }
    let local = (scene.t - 1.2).max(0.) / TIP_EVERY;
    let (keys, what) = &scene.tips[local as usize % scene.tips.len()];
    let p = local.fract() * TIP_EVERY;
    let shown = quint(beat(p, 0., 0.5)) * (1. - beat(p, TIP_EVERY - 0.4, TIP_EVERY));
    let shown = shown * quint(beat(scene.t, 1.2, 1.7)) * (1. - beat(scene.outro, 0., 0.3));
    slot.child(
        div()
            .flex()
            .items_center()
            .gap_2()
            .opacity(shown)
            .mt(px(6. * (1. - shown)))
            .text_sm()
            .text_color(theme.text_dim)
            .child(
                div()
                    .text_xs()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(fixed::brand_sky())
                    .child("TIP"),
            )
            .children(
                keys.clone()
                    .map(|keys| keycap(theme, keys).h(px(22.)).px_1p5().text_xs()),
            )
            .child(*what),
    )
}

// ---- Shared step pieces ----------------------------------------------------

/// The step's headline.
pub fn hero(theme: &Theme, text: impl Into<SharedString>) -> Div {
    div()
        .w_full()
        .text_size(px(40.))
        .line_height(px(48.))
        .font_weight(gpui::FontWeight::EXTRA_BOLD)
        .text_color(theme.text)
        .child(text.into())
}

/// The sentence under a headline.
pub fn lede(theme: &Theme, text: impl Into<SharedString>) -> Div {
    div()
        .w_full()
        .text_base()
        .text_color(theme.text_dim)
        .child(text.into())
}

/// Small dim line (the demo's latency readout).
pub fn caption(theme: &Theme, text: impl Into<SharedString>) -> Div {
    div()
        .h(px(18.))
        .text_sm()
        .text_color(theme.text_dim)
        .child(text.into())
}

/// A labelled control row on the theme step.
pub fn field(theme: &Theme, label: &'static str, control: AnyElement) -> Div {
    div()
        .flex()
        .items_center()
        .gap_4()
        .child(
            div()
                .w(px(72.))
                .flex_none()
                .text_sm()
                .text_color(theme.text_dim)
                .child(label),
        )
        .child(control)
}

/// A static slice of a file list, painted from the live theme so the
/// theme step previews every pick as it is made.
pub fn sample_rows(theme: &Theme) -> Div {
    let rows = [
        ("Projects", true, false),
        ("Holiday photos", true, true),
        ("Quarterly report.pdf", false, false),
    ];
    super::card(theme)
        .p_1()
        .children(rows.into_iter().map(|(name, folder, selected)| {
            div()
                .flex()
                .items_center()
                .gap_2()
                .px_2()
                .h(px(theme.row_height))
                .rounded_md()
                .text_sm()
                .when(selected, |row| row.bg(theme.selected))
                .child(
                    ui_icon(
                        if folder {
                            "icons/folder.svg"
                        } else {
                            "icons/file-text.svg"
                        },
                        if folder { theme.accent } else { theme.text_dim },
                    )
                    .size(px(theme.icon_size.min(18.))),
                )
                .child(name)
        }))
}

/// A checkbox row: name over its full path. The whole row toggles.
pub fn check_row(
    theme: &Theme,
    id: impl Into<ElementId>,
    name: impl Into<SharedString>,
    path: impl Into<SharedString>,
    on: bool,
) -> Stateful<Div> {
    let hover = theme.hover;
    let tick = div()
        .size(px(18.))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(5.))
        .border_1()
        .border_color(if on { theme.accent } else { theme.border })
        .when(on, |tick| {
            tick.bg(theme.accent)
                .child(ui_icon("icons/check.svg", theme.on_accent).size(px(12.)))
        });
    div()
        .id(id)
        .flex()
        .items_center()
        .gap_3()
        .px_3()
        .py_2()
        .rounded_lg()
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
        .child(tick)
        .child(ui_icon("icons/star.svg", theme.accent).size(px(14.)))
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .child(div().w_full().text_sm().truncate().child(name.into()))
                .child(
                    div()
                        .w_full()
                        .text_xs()
                        .text_color(theme.text_dim)
                        .truncate()
                        .child(path.into()),
                ),
        )
}

/// The big index counter. While `live`, the caption pulses so a number
/// that is still climbing reads as work in progress.
pub fn counter(theme: &Theme, value: String, caption: impl Into<SharedString>, live: bool) -> Div {
    let caption = div()
        .text_sm()
        .text_color(if live { theme.text_dim } else { theme.success })
        .child(caption.into());
    div()
        .flex()
        .items_baseline()
        .gap_3()
        .child(
            div()
                .text_size(px(48.))
                .line_height(px(56.))
                .font_weight(gpui::FontWeight::EXTRA_BOLD)
                .text_color(theme.accent)
                .child(value),
        )
        .child(if live {
            caption
                .with_animation(
                    "onboarding-counter",
                    Animation::new(Duration::from_millis(1200))
                        .repeat()
                        .with_easing(gpui::pulsating_between(0.4, 1.)),
                    |caption, delta| caption.opacity(delta),
                )
                .into_any_element()
        } else {
            caption.into_any_element()
        })
}

/// One demo search hit: icon, name, dim path.
pub fn result_row(
    theme: &Theme,
    ix: usize,
    name: SharedString,
    path: SharedString,
    is_dir: bool,
) -> Stateful<Div> {
    div()
        .id(("onboarding-result", ix))
        .flex()
        .flex_none()
        .items_center()
        .gap_3()
        .h(px(40.))
        .px_3()
        .rounded_lg()
        .when(ix == 0, |row| row.bg(theme.selected))
        .child(
            ui_icon(
                if is_dir {
                    "icons/folder.svg"
                } else {
                    "icons/file.svg"
                },
                if is_dir { theme.accent } else { theme.text_dim },
            )
            .size(px(16.)),
        )
        .child(
            div()
                .flex_none()
                .max_w(px(220.))
                .text_sm()
                .truncate()
                .child(name),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_xs()
                .text_color(theme.text_dim)
                .truncate()
                .child(path),
        )
}

/// A keycap and what it does, sliding in 90 ms after the one before.
pub fn shortcut_tip(theme: &Theme, keys: String, what: &'static str, ix: usize, t: f32) -> Div {
    let at = 0.1 + 0.09 * ix as f32;
    let p = quint(beat(t, at, at + 0.5));
    div()
        .flex()
        .items_center()
        .gap_4()
        .opacity(p)
        .ml(px(-16. * (1. - p)))
        .child(
            div()
                .w(px(96.))
                .flex()
                .flex_none()
                .justify_end()
                .child(keycap(theme, keys).h(px(26.)).px_2().text_sm()),
        )
        .child(div().text_base().child(what))
}

// ---- Magic mode demo -------------------------------------------------------

/// The command the demo types. A test runs it through the real parser so
/// the demo never shows syntax Magic mode doesn't accept.
pub const MAGIC_COMMAND: &str = "move pdfs older than 30 days to Archive";
const MAGIC_CHIPS: [&str; 4] = ["Move", "PDFs", "Older than 30 days", "→ Archive"];
const MAGIC_FILES: [&str; 4] = [
    "Invoice_0193.pdf",
    "Bank statement August.pdf",
    "Lease agreement 2025.pdf",
    "Flight tickets BLR–DEL.pdf",
];

/// The Magic command box: violet-bordered, with the sparkles mark. Callers
/// add the text (typed demo or the live input).
pub fn magic_box(theme: &Theme) -> Div {
    let violet = fixed::magic_violet();
    div()
        .flex()
        .items_center()
        .gap_2()
        .w_full()
        .h(px(44.))
        .px_3()
        .rounded(px(10.))
        .border_1()
        .border_color(violet)
        .bg(theme.panel)
        .text_base()
        .child(ui_icon("icons/sparkles.svg", violet).size(px(16.)))
}

/// A scripted Magic mode run, played once: the command types out, its
/// parsed parts pop up as chips, the plan cascades in, and Apply glows
/// until the step hands over ([`MAGIC_HANDOVER`]). Nothing here touches
/// files; `t` is the step clock.
pub fn magic_demo(theme: &Theme, t: f32) -> Div {
    let violet = fixed::magic_violet();
    let typed = (MAGIC_COMMAND.len() as f32 * beat(t, 0.3, 2.6)) as usize;
    let caret_on = ((t * 2.5) as u32).is_multiple_of(2);

    let command = magic_box(theme)
        .child(MAGIC_COMMAND[..typed].to_string())
        .child(
            div()
                .w(px(1.5))
                .h(px(18.))
                .bg(violet)
                .opacity(if caret_on { 1. } else { 0. }),
        );

    let chips = div()
        .flex()
        .gap(px(6.))
        .h(px(24.))
        .children(MAGIC_CHIPS.iter().enumerate().map(|(ix, label)| {
            let at = 2.7 + ix as f32 * 0.12;
            let p = back_out(beat(t, at, at + 0.4));
            div()
                .flex()
                .items_center()
                .px(px(10.))
                .h(px(22.))
                .rounded_full()
                .border_1()
                .border_color(fixed::with_alpha(violet, 0.4))
                .bg(fixed::with_alpha(violet, 0.16))
                .text_xs()
                .text_color(theme.text)
                .opacity((p * 2.).min(1.))
                .mt(px(8. * (1. - p)))
                .child(*label)
        }));

    let plan = super::card(theme).p_1().h(px(4. * 30. + 10.)).children(
        MAGIC_FILES.iter().enumerate().map(|(ix, name)| {
            let at = 3.3 + ix as f32 * 0.12;
            let p = quint(beat(t, at, at + 0.5));
            div()
                .flex()
                .items_center()
                .gap_3()
                .h(px(30.))
                .px_3()
                .rounded_md()
                .opacity(p)
                .ml(px(-16. * (1. - p)))
                .child(ui_icon("icons/file-text.svg", theme.text_dim).size(px(14.)))
                .child(div().flex_1().min_w_0().text_sm().truncate().child(*name))
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.text_dim)
                        .child("~/Downloads"),
                )
                .child(div().text_sm().text_color(violet).child("→"))
                .child(div().text_sm().child("Archive"))
        }),
    );

    let glow = if t > 4.9 {
        0.5 + 0.5 * ((t - 4.9) * 4.).sin()
    } else {
        0.
    };
    let apply = div()
        .flex()
        .items_center()
        .justify_between()
        .opacity(quint(beat(t, 4.4, 4.9)))
        .child(
            div()
                .text_xs()
                .text_color(theme.text_dim)
                .child("4 files · nothing moves until you confirm"),
        )
        .child(
            div()
                .relative()
                .child(
                    div()
                        .absolute()
                        .top(px(-4.))
                        .left(px(-4.))
                        .right(px(-4.))
                        .bottom(px(-4.))
                        .rounded_lg()
                        .bg(fixed::with_alpha(violet, 0.15 + 0.3 * glow)),
                )
                .child(
                    div()
                        .relative()
                        .px_3()
                        .h(px(30.))
                        .flex()
                        .items_center()
                        .rounded_lg()
                        .bg(violet)
                        .text_sm()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(rgb(0xffffff))
                        .child("Apply"),
                ),
        );

    div()
        .flex()
        .flex_col()
        .gap_3()
        .child(command)
        .child(chips)
        .child(plan)
        .child(apply)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beats_window_the_scene_clock() {
        assert_eq!(beat(0., 1.0, 1.55), 0.);
        assert_eq!(beat(1.0, 1.0, 1.55), 0.);
        assert_eq!(beat(9., 1.0, 1.55), 1.);
        assert!((beat(1.275, 1.0, 1.55) - 0.5).abs() < 1e-5);
    }

    #[test]
    fn pops_overshoot_then_land_exactly() {
        assert_eq!(back_out(0.), 0.);
        assert!((back_out(1.) - 1.).abs() < 1e-6);
        let peak = (0..=100)
            .map(|i| back_out(i as f32 / 100.))
            .fold(0., f32::max);
        assert!(peak > 1.05 && peak < 1.15, "peak {peak}");
    }

    #[test]
    fn the_intro_settles_before_its_declared_length() {
        // Every one-shot beat ends by INTRO_LEN; only loops run after it.
        let last_beats = [
            1.10 + 0.28,
            0.95 + 0.15 + 0.5,
            2.1,
            1.85 + 0.75,
            2.75,
            2.8 + 0.6,
        ];
        assert!(last_beats.iter().all(|end| *end <= INTRO_LEN));
    }

    #[test]
    fn the_magic_still_shows_the_whole_plan() {
        // Reduced motion freezes the demo here: every part must be visible.
        assert_eq!(beat(MAGIC_STILL, 0.3, 2.6), 1.);
        assert_eq!(beat(MAGIC_STILL, 3.3 + 0.36, 3.8 + 0.36), 1.);
        assert_eq!(beat(MAGIC_STILL, 4.4, 4.9), 1.);
        const { assert!(MAGIC_STILL < MAGIC_HANDOVER) };
    }

    #[test]
    fn the_demo_command_is_real_magic_syntax() {
        let now = 1_791_700_000; // 2026-10-11
        let command = filex::magic::parse_with_gate(MAGIC_COMMAND, now, true)
            .expect("Magic mode no longer parses the demo command");
        // The chips and rows promise PDFs only; the real parse must agree.
        assert_eq!(command.verb, filex::magic::Verb::Move);
        assert_eq!(command.selection.text, "");
        assert!(
            command
                .selection
                .filters
                .contains(&filex::search::filter::Filter::Ext("pdf".into()))
        );
    }

    #[test]
    fn the_hand_off_ends_with_the_app_in_place() {
        assert_eq!(reveal_offset(0.), 16.);
        assert_eq!(reveal_offset(REVEAL_LEN), 0.);
        // The old screen is gone before the sheet finishes dissolving.
        assert_eq!(quint(beat(0.3, 0., 0.3)), 1.);
        const { assert!(0.3 < REVEAL_LEN) };
    }

    #[test]
    fn counts_get_thousands_separators() {
        assert_eq!(with_commas(0), "0");
        assert_eq!(with_commas(999), "999");
        assert_eq!(with_commas(1000), "1,000");
        assert_eq!(with_commas(312_481), "312,481");
        assert_eq!(with_commas(1_234_567), "1,234,567");
    }
}

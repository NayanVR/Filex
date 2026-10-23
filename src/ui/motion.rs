//! Motion: the eased transitions and smooth wheel scrolling the UI uses.
//!
//! The rule (docs/design-system.md): animate what doesn't make the user
//! wait — panels opening, dialogs settling, a wheel notch gliding — and
//! never what they're waiting *for* (search results, a folder's
//! contents, the list itself). Durations stay short so motion reads as
//! feedback, not delay.
//!
//! The curves are pure functions of elapsed time so they're unit-tested
//! without GPUI; the stateful pieces only carry a start instant and ask
//! for the next frame while they're moving.

use std::time::{Duration, Instant};

use gpui::{Pixels, ScrollHandle, ScrollWheelEvent, Window, point, px};

/// Side panels sliding open or shut.
pub const PANEL: Duration = Duration::from_millis(180);

/// Dialogs and tooltips fading in.
pub const FADE: Duration = Duration::from_millis(140);

/// How far a dialog rises while it fades in (px).
pub const DIALOG_RISE: f32 = 8.;

/// Cubic ease-out: fast start, gentle landing — the curve for anything
/// arriving on screen.
pub fn ease_out(t: f32) -> f32 {
    let t = t.clamp(0., 1.);
    1. - (1. - t).powi(3)
}

/// How open a toggling panel is, `elapsed` into a [`PANEL`]-long
/// transition towards `opening`: 0 = shut, 1 = fully open, eased. `None`
/// once the transition has finished (the caller then uses the settled
/// state and stops asking for frames).
pub fn openness(elapsed: Duration, opening: bool) -> Option<f32> {
    if elapsed >= PANEL {
        return None;
    }
    let eased = ease_out(elapsed.as_secs_f32() / PANEL.as_secs_f32());
    Some(if opening { eased } else { 1. - eased })
}

/// A panel's open/closed state plus any transition in flight.
#[derive(Default)]
pub struct Toggle {
    /// When the current transition started and which way it's going.
    moving: Option<(Instant, bool)>,
}

impl Toggle {
    /// Begin animating towards `open`.
    pub fn start(&mut self, open: bool) {
        self.moving = Some((Instant::now(), open));
    }

    /// The panel's openness this frame given its settled state `open`,
    /// requesting another frame while a transition is running.
    pub fn fraction(&mut self, open: bool, window: &mut Window) -> f32 {
        let settled = if open { 1. } else { 0. };
        let Some((started, opening)) = self.moving else {
            return settled;
        };
        match openness(started.elapsed(), opening) {
            Some(f) => {
                window.request_animation_frame();
                f
            }
            None => {
                self.moving = None;
                settled
            }
        }
    }
}

/// Rate (per second) at which a smooth scroll closes the remaining gap:
/// about 95% of a wheel notch lands within ~120ms.
const SCROLL_RATE: f32 = 25.;

/// One frame of smooth scrolling: move `current` towards `target` by the
/// frame-rate-independent fraction for `dt` seconds, snapping once within
/// half a pixel so the animation ends.
pub fn approach(current: f32, target: f32, dt: f32) -> f32 {
    let next = current + (target - current) * (1. - (-SCROLL_RATE * dt).exp());
    if (target - next).abs() < 0.5 {
        target
    } else {
        next
    }
}

/// Smooth wheel scrolling for a vertically scrolling container.
///
/// gpui applies a mouse-wheel notch as one instant jump of several lines,
/// which reads as jank on a short page. Pair this with a container that
/// is `overflow_hidden()` + `track_scroll(handle)` (so gpui's own wheel
/// handler stays out of it) and route its `on_scroll_wheel` to
/// [`SmoothScroll::wheel`]. Precise (touchpad) deltas still apply 1:1 —
/// they're already smooth, and easing them would add lag.
#[derive(Default)]
pub struct SmoothScroll {
    pub handle: ScrollHandle,
    /// Where the eased scroll is heading (gpui offsets: 0 at the top,
    /// negative going down), while a wheel glide is in flight.
    target: Option<f32>,
    last_tick: Option<Instant>,
}

impl SmoothScroll {
    /// Feed a wheel event. Returns true when the view should re-render.
    pub fn wheel(&mut self, event: &ScrollWheelEvent, line_height: Pixels) -> bool {
        let delta = f32::from(event.delta.pixel_delta(line_height).y);
        if delta == 0. {
            return false;
        }
        let max = f32::from(self.handle.max_offset().y);
        let clamp = |y: f32| y.clamp(-max, 0.);
        let current = f32::from(self.handle.offset().y);
        if event.delta.precise() {
            self.target = None;
            self.set(clamp(current + delta));
        } else {
            self.target = Some(clamp(self.target.unwrap_or(current) + delta));
            self.last_tick = Some(Instant::now());
        }
        true
    }

    /// Advance a glide in flight; call once per render.
    pub fn tick(&mut self, window: &mut Window) {
        let Some(target) = self.target else {
            return;
        };
        let now = Instant::now();
        let dt = self
            .last_tick
            .map_or(0., |last| now.duration_since(last).as_secs_f32());
        self.last_tick = Some(now);
        let next = approach(f32::from(self.handle.offset().y), target, dt);
        self.set(next);
        if next == target {
            self.target = None;
        } else {
            window.request_animation_frame();
        }
    }

    /// Jump back to the top (e.g. switching settings pages).
    pub fn reset(&mut self) {
        self.target = None;
        self.set(0.);
    }

    fn set(&self, y: f32) {
        let x = self.handle.offset().x;
        self.handle.set_offset(point(x, px(y)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ease_out_starts_fast_and_lands_on_one() {
        assert_eq!(ease_out(0.), 0.);
        assert_eq!(ease_out(1.), 1.);
        // Past the halfway mark already by a third of the way through.
        assert!(ease_out(1. / 3.) > 0.5);
        // Out-of-range input is clamped, never overshoots.
        assert_eq!(ease_out(2.), 1.);
        assert_eq!(ease_out(-1.), 0.);
    }

    #[test]
    fn openness_runs_each_way_then_settles() {
        let mid = PANEL / 2;
        let opening = openness(mid, true).unwrap();
        let closing = openness(mid, false).unwrap();
        assert!(opening > 0.5 && opening < 1.);
        assert!((opening + closing - 1.).abs() < 1e-6);
        assert_eq!(openness(Duration::ZERO, true), Some(0.));
        assert_eq!(openness(Duration::ZERO, false), Some(1.));
        assert_eq!(openness(PANEL, true), None);
    }

    #[test]
    fn approach_is_frame_rate_independent_and_terminates() {
        // Two 8ms frames land where one 16ms frame does.
        let one = approach(0., -100., 0.016);
        let two = approach(approach(0., -100., 0.008), -100., 0.008);
        assert!((one - two).abs() < 0.01);
        // Repeated frames reach the target exactly (no endless creep).
        let mut y = 0.;
        for _ in 0..200 {
            y = approach(y, -120., 0.016);
        }
        assert_eq!(y, -120.);
        // A zero-length frame doesn't move.
        assert_eq!(approach(-40., -100., 0.), -40.);
    }
}

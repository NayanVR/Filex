//! Mac-style overlay scrollbar for `uniform_list`.
//!
//! GPUI ships scroll *behavior* (`UniformListScrollHandle`) but paints no
//! scrollbar of its own. This module fills that gap with a thin, rounded,
//! auto-hiding overlay thumb — the macOS look — that plugs into any
//! `uniform_list` through its decoration hook:
//!
//! ```ignore
//! uniform_list("id", len, render_fn)
//!     .track_scroll(handle.clone())
//!     .with_decoration(scrollbar(handle, state, &theme))
//! ```
//!
//! It reads its geometry straight from the decoration callback (viewport
//! bounds, item height, item count) rather than re-measuring, and writes
//! back through the scroll handle only while the thumb is dragged. So it
//! adds no per-row work and no layout pass of its own — a hard requirement
//! for the search-as-you-type / big-folder latency budget.

use std::cell::Cell;
use std::ops::Range;
use std::rc::Rc;

use gpui::{
    AnyElement, App, Bounds, DispatchPhase, Element, ElementId, GlobalElementId, Hitbox,
    HitboxBehavior, Hsla, InspectorElementId, IntoElement, LayoutId, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, Point, Position, ScrollHandle, Style,
    UniformListDecoration, UniformListScrollHandle, Window, fill, point, px, relative, size,
};

use super::theme::Theme;

/// Thumb width when idle.
const IDLE_WIDTH: f32 = 6.;
/// Thumb width when the pointer is over the track (macOS widens on hover).
const HOVER_WIDTH: f32 = 10.;
/// Gap between the thumb and the right edge of the list.
const MARGIN: f32 = 3.;
/// Inset at the top and bottom of the track so the thumb never touches the corners.
const PAD: f32 = 2.;
/// Shortest the thumb is allowed to get on very long lists.
const MIN_THUMB: f32 = 28.;
/// Width of the interactive strip on the right edge (hover + click-to-page).
const TRACK_WIDTH: f32 = 14.;

/// Per-list drag/hover state that must survive across frames. Cheap to
/// clone (an `Rc`), so the workspace holds one per scrollable list and
/// hands a clone to the decoration on every render.
#[derive(Clone, Default)]
pub struct ScrollbarState(Rc<Cell<Inner>>);

#[derive(Clone, Copy, Default)]
struct Inner {
    dragging: bool,
    /// Distance from the thumb's top edge to the mouse when the drag began,
    /// so the thumb doesn't jump under the cursor on grab.
    grab_offset: Pixels,
    /// Pointer is over the track — drives the hover-widen effect.
    hovered: bool,
}

impl ScrollbarState {
    pub fn new() -> Self {
        Self::default()
    }

    fn get(&self) -> Inner {
        self.0.get()
    }

    fn set(&self, inner: Inner) {
        self.0.set(inner);
    }
}

/// Attach a Mac-style scrollbar to a `uniform_list`. Pass the same
/// `UniformListScrollHandle` given to `.track_scroll()`, a persistent
/// [`ScrollbarState`], and the active theme.
pub fn scrollbar(
    handle: UniformListScrollHandle,
    state: ScrollbarState,
    theme: &Theme,
) -> ScrollbarDecoration {
    let base: Hsla = theme.text_dim.into();
    let idle = Hsla { a: 0.30, ..base };
    let hover = Hsla { a: 0.55, ..base };
    ScrollbarDecoration {
        handle,
        state,
        thumb: idle,
        thumb_hover: hover,
    }
}

/// The [`UniformListDecoration`] adapter — turns the geometry GPUI reports
/// each frame into a freshly-computed [`Scrollbar`] element.
pub struct ScrollbarDecoration {
    handle: UniformListScrollHandle,
    state: ScrollbarState,
    thumb: Hsla,
    thumb_hover: Hsla,
}

impl UniformListDecoration for ScrollbarDecoration {
    fn compute(
        &self,
        _visible_range: Range<usize>,
        _bounds: Bounds<Pixels>,
        scroll_offset: Point<Pixels>,
        item_height: Pixels,
        item_count: usize,
        _window: &mut Window,
        _cx: &mut App,
    ) -> AnyElement {
        Scrollbar {
            handle: self.handle.clone(),
            state: self.state.clone(),
            content: f32::from(item_height) * item_count as f32,
            // `scroll_offset.y` is <= 0 as the list scrolls down.
            scroll_top: (-f32::from(scroll_offset.y)).max(0.),
            thumb: self.thumb,
            thumb_hover: self.thumb_hover,
        }
        .into_any_element()
    }
}

/// The scrollbar element itself. Painted as an absolutely-positioned
/// overlay filling the list's viewport; only the thumb quad is drawn.
pub struct Scrollbar {
    handle: UniformListScrollHandle,
    state: ScrollbarState,
    /// Total scrollable content height in pixels.
    content: f32,
    /// Current distance scrolled from the top in pixels.
    scroll_top: f32,
    thumb: Hsla,
    thumb_hover: Hsla,
}

/// Thumb size and position along the track. Kept as a plain-`f32` struct
/// (no GPUI types) so the math can be unit-tested without a window.
#[derive(Clone, Copy, Debug, PartialEq)]
struct ThumbGeometry {
    /// Thumb length in pixels.
    height: f32,
    /// Free space the thumb's top can move within the track.
    travel: f32,
    /// The thumb's top offset from the track top, in pixels.
    offset: f32,
}

/// Resolve the thumb's height and offset from the list metrics. All inputs
/// and outputs are pixels; `track_height` is the drawable track (viewport
/// minus padding). The thumb is proportional to how much of the content
/// fits, floored at [`MIN_THUMB`] (but never taller than the track, which
/// also avoids a `clamp` panic on very short viewports).
fn thumb_geometry(
    viewport: f32,
    content: f32,
    scroll_top: f32,
    track_height: f32,
) -> ThumbGeometry {
    if content <= 0. || track_height <= 0. {
        return ThumbGeometry {
            height: track_height.max(0.),
            travel: 0.,
            offset: 0.,
        };
    }
    let min_thumb = MIN_THUMB.min(track_height);
    let height = (viewport / content * track_height).clamp(min_thumb, track_height);
    let travel = (track_height - height).max(0.);
    let max_scroll = (content - viewport).max(0.);
    let ratio = if max_scroll > 0. {
        (scroll_top / max_scroll).clamp(0., 1.)
    } else {
        0.
    };
    ThumbGeometry {
        height,
        travel,
        offset: travel * ratio,
    }
}

/// Thumb geometry resolved during prepaint, consumed in paint.
pub struct ScrollbarLayout {
    thumb_bounds: Bounds<Pixels>,
    thumb_hitbox: Hitbox,
    track_hitbox: Hitbox,
    /// Top of the usable track in window coordinates.
    track_top: f32,
    /// Distance the thumb's top can travel between the ends of the track.
    travel: f32,
    /// Maximum scrollable distance (content minus viewport).
    max_scroll: f32,
    active: bool,
}

impl IntoElement for Scrollbar {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for Scrollbar {
    type RequestLayoutState = ();
    type PrepaintState = Option<ScrollbarLayout>;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        // Fill the list's viewport without disturbing its layout.
        let mut style = Style::default();
        style.position = Position::Absolute;
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        window: &mut Window,
        _cx: &mut App,
    ) -> Option<ScrollbarLayout> {
        let viewport = f32::from(bounds.size.height);
        // Nothing to scroll — hide the bar entirely (macOS does the same).
        if self.content <= viewport + 0.5 || viewport <= 0. {
            return None;
        }

        let inner = self.state.get();
        let active = inner.dragging || inner.hovered;
        let width = if active { HOVER_WIDTH } else { IDLE_WIDTH };

        let track_top = f32::from(bounds.origin.y) + PAD;
        let track_height = (viewport - 2. * PAD).max(0.);
        let geo = thumb_geometry(viewport, self.content, self.scroll_top, track_height);
        let (thumb_height, travel, thumb_offset) = (geo.height, geo.travel, geo.offset);
        let max_scroll = (self.content - viewport).max(0.);

        let thumb_top = track_top + thumb_offset;
        let thumb_left = f32::from(bounds.origin.x) + f32::from(bounds.size.width) - width - MARGIN;
        let thumb_bounds = Bounds {
            origin: point(px(thumb_left), px(thumb_top)),
            size: size(px(width), px(thumb_height)),
        };

        // The full-height strip on the right captures hover and click-to-page.
        let track_bounds = Bounds {
            origin: point(
                px(f32::from(bounds.origin.x) + f32::from(bounds.size.width) - TRACK_WIDTH),
                bounds.origin.y,
            ),
            size: size(px(TRACK_WIDTH), bounds.size.height),
        };

        let thumb_hitbox = window.insert_hitbox(thumb_bounds, HitboxBehavior::Normal);
        let track_hitbox = window.insert_hitbox(track_bounds, HitboxBehavior::Normal);

        Some(ScrollbarLayout {
            thumb_bounds,
            thumb_hitbox,
            track_hitbox,
            track_top,
            travel,
            max_scroll,
            active,
        })
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        prepaint: &mut Option<ScrollbarLayout>,
        window: &mut Window,
        _cx: &mut App,
    ) {
        let Some(layout) = prepaint.take() else {
            return;
        };

        let color = if layout.active {
            self.thumb_hover
        } else {
            self.thumb
        };
        let radius = f32::from(layout.thumb_bounds.size.width) / 2.;
        window.paint_quad(fill(layout.thumb_bounds, color).corner_radii(px(radius)));

        // The base `ScrollHandle` is what actually moves the list; setting
        // its offset here re-runs the list's layout on the next frame.
        let base: ScrollHandle = self.handle.0.borrow().base_handle.clone();
        let state = self.state.clone();
        let thumb_hitbox = layout.thumb_hitbox.clone();
        let track_hitbox = layout.track_hitbox.clone();
        let thumb_top = layout.thumb_bounds.origin.y;
        let thumb_height = f32::from(layout.thumb_bounds.size.height);
        let ScrollbarLayout {
            track_top,
            travel,
            max_scroll,
            ..
        } = layout;

        // Convert a desired thumb-top (window coords) into a scroll offset
        // and apply it, preserving any horizontal offset.
        let scroll_to = {
            let base = base.clone();
            move |thumb_top_px: f32, window: &mut Window| {
                let ratio = if travel > 0. {
                    ((thumb_top_px - track_top) / travel).clamp(0., 1.)
                } else {
                    0.
                };
                let x = base.offset().x;
                base.set_offset(point(x, px(-(ratio * max_scroll))));
                window.refresh();
            }
        };

        // Grab the thumb, or page toward a click on the empty track.
        window.on_mouse_event({
            let state = state.clone();
            let scroll_to = scroll_to.clone();
            let track_hitbox = track_hitbox.clone();
            move |event: &MouseDownEvent,
                  phase: DispatchPhase,
                  window: &mut Window,
                  _cx: &mut App| {
                if phase != DispatchPhase::Bubble || event.button != MouseButton::Left {
                    return;
                }
                if thumb_hitbox.is_hovered(window) {
                    let mut inner = state.get();
                    inner.dragging = true;
                    inner.grab_offset = event.position.y - thumb_top;
                    state.set(inner);
                    window.refresh();
                } else if track_hitbox.is_hovered(window) {
                    // Center the thumb on the click.
                    scroll_to(f32::from(event.position.y) - thumb_height / 2., window);
                }
            }
        });

        // Track hover (widen) and drag-follow.
        window.on_mouse_event({
            let state = state.clone();
            let scroll_to = scroll_to.clone();
            move |event: &MouseMoveEvent,
                  phase: DispatchPhase,
                  window: &mut Window,
                  _cx: &mut App| {
                if phase != DispatchPhase::Bubble {
                    return;
                }
                let mut inner = state.get();
                let mut dirty = false;

                let hovered = track_hitbox.is_hovered(window);
                if hovered != inner.hovered {
                    inner.hovered = hovered;
                    dirty = true;
                }

                if inner.dragging {
                    if event.pressed_button == Some(MouseButton::Left) {
                        let thumb_top_px = f32::from(event.position.y - inner.grab_offset);
                        scroll_to(thumb_top_px, window);
                    } else {
                        // Button released off-thumb — end the drag.
                        inner.dragging = false;
                        dirty = true;
                    }
                }

                if dirty {
                    state.set(inner);
                    window.refresh();
                }
            }
        });

        // Release ends the drag.
        window.on_mouse_event({
            let state = state.clone();
            move |_event: &MouseUpEvent,
                  phase: DispatchPhase,
                  window: &mut Window,
                  _cx: &mut App| {
                if phase != DispatchPhase::Bubble {
                    return;
                }
                let mut inner = state.get();
                if inner.dragging {
                    inner.dragging = false;
                    state.set(inner);
                    window.refresh();
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thumb_is_proportional_to_visible_fraction() {
        // Half the content fits → thumb is half the track.
        let g = thumb_geometry(200., 400., 0., 200.);
        assert!((g.height - 100.).abs() < 0.01, "height={}", g.height);
        assert!((g.travel - 100.).abs() < 0.01, "travel={}", g.travel);
        assert_eq!(g.offset, 0., "at top, no offset");
    }

    #[test]
    fn offset_tracks_scroll_position() {
        // content 400, viewport 200 → max_scroll 200, travel 100.
        let top = thumb_geometry(200., 400., 0., 200.);
        let mid = thumb_geometry(200., 400., 100., 200.);
        let bottom = thumb_geometry(200., 400., 200., 200.);
        assert_eq!(top.offset, 0.);
        assert!((mid.offset - 50.).abs() < 0.01, "mid={}", mid.offset);
        assert!(
            (bottom.offset - 100.).abs() < 0.01,
            "bottom={}",
            bottom.offset
        );
    }

    #[test]
    fn scroll_beyond_bounds_is_clamped() {
        // Over-scroll should pin the thumb to the end, not run off the track.
        let g = thumb_geometry(200., 400., 10_000., 200.);
        assert!((g.offset - g.travel).abs() < 0.01);
    }

    #[test]
    fn thumb_never_shrinks_below_minimum() {
        // A huge list would give a sub-pixel thumb; it's floored at MIN_THUMB.
        let g = thumb_geometry(200., 100_000., 0., 200.);
        assert!(g.height >= MIN_THUMB, "height={}", g.height);
    }

    #[test]
    fn short_viewport_does_not_panic_and_fills_track() {
        // track shorter than MIN_THUMB must not panic on clamp; thumb fills it.
        let g = thumb_geometry(10., 40., 5., 10.);
        assert!(g.height <= 10.0 + 0.01);
        assert!(g.travel >= 0.);
    }

    #[test]
    fn degenerate_inputs_are_safe() {
        let g = thumb_geometry(0., 0., 0., 0.);
        assert_eq!(g.travel, 0.);
        assert_eq!(g.offset, 0.);
    }
}

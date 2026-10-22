//! The Space-bar preview window, drawn by GPUI: toolbar, navigation,
//! decoded images and the no-preview card. Formats Filex can't draw itself
//! go to a [`NativeSurface`], a platform preview handler that paints into a
//! transparent region this view leaves for it.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    Bounds, Context, Corners, EventEmitter, FocusHandle, KeyDownEvent, Pixels, RenderImage, Rgba,
    SharedString, Task, Window, canvas, div, point, prelude::*, px, size,
};

use filex::listing::{FileKind, format_size};

use crate::ui::button::{self, IconSize, Size, Variant};
use crate::ui::icon;
use crate::ui::theme::{ActiveTheme, Theme};

pub const TOOLBAR_HEIGHT: f32 = 44.;

/// A region of the preview window in physical pixels, relative to its
/// client area.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SurfaceRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl SurfaceRect {
    /// Snap logical `bounds` to whole device pixels; `None` when empty.
    pub fn from_bounds(bounds: Bounds<Pixels>, scale: f32) -> Option<Self> {
        let left = (f32::from(bounds.origin.x) * scale).round() as i32;
        let top = (f32::from(bounds.origin.y) * scale).round() as i32;
        let right = (f32::from(bounds.origin.x + bounds.size.width) * scale).round() as i32;
        let bottom = (f32::from(bounds.origin.y + bounds.size.height) * scale).round() as i32;
        (right > left && bottom > top && left >= 0 && top >= 0).then_some(Self {
            x: left,
            y: top,
            width: right - left,
            height: bottom - top,
        })
    }
}

/// A platform preview handler drawing into part of this window. Calls only
/// queue work; none may block the UI thread.
pub trait NativeSurface {
    /// Load `path` at `rect`; the returned id comes back in
    /// [`PreviewWindow::native_ready`].
    fn present(&self, path: PathBuf, rect: SurfaceRect, background: Rgba) -> u64;
    fn resize(&self, rect: SurfaceRect);
    /// Unload and hide whatever the surface shows.
    fn hide(&self);
    /// Take keyboard focus back from the handler, after a click on this
    /// view's own chrome.
    fn return_focus(&self);
}

/// What the viewer asks of whoever opened it.
#[derive(Clone, Debug, PartialEq)]
pub enum ViewerEvent {
    Close,
    Open(PathBuf),
}

/// The files being previewed and which one is showing.
#[derive(Clone, Debug, PartialEq)]
pub struct Selection {
    paths: Vec<PathBuf>,
    index: usize,
}

impl Selection {
    pub fn new(paths: Vec<PathBuf>, selected: usize) -> Option<Self> {
        let index = selected.min(paths.len().checked_sub(1)?);
        Some(Self { paths, index })
    }
    pub fn current(&self) -> &Path {
        &self.paths[self.index]
    }
    pub fn has_previous(&self) -> bool {
        self.index > 0
    }
    pub fn has_next(&self) -> bool {
        self.index + 1 < self.paths.len()
    }
    pub fn is_multiple(&self) -> bool {
        self.paths.len() > 1
    }
    /// Moves one item; `false` (and no change) at either end.
    pub fn step(&mut self, forward: bool) -> bool {
        let moved = if forward {
            self.has_next()
        } else {
            self.has_previous()
        };
        if moved {
            self.index = if forward {
                self.index + 1
            } else {
                self.index - 1
            };
        }
        moved
    }
    /// "2 of 5" for a multi-selection, nothing for a single file.
    pub fn position(&self) -> Option<String> {
        self.is_multiple()
            .then(|| format!("{} of {}", self.index + 1, self.paths.len()))
    }
}

/// How a file gets drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    /// Decoded by Filex's own bounded image decoder.
    Image,
    /// Handed to the platform's preview handler.
    Native,
    Unavailable,
}

pub fn route(path: &Path, has_native: bool) -> Route {
    if filex::platform_preview::policy::rust_image(path) {
        Route::Image
    } else if has_native {
        Route::Native
    } else {
        Route::Unavailable
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
}

enum Content {
    Loading,
    Image(Arc<RenderImage>),
    /// The native surface shows the file once `ready`; until then this view
    /// paints over it.
    Native {
        ready: bool,
    },
    Unavailable,
}

/// Size line for the toolbar and fallback card, read off the UI thread.
#[derive(Clone)]
struct Facts {
    detail: SharedString,
}

/// Bridges layout and the native surface: presentation waits for the first
/// laid-out bounds, and later bounds changes become resizes.
struct NativeSlot {
    surface: Rc<dyn NativeSurface>,
    state: RefCell<SlotState>,
}

#[derive(Default)]
struct SlotState {
    rect: Option<SurfaceRect>,
    pending: Option<(PathBuf, Rgba)>,
    request: u64,
}

impl NativeSlot {
    fn present(&self, path: PathBuf, background: Rgba) {
        let mut state = self.state.borrow_mut();
        state.request = 0;
        match state.rect {
            Some(rect) => state.request = self.surface.present(path, rect, background),
            None => state.pending = Some((path, background)),
        }
    }
    fn layout(&self, rect: SurfaceRect) {
        let mut state = self.state.borrow_mut();
        let moved = state.rect != Some(rect);
        state.rect = Some(rect);
        if let Some((path, background)) = state.pending.take() {
            state.request = self.surface.present(path, rect, background);
        } else if moved {
            self.surface.resize(rect);
        }
    }
    fn hide(&self) {
        let mut state = self.state.borrow_mut();
        state.pending = None;
        state.request = 0;
        self.surface.hide();
    }
    fn is_current(&self, request: u64) -> bool {
        request != 0 && self.state.borrow().request == request
    }
}

pub struct PreviewWindow {
    focus: FocusHandle,
    selection: Option<Selection>,
    content: Content,
    facts: Option<Facts>,
    native: Option<NativeSlot>,
    /// Dropping a task cancels it, so a decode for a file the user has
    /// already moved past stops spending CPU.
    loading: Option<Task<()>>,
    reading: Option<Task<()>>,
}

impl EventEmitter<ViewerEvent> for PreviewWindow {}

impl PreviewWindow {
    pub fn new(surface: Option<Rc<dyn NativeSurface>>, cx: &mut Context<Self>) -> Self {
        // The workspace swaps the theme global; repaint with it.
        cx.observe_global::<Theme>(|_, cx| cx.notify()).detach();
        Self {
            focus: cx.focus_handle(),
            selection: None,
            content: Content::Loading,
            facts: None,
            native: surface.map(|surface| NativeSlot {
                surface,
                state: RefCell::default(),
            }),
            loading: None,
            reading: None,
        }
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    /// Show `paths`, starting at `selected`. Re-showing the same file keeps
    /// what is already loaded.
    pub fn set_selection(
        &mut self,
        paths: Vec<PathBuf>,
        selected: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(selection) = Selection::new(paths, selected) else {
            cx.emit(ViewerEvent::Close);
            return;
        };
        let same_file = self
            .selection
            .as_ref()
            .is_some_and(|old| old.current() == selection.current());
        self.selection = Some(selection);
        if same_file && !matches!(self.content, Content::Unavailable) {
            cx.notify();
        } else {
            self.load(window, cx);
        }
    }

    /// Drop the shown file and anything still loading, ready for reuse.
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.selection = None;
        self.content = Content::Loading;
        self.facts = None;
        self.loading = None;
        self.reading = None;
        if let Some(native) = &self.native {
            native.hide();
        }
        cx.notify();
    }

    /// The native surface finished request `request`.
    pub fn native_ready(&mut self, request: u64, shown: bool, cx: &mut Context<Self>) {
        let current = self.native.as_ref().is_some_and(|n| n.is_current(request));
        if current && matches!(self.content, Content::Native { ready: false }) {
            self.content = if shown {
                Content::Native { ready: true }
            } else {
                Content::Unavailable
            };
            cx.notify();
        }
    }

    /// The native helper stopped; whatever it was loading can't be shown.
    pub fn native_failed(&mut self, cx: &mut Context<Self>) {
        if let Some(native) = &self.native {
            native.hide();
        }
        if matches!(self.content, Content::Native { .. }) {
            self.content = Content::Unavailable;
            cx.notify();
        }
    }

    pub fn step(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.selection.as_mut().is_some_and(|s| s.step(forward)) {
            self.load(window, cx);
        }
    }

    pub fn open(&mut self, cx: &mut Context<Self>) {
        if let Some(selection) = &self.selection {
            cx.emit(ViewerEvent::Open(selection.current().to_path_buf()));
        }
    }

    fn load(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.selection.as_ref().map(|s| s.current().to_path_buf()) else {
            return;
        };
        window.set_window_title(&format!("{} — Filex", file_name(&path)));
        self.loading = None;
        self.facts = None;
        self.read_facts(path.clone(), cx);
        match route(&path, self.native.is_some()) {
            Route::Image => {
                self.hide_native();
                self.content = Content::Loading;
                self.decode(path, cx);
            }
            Route::Native => {
                self.content = Content::Native { ready: false };
                if let Some(native) = &self.native {
                    native.present(path, cx.theme().bg);
                }
            }
            Route::Unavailable => {
                self.hide_native();
                self.content = Content::Unavailable;
            }
        }
        cx.notify();
    }

    fn hide_native(&self) {
        if let Some(native) = &self.native {
            native.hide();
        }
    }

    fn decode(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let decoding = cx
            .background_executor()
            .spawn(async move { crate::thumbnails::decode_preview(&path) });
        self.loading = Some(cx.spawn(async move |this, cx| {
            let decoded = decoding.await;
            let _ = this.update(cx, |this, cx| {
                this.content = match decoded {
                    Ok(image) => Content::Image(image),
                    Err(_) => Content::Unavailable,
                };
                cx.notify();
            });
        }));
    }

    fn read_facts(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let reading = cx.background_executor().spawn(async move {
            match std::fs::metadata(&path) {
                Ok(meta) if meta.is_dir() => "Folder".to_string(),
                Ok(meta) => format_size(meta.len()),
                Err(_) => "File is unavailable".to_string(),
            }
        });
        self.reading = Some(cx.spawn(async move |this, cx| {
            let detail = reading.await;
            let _ = this.update(cx, |this, cx| {
                this.facts = Some(Facts {
                    detail: detail.into(),
                });
                cx.notify();
            });
        }));
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        if keystroke.modifiers.modified() {
            return;
        }
        match keystroke.key.as_str() {
            "escape" | "space" => cx.emit(ViewerEvent::Close),
            "left" => self.step(false, window, cx),
            "right" => self.step(true, window, cx),
            "enter" => self.open(cx),
            _ => return,
        }
        cx.stop_propagation();
    }

    fn render_toolbar(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let selection = self.selection.as_ref();
        let name = selection
            .map(|s| file_name(s.current()))
            .unwrap_or_default();
        let mut meta: Vec<String> = Vec::new();
        if let Some(position) = selection.and_then(Selection::position) {
            meta.push(position);
        }
        if let Some(facts) = &self.facts {
            meta.push(facts.detail.to_string());
        }
        let mut bar = div()
            .flex()
            .flex_none()
            .items_center()
            .gap_1()
            .h(px(TOOLBAR_HEIGHT))
            .px_2()
            .bg(theme.panel)
            .border_b_1()
            .border_color(theme.border);
        if selection.is_some_and(Selection::is_multiple) {
            bar = bar
                .child(self.nav_button(theme, false, cx))
                .child(self.nav_button(theme, true, cx));
        }
        bar.child(
            div()
                .flex()
                .flex_1()
                .min_w_0()
                .items_baseline()
                .gap_2()
                .px_2()
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_sm()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .child(name),
                )
                .child(
                    div()
                        .flex_none()
                        .text_xs()
                        .text_color(theme.text_dim)
                        .child(meta.join("  ·  ")),
                ),
        )
        .child(
            button::button(
                theme,
                "preview-open",
                "Open",
                Variant::Secondary,
                Size::Regular,
            )
            .on_click(cx.listener(|this, _, _, cx| this.open(cx))),
        )
    }

    fn nav_button(&self, theme: &Theme, forward: bool, cx: &mut Context<Self>) -> gpui::AnyElement {
        let enabled = self.selection.as_ref().is_some_and(|s| {
            if forward {
                s.has_next()
            } else {
                s.has_previous()
            }
        });
        let (id, glyph) = if forward {
            ("preview-next", "icons/chevron-right.svg")
        } else {
            ("preview-previous", "icons/chevron-left.svg")
        };
        let color = if enabled { theme.text } else { theme.text_dim };
        let button = button::icon_button(theme, id, glyph, color, IconSize::Large);
        if enabled {
            button
                .on_click(cx.listener(move |this, _, window, cx| this.step(forward, window, cx)))
                .into_any_element()
        } else {
            button.opacity(0.4).cursor_default().into_any_element()
        }
    }

    fn render_body(&self, theme: &Theme, cx: &mut Context<Self>) -> gpui::AnyElement {
        let body = div().relative().flex_1().min_h_0().w_full();
        match &self.content {
            Content::Loading => body.bg(theme.bg).child(spinner(theme)).into_any_element(),
            Content::Image(image) => body
                .bg(theme.bg)
                .child(
                    div()
                        .absolute()
                        .inset_0()
                        .p_6()
                        .child(image_canvas(image.clone())),
                )
                .into_any_element(),
            Content::Native { ready } => {
                // Nothing painted where the surface sits: GPUI composites
                // above it, so a transparent region reveals it.
                let body = body.child(self.surface_tracker(cx));
                if *ready {
                    body.into_any_element()
                } else {
                    body.child(
                        div()
                            .absolute()
                            .inset_0()
                            .bg(theme.bg)
                            .child(spinner(theme)),
                    )
                    .into_any_element()
                }
            }
            Content::Unavailable => body
                .bg(theme.bg)
                .child(self.unavailable_card(theme, cx))
                .into_any_element(),
        }
    }

    /// Reports the native region's laid-out bounds to the surface each frame.
    fn surface_tracker(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.entity().downgrade();
        canvas(
            |bounds, _, _| bounds,
            move |_, bounds, window, cx| {
                let scale = window.scale_factor();
                if let Some(rect) = SurfaceRect::from_bounds(bounds, scale)
                    && let Some(this) = this.upgrade()
                {
                    // Read-only access: the slot's own RefCell takes the update.
                    if let Some(native) = &this.read(cx).native {
                        native.layout(rect);
                    }
                }
            },
        )
        .absolute()
        .inset_0()
    }

    fn unavailable_card(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let name = self
            .selection
            .as_ref()
            .map(|s| file_name(s.current()))
            .unwrap_or_default();
        let kind = FileKind::of(&name, false);
        let detail = self
            .facts
            .as_ref()
            .map(|f| f.detail.clone())
            .unwrap_or_default();
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_3()
            .p_6()
            .child(icon::file_icon(theme, kind, &name, 96.))
            .child(
                div()
                    .max_w(px(420.))
                    .text_center()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .child(name),
            )
            .child(div().text_sm().text_color(theme.text_dim).child(detail))
            .child(
                div()
                    .text_sm()
                    .text_color(theme.text_dim)
                    .child("No preview available for this file."),
            )
            .child(
                button::button(
                    theme,
                    "preview-open-default",
                    "Open in default app",
                    Variant::Primary,
                    Size::Regular,
                )
                .mt_2()
                .on_click(cx.listener(|this, _, _, cx| this.open(cx))),
            )
    }
}

/// Where an image of `image` device pixels sits in `bounds`: at its own
/// size (one image pixel per device pixel), shrunk to fit if larger, never
/// enlarged, centred.
pub fn fit_image(bounds: Bounds<Pixels>, image: (u32, u32), scale: f32) -> Bounds<Pixels> {
    let natural = (image.0 as f32 / scale, image.1 as f32 / scale);
    let room = (f32::from(bounds.size.width), f32::from(bounds.size.height));
    let shrink = (room.0 / natural.0).min(room.1 / natural.1).min(1.);
    let (width, height) = (natural.0 * shrink, natural.1 * shrink);
    Bounds {
        origin: point(
            bounds.origin.x + px((room.0 - width) / 2.),
            bounds.origin.y + px((room.1 - height) / 2.),
        ),
        size: size(px(width), px(height)),
    }
}

/// Paints `image` fitted into the element's own bounds. A canvas rather
/// than `img()`: gpui's image element sets an aspect ratio on its style,
/// which lets layout derive its height from its width and overflow.
fn image_canvas(image: Arc<RenderImage>) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            let pixels = image.size(0);
            if pixels.width.0 <= 0 || pixels.height.0 <= 0 {
                return;
            }
            let dimensions = (pixels.width.0 as u32, pixels.height.0 as u32);
            let fitted = fit_image(bounds, dimensions, window.scale_factor());
            // A failed atlas upload leaves the area blank; nothing to report.
            let _ = window.paint_image(bounds, fitted, Corners::default(), image, 0, false);
        },
    )
    .size_full()
}

fn spinner(theme: &Theme) -> impl IntoElement {
    div()
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        .child(icon::spinner(
            "icons/loader-circle.svg",
            theme.text_dim,
            20.,
            "preview-spinner",
        ))
}

impl Render for PreviewWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *cx.theme();
        // No root background: the native region must stay transparent.
        div()
            .size_full()
            .flex()
            .flex_col()
            .font_family(crate::ui::fonts::UI_FONT_FAMILY)
            .text_color(theme.text)
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key_down))
            // Clicks on GPUI-drawn parts never reach the handler's window,
            // but its keyboard focus would otherwise outlive them.
            .capture_any_mouse_down(cx.listener(|this, _, _, _| {
                if let Some(native) = &this.native {
                    native.surface.return_focus();
                }
            }))
            .child(self.render_toolbar(&theme, cx))
            .child(self.render_body(&theme, cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, size};

    fn paths(n: usize) -> Vec<PathBuf> {
        (0..n).map(|i| PathBuf::from(format!("{i}.txt"))).collect()
    }

    #[test]
    fn selection_clamps_steps_and_labels_position() {
        assert!(Selection::new(Vec::new(), 0).is_none());
        let mut selection = Selection::new(paths(3), 9).unwrap();
        assert_eq!(selection.current(), Path::new("2.txt"));
        assert!(!selection.step(true), "stepping past the end is a no-op");
        assert!(selection.step(false));
        assert_eq!(selection.position().as_deref(), Some("2 of 3"));
        assert!(selection.step(false));
        assert!(!selection.has_previous() && !selection.step(false));
        let single = Selection::new(paths(1), 0).unwrap();
        assert!(!single.is_multiple());
        assert_eq!(single.position(), None);
    }

    #[test]
    fn images_decode_in_filex_and_other_files_need_a_native_handler() {
        assert_eq!(route(Path::new("photo.JPG"), true), Route::Image);
        assert_eq!(route(Path::new("photo.webp"), false), Route::Image);
        assert_eq!(route(Path::new("report.pdf"), true), Route::Native);
        assert_eq!(route(Path::new("report.pdf"), false), Route::Unavailable);
        assert_eq!(route(Path::new("README"), true), Route::Native);
    }

    #[test]
    fn images_shrink_to_fit_but_never_enlarge() {
        let bounds = Bounds {
            origin: point(px(24.), px(68.)),
            size: size(px(800.), px(500.)),
        };
        // Portrait, taller than the room: height-limited, centred across.
        let tall = fit_image(bounds, (1024, 1536), 1.);
        assert_eq!(tall.size.height, px(500.));
        assert!((f32::from(tall.size.width) - 333.33).abs() < 0.01);
        assert_eq!(tall.origin.y, px(68.));
        assert!(tall.origin.x > bounds.origin.x);
        // Small: device-pixel size at 125%, centred, not enlarged.
        let small = fit_image(bounds, (100, 50), 1.25);
        assert_eq!(small.size, size(px(80.), px(40.)));
        assert_eq!(small.origin, point(px(24. + 360.), px(68. + 230.)));
    }

    #[test]
    fn surface_rect_snaps_to_device_pixels_and_rejects_empty_bounds() {
        let bounds = Bounds {
            origin: point(px(0.), px(TOOLBAR_HEIGHT)),
            size: size(px(800.), px(555.5)),
        };
        assert_eq!(
            SurfaceRect::from_bounds(bounds, 1.25),
            Some(SurfaceRect {
                x: 0,
                y: 55,
                width: 1000,
                height: 694,
            })
        );
        let empty = Bounds {
            origin: point(px(0.), px(44.)),
            size: size(px(800.), px(0.)),
        };
        assert_eq!(SurfaceRect::from_bounds(empty, 2.), None);
    }

    struct Recorder(RefCell<Vec<String>>);
    impl NativeSurface for Recorder {
        fn present(&self, path: PathBuf, rect: SurfaceRect, _: Rgba) -> u64 {
            let mut log = self.0.borrow_mut();
            log.push(format!("present {} {}", path.display(), rect.width));
            log.len() as u64
        }
        fn resize(&self, rect: SurfaceRect) {
            self.0.borrow_mut().push(format!("resize {}", rect.width));
        }
        fn hide(&self) {
            self.0.borrow_mut().push("hide".into());
        }
        fn return_focus(&self) {}
    }

    #[test]
    fn native_presentation_waits_for_layout_and_ignores_stale_requests() {
        let recorder = Rc::new(Recorder(RefCell::default()));
        let slot = NativeSlot {
            surface: recorder.clone(),
            state: RefCell::default(),
        };
        let rect = |width| SurfaceRect {
            x: 0,
            y: 55,
            width,
            height: 400,
        };
        slot.present("a.pdf".into(), Rgba::default());
        assert!(recorder.0.borrow().is_empty(), "no bounds yet");
        slot.layout(rect(600));
        slot.layout(rect(600)); // unchanged bounds: no resize
        slot.layout(rect(700));
        assert_eq!(*recorder.0.borrow(), ["present a.pdf 600", "resize 700"]);
        assert!(slot.is_current(1));
        slot.present("b.pdf".into(), Rgba::default());
        assert!(!slot.is_current(1), "earlier file's completion is stale");
        assert!(slot.is_current(3));
        slot.hide();
        assert!(!slot.is_current(3), "nothing is current once hidden");
        assert!(!slot.is_current(0));
    }
}

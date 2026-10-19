//! Windows Space-bar viewer: a GPUI [`PreviewWindow`] owned by the Filex
//! window. Images are decoded in-process; other formats are drawn by the
//! isolated preview-handler helper (`filex::platform_preview`) inside a
//! region the view leaves transparent.
use super::preview_window::{NativeSurface, PreviewWindow, SurfaceRect, ViewerEvent};
use filex::platform_preview::{Key, Preview, PreviewEvent, Rect};
use futures::StreamExt;
use gpui::{
    App, AppContext as _, AsyncApp, Bounds, DisplayId, Rgba, TitlebarOptions, Window,
    WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowOptions, px, size,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::{
    cell::{Cell, RefCell},
    path::PathBuf,
    rc::Rc,
};
use windows::Win32::{
    Foundation::HWND,
    UI::WindowsAndMessaging::{
        GWL_EXSTYLE, GWLP_HWNDPARENT, GetWindowLongPtrW, SW_HIDE, SW_SHOW, SetForegroundWindow,
        SetWindowLongPtrW, ShowWindow, WS_EX_APPWINDOW,
    },
};

/// The helper's surface, parented to the preview window once it exists.
struct HandlerSurface {
    preview: Preview,
    parent: Cell<isize>,
}

impl NativeSurface for HandlerSurface {
    fn present(&self, path: PathBuf, rect: SurfaceRect, background: Rgba) -> u64 {
        tracing::debug!(?rect, "preview handler: present");
        self.preview.present(
            path,
            self.parent.get(),
            helper_rect(rect),
            colorref(background),
        )
    }
    fn resize(&self, rect: SurfaceRect) {
        tracing::debug!(?rect, "preview handler: resize");
        self.preview.resize(helper_rect(rect));
    }
    fn hide(&self) {
        self.preview.hide();
    }
    fn return_focus(&self) {
        self.preview.return_focus();
    }
}

fn helper_rect(rect: SurfaceRect) -> Rect {
    Rect {
        x: rect.x,
        y: rect.y,
        width: rect.width,
        height: rect.height,
    }
}

/// GDI's 0x00BBGGRR for an opaque theme colour.
fn colorref(color: Rgba) -> u32 {
    let channel = |value: f32| (value.clamp(0., 1.) * 255.).round() as u32;
    channel(color.red) | channel(color.green) << 8 | channel(color.blue) << 16
}

fn hwnd_of(window: &Window) -> anyhow::Result<isize> {
    match HasWindowHandle::window_handle(window)
        .map_err(|error| anyhow::anyhow!("No native window handle: {error}"))?
        .as_raw()
    {
        RawWindowHandle::Win32(handle) => Ok(handle.hwnd.get()),
        _ => anyhow::bail!("Windows preview requires a Windows window"),
    }
}

enum Request {
    Show(Vec<PathBuf>, usize),
    Close,
    /// The workspace is going: remove the window, so the app can quit.
    Destroy,
}

struct Opened {
    handle: WindowHandle<PreviewWindow>,
    hwnd: isize,
}

struct Inner {
    owner: isize,
    /// The Filex window's display, where the preview opens centred.
    display: Option<DisplayId>,
    surface: Rc<HandlerSurface>,
    app: AsyncApp,
    opened: RefCell<Option<Opened>>,
    visible: Cell<bool>,
    pending: RefCell<Option<Request>>,
    scheduled: Cell<bool>,
    on_error: Box<dyn Fn(anyhow::Error)>,
}

/// Owns the preview window and its helper. Every native window call runs
/// in a foreground task, outside GPUI updates: showing, hiding and owning
/// windows sends synchronous messages that GPUI would otherwise drop.
pub struct Viewer {
    inner: Rc<Inner>,
}

impl Viewer {
    pub fn new(
        window: &Window,
        cx: &mut App,
        on_error: impl Fn(anyhow::Error) + 'static,
    ) -> anyhow::Result<Self> {
        let owner = hwnd_of(window)?;
        let (preview, mut events) = Preview::new()?;
        let display = window.display(cx).map(|display| display.id());
        let inner = Rc::new(Inner {
            owner,
            display,
            surface: Rc::new(HandlerSurface {
                preview,
                parent: Cell::new(0),
            }),
            app: cx.to_async(),
            opened: RefCell::default(),
            visible: Cell::new(false),
            pending: RefCell::default(),
            scheduled: Cell::new(false),
            on_error: Box::new(on_error),
        });
        // Pipe/watchdog callbacks never borrow GPUI. Deliver helper events
        // in a foreground task, after the input/render update has returned.
        let weak = Rc::downgrade(&inner);
        cx.spawn(async move |cx| {
            while let Some(event) = events.next().await {
                let Some(inner) = weak.upgrade() else { break };
                inner.native_event(event, cx);
            }
        })
        .detach();
        Ok(Self { inner })
    }

    pub fn is_visible(&self) -> bool {
        self.inner.visible.get()
    }

    pub fn show(&self, paths: Vec<PathBuf>, selected: usize) {
        Inner::enqueue(&self.inner, Request::Show(paths, selected));
    }

    pub fn update(&self, paths: Vec<PathBuf>, selected: usize) {
        if self.is_visible() {
            if paths.is_empty() {
                self.close();
            } else {
                self.show(paths, selected);
            }
        }
    }

    pub fn close(&self) {
        Inner::enqueue(&self.inner, Request::Close);
    }
}

impl Drop for Viewer {
    fn drop(&mut self) {
        // A hidden preview window would otherwise keep the app running
        // after the Filex window closes.
        Inner::enqueue(&self.inner, Request::Destroy);
    }
}

impl Inner {
    /// Coalesces requests: only the latest selection or close applies.
    fn enqueue(this: &Rc<Self>, request: Request) {
        this.visible.set(matches!(request, Request::Show(..)));
        *this.pending.borrow_mut() = Some(request);
        if this.scheduled.replace(true) {
            return;
        }
        let this = this.clone();
        let app = this.app.clone();
        app.spawn(async move |cx| {
            this.scheduled.set(false);
            match this.pending.take() {
                Some(Request::Show(paths, selected)) => this.apply_show(paths, selected, cx),
                Some(Request::Close) => this.apply_close(cx),
                Some(Request::Destroy) => this.apply_destroy(cx),
                None => {}
            }
        })
        .detach();
    }

    fn apply_show(self: &Rc<Self>, paths: Vec<PathBuf>, selected: usize, cx: &mut AsyncApp) {
        let fresh = self.opened.borrow().is_none();
        if fresh {
            match self.open_window(cx) {
                Ok(opened) => {
                    adopt(opened.hwnd, self.owner);
                    self.surface.parent.set(opened.hwnd);
                    *self.opened.borrow_mut() = Some(opened);
                }
                Err(error) => {
                    self.visible.set(false);
                    (self.on_error)(error);
                    return;
                }
            }
        }
        let Some((handle, hwnd)) = self.opened.borrow().as_ref().map(|o| (o.handle, o.hwnd)) else {
            return;
        };
        if !fresh {
            // Hidden by `apply_close`. GPUI's activation restores focus but
            // not visibility; a fresh window gets both from its first
            // activation, which also applies the bounds it opened with.
            unsafe {
                let _ = ShowWindow(HWND(hwnd as *mut _), SW_SHOW);
            }
        }
        let updated = handle.update(cx, |view, window, cx| {
            view.set_selection(paths, selected, window, cx);
            window.focus(view.focus_handle(), cx);
            window.activate_window();
        });
        if updated.is_err() {
            // The window is gone (closed with the app); start over next time.
            self.opened.borrow_mut().take();
            self.visible.set(false);
        }
    }

    fn apply_close(&self, cx: &mut AsyncApp) {
        let Some((handle, hwnd)) = self.opened.borrow().as_ref().map(|o| (o.handle, o.hwnd)) else {
            return;
        };
        unsafe {
            let _ = ShowWindow(HWND(hwnd as *mut _), SW_HIDE);
            let _ = SetForegroundWindow(HWND(self.owner as *mut _));
        }
        let _ = handle.update(cx, |view, _, cx| view.clear(cx));
        // Stop the helper; the next native file starts a fresh one.
        self.surface.preview.close();
    }

    fn apply_destroy(&self, cx: &mut AsyncApp) {
        // Stop the helper first: its surface is a child of this window, and
        // destroying a parent waits on children owned by other threads.
        self.surface.preview.close();
        if let Some(opened) = self.opened.borrow_mut().take() {
            let _ = opened
                .handle
                .update(cx, |_, window, _| window.remove_window());
        }
    }

    fn open_window(self: &Rc<Self>, cx: &mut AsyncApp) -> anyhow::Result<Opened> {
        let weak = Rc::downgrade(self);
        let surface: Rc<dyn NativeSurface> = self.surface.clone();
        let handle = cx.update(|cx| {
            let bounds = Bounds::centered(self.display, size(px(960.), px(720.)), cx);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    window_min_size: Some(size(px(420.), px(300.))),
                    titlebar: Some(TitlebarOptions {
                        title: Some("Filex Preview".into()),
                        ..Default::default()
                    }),
                    // Shown once it is owned by the Filex window, so it
                    // never flashes a taskbar button.
                    show: false,
                    focus: true,
                    // Cleared to transparent, so the handler's region shows.
                    window_background: WindowBackgroundAppearance::Transparent,
                    app_id: Some("dev.filex.app".into()),
                    ..Default::default()
                },
                |window, cx| {
                    let view = cx.new(|cx| PreviewWindow::new(Some(surface), cx));
                    let events = weak.clone();
                    cx.subscribe(&view, move |_, event: &ViewerEvent, _| {
                        if let Some(inner) = events.upgrade() {
                            inner.viewer_event(event);
                        }
                    })
                    .detach();
                    let closing = weak.clone();
                    window.on_window_should_close(cx, move |_, _| {
                        // The close box hides; the window is reused.
                        if let Some(inner) = closing.upgrade() {
                            Inner::enqueue(&inner, Request::Close);
                        }
                        false
                    });
                    view
                },
            )
        })?;
        let hwnd = handle.update(cx, |_, window, _| hwnd_of(window))??;
        Ok(Opened { handle, hwnd })
    }

    fn viewer_event(self: &Rc<Self>, event: &ViewerEvent) {
        match event {
            ViewerEvent::Close => Inner::enqueue(self, Request::Close),
            ViewerEvent::Open(path) => {
                if let Err(error) = crate::workspace::open_with_default_app(path) {
                    (self.on_error)(anyhow::anyhow!("Could not open default app: {error}"));
                }
            }
        }
    }

    fn native_event(self: &Rc<Self>, event: PreviewEvent, cx: &mut AsyncApp) {
        tracing::debug!(?event, "preview handler: event");
        let Some(handle) = self.opened.borrow().as_ref().map(|o| o.handle) else {
            return;
        };
        match event {
            PreviewEvent::Ready { native, request_id } => {
                let _ = handle.update(cx, |view, _, cx| view.native_ready(request_id, native, cx));
            }
            PreviewEvent::Key(Key::Close) => Inner::enqueue(self, Request::Close),
            PreviewEvent::Key(key) => {
                let forward = key == Key::Next;
                let _ = handle.update(cx, |view, window, cx| view.step(forward, window, cx));
            }
            PreviewEvent::Error(error) => {
                let _ = handle.update(cx, |view, _, cx| view.native_failed(cx));
                if self.visible.get() {
                    (self.on_error)(anyhow::Error::msg(error));
                }
            }
        }
    }
}

/// Make `hwnd` an owned window of the Filex window: it stays above it,
/// minimises with it, and has no taskbar button of its own. Call while
/// `hwnd` is hidden; the taskbar only rereads `WS_EX_APPWINDOW` on show.
fn adopt(hwnd: isize, owner: isize) {
    unsafe {
        let hwnd = HWND(hwnd as *mut _);
        SetWindowLongPtrW(hwnd, GWLP_HWNDPARENT, owner);
        let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style & !(WS_EX_APPWINDOW.0 as isize));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_colours_become_gdi_colorrefs() {
        assert_eq!(colorref(gpui::rgb(0xff8000)), 0x0000_80ff);
        assert_eq!(colorref(gpui::rgb(0x0000ff)), 0x00ff_0000);
    }
}

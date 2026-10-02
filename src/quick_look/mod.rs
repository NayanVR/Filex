//! Native, on-demand full-file previews, separate from the thumbnail cache.
//! macOS uses Quick Look. Windows preview-handler hosting and Linux desktop
//! preview services can implement the same interface; no shortcut is exposed
//! on those platforms until a full viewer is available.

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos::Viewer as NativeViewer;

#[cfg(not(target_os = "macos"))]
mod unsupported {
    use std::path::PathBuf;

    pub struct Viewer;
    impl Viewer {
        pub fn new(_: &gpui::Window) -> anyhow::Result<Self> {
            anyhow::bail!("Quick Look is not available on this platform yet")
        }
        pub fn is_visible(&self) -> bool {
            false
        }
        pub fn show(&self, _: Vec<PathBuf>, _: usize) -> anyhow::Result<()> {
            Ok(())
        }
        pub fn update(&self, _: Vec<PathBuf>, _: usize) {}
        pub fn close(&self) {}
    }
}
#[cfg(not(target_os = "macos"))]
use unsupported::Viewer as NativeViewer;

use std::path::PathBuf;
use std::rc::Rc;

mod driver;
use driver::{Backend, Driver, Request};

impl Backend for NativeViewer {
    fn is_visible(&self) -> bool {
        self.is_visible()
    }

    fn present(&self, paths: Vec<PathBuf>, selected: usize) -> anyhow::Result<()> {
        if self.is_visible() {
            self.update(paths, selected);
            Ok(())
        } else {
            self.show(paths, selected)
        }
    }

    fn close(&self) {
        self.close();
    }
}

struct State {
    driver: Driver<NativeViewer>,
    on_error: Box<dyn Fn(anyhow::Error)>,
}

/// Queues native operations outside GPUI app/entity/window updates.
/// Quick Look can pump the macOS event loop synchronously, allowing other
/// foreground tasks to run. Holding a GPUI borrow during that work panics.
pub struct Viewer {
    state: Rc<State>,
    executor: gpui::ForegroundExecutor,
}

impl Viewer {
    pub fn new(
        window: &gpui::Window,
        executor: gpui::ForegroundExecutor,
        on_error: impl Fn(anyhow::Error) + 'static,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            state: Rc::new(State {
                driver: Driver::new(NativeViewer::new(window)?),
                on_error: Box::new(on_error),
            }),
            executor,
        })
    }

    pub fn is_visible(&self) -> bool {
        self.state.driver.is_visible()
    }

    pub fn show(&self, paths: Vec<PathBuf>, selected: usize) {
        self.enqueue(Request::Present(paths, selected));
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
        self.enqueue(Request::Close);
    }

    fn enqueue(&self, request: Request) {
        if !self.state.driver.submit(request) {
            return;
        }
        let state = self.state.clone();
        self.executor
            .spawn(async move {
                state.driver.drain(|error| (state.on_error)(error));
            })
            .detach();
    }
}

impl Drop for Viewer {
    fn drop(&mut self) {
        // Cancel a queued open and release the native owner outside GPUI's
        // update too. The driver's Rc keeps it alive through that cleanup.
        self.close();
    }
}

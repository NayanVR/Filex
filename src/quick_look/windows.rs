use futures::StreamExt;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::path::PathBuf;
pub struct Viewer {
    preview: filex::platform_preview::Preview,
}
impl Viewer {
    pub fn new(
        window: &gpui::Window,
        executor: gpui::ForegroundExecutor,
        on_error: impl Fn(anyhow::Error) + 'static,
    ) -> anyhow::Result<Self> {
        let owner = match HasWindowHandle::window_handle(window)
            .map_err(|error| anyhow::anyhow!("No native window handle: {error}"))?
            .as_raw()
        {
            RawWindowHandle::Win32(handle) => handle.hwnd.get(),
            _ => anyhow::bail!("Windows preview requires a Windows window"),
        };
        let (preview, mut errors) = filex::platform_preview::Preview::new(owner)?;
        // Pipe/watchdog callbacks never borrow GPUI. Deliver errors in a
        // foreground task, after the input/render update has returned.
        executor
            .spawn(async move {
                while let Some(error) = errors.next().await {
                    on_error(anyhow::Error::msg(error));
                }
            })
            .detach();
        Ok(Self { preview })
    }
    pub fn is_visible(&self) -> bool {
        self.preview.is_visible()
    }
    pub fn show(&self, paths: Vec<PathBuf>, selected: usize) {
        self.preview.show(paths, selected);
    }
    pub fn update(&self, paths: Vec<PathBuf>, selected: usize) {
        if self.is_visible() {
            self.show(paths, selected);
        }
    }
    pub fn close(&self) {
        self.preview.close();
    }
}

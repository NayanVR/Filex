//! Per-OS shell integration for the workspace: launching files with the
//! system's apps, the update affordance and manifest, and window chrome.
//! Platform-specific workspace code lives here so the rest of
//! `workspace::*` stays OS-agnostic.

use std::path::Path;

use gpui::TitlebarOptions;
#[cfg(target_os = "macos")]
use gpui::px;

#[cfg(target_os = "macos")]
use crate::ui;

/// Open a file with the platform's default application. Detached — the
/// launched app owns its own lifetime.
pub(super) fn open_with_default_app(path: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = std::process::Command::new("open");
        command.arg(path);
        command
    };
    #[cfg(target_os = "linux")]
    let mut command = {
        let mut command = std::process::Command::new("xdg-open");
        command.arg(path);
        command
    };
    #[cfg(target_os = "windows")]
    let mut command = {
        use std::os::windows::process::CommandExt as _;
        // `start` is a cmd builtin; the empty string fills the window
        // title slot so paths with spaces aren't parsed as one.
        // CREATE_NO_WINDOW stops a console flashing on every open.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let mut command = std::process::Command::new("cmd");
        command
            .args(["/C", "start", ""])
            .arg(path)
            .creation_flags(CREATE_NO_WINDOW);
        command
    };
    command.spawn().map(drop)
}

/// Whether this platform can show an OS "Open with…" chooser. macOS has no
/// CLI entry point (it needs LaunchServices), so the entry is hidden there
/// rather than offering an action that can't work.
pub(super) fn open_with_supported() -> bool {
    !cfg!(target_os = "macos")
}

/// Show the platform's native "Open with…" application chooser for
/// `path`, so the user can pick a program other than the default.
/// Detached — the chosen app owns its own lifetime.
pub(super) fn open_with_dialog(path: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt as _;
        // The Shell's classic "How do you want to open this file?" dialog.
        // CREATE_NO_WINDOW keeps rundll32 from flashing a console.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        std::process::Command::new("rundll32.exe")
            .arg("shell32.dll,OpenAs_RunDLL")
            .arg(path)
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map(drop)
    }
    #[cfg(target_os = "linux")]
    {
        // `mimeopen -d` (perl-file-mimeinfo) prompts for the application;
        // plain `xdg-open` would silently use the default instead.
        std::process::Command::new("mimeopen")
            .arg("-d")
            .arg(path)
            .spawn()
            .map(drop)
    }
    #[cfg(target_os = "macos")]
    {
        let _ = path;
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Open with… is not yet available on macOS",
        ))
    }
}

/// The update affordance for this platform's UI banner. macOS copies a
/// `brew` command; Linux opens the releases page to re-download the
/// tarball. Windows opens the release page for its per-machine MSI.
#[cfg(feature = "updater")]
pub(super) fn update_affordance() -> filex::update::UpdateAffordance {
    #[cfg(target_os = "macos")]
    {
        filex::update::UpdateAffordance::RunCommand("brew upgrade filex".to_string())
    }
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    {
        // TODO(block 5): point at the real releases URL once the repo is
        // published.
        filex::update::UpdateAffordance::OpenUrl(
            "https://github.com/NayanVR/filex/releases/latest".to_string(),
        )
    }
}
/// Manifest URL for the UI-side "is there a newer version?" check.
/// Per-OS, since each platform publishes its own manifest; the
/// `latest/download/…` path always resolves to the newest release.
#[cfg(all(target_os = "macos", feature = "updater"))]
pub(super) const UPDATE_MANIFEST_URL: &str =
    "https://github.com/NayanVR/filex/releases/latest/download/filex-macos.json";

#[cfg(all(target_os = "windows", feature = "updater"))]
pub(super) const UPDATE_MANIFEST_URL: &str =
    "https://github.com/NayanVR/filex/releases/latest/download/filex-windows.json";

#[cfg(all(target_os = "linux", feature = "updater"))]
pub(super) const UPDATE_MANIFEST_URL: &str =
    "https://github.com/NayanVR/filex/releases/latest/download/filex-linux.json";

/// Main-window titlebar. macOS: unified titlebar — the system bar goes
/// transparent and the traffic lights inset into our top bar, which pads
/// left to clear them. Elsewhere the native titlebar stays.
pub(super) fn titlebar_options() -> TitlebarOptions {
    #[cfg(target_os = "macos")]
    {
        TitlebarOptions {
            title: None,
            appears_transparent: true,
            // The tab bar is the topmost bar, so the inset traffic lights
            // are centered against its height, not the nav bar's.
            traffic_light_position: Some(gpui::point(
                px(12.),
                px((ui::tabs::TAB_BAR_HEIGHT - 12.) / 2.),
            )),
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        TitlebarOptions {
            title: Some("filex".into()),
            ..Default::default()
        }
    }
}

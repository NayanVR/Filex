//! Per-OS shell integration for the workspace: launching files with the
//! system's apps, the update affordance and manifest, and window chrome.
//! Platform-specific workspace code lives here so the rest of
//! `workspace::*` stays OS-agnostic.

use std::path::{Path, PathBuf};

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

/// Context-menu label: a submenu on macOS, the OS dialog elsewhere.
pub(super) const OPEN_WITH_LABEL: &str = if cfg!(target_os = "macos") {
    "Open With  ›"
} else {
    "Open With…"
};

/// An application that can open a file, for the in-app "Open With…" list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AppChoice {
    pub(super) name: String,
    pub(super) path: PathBuf,
}

/// Applications registered to open `path`, default app first, then by
/// name — or `None` where the OS has its own chooser dialog instead
/// (Windows, Linux: see [`open_with_dialog`]). Queries LaunchServices on
/// macOS, which can be slow on a cold cache: call it off the UI thread.
pub(super) fn open_with_choices(path: &Path) -> Option<Vec<AppChoice>> {
    #[cfg(target_os = "macos")]
    {
        Some(macos_apps_for(path))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = path;
        None
    }
}

/// Assumes LaunchServices' `URLsForApplicationsToOpenURL:` (macOS 12+)
/// already lists the default handler; it's moved to the front here.
/// NSWorkspace's lookup methods are documented thread-safe.
#[cfg(target_os = "macos")]
fn macos_apps_for(path: &Path) -> Vec<AppChoice> {
    use objc2_app_kit::NSWorkspace;
    use objc2_foundation::{NSFileManager, NSString, NSURL};

    let Some(path_str) = path.to_str() else {
        return Vec::new();
    };
    let workspace = NSWorkspace::sharedWorkspace();
    let manager = NSFileManager::defaultManager();
    let url = NSURL::fileURLWithPath(&NSString::from_str(path_str));
    let to_path = |url: &NSURL| url.path().map(|p| PathBuf::from(p.to_string()));
    let default = workspace
        .URLForApplicationToOpenURL(&url)
        .and_then(|u| to_path(&u));
    let mut apps: Vec<AppChoice> = workspace
        .URLsForApplicationsToOpenURL(&url)
        .iter()
        .filter_map(|u| to_path(&u))
        .map(|path| {
            let name = manager
                .displayNameAtPath(&NSString::from_str(&path.to_string_lossy()))
                .to_string();
            AppChoice {
                name: name.trim_end_matches(".app").to_string(),
                path,
            }
        })
        .collect();
    sort_choices(&mut apps, default.as_deref());
    apps
}

/// Default app first, the rest by name; duplicates (same app path) dropped.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn sort_choices(apps: &mut Vec<AppChoice>, default: Option<&Path>) {
    apps.sort_by(|a, b| {
        let a_default = Some(a.path.as_path()) != default;
        let b_default = Some(b.path.as_path()) != default;
        (a_default, a.name.to_lowercase()).cmp(&(b_default, b.name.to_lowercase()))
    });
    apps.dedup_by(|a, b| a.path == b.path);
}

/// Open `file` with a specific application. Detached. Only macOS builds an
/// app list ([`open_with_choices`]), so only macOS launches from one.
pub(super) fn open_with_app(file: &Path, app: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg("-a")
            .arg(app)
            .arg(file)
            .spawn()
            .map(drop)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (file, app);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "launching a chosen app is macOS-only; other platforms use the OS chooser",
        ))
    }
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
            "macOS lists apps in-app; see open_with_choices",
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

#[cfg(test)]
mod tests {
    use super::*;

    fn app(name: &str, path: &str) -> AppChoice {
        AppChoice {
            name: name.into(),
            path: PathBuf::from(path),
        }
    }

    #[test]
    fn default_app_first_then_by_name_without_duplicates() {
        let mut apps = vec![
            app("Zed", "/Applications/Zed.app"),
            app("preview", "/System/Applications/Preview.app"),
            app("TextEdit", "/System/Applications/TextEdit.app"),
            app("Zed", "/Applications/Zed.app"),
        ];
        sort_choices(&mut apps, Some(Path::new("/Applications/Zed.app")));
        let names: Vec<_> = apps.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["Zed", "preview", "TextEdit"]);
    }
}

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
pub(crate) fn open_with_default_app(path: &Path) -> std::io::Result<()> {
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

/// The sidebar's whole-machine place as (icon, label, location). Windows
/// has no single root, so it opens the This PC drives page; macOS and
/// Linux go to `/`.
pub(super) fn machine_place() -> (&'static str, &'static str, PathBuf) {
    if cfg!(windows) {
        (
            "icons/hard-drive.svg",
            filex::drives::THIS_PC_LABEL,
            filex::drives::this_pc(),
        )
    } else {
        ("icons/hard-drive.svg", "Root", PathBuf::from("/"))
    }
}

/// Last submenu entry, for apps the list leaves out: opens the OS chooser
/// ([`open_with_dialog`]). macOS has no such dialog, and Linux has no
/// desktop-neutral one (`mimeopen -d` needs a terminal), so its list
/// already holds every app registered for the type.
pub(super) const OPEN_WITH_OTHER: Option<&str> = if cfg!(target_os = "windows") {
    Some("Choose another app…")
} else {
    None
};

/// An application that can open a file, for the in-app "Open With…" list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AppChoice {
    pub(super) name: String,
    /// What [`open_with_app`] launches by: the `.app` bundle path on
    /// macOS; the Shell handler's name on Windows (an exe path, or a Store
    /// app's ID); the `.desktop` file on Linux.
    pub(super) id: PathBuf,
}

/// Applications to list for `path`, default app first, then by name — or
/// `None` to show the OS chooser instead ([`open_with_dialog`]): on Windows
/// when the Shell has nothing to suggest, on Linux when the MIME type can't
/// be resolved. Queries LaunchServices / the Shell / the XDG files, which
/// can be slow on a cold cache: call it off the UI thread.
pub(super) fn open_with_choices(path: &Path) -> Option<Vec<AppChoice>> {
    #[cfg(target_os = "macos")]
    {
        Some(macos_apps_for(path))
    }
    #[cfg(target_os = "windows")]
    {
        super::open_with_windows::apps_for(path)
    }
    #[cfg(target_os = "linux")]
    {
        super::open_with_linux::apps_for(path)
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
                id: path,
            }
        })
        .collect();
    sort_choices(&mut apps, default.as_deref());
    apps
}

/// Default app first, the rest by name; duplicates (same app id) dropped.
pub(super) fn sort_choices(apps: &mut Vec<AppChoice>, default: Option<&Path>) {
    apps.sort_by(|a, b| {
        let a_default = Some(a.id.as_path()) != default;
        let b_default = Some(b.id.as_path()) != default;
        (a_default, a.name.to_lowercase()).cmp(&(b_default, b.name.to_lowercase()))
    });
    apps.dedup_by(|a, b| a.id == b.id);
}

/// Open `file` with a chosen app from [`open_with_choices`]. Blocks while
/// Windows' Shell hands the file over (Linux re-reads the `.desktop`
/// file): call it off the UI thread.
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
    #[cfg(target_os = "windows")]
    {
        super::open_with_windows::launch(file, app)
    }
    #[cfg(target_os = "linux")]
    {
        super::open_with_linux::launch(file, app)
    }
}

/// Show the platform's native "Open with…" application chooser for
/// `path`, so the user can pick a program other than the default.
/// Detached — the chosen app owns its own lifetime.
pub(super) fn open_with_dialog(path: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "windows")]
    {
        super::open_with_windows::dialog(path)
    }
    #[cfg(target_os = "linux")]
    {
        // Only reached when `xdg-mime` couldn't type the file, so there's
        // no in-app list. `mimeopen -d` (perl-file-mimeinfo) prompts for
        // the application; plain `xdg-open` would silently use the default.
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

/// Open a terminal window whose shell starts in `dir` (context menu "Open
/// in Terminal"). Windows: Windows Terminal (`wt -d`), else a PowerShell
/// console; macOS: Terminal.app; Linux: `$TERMINAL`, then the Debian
/// alternative, then common emulators, each started *in* `dir` since every
/// one of them opens its shell in its working directory. Detached.
pub(super) fn open_terminal(dir: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
        // `wt.exe` is an app-execution alias under WindowsApps; on some
        // installs it can't be started at all (os error 1920, even from a
        // shell). Any failure falls back to a PowerShell console in the
        // folder, which Windows 11 hosts in Terminal anyway when that's
        // the default terminal app.
        let wt = std::process::Command::new("wt.exe")
            .arg("-d")
            .arg(filex::listing::display_path(dir))
            .spawn();
        match wt {
            Ok(_) => Ok(()),
            Err(err) => {
                tracing::debug!("wt.exe unavailable ({err}); opening PowerShell");
                std::process::Command::new("powershell.exe")
                    .current_dir(dir)
                    .creation_flags(CREATE_NEW_CONSOLE)
                    .spawn()
                    .map(drop)
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .args(["-a", "Terminal"])
            .arg(dir)
            .spawn()
            .map(drop)
    }
    #[cfg(target_os = "linux")]
    {
        let mut last = std::io::Error::new(std::io::ErrorKind::NotFound, "no terminal found");
        for program in terminal_candidates(std::env::var("TERMINAL").ok()) {
            match std::process::Command::new(&program)
                .current_dir(dir)
                .spawn()
            {
                Ok(_) => return Ok(()),
                Err(err) => last = err,
            }
        }
        Err(last)
    }
}

/// Linux terminal emulators to try, in order: the user's `$TERMINAL`
/// (first word only — it's a program, not a command line), Debian's
/// `x-terminal-emulator` alternative, then the desktops' defaults.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn terminal_candidates(env_terminal: Option<String>) -> Vec<String> {
    const KNOWN: &[&str] = &[
        "x-terminal-emulator",
        "gnome-terminal",
        "konsole",
        "xfce4-terminal",
        "kgx",
        "ptyxis",
        "alacritty",
        "kitty",
        "wezterm",
        "foot",
        "xterm",
    ];
    let mut out: Vec<String> = env_terminal
        .as_deref()
        .and_then(|t| t.split_whitespace().next())
        .map(str::to_owned)
        .into_iter()
        .collect();
    for known in KNOWN {
        if !out.iter().any(|p| p == known) {
            out.push((*known).to_owned());
        }
    }
    out
}

/// The window's native handle (HWND) for Shell APIs that parent a
/// dialog or sheet to it.
#[cfg(target_os = "windows")]
pub(super) fn native_window(window: &gpui::Window) -> Option<isize> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match HasWindowHandle::window_handle(window).ok()?.as_raw() {
        RawWindowHandle::Win32(handle) => Some(handle.hwnd.get()),
        _ => None,
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

/// Whether a drag leaving the window must be handed to the OS by filex
/// itself. macOS: no — gpui-ce promotes it natively from
/// `external_drag_payload`. Windows: yes, via the Shell's drag source
/// ([`start_os_drag`]). Linux: not yet (no XDND/Wayland source in gpui-ce
/// or here), so drags stay in-app.
pub(super) fn hands_off_drags() -> bool {
    cfg!(target_os = "windows")
}

/// Whether a drag's pointer has left filex for another app, so the drag
/// should become an OS file drag.
///
/// Windows: the cursor is outside filex's window, or another app's window
/// covers it there. The rectangle alone isn't enough — with filex
/// maximized (or simply underneath), the drop target sits *on top of*
/// filex, inside its rectangle. Elsewhere: outside the window's bounds.
#[cfg(target_os = "windows")]
pub(super) fn pointer_left_app(
    window: &gpui::Window,
    _position: gpui::Point<gpui::Pixels>,
) -> bool {
    native_window(window).is_some_and(super::shell_windows::pointer_over_other_app)
}

#[cfg(not(target_os = "windows"))]
pub(super) fn pointer_left_app(window: &gpui::Window, position: gpui::Point<gpui::Pixels>) -> bool {
    !gpui::Bounds::new(gpui::Point::default(), window.viewport_size()).contains(&position)
}

/// Whether the primary mouse button is still held (a drag in progress).
#[cfg(target_os = "windows")]
pub(super) fn primary_button_down() -> bool {
    super::shell_windows::primary_button_down()
}

#[cfg(not(target_os = "windows"))]
pub(super) fn primary_button_down() -> bool {
    false
}

/// Begin the OS file drag for `paths` (see [`hands_off_drags`]). Returns
/// at once: the drag starts after the calling handler has returned.
#[cfg(target_os = "windows")]
pub(super) fn start_os_drag(paths: Vec<PathBuf>) {
    super::shell_windows::drag_out_soon(paths);
}

#[cfg(not(target_os = "windows"))]
pub(super) fn start_os_drag(_paths: Vec<PathBuf>) {}

/// Whether the main window hides the system titlebar and the tab bar
/// draws the minimize/maximize/close buttons itself
/// ([`ui::window_controls`]). Windows only: gpui-ce maps our control
/// areas onto the native caption hit-tests there, so Snap Layouts and
/// drag-to-move keep working. macOS keeps its own traffic lights (inset,
/// see [`titlebar_options`]). Linux keeps server-side decorations until
/// client-side resize edges and a Wayland/X11 decoration story exist.
pub(super) fn draws_own_caption() -> bool {
    cfg!(target_os = "windows")
}

/// Main-window titlebar. macOS: unified titlebar — the system bar goes
/// transparent and the traffic lights inset into our top bar, which pads
/// left to clear them. Windows: the system bar is hidden and the tab bar
/// is the titlebar ([`draws_own_caption`]). Linux keeps the native bar.
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
    #[cfg(target_os = "windows")]
    {
        TitlebarOptions {
            // Still the taskbar / Alt-Tab title.
            title: Some("filex".into()),
            appears_transparent: true,
            ..Default::default()
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
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
            id: PathBuf::from(path),
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

    #[test]
    fn terminal_env_wins_and_is_not_repeated() {
        let list = terminal_candidates(Some("kitty --single-instance".into()));
        assert_eq!(list[0], "kitty");
        assert_eq!(list.iter().filter(|p| *p == "kitty").count(), 1);
        assert_eq!(list[1], "x-terminal-emulator");
        let list = terminal_candidates(None);
        assert_eq!(
            list.first().map(String::as_str),
            Some("x-terminal-emulator")
        );
        assert_eq!(list.last().map(String::as_str), Some("xterm"));
        // A blank $TERMINAL is ignored rather than tried as "".
        assert_eq!(
            terminal_candidates(Some("  ".into())),
            terminal_candidates(None)
        );
    }
}

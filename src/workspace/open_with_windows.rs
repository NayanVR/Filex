//! Windows "Open With": the in-app app list and its launcher, read from the
//! Shell's association handlers â€” the same source as Explorer's own
//! "Open with â€º" submenu, so classic programs and Store apps both appear â€”
//! plus the Shell's chooser dialog for everything else.
//!
//! Every call here runs on its own single-threaded-apartment thread: the
//! handlers are apartment-bound COM objects, and initialising COM on a
//! shared executor thread could collide with whatever apartment it already
//! joined. Handlers can't cross threads either, so the list carries each
//! app's handler *name* and launching looks the handler up again by it.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use windows::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize, IDataObject,
};
use windows::Win32::UI::Shell::{
    ASSOC_FILTER_RECOMMENDED, ASSOCF_INIT_IGNOREUNKNOWN, ASSOCSTR_FRIENDLYAPPNAME,
    AssocQueryStringW, BHID_DataObject, IAssocHandler, IShellItem, OAIF_ALLOW_REGISTRATION,
    OAIF_EXEC, OPENASINFO, SHAssocEnumHandlers, SHCreateItemFromParsingName, SHOpenWithDialog,
};
use windows::core::{HSTRING, PCWSTR, PWSTR};

use super::platform::{AppChoice, sort_choices};

/// Apps recommended for `path`'s extension, the default first â€” or `None`
/// (no extension, lookup failed, nothing registered) so the caller falls
/// back to the chooser dialog. Blocks while the Shell answers, which can
/// take a moment on a cold cache: call it off the UI thread.
pub(super) fn apps_for(path: &Path) -> Option<Vec<AppChoice>> {
    let ext = extension_key(path)?;
    let apps = on_sta_thread("open-with-apps", move || unsafe { recommended_apps(&ext) })
        .ok()?
        .ok()?;
    (!apps.is_empty()).then_some(apps)
}

/// Open `file` with the handler named `id` (from [`apps_for`]). Blocks
/// until the handler has been asked to launch: call it off the UI thread.
pub(super) fn launch(file: &Path, id: &Path) -> std::io::Result<()> {
    let ext = extension_key(file).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "file has no extension")
    })?;
    let (file, id) = (file.as_os_str().to_owned(), id.as_os_str().to_owned());
    let found = on_sta_thread("open-with-launch", move || unsafe {
        invoke(&ext, &file, &id)
    })??;
    if found {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "that app is no longer registered for this file type",
        ))
    }
}

/// The Shell's "How do you want to open this file?" dialog via
/// `SHOpenWithDialog` (Vista+), with `OAIF_EXEC` so the pick launches the
/// file. Not `rundll32 shell32.dll,OpenAs_RunDLL`: it takes its command
/// line raw, so the quotes `Command` adds around paths with spaces reach
/// the Shell verbatim and the dialog silently never appears. The dialog is
/// modal, so its thread is detached rather than joined.
pub(super) fn dialog(path: &Path) -> std::io::Result<()> {
    let wide = wide_path(path);
    spawn_sta("open-with-dialog", move || unsafe {
        let info = OPENASINFO {
            pcszFile: PCWSTR(wide.as_ptr()),
            pcszClass: PCWSTR::null(),
            oaifInFlags: OAIF_ALLOW_REGISTRATION | OAIF_EXEC,
        };
        // Cancelling the dialog returns an error HRESULT; nothing to do.
        let _ = SHOpenWithDialog(None, &info);
    })
    .map(drop)
}

/// Start `f` on a new thread inside a single-threaded COM apartment, torn
/// down after `f` returns â€” so `f` must drop every COM object it made.
fn spawn_sta<T: Send + 'static>(
    name: &str,
    f: impl FnOnce() -> T + Send + 'static,
) -> std::io::Result<std::thread::JoinHandle<T>> {
    std::thread::Builder::new()
        .name(name.into())
        .spawn(move || unsafe {
            let com = CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok();
            let out = f();
            if com {
                CoUninitialize();
            }
            out
        })
}

/// [`spawn_sta`], waiting for the result.
fn on_sta_thread<T: Send + 'static>(
    name: &str,
    f: impl FnOnce() -> T + Send + 'static,
) -> std::io::Result<T> {
    spawn_sta(name, f)?
        .join()
        .map_err(|_| std::io::Error::other("open-with thread panicked"))
}

/// Assumes `SHAssocEnumHandlers` (Windows 7+) with the recommended filter
/// lists what Explorer's "Open with" submenu shows, and that the default
/// handler's friendly name (`ASSOCSTR_FRIENDLYAPPNAME`) matches one
/// handler's UI name â€” when it doesn't, nothing is promoted to the top.
unsafe fn recommended_apps(ext: &str) -> windows::core::Result<Vec<AppChoice>> {
    unsafe {
        let mut apps = Vec::new();
        for_each_handler(ext, |handler| {
            let id = take_string(handler.GetName());
            let name = take_string(handler.GetUIName());
            if let (Some(id), Some(name)) = (id, name) {
                apps.push(AppChoice {
                    name,
                    id: PathBuf::from(id),
                });
            }
            false
        })?;
        let default = default_app_name(ext).and_then(|name| default_id(&apps, &name));
        sort_choices(&mut apps, default.as_deref());
        Ok(apps)
    }
}

/// Re-find the handler named `id` for `ext` and hand it `file` as a shell
/// data object, the way Explorer invokes one. `Ok(false)` if no handler by
/// that name is registered any more.
unsafe fn invoke(ext: &str, file: &OsString, id: &OsString) -> windows::core::Result<bool> {
    unsafe {
        let mut chosen = None;
        for_each_handler(ext, |handler| {
            let matches = take_string(handler.GetName()).is_some_and(|name| *id == *name);
            if matches {
                chosen = Some(handler);
            }
            matches
        })?;
        let Some(handler) = chosen else {
            return Ok(false);
        };
        let item: IShellItem = SHCreateItemFromParsingName(&HSTRING::from(file), None)?;
        let data: IDataObject = item.BindToHandler(None, &BHID_DataObject)?;
        handler.Invoke(&data)?;
        Ok(true)
    }
}

/// Visit `ext`'s recommended handlers until `visit` returns true.
unsafe fn for_each_handler(
    ext: &str,
    mut visit: impl FnMut(IAssocHandler) -> bool,
) -> windows::core::Result<()> {
    unsafe {
        let handlers = SHAssocEnumHandlers(&HSTRING::from(ext), ASSOC_FILTER_RECOMMENDED)?;
        loop {
            let mut slot = [None];
            if handlers.Next(&mut slot, None).is_err() {
                break;
            }
            let Some(handler) = slot[0].take() else { break };
            if visit(handler) {
                break;
            }
        }
        Ok(())
    }
}

/// The default app's display name for `ext`, e.g. "Photos".
unsafe fn default_app_name(ext: &str) -> Option<String> {
    unsafe {
        let mut buf = [0u16; 260];
        let mut len = buf.len() as u32;
        AssocQueryStringW(
            ASSOCF_INIT_IGNOREUNKNOWN,
            ASSOCSTR_FRIENDLYAPPNAME,
            &HSTRING::from(ext),
            PCWSTR::null(),
            Some(PWSTR(buf.as_mut_ptr())),
            &mut len,
        )
        .ok()
        .ok()?;
        // `len` counts the terminating NUL.
        let text = String::from_utf16_lossy(&buf[..(len as usize).saturating_sub(1)]);
        (!text.is_empty()).then_some(text)
    }
}

/// Take ownership of a Shell-allocated string, freeing it.
unsafe fn take_string(result: windows::core::Result<PWSTR>) -> Option<String> {
    unsafe {
        let ptr = result.ok()?;
        let text = ptr.to_string().ok();
        CoTaskMemFree(Some(ptr.0 as *const _));
        text
    }
}

/// The association key the Shell looks handlers up by: `.png`. Files
/// without an extension have no per-type handlers.
fn extension_key(path: &Path) -> Option<String> {
    let ext = path.extension()?.to_str()?;
    (!ext.is_empty()).then(|| format!(".{ext}"))
}

/// The handler whose UI name is the default app's friendly name.
fn default_id(apps: &[AppChoice], default_name: &str) -> Option<PathBuf> {
    apps.iter()
        .find(|app| app.name.eq_ignore_ascii_case(default_name))
        .map(|app| app.id.clone())
}

/// `path` as a NUL-terminated UTF-16 string, unquoted, for Win32 calls.
fn wide_path(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt as _;
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(name: &str, id: &str) -> AppChoice {
        AppChoice {
            name: name.into(),
            id: PathBuf::from(id),
        }
    }

    #[test]
    fn extension_key_is_dotted_and_absent_without_extension() {
        assert_eq!(
            extension_key(Path::new(r"C:\a\2'x3' Banner.png")).as_deref(),
            Some(".png")
        );
        assert_eq!(extension_key(Path::new(r"C:\a\Makefile")), None);
        assert_eq!(extension_key(Path::new(r"C:\a\trailing.")), None);
    }

    #[test]
    fn default_is_matched_by_display_name_ignoring_case() {
        let apps = [
            app("Paint", r"C:\Windows\system32\mspaint.exe"),
            app("Photos", "Microsoft.Windows.Photos_8wekyb3d8bbwe!App"),
        ];
        assert_eq!(
            default_id(&apps, "photos"),
            Some(PathBuf::from("Microsoft.Windows.Photos_8wekyb3d8bbwe!App"))
        );
        assert_eq!(default_id(&apps, "Notepad"), None);
    }

    /// Regression: the old `rundll32 OpenAs_RunDLL` route received the
    /// path quoted (it contains spaces) and silently showed nothing. The
    /// dialog must get the raw path, NUL-terminated, with no quotes added.
    #[test]
    fn dialog_path_is_unquoted_and_nul_terminated() {
        let wide = wide_path(Path::new(r"C:\Users\me\2'x3' Banner.png"));
        assert_eq!(wide.last(), Some(&0));
        let text = String::from_utf16(&wide[..wide.len() - 1]).unwrap();
        assert_eq!(text, r"C:\Users\me\2'x3' Banner.png");
    }

    /// Real Shell lookup: `.txt` always has a handler (Notepad) on Windows.
    /// Launching isn't exercised â€” it would open a window.
    #[test]
    fn txt_files_list_at_least_one_app() {
        let apps = apps_for(Path::new(r"C:\does-not-need-to-exist.txt"))
            .expect(".txt has recommended handlers");
        assert!(apps.iter().all(|a| !a.name.is_empty()));
    }
}

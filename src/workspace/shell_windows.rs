//! Windows Shell surfaces the context menu opens: the Properties dialog
//! and the Share sheet. macOS and Linux equivalents live in
//! `workspace::platform` (Properties shows Filex's details panel there;
//! Share is Windows-only for now — see `filex::context_menu::Capabilities`).

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use windows::ApplicationModel::DataTransfer::{DataRequestedEventArgs, DataTransferManager};
use windows::Foundation::TypedEventHandler;
use windows::Storage::{IStorageItem, StorageFile};
use windows::Win32::Foundation::{HWND, LPARAM};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    BHID_DataObject, IDataTransferManagerInterop, ILFree, IShellItemArray, SEE_MASK_FLAG_NO_UI,
    SEE_MASK_INVOKEIDLIST, SHCreateShellItemArrayFromIDLists, SHELLEXECUTEINFOW,
    SHMultiFileProperties, SHParseDisplayName, ShellExecuteExW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, EnumThreadWindows, MSG, MsgWaitForMultipleObjects, PM_REMOVE, PeekMessageW,
    QS_ALLINPUT, SW_SHOW, TranslateMessage,
};
use windows::core::{BOOL, HSTRING, Interface as _, PCWSTR};

/// Open the Shell's Properties dialog for `paths` — one item's own sheet
/// (the `properties` verb, as Explorer's menu invokes it), or the combined
/// multi-item sheet Explorer shows for a selection.
///
/// Runs on a dedicated STA thread, off the UI thread: building the sheet
/// loads property-sheet handlers and reads metadata. The Shell may create
/// the sheet's window on *this* thread rather than one of its own, so the
/// thread pumps messages until it owns no windows (after a short grace
/// period for the sheet to appear) instead of exiting straight away and
/// taking the dialog with it. `owner` (the Filex window) only places the
/// sheet; the sheet stays modeless.
pub(super) fn show_properties(owner: Option<isize>, paths: Vec<PathBuf>) {
    std::thread::spawn(move || {
        // SAFETY: COM is initialized once for this new thread, which owns
        // every interface created below.
        let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        let owner = HWND(owner.unwrap_or_default() as *mut _);
        match properties_on_this_thread(owner, &paths) {
            Ok(()) => pump_while_windows_open(),
            Err(err) => tracing::warn!("properties dialog failed: {err:#}"),
        }
    });
}

fn properties_on_this_thread(owner: HWND, paths: &[PathBuf]) -> Result<()> {
    if let [path] = paths {
        let file = HSTRING::from(path.as_os_str());
        let verb = HSTRING::from("properties");
        let mut info = SHELLEXECUTEINFOW {
            cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_INVOKEIDLIST | SEE_MASK_FLAG_NO_UI,
            hwnd: owner,
            lpVerb: PCWSTR(verb.as_ptr()),
            lpFile: PCWSTR(file.as_ptr()),
            nShow: SW_SHOW.0,
            ..Default::default()
        };
        // SAFETY: `file`/`verb` outlive the call; `info` is fully sized.
        return unsafe { ShellExecuteExW(&mut info) }.context("ShellExecuteExW(properties)");
    }
    let items = shell_items(paths)?;
    // SAFETY: plain COM calls on interfaces owned by this thread.
    unsafe {
        let data: windows::Win32::System::Com::IDataObject =
            items.BindToHandler(None, &BHID_DataObject)?;
        SHMultiFileProperties(&data, 0)?;
    }
    Ok(())
}

/// Keep this thread's message loop running while it owns any window (a
/// property sheet the Shell placed here). Gives the sheet a few seconds to
/// appear first; if none ever does (the Shell used its own thread), the
/// thread just ends.
fn pump_while_windows_open() {
    const GRACE: std::time::Duration = std::time::Duration::from_secs(5);
    let started = std::time::Instant::now();
    let mut seen = false;
    loop {
        let mut msg = MSG::default();
        // SAFETY: standard message pump for windows owned by this thread.
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        let open = thread_has_windows();
        seen |= open;
        if !open && (seen || started.elapsed() > GRACE) {
            return;
        }
        // SAFETY: no handles; wakes on input for this thread or timeout.
        unsafe { MsgWaitForMultipleObjects(None, false, 250, QS_ALLINPUT) };
    }
}

fn thread_has_windows() -> bool {
    unsafe extern "system" fn found(_: HWND, flag: LPARAM) -> BOOL {
        // SAFETY: `flag` points at the bool below for the call's duration.
        unsafe { *(flag.0 as *mut bool) = true };
        BOOL(0)
    }
    let mut any = false;
    // SAFETY: the callback only writes `any`, which outlives the call.
    unsafe {
        let _ = EnumThreadWindows(
            GetCurrentThreadId(),
            Some(found),
            LPARAM(&mut any as *mut bool as isize),
        );
    }
    any
}

/// The Shell's item array for absolute `paths` (any folders, unlike a
/// single-parent data object).
fn shell_items(paths: &[PathBuf]) -> Result<IShellItemArray> {
    let mut pidls: Vec<*mut ITEMIDLIST> = Vec::with_capacity(paths.len());
    let result = (|| {
        for path in paths {
            let mut pidl = std::ptr::null_mut();
            // SAFETY: SHParseDisplayName writes one PIDL we free below.
            unsafe {
                SHParseDisplayName(&HSTRING::from(path.as_os_str()), None, &mut pidl, 0, None)
            }
            .with_context(|| format!("parsing {}", path.display()))?;
            pidls.push(pidl);
        }
        let consts: Vec<*const ITEMIDLIST> = pidls.iter().map(|p| p.cast_const()).collect();
        // SAFETY: every PIDL is valid until freed after this call (the
        // array copies them).
        Ok(unsafe { SHCreateShellItemArrayFromIDLists(&consts)? })
    })();
    for pidl in pidls {
        // SAFETY: allocated by SHParseDisplayName above.
        unsafe { ILFree(Some(pidl.cast_const())) };
    }
    result
}

thread_local! {
    /// The DataRequested registration of the last Share, removed before
    /// the next one so old file lists never answer a new request.
    static SHARE_TOKEN: std::cell::Cell<Option<i64>> = const { std::cell::Cell::new(None) };
}

/// Show the Windows Share sheet for `paths` (files only — the sheet
/// can't share folders), owned by window `hwnd`.
///
/// Must run on the thread that owns `hwnd` (GPUI's UI thread): the
/// DataTransferManager is per-window and raises `DataRequested` there.
/// The handler takes a deferral and resolves the `StorageFile`s on a
/// worker thread, so the UI thread never waits on file I/O.
pub(super) fn share(hwnd: isize, paths: Vec<PathBuf>) -> Result<()> {
    let hwnd = HWND(hwnd as *mut _);
    let interop = windows::core::factory::<DataTransferManager, IDataTransferManagerInterop>()
        .context("share interop")?;
    // SAFETY: `hwnd` is the live GPUI window on this thread.
    let manager: DataTransferManager = unsafe { interop.GetForWindow(hwnd) }?;
    if let Some(old) = SHARE_TOKEN.take() {
        let _ = manager.RemoveDataRequested(old);
    }
    let title = share_title(&paths);
    let token = manager.DataRequested(&TypedEventHandler::<
        DataTransferManager,
        DataRequestedEventArgs,
    >::new(move |_, args| {
        let Some(args) = args.as_ref() else {
            return Ok(());
        };
        let request = args.Request()?;
        request
            .Data()?
            .Properties()?
            .SetTitle(&HSTRING::from(title.as_str()))?;
        let deferral = request.GetDeferral()?;
        let paths = paths.clone();
        std::thread::spawn(move || {
            // SAFETY: COM for this worker thread only.
            let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            let items = storage_items(&paths);
            let set = request
                .Data()
                .and_then(|data| data.SetStorageItems(&items, true));
            if let Err(err) = set {
                tracing::warn!("share: couldn't attach files: {err}");
            }
            let _ = deferral.Complete();
        });
        Ok(())
    }))?;
    SHARE_TOKEN.set(Some(token));
    // SAFETY: as above.
    unsafe { interop.ShowShareUIForWindow(hwnd) }.context("ShowShareUIForWindow")
}

/// Look each path up as a WinRT `StorageFile`; ones that fail (gone,
/// not a file) are skipped and logged. Blocking — worker thread only.
fn storage_items(paths: &[PathBuf]) -> windows_collections::IIterable<IStorageItem> {
    let items: Vec<Option<IStorageItem>> = paths
        .iter()
        .filter_map(|path| match storage_file(path) {
            Ok(file) => file.cast::<IStorageItem>().ok(),
            Err(err) => {
                tracing::warn!("share: skipping a file: {err}");
                None
            }
        })
        .map(Some)
        .collect();
    items.into()
}

/// Assumes WinRT's storage broker, unlike Win32, rejects `\\?\` verbatim
/// paths (which Filex's own paths may carry), so the plain spelling from
/// `display_path` is what's handed over.
fn storage_file(path: &Path) -> windows::core::Result<StorageFile> {
    let plain = filex::listing::display_path(path);
    StorageFile::GetFileFromPathAsync(&HSTRING::from(plain.as_str()))?.join()
}

/// The sheet's heading: the file name, or "N files".
fn share_title(paths: &[PathBuf]) -> String {
    match paths {
        [one] => one
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
        many => format!("{} files", many.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn share_title_names_one_file_or_counts_many() {
        assert_eq!(
            share_title(&[PathBuf::from(r"C:\a\report.pdf")]),
            "report.pdf"
        );
        assert_eq!(
            share_title(&[PathBuf::from(r"C:\a"), PathBuf::from(r"C:\b")]),
            "2 files"
        );
    }
}

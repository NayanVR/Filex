//! Windows Shell surfaces the context menu opens: the Properties dialog
//! and the Share sheet. macOS and Linux equivalents live in
//! `workspace::platform` (Properties shows Filex's details panel there;
//! Share is Windows-only for now — see `filex::context_menu::Capabilities`).

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use windows::ApplicationModel::DataTransfer::{DataRequestedEventArgs, DataTransferManager};
use windows::Foundation::TypedEventHandler;
use windows::Storage::{IStorageItem, StorageFile};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, POINT, RECT, SIZE};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows::Win32::Graphics::Gdi::{BITMAP, DeleteObject, GetObjectW, HGDIOBJ};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, COINIT_MULTITHREADED, CoCreateInstance,
    CoInitializeEx, IDataObject,
};
use windows::Win32::System::Ole::{DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE};
use windows::Win32::System::Threading::{GetCurrentProcessId, GetCurrentThreadId};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON, VK_RBUTTON};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    BHID_DataObject, CLSID_DragDropHelper, DSH_ALLOWDROPDESCRIPTIONTEXT,
    IDataTransferManagerInterop, IDragSourceHelper, IDragSourceHelper2, ILFree, IShellItemArray,
    IShellItemImageFactory, SEE_MASK_FLAG_NO_UI, SEE_MASK_INVOKEIDLIST,
    SHCreateShellItemArrayFromIDLists, SHDRAGIMAGE, SHDoDragDrop, SHELLEXECUTEINFOW,
    SHMultiFileProperties, SHParseDisplayName, SIIGBF_RESIZETOFIT, ShellExecuteExW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, EnumThreadWindows, GW_HWNDNEXT, GWL_EXSTYLE, GetCursorPos, GetSystemMetrics,
    GetTopWindow, GetWindow, GetWindowLongW, GetWindowRect, GetWindowThreadProcessId, IsIconic,
    IsWindowVisible, KillTimer, MSG, MsgWaitForMultipleObjects, PM_REMOVE, PeekMessageW,
    QS_ALLINPUT, SM_SWAPBUTTON, SW_SHOW, SetTimer, TranslateMessage, WS_EX_TRANSPARENT,
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

/// Hand `paths` to the OS as an outbound file drag — the same drag source
/// Explorer uses, so any program that accepts dropped files (mail, chat,
/// editors, Explorer itself) gets real files, with copy / move / link
/// chosen by the target and the user's modifier keys exactly as in
/// Explorer, and the Shell's drag thumbnails.
///
/// Whether the primary mouse button is physically held right now.
/// `GetAsyncKeyState` reads physical buttons, so with swapped buttons
/// (left-handed setup) the primary one is `VK_RBUTTON`. A plain query.
pub(super) fn primary_button_down() -> bool {
    // SAFETY: stateless Win32 queries.
    unsafe {
        let swapped = GetSystemMetrics(SM_SWAPBUTTON) != 0;
        let key = if swapped { VK_RBUTTON } else { VK_LBUTTON };
        (GetAsyncKeyState(key.0 as i32) as u16 & 0x8000) != 0
    }
}

/// Whether the cursor is over another app rather than filex's window
/// `ours`: outside our window, or inside it but under a window from
/// another process that sits above ours in the z-order.
///
/// Called from GPUI event handlers, so it **must not send window
/// messages**: `WindowFromPoint` hit-tests with a synchronous
/// `WM_NCHITTEST`, which lands back in filex's own window procedure while
/// GPUI's app state is borrowed (logged as "RefCell already borrowed" on
/// every drag move). This walks the z-order with plain queries instead.
/// Assumes top-level windows only (child windows can't cover ours).
pub(super) fn pointer_over_other_app(ours: isize) -> bool {
    let ours = HWND(ours as *mut _);
    let mut cursor = POINT::default();
    let mut stack = Vec::new();
    // SAFETY: read-only Win32 queries on window handles; none sends a
    // message. Handles may die mid-walk, which just fails the query.
    unsafe {
        if GetCursorPos(&mut cursor).is_err() {
            return false;
        }
        let our_pid = GetCurrentProcessId();
        let mut hwnd = GetTopWindow(None).unwrap_or_default();
        // Bounded: a pathological z-order can't stall a drag.
        for _ in 0..512 {
            if hwnd.is_invalid() {
                break;
            }
            stack.push(window_facts(hwnd, hwnd == ours, our_pid));
            if hwnd == ours {
                break;
            }
            hwnd = GetWindow(hwnd, GW_HWNDNEXT).unwrap_or_default();
        }
    }
    covered_by_other_app(&stack, (cursor.x, cursor.y))
}

/// What the z-order walk needs to know about one top-level window.
#[derive(Clone, Copy, Debug, Default)]
struct WindowFacts {
    ours: bool,
    /// Visible, not minimized, and not cloaked (another virtual desktop,
    /// a suspended UWP app) — actually on screen.
    shown: bool,
    /// Click-through (`WS_EX_TRANSPARENT`): overlays that never take input.
    click_through: bool,
    same_process: bool,
    /// Screen rect: left, top, right, bottom.
    rect: (i32, i32, i32, i32),
}

/// # Safety
/// `hwnd` must be a window handle (it may be stale; queries then fail).
unsafe fn window_facts(hwnd: HWND, ours: bool, our_pid: u32) -> WindowFacts {
    let mut rect = RECT::default();
    let mut pid = 0u32;
    let mut cloaked = 0u32;
    // SAFETY: per the caller; every out-pointer outlives its call.
    unsafe {
        let _ = GetWindowRect(hwnd, &mut rect);
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        let _ = DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut _,
            std::mem::size_of::<u32>() as u32,
        );
        let ex_style = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        WindowFacts {
            ours,
            shown: IsWindowVisible(hwnd).as_bool() && !IsIconic(hwnd).as_bool() && cloaked == 0,
            click_through: ex_style & WS_EX_TRANSPARENT.0 != 0,
            same_process: pid == our_pid,
            rect: (rect.left, rect.top, rect.right, rect.bottom),
        }
    }
}

/// The decision behind [`pointer_over_other_app`], over the z-order
/// walked top-down and ending at our window: true when the point is
/// outside our window, or a shown, input-taking window of another process
/// above ours contains it. Pure, so it's unit-tested.
fn covered_by_other_app(stack: &[WindowFacts], (x, y): (i32, i32)) -> bool {
    let contains = |(l, t, r, b): (i32, i32, i32, i32)| x >= l && x < r && y >= t && y < b;
    let Some(ours) = stack.iter().find(|w| w.ours) else {
        return false;
    };
    if !contains(ours.rect) {
        return true;
    }
    stack
        .iter()
        .take_while(|w| !w.ours)
        .any(|w| w.shown && !w.click_through && !w.same_process && contains(w.rect))
}

/// Start that drag as soon as the UI thread is back at the top of its
/// message loop. Called from a GPUI event handler, with the left button
/// still down.
///
/// The OLE drag loop is modal and pumps window messages, so it must run
/// on the UI thread (it needs that thread's mouse capture and input
/// state — a separate thread never sees the drag progress) but *outside*
/// any GPUI code: started from a GPUI handler or foreground task, every
/// message GPUI receives during the drag (repaints, Alt+Tab activation)
/// finds the app state borrowed and is dropped, and the window stops
/// painting. A one-shot thread timer's callback is invoked by GPUI's own
/// top-level `GetMessage`/`DispatchMessage` loop, with nothing of GPUI's
/// on the stack.
///
/// The returned receiver resolves once the drag is over (dropped,
/// cancelled or failed), so the caller can refresh what it moved.
pub(super) fn drag_out_soon(paths: Vec<PathBuf>) -> futures::channel::oneshot::Receiver<()> {
    let (done, finished) = futures::channel::oneshot::channel();
    PENDING_DRAG.with(|pending| *pending.borrow_mut() = Some((paths, done)));
    // SAFETY: a one-shot thread timer (no window); the callback kills it.
    if unsafe { SetTimer(None, 0, 0, Some(run_pending_drag)) } == 0 {
        tracing::warn!("file drag to another app: could not schedule the drag");
        // Dropping the queued sender resolves the receiver as cancelled.
        PENDING_DRAG.with(|pending| pending.borrow_mut().take());
    }
    finished
}

thread_local! {
    /// The paths [`drag_out_soon`] queued for [`run_pending_drag`], with
    /// the sender that reports the drag finished.
    static PENDING_DRAG: std::cell::RefCell<Option<(Vec<PathBuf>, futures::channel::oneshot::Sender<()>)>> =
        const { std::cell::RefCell::new(None) };
}

/// Timer callback that runs the queued drag (see [`drag_out_soon`]).
unsafe extern "system" fn run_pending_drag(_: HWND, _: u32, id: usize, _: u32) {
    // SAFETY: kills the thread timer this callback was invoked for.
    let _ = unsafe { KillTimer(None, id) };
    let Some((paths, done)) = PENDING_DRAG.with(|pending| pending.borrow_mut().take()) else {
        tracing::debug!("drag timer fired with nothing queued");
        return;
    };
    tracing::debug!(items = paths.len(), "OS drag loop starting");
    if let Err(err) = drag_out(&paths) {
        tracing::warn!("file drag to another app failed: {err:#}");
    }
    let _ = done.send(());
}

/// Runs the OLE drag loop until the user drops or cancels (Esc). Assumes
/// the UI thread (OLE-initialized by gpui for its own drop target), called
/// from [`run_pending_drag`] — see [`drag_out_soon`].
fn drag_out(paths: &[PathBuf]) -> Result<()> {
    let items = shell_items(paths)?;
    // SAFETY: COM calls on interfaces owned by this (the UI) thread; a
    // null drop source asks the Shell to supply its default one.
    unsafe {
        let data: windows::Win32::System::Com::IDataObject =
            items.BindToHandler(None, &BHID_DataObject)?;
        // Without a picture the dragged file vanishes from under the cursor
        // the moment the drag leaves filex (or filex is Alt+Tabbed away):
        // the drag is still live, but nothing shows it.
        if let Err(err) = attach_drag_image(&items, &data) {
            tracing::debug!("file drag to another app: no drag image: {err:#}");
        }
        let effect = SHDoDragDrop(
            None,
            &data,
            None,
            DROPEFFECT_COPY | DROPEFFECT_MOVE | DROPEFFECT_LINK,
        )
        .context("SHDoDragDrop")?;
        // DROPEFFECT_NONE here means cancelled or refused by the target.
        tracing::info!(
            items = paths.len(),
            effect = effect.0,
            "file drag to another app finished"
        );
    }
    Ok(())
}

/// Edge (px) of the picture shown under the cursor during an OS drag.
const DRAG_IMAGE_EDGE: i32 = 96;

/// Give the OS drag a picture, as Explorer does: the first item's Shell
/// thumbnail (a photo's preview, else its file-type icon), with drop
/// descriptions ("Copy to Downloads") enabled. Windows keeps drawing it
/// over other apps and across Alt+Tab, and adds the item-count badge for
/// multi-item drags.
///
/// Assumes the UI thread (COM initialized) and that `data` is the Shell
/// data object for `items`.
fn attach_drag_image(items: &IShellItemArray, data: &IDataObject) -> Result<()> {
    // SAFETY: COM calls on interfaces owned by this thread; the bitmap is
    // handed to the helper on success and freed here otherwise.
    unsafe {
        let helper: IDragSourceHelper =
            CoCreateInstance(&CLSID_DragDropHelper, None, CLSCTX_INPROC_SERVER)
                .context("creating the drag image helper")?;
        // Flags must be set before the image is attached.
        if let Ok(helper2) = helper.cast::<IDragSourceHelper2>() {
            let _ = helper2.SetFlags(DSH_ALLOWDROPDESCRIPTIONTEXT.0 as u32);
        }
        let first: IShellItemImageFactory = items.GetItemAt(0)?.cast()?;
        let edge = SIZE {
            cx: DRAG_IMAGE_EDGE,
            cy: DRAG_IMAGE_EDGE,
        };
        let bitmap = first
            .GetImage(edge, SIIGBF_RESIZETOFIT)
            .context("rendering the thumbnail")?;
        let mut info = BITMAP::default();
        GetObjectW(
            HGDIOBJ(bitmap.0),
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut info as *mut BITMAP as *mut _),
        );
        let (w, h) = (info.bmWidth.max(1), info.bmHeight.max(1));
        let image = SHDRAGIMAGE {
            sizeDragImage: SIZE { cx: w, cy: h },
            // Cursor at the picture's bottom centre, as Explorer places it.
            ptOffset: POINT { x: w / 2, y: h },
            hbmpDragImage: bitmap,
            // CLR_NONE: the thumbnail carries its own alpha.
            crColorKey: COLORREF(0xFFFF_FFFF),
        };
        if let Err(err) = helper.InitializeFromBitmap(&image, data) {
            let _ = DeleteObject(HGDIOBJ(bitmap.0));
            return Err(err).context("attaching the drag image");
        }
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
            // The Shell's parser rejects verbatim `\\?\` paths
            // (E_INVALIDARG), which is what `canonicalize` hands back — so
            // any folder reached through it broke drag-out, Properties and
            // Share. Parse the plain spelling instead.
            let path = filex::listing::plain_path(path);
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

    fn window(rect: (i32, i32, i32, i32)) -> WindowFacts {
        WindowFacts {
            shown: true,
            rect,
            ..Default::default()
        }
    }

    fn ours(rect: (i32, i32, i32, i32)) -> WindowFacts {
        WindowFacts {
            ours: true,
            same_process: true,
            ..window(rect)
        }
    }

    /// The reported bug: filex maximized, the drop target on top of it —
    /// the pointer never leaves filex's rectangle.
    #[test]
    fn another_apps_window_over_ours_counts_as_leaving() {
        let target = window((1000, 300, 1700, 800));
        let us = ours((0, 0, 1920, 1040));
        assert!(covered_by_other_app(&[target, us], (1200, 500)));
        // Beside the target, still over filex: stay in-app.
        assert!(!covered_by_other_app(&[target, us], (200, 500)));
    }

    #[test]
    fn outside_our_window_counts_as_leaving() {
        let us = ours((100, 100, 900, 700));
        assert!(covered_by_other_app(&[us], (950, 300)));
        assert!(!covered_by_other_app(&[us], (300, 300)));
    }

    #[test]
    fn hidden_overlay_and_own_windows_dont_cover() {
        let us = ours((0, 0, 1920, 1040));
        let pt = (500, 500);
        let full = (0, 0, 1920, 1040);
        let cloaked = WindowFacts {
            shown: false,
            ..window(full)
        };
        let overlay = WindowFacts {
            click_through: true,
            ..window(full)
        };
        let own_popup = WindowFacts {
            same_process: true,
            ..window(full)
        };
        assert!(!covered_by_other_app(
            &[cloaked, overlay, own_popup, us],
            pt
        ));
        // Windows *below* ours never count.
        assert!(!covered_by_other_app(&[us, window(full)], pt));
    }

    /// Regression: an OS drag without a picture made the dragged file
    /// vanish from under the cursor (e.g. on Alt+Tab mid-drag). The Shell
    /// must accept a thumbnail-backed drag image for an ordinary file.
    #[test]
    fn drag_image_attaches_for_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("photo.txt");
        std::fs::write(&file, "x").unwrap();
        // SAFETY: COM initialized once for this test thread.
        let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        let items = shell_items(&[file]).unwrap();
        // SAFETY: plain COM call on an interface owned by this thread.
        let data: IDataObject = unsafe { items.BindToHandler(None, &BHID_DataObject) }.unwrap();
        attach_drag_image(&items, &data).expect("drag image should attach");
    }

    /// Regression: `canonicalize` yields `\\?\C:\…` paths, which
    /// `SHParseDisplayName` rejects — dragging a file out of any folder
    /// reached through a canonical path failed with E_INVALIDARG.
    #[test]
    fn shell_items_accept_verbatim_paths() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("drag me.txt");
        std::fs::write(&file, "x").unwrap();
        let verbatim = std::fs::canonicalize(&file).unwrap();
        assert!(verbatim.to_string_lossy().starts_with(r"\\?\"));
        // SAFETY: COM initialized once for this test thread.
        let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        let items = shell_items(&[verbatim]).expect("verbatim path should parse");
        // SAFETY: plain COM call on an interface owned by this thread.
        assert_eq!(unsafe { items.GetCount() }.unwrap(), 1);
    }
}

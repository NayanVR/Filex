//! Running `filex --pick` from inside the host app, modally.
//!
//! Assumes it is called on the app's UI (STA) thread from `Show`, like the
//! real dialog: the owner window is disabled and messages keep being
//! pumped while Filex is up, so the app repaints instead of reading as
//! "Not responding", and input to it is blocked until the pick finishes.

use crate::protocol::{Request, Response, parse_response};
use std::io::{Read as _, Write as _};
use std::os::windows::io::AsRawHandle as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use windows::Win32::Foundation::{HANDLE, HMODULE, HWND, WAIT_OBJECT_0};
use windows::Win32::System::LibraryLoader::{
    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
    GetModuleFileNameW, GetModuleHandleExW,
};
use windows::Win32::System::Threading::INFINITE;
use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, DispatchMessageW, MSG, MsgWaitForMultipleObjects, PM_REMOVE,
    PeekMessageW, PostQuitMessage, QS_ALLINPUT, SetForegroundWindow, TranslateMessage, WM_QUIT,
};
use windows::core::PCWSTR;

/// Whether this process should get the system dialog regardless: Filex
/// itself (no recursion), or the user opting out for one run.
pub fn should_bypass() -> bool {
    if std::env::var_os("FILEX_DIALOG_SHIM_DISABLE").is_some() {
        return true;
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.file_name().map(|n| n.to_ascii_lowercase()))
        .is_some_and(|name| name == "filex.exe")
}

/// `filex.exe`, installed next to this DLL.
pub fn filex_exe() -> Option<PathBuf> {
    let mut module = HMODULE::default();
    // SAFETY: the address is a function in this DLL; the handle is not
    // ref-counted, so nothing to release.
    unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            PCWSTR(filex_exe as *const u16),
            &mut module,
        )
        .ok()?;
    }
    let mut buf = vec![0u16; 1024];
    // SAFETY: `buf` is writable for its length.
    let len = unsafe { GetModuleFileNameW(Some(module), &mut buf) } as usize;
    if len == 0 || len >= buf.len() {
        return None;
    }
    let dll = PathBuf::from(String::from_utf16_lossy(&buf[..len]));
    let exe = dll.with_file_name("filex.exe");
    exe.is_file().then_some(exe)
}

/// Run the picker modally over `owner`. `None` means "fall back".
pub fn pick(request: &Request, owner: HWND) -> Option<Response> {
    let exe = filex_exe()?;
    let mut child = Command::new(exe)
        .arg("--pick")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // Let the picker take the foreground from us (the host app is in front).
    // SAFETY: plain call; failure only affects focus.
    let _ = unsafe { AllowSetForegroundWindow(child.id()) };
    {
        let mut stdin = child.stdin.take()?;
        stdin.write_all(request.to_json().as_bytes()).ok()?;
    } // EOF: the picker reads to end.
    // Drain stdout on a thread so a chatty child can never block on a
    // full pipe while we wait on its process handle.
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut out = String::new();
        stdout.read_to_string(&mut out).ok().map(|_| out)
    });
    let process = HANDLE(child.as_raw_handle());
    modal_wait(process, owner);
    let status = child.wait().ok()?;
    let out = reader.join().ok()??;
    if !status.success() {
        return None;
    }
    parse_response(&out)
}

/// Block until `process` exits, pumping messages and keeping `owner`
/// disabled, as a modal dialog does.
fn modal_wait(process: HANDLE, owner: HWND) {
    let has_owner = !owner.is_invalid();
    if has_owner {
        // SAFETY: `owner` is the app's window handle given to Show.
        unsafe {
            let _ = EnableWindow(owner, false);
        }
    }
    // A WM_QUIT seen while pumping belongs to the app's own loop: hold it
    // and re-post it once we are done, as modal loops do.
    let mut quit: Option<i32> = None;
    loop {
        // SAFETY: `process` stays valid until `child.wait()` below.
        let woke =
            unsafe { MsgWaitForMultipleObjects(Some(&[process]), false, INFINITE, QS_ALLINPUT) };
        if woke == WAIT_OBJECT_0 || woke.0 > WAIT_OBJECT_0.0 + 1 {
            break;
        }
        let mut msg = MSG::default();
        // SAFETY: standard message pump on this thread's queue.
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    quit = Some(msg.wParam.0 as i32);
                    continue;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
    if has_owner {
        // SAFETY: as above.
        unsafe {
            let _ = EnableWindow(owner, true);
            let _ = SetForegroundWindow(owner);
        }
    }
    if let Some(code) = quit {
        // SAFETY: re-posts to this thread's queue.
        unsafe { PostQuitMessage(code) };
    }
}

//! Launching the app back into the user's session after an install.
//!
//! Assumes the caller is LocalSystem (holds `SeTcbPrivilege`, required by
//! `WTSQueryUserToken`). The token returned for a UAC-enabled administrator
//! is the *filtered* (unelevated) one, so the app — and the per-user index
//! daemon it starts — run with the user's ordinary rights.

use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use anyhow::{Context, Result};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows::Win32::System::RemoteDesktop::WTSQueryUserToken;
use windows::Win32::System::Threading::{
    CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, PROCESS_INFORMATION, STARTUPINFOW,
};
use windows::core::{PCWSTR, PWSTR};

fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

/// The command line for `exe`: the quoted path alone.
fn command_line(exe: &Path) -> Vec<u16> {
    let mut quoted = std::ffi::OsString::from("\"");
    quoted.push(exe.as_os_str());
    quoted.push("\"");
    wide(&quoted)
}

/// Start `exe` in `session` as that session's user, on the interactive
/// desktop. Fails if nobody is logged on there.
pub fn launch(session: u32, exe: &Path) -> Result<()> {
    let mut token = HANDLE::default();
    // SAFETY: out-parameter for the session's primary token.
    unsafe { WTSQueryUserToken(session, &mut token) }
        .with_context(|| format!("no user token for session {session}"))?;
    let result = launch_with_token(token, exe);
    // SAFETY: closing the token we were given.
    unsafe {
        let _ = CloseHandle(token);
    }
    result
}

fn launch_with_token(token: HANDLE, exe: &Path) -> Result<()> {
    let mut environment: *mut core::ffi::c_void = std::ptr::null_mut();
    // SAFETY: builds the user's environment block; destroyed below.
    unsafe { CreateEnvironmentBlock(&mut environment, Some(token), false) }
        .context("building the user's environment")?;
    let application = wide(exe.as_os_str());
    let mut command = command_line(exe);
    let directory = exe.parent().map(|d| wide(d.as_os_str()));
    let mut desktop = wide(std::ffi::OsStr::new("winsta0\\default"));
    let startup = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        lpDesktop: PWSTR(desktop.as_mut_ptr()),
        ..Default::default()
    };
    let mut info = PROCESS_INFORMATION::default();
    // SAFETY: every buffer outlives the call; the command line is mutable
    // as CreateProcessAsUserW requires.
    let created = unsafe {
        CreateProcessAsUserW(
            Some(token),
            PCWSTR(application.as_ptr()),
            Some(PWSTR(command.as_mut_ptr())),
            None,
            None,
            false,
            CREATE_UNICODE_ENVIRONMENT,
            Some(environment),
            directory
                .as_ref()
                .map_or(PCWSTR::null(), |d| PCWSTR(d.as_ptr())),
            &startup,
            &mut info,
        )
    };
    // SAFETY: releasing what we created, whether or not the launch worked.
    unsafe {
        let _ = DestroyEnvironmentBlock(environment);
        if created.is_ok() {
            let _ = CloseHandle(info.hThread);
            let _ = CloseHandle(info.hProcess);
        }
    }
    created.with_context(|| format!("starting {}", exe.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_line_quotes_paths_with_spaces() {
        let line = command_line(Path::new(r"C:\Program Files\Filex\filex.exe"));
        let text = String::from_utf16_lossy(&line[..line.len() - 1]);
        assert_eq!(text, r#""C:\Program Files\Filex\filex.exe""#);
        assert_eq!(line.last(), Some(&0));
    }

    #[test]
    fn launching_without_tcb_privilege_fails_cleanly() {
        // Unelevated test processes lack SeTcbPrivilege: an error, not a crash.
        if launch(u32::MAX, Path::new(r"C:\nonexistent\filex.exe")).is_ok() {
            panic!("launch into a nonexistent session should fail");
        }
    }
}

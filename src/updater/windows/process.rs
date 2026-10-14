//! Finding and stopping Filex processes before an install.
//!
//! Assumes the updater runs as LocalSystem, which can open and terminate
//! processes in every session. A process is "Filex's" only if its full image
//! path is `<install dir>\<binary>` — matching on the bare file name would
//! let an unrelated `filex.exe` elsewhere be killed.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    PROCESS_TERMINATE, QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject,
};
use windows::core::PWSTR;

/// An open handle to a matching process.
pub struct FilexProcess {
    pub pid: u32,
    pub session: u32,
    handle: HANDLE,
}

impl Drop for FilexProcess {
    fn drop(&mut self) {
        // SAFETY: closing the handle we opened.
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

impl FilexProcess {
    /// Terminate and wait (briefly) for the exit.
    pub fn terminate(&self) -> Result<()> {
        // SAFETY: a handle opened with PROCESS_TERMINATE | SYNCHRONIZE.
        unsafe { TerminateProcess(self.handle, 1) }
            .with_context(|| format!("terminating process {}", self.pid))?;
        // SAFETY: as above.
        let _ = unsafe { WaitForSingleObject(self.handle, 5_000) };
        Ok(())
    }

    pub fn has_exited(&self) -> bool {
        // SAFETY: a handle opened with SYNCHRONIZE.
        unsafe { WaitForSingleObject(self.handle, 0) == WAIT_OBJECT_0 }
    }
}

/// Case-insensitive path equality, as NTFS (by default) compares names.
fn same_path(a: &Path, b: &Path) -> bool {
    a.as_os_str()
        .to_string_lossy()
        .eq_ignore_ascii_case(&b.as_os_str().to_string_lossy())
}

fn image_path(handle: HANDLE) -> Option<PathBuf> {
    let mut buffer = vec![0u16; 32_768];
    let mut len = buffer.len() as u32;
    // SAFETY: buffer and length describe valid writable memory.
    unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut len,
        )
    }
    .ok()?;
    Some(PathBuf::from(String::from_utf16_lossy(
        &buffer[..len as usize],
    )))
}

/// Every running process whose image is `dir\binary`.
pub fn find(dir: &Path, binary: &str) -> Result<Vec<FilexProcess>> {
    let expected = dir.join(binary);
    // SAFETY: process snapshot; closed below.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }
        .context("snapshotting processes")?;
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut found = Vec::new();
    // SAFETY: a valid snapshot and a correctly sized entry.
    let mut more = unsafe { Process32FirstW(snapshot, &mut entry) }.is_ok();
    while more {
        let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(0);
        let name = String::from_utf16_lossy(&entry.szExeFile[..len]);
        if name.eq_ignore_ascii_case(binary)
            && let Some(process) = open_if_matching(entry.th32ProcessID, &expected)
        {
            found.push(process);
        }
        // SAFETY: as above.
        more = unsafe { Process32NextW(snapshot, &mut entry) }.is_ok();
    }
    // SAFETY: closing the snapshot we created.
    unsafe {
        let _ = CloseHandle(snapshot);
    }
    Ok(found)
}

fn open_if_matching(pid: u32, expected: &Path) -> Option<FilexProcess> {
    // SAFETY: plain OpenProcess; the handle is owned by FilexProcess.
    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE | PROCESS_SYNCHRONIZE,
            false,
            pid,
        )
    }
    .ok()?;
    let mut session = 0;
    // SAFETY: querying a pid's session.
    let session_ok = unsafe { ProcessIdToSessionId(pid, &mut session) }.is_ok();
    let process = FilexProcess {
        pid,
        session,
        handle,
    };
    (session_ok && image_path(handle).is_some_and(|path| same_path(&path, expected)))
        .then_some(process)
}

/// Wait until no `dir\binary` process remains in `session`, up to `timeout`.
/// Returns whether they all exited.
pub fn wait_for_exit_in_session(
    dir: &Path,
    binary: &str,
    session: u32,
    timeout: Duration,
) -> Result<bool> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = find(dir, binary)?
            .into_iter()
            .filter(|p| p.session == session && !p.has_exited())
            .count();
        if remaining == 0 {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Terminate every `dir\binary` process; returns how many were terminated.
pub fn terminate_all(dir: &Path, binary: &str) -> Result<usize> {
    let mut count = 0;
    for process in find(dir, binary)? {
        if process.has_exited() {
            continue;
        }
        tracing::info!(
            "terminating {binary} (pid {}, session {})",
            process.pid,
            process.session
        );
        process.terminate()?;
        count += 1;
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_this_test_process_by_full_path_only() {
        let exe = std::env::current_exe().unwrap();
        let dir = exe.parent().unwrap();
        let binary = exe.file_name().unwrap().to_string_lossy().into_owned();
        let me = std::process::id();
        assert!(find(dir, &binary).unwrap().iter().any(|p| p.pid == me));
        // Same file name, different directory: not ours.
        let elsewhere = dir.join("not-the-install-dir");
        assert!(
            find(&elsewhere, &binary)
                .unwrap()
                .iter()
                .all(|p| p.pid != me)
        );
    }

    #[test]
    fn terminates_and_waits_for_a_child() {
        // A copy of a harmless system binary in a temp dir, so the match is
        // by full path and nothing else can be hit.
        let dir = tempfile::tempdir().unwrap();
        let system = std::env::var_os("SystemRoot").unwrap();
        let source = Path::new(&system).join("System32").join("ping.exe");
        let binary = "filex-test-sleeper.exe";
        std::fs::copy(&source, dir.path().join(binary)).unwrap();
        let mut child = std::process::Command::new(dir.path().join(binary))
            .args(["-n", "60", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let session = find(dir.path(), binary).unwrap()[0].session;
        assert!(
            !wait_for_exit_in_session(dir.path(), binary, session, Duration::from_millis(300))
                .unwrap()
        );
        assert_eq!(terminate_all(dir.path(), binary).unwrap(), 1);
        child.wait().unwrap();
        assert!(
            wait_for_exit_in_session(dir.path(), binary, session, Duration::from_secs(1)).unwrap()
        );
    }

    #[test]
    fn path_comparison_ignores_case() {
        assert!(same_path(
            Path::new(r"C:\Program Files\Filex\filex.exe"),
            Path::new(r"c:\program files\filex\FILEX.EXE")
        ));
        assert!(!same_path(
            Path::new(r"C:\Program Files\Filex\filex.exe"),
            Path::new(r"C:\Temp\filex.exe")
        ));
    }
}

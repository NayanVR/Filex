//! Opt-in OS integration: Filex as the default file explorer, and Filex
//! in place of the system Open/Save dialogs. See
//! docs/design-default-explorer.md.
//!
//! Each platform module exposes the same four operations; this front end
//! only routes to it, so the settings UI and the `--register-*` flags
//! never touch platform code directly.

use anyhow::Result;
use std::path::{Path, PathBuf};

#[cfg(target_os = "windows")]
pub mod windows;

/// What is currently registered for this user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Status {
    pub default_explorer: bool,
    pub dialogs: bool,
}

/// Whether this platform can do each integration at all (drives what the
/// Settings page offers).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Support {
    pub default_explorer: bool,
    pub dialogs: bool,
}

pub fn support() -> Support {
    Support {
        default_explorer: cfg!(target_os = "windows"),
        dialogs: cfg!(target_os = "windows"),
    }
}

pub fn status() -> Status {
    #[cfg(target_os = "windows")]
    {
        windows::status(&windows::Hive::user_classes())
    }
    #[cfg(not(target_os = "windows"))]
    {
        Status::default()
    }
}

/// Make folders, drives and the File Explorer launcher open `exe`.
pub fn register_default(exe: &Path) -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        windows::register_default(&windows::Hive::user_classes(), exe)?;
        windows::notify_associations_changed();
        Ok(())
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = exe;
        anyhow::bail!("not supported on this platform yet")
    }
}

pub fn unregister_default() -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        windows::unregister_default(&windows::Hive::user_classes())?;
        windows::notify_associations_changed();
        Ok(())
    }
    #[cfg(not(target_os = "windows"))]
    {
        Ok(())
    }
}

/// Route other apps' Open/Save dialogs through the shim DLL at `shim`.
pub fn register_dialogs(shim: &Path) -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        windows::register_dialogs(&windows::Hive::user_classes(), shim)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = shim;
        anyhow::bail!("not supported on this platform yet")
    }
}

pub fn unregister_dialogs() -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        windows::unregister_dialogs(&windows::Hive::user_classes())
    }
    #[cfg(not(target_os = "windows"))]
    {
        Ok(())
    }
}

/// The dialog shim that ships next to `exe`.
pub fn dialog_shim_path(exe: &Path) -> PathBuf {
    exe.with_file_name(if cfg!(windows) {
        "filex_dialog_shim.dll"
    } else {
        "libfilex_dialog_shim.so"
    })
}

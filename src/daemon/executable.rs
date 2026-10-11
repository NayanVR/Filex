//! Locate the companion executable in installed bundles and Cargo test builds.
use anyhow::{Context, Result, ensure};
use std::{path::PathBuf, sync::OnceLock, time::SystemTime};

pub(super) fn daemon() -> Result<PathBuf> {
    let current = std::env::current_exe().context("locating the running executable")?;
    let name = if cfg!(windows) {
        "filex-indexd.exe"
    } else {
        "filex-indexd"
    };
    let mut directory = current
        .parent()
        .context("executable has no parent directory")?;
    // Cargo integration tests live in target/{profile}/deps; companion binaries
    // live one directory above. Installed applications use the sibling directly.
    if directory.file_name().is_some_and(|name| name == "deps") {
        directory = directory
            .parent()
            .context("test executable has no profile directory")?;
    }
    let executable = directory.join(name);
    ensure!(
        executable.is_file(),
        "companion daemon is missing: {}",
        executable.display()
    );
    Ok(executable)
}

/// Enough of a file's metadata to tell that it was replaced.
#[derive(Debug, PartialEq, Eq)]
struct Stamp {
    modified: Option<SystemTime>,
    len: u64,
}

fn stamp() -> Option<Stamp> {
    let meta = std::fs::metadata(daemon().ok()?).ok()?;
    Some(Stamp {
        modified: meta.modified().ok(),
        len: meta.len(),
    })
}

static AT_STARTUP: OnceLock<Option<Stamp>> = OnceLock::new();

/// Record the daemon binary as it is now. Call once, at daemon startup.
pub(super) fn note_startup() {
    AT_STARTUP.get_or_init(stamp);
}

/// Whether `filex-indexd` on disk is no longer the file this daemon started
/// from — an upgrade or rebuild replaced it, so workers it spawns run newer
/// code than it does. `false` when nothing was recorded.
pub(super) fn replaced_since_startup() -> bool {
    match AT_STARTUP.get() {
        Some(Some(start)) => stamp().as_ref() != Some(start),
        _ => false,
    }
}

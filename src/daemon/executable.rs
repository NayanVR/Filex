//! Locate the companion executable in installed bundles and Cargo test builds.
use anyhow::{Context, Result, ensure};
use std::path::PathBuf;

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

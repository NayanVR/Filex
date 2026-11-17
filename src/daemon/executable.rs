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
    let executable = beside_or_above(&current, name);
    ensure!(
        executable.is_file(),
        "companion daemon is missing: {}",
        executable.display()
    );
    Ok(executable)
}

/// Where `name` is for the executable at `current`: its sibling in an
/// installed app. Cargo test binaries sit deeper — `target/<profile>/deps`,
/// or `target/<profile>/build/<pkg>/<hash>/out` since Cargo 1.100's
/// build-dir layout — while bin targets are uplifted to `target/<profile>`,
/// so a few parents up are tried too. Returns the sibling path when none
/// exists, for the error message.
fn beside_or_above(current: &std::path::Path, name: &str) -> PathBuf {
    let mut dirs = current.ancestors().skip(1).take(5);
    let sibling = dirs.next().unwrap_or(current).join(name);
    if sibling.is_file() {
        return sibling;
    }
    dirs.map(|dir| dir.join(name))
        .find(|path| path.is_file())
        .unwrap_or(sibling)
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

#[cfg(test)]
mod tests {
    use super::beside_or_above;

    /// Regression: Rust 1.100 moved test binaries from `target/debug/deps`
    /// to `target/debug/build/<pkg>/<hash>/out`, and the daemon lookup only
    /// knew `deps`.
    #[test]
    fn finds_the_daemon_from_every_cargo_layout_and_beside_an_app() {
        let root = tempfile::tempdir().unwrap();
        let profile = root.path().join("debug");
        let deep = profile.join("build/filex/abc123/out");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::create_dir_all(profile.join("deps")).unwrap();
        std::fs::write(profile.join("indexd"), "").unwrap();
        for exe in [
            deep.join("test"),
            profile.join("deps/test"),
            profile.join("filex"),
        ] {
            assert_eq!(beside_or_above(&exe, "indexd"), profile.join("indexd"));
        }
        // An app's own sibling wins over anything further up.
        std::fs::write(deep.join("indexd"), "").unwrap();
        assert_eq!(
            beside_or_above(&deep.join("test"), "indexd"),
            deep.join("indexd")
        );
        // Missing everywhere: the sibling path, for the error.
        let lone = root.path().join("elsewhere/app");
        assert_eq!(
            beside_or_above(&lone, "nope"),
            root.path().join("elsewhere/nope")
        );
    }
}

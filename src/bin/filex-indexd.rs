//! filex-indexd — the elevated index service for Windows.
//!
//! Runs the volume indexes (USN fast path when elevated on volume roots,
//! walk+RDCW otherwise) and serves search/status to unelevated filex
//! instances over the named pipe `\\.\pipe\filex-index`. This is the
//! "Everything" split: install once with admin consent, and every
//! standard-user session gets instant whole-volume search.
//!
//! v1 registration is Task Scheduler (an SCM-native service wrapper is
//! planned):
//!
//! ```text
//! schtasks /Create /TN "filex index service" /RU SYSTEM /RL HIGHEST ^
//!          /SC ONSTART /TR "C:\path\to\filex-indexd.exe C:\"
//! ```
//!
//! Roots come from the command line, or the shared roots.list when no
//! arguments are given.

#[cfg(target_os = "windows")]
fn main() -> anyhow::Result<()> {
    use filex::index::ipc::{IndexHost, MultiRootHost, PIPE_NAME};
    use filex::index::{manager, start_live_index, windows};
    use std::path::PathBuf;
    use std::sync::Arc;

    let _logging_guard = filex::logging::init("filex-indexd");
    let mut roots: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    if roots.is_empty() {
        // Same sources as the UI: settings.json, with the legacy
        // roots.list as first-launch migration / fallback.
        let legacy = manager::default_roots_file();
        roots = match filex::settings::default_settings_file() {
            Some(file) => match filex::settings::Settings::load(&file, legacy.as_deref()) {
                Ok(settings) => settings.roots,
                Err(err) => {
                    tracing::warn!("unusable settings file ({err:#}); using legacy roots.list");
                    legacy.as_deref().map(manager::load_roots).unwrap_or_default()
                }
            },
            None => legacy.as_deref().map(manager::load_roots).unwrap_or_default(),
        };
    }
    if roots.is_empty() {
        anyhow::bail!(
            "no roots to index: pass paths as arguments (e.g. filex-indexd C:\\) \
             or configure settings.json"
        );
    }

    // Bootstrap every root before serving; LiveIndexes must stay alive
    // for live updates (and to snapshot on shutdown).
    let mut live_indexes = Vec::new();
    let mut host_roots = Vec::new();
    for root in roots {
        tracing::info!("indexing {}", root.display());
        match start_live_index(&root, || {}) {
            Ok(live) => {
                host_roots.push((root, live.index.clone()));
                live_indexes.push(live);
            }
            Err(err) => tracing::warn!("skipping {}: {err:#}", root.display()),
        }
    }
    if host_roots.is_empty() {
        anyhow::bail!("every configured root failed to index");
    }

    let host: Arc<dyn IndexHost> = Arc::new(MultiRootHost { roots: host_roots });
    tracing::info!("serving on {PIPE_NAME}");
    windows::run_pipe_server(PIPE_NAME, host, None)
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!(
        "filex-indexd is the Windows index service; on this platform filex \
         indexes in-process (fanotify/FSEvents need no privilege split)."
    );
    std::process::exit(1);
}

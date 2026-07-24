//! filex-indexd — the elevated index service for Windows.
//!
//! Runs the volume indexes (USN fast path when elevated on volume roots,
//! walk+RDCW otherwise) and serves search/status to unelevated filex
//! instances over the named pipe `\\.\pipe\filex-index`. This is the
//! "Everything" split: install once with admin consent, and every
//! standard-user session gets instant whole-volume search.
//!
//! Runs either as a real SCM service (installed by the MSI — the normal
//! path) or, when *not* launched by the Service Control Manager (a dev run
//! or the legacy Task Scheduler registration), as a plain console process
//! that serves until killed. A service Stop/Shutdown drops the LiveIndexes
//! so their snapshots save cleanly.
//!
//! Roots come from the command line, or the shared settings/roots.list
//! when no arguments are given.

#[cfg(target_os = "windows")]
fn main() -> anyhow::Result<()> {
    service::main()
}

#[cfg(target_os = "windows")]
mod service {
    use std::ffi::OsString;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use anyhow::Result;
    use windows_service::service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
        ServiceType,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
    use windows_service::{Error as ServiceError, define_windows_service, service_dispatcher};

    use filex::index::ipc::{IndexHost, MultiRootHost, PIPE_NAME};
    use filex::index::{manager, start_live_index, windows};

    /// The SCM service name; must match the MSI's ServiceInstall entry.
    const SERVICE_NAME: &str = "filex-indexd";
    const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;
    /// `ERROR_FAILED_SERVICE_CONTROLLER_CONNECT` — returned by
    /// `StartServiceCtrlDispatcher` when the process wasn't launched by the
    /// SCM, i.e. it's a console/dev run.
    const NOT_STARTED_BY_SCM: i32 = 1063;

    pub fn main() -> Result<()> {
        let _logging_guard = filex::logging::init("filex-indexd");
        // Try to attach to the SCM (blocks until the service stops). If we
        // weren't started by the SCM, serve in the console instead.
        match service_dispatcher::start(SERVICE_NAME, ffi_service_main) {
            Ok(()) => Ok(()),
            Err(ServiceError::Winapi(io)) if io.raw_os_error() == Some(NOT_STARTED_BY_SCM) => {
                tracing::info!("not started by the SCM; serving in console mode");
                run_indexes(resolve_roots(env_root_args()), None)
            }
            Err(err) => Err(err.into()),
        }
    }

    define_windows_service!(ffi_service_main, service_main);

    /// SCM entry point (background thread). No stdout/stderr here — logging
    /// goes to the file sink configured above.
    fn service_main(arguments: Vec<OsString>) {
        if let Err(err) = run_service(arguments) {
            tracing::error!("service failed: {err:#}");
        }
    }

    fn run_service(arguments: Vec<OsString>) -> Result<()> {
        let shutdown = Arc::new(AtomicBool::new(false));

        // Stop/Shutdown set the flag and wake the pipe accept so the serve
        // loop returns and the LiveIndexes drop (snapshots save).
        let event_handler = {
            let shutdown = shutdown.clone();
            move |control| -> ServiceControlHandlerResult {
                match control {
                    ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                    ServiceControl::Stop | ServiceControl::Shutdown => {
                        shutdown.store(true, Ordering::Relaxed);
                        windows::wake_pipe_server(PIPE_NAME);
                        ServiceControlHandlerResult::NoError
                    }
                    _ => ServiceControlHandlerResult::NotImplemented,
                }
            }
        };
        let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)?;

        let report = |state, accepted| -> Result<()> {
            status_handle.set_service_status(ServiceStatus {
                service_type: SERVICE_TYPE,
                current_state: state,
                controls_accepted: accepted,
                exit_code: ServiceExitCode::Win32(0),
                checkpoint: 0,
                wait_hint: Duration::default(),
                process_id: None,
            })?;
            Ok(())
        };
        report(
            ServiceState::Running,
            ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
        )?;

        // SCM passes the service name as argv[0]; the rest are root paths.
        let roots =
            resolve_roots(arguments.into_iter().skip(1).map(PathBuf::from).collect());
        let result = run_indexes(roots, Some(shutdown));
        if let Err(err) = &result {
            tracing::error!("index service stopped with error: {err:#}");
        }

        // Always report Stopped, even on error, so the SCM doesn't hang.
        report(ServiceState::Stopped, ServiceControlAccept::empty())?;
        result
    }

    /// Root paths from the command line (skips argv[0], the binary path).
    fn env_root_args() -> Vec<PathBuf> {
        std::env::args_os().skip(1).map(PathBuf::from).collect()
    }

    /// The roots to index: explicit `path_args` if given, else the shared
    /// settings.json (with the legacy roots.list as first-launch fallback)
    /// — the same sources the UI uses.
    fn resolve_roots(path_args: Vec<PathBuf>) -> Vec<PathBuf> {
        if !path_args.is_empty() {
            return path_args;
        }
        let legacy = manager::default_roots_file();
        match filex::settings::default_settings_file() {
            Some(file) => match filex::settings::Settings::load(&file, legacy.as_deref()) {
                Ok(settings) => settings.roots,
                Err(err) => {
                    tracing::warn!("unusable settings file ({err:#}); using legacy roots.list");
                    legacy.as_deref().map(manager::load_roots).unwrap_or_default()
                }
            },
            None => legacy.as_deref().map(manager::load_roots).unwrap_or_default(),
        }
    }

    /// Bootstrap every root and serve the pipe until `shutdown` is set (or,
    /// in console mode with `None`, until the process is killed). The
    /// LiveIndexes are dropped on return, which saves their snapshots — the
    /// clean-shutdown contract the SCM stop relies on.
    fn run_indexes(roots: Vec<PathBuf>, shutdown: Option<Arc<AtomicBool>>) -> Result<()> {
        if roots.is_empty() {
            anyhow::bail!(
                "no roots to index: pass paths as arguments (e.g. filex-indexd C:\\) \
                 or configure settings.json"
            );
        }

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
        let result = windows::run_pipe_server(PIPE_NAME, host, None, shutdown);
        // `live_indexes` drops here: watchers/writers stop and each snapshot
        // saves with its exact checkpoint.
        drop(live_indexes);
        tracing::info!("index service stopped; snapshots saved");
        result
    }
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!(
        "filex-indexd is the Windows index service; on this platform filex \
         indexes in-process (fanotify/FSEvents need no privilege split)."
    );
    std::process::exit(1);
}

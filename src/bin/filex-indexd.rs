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
    use filex::index::{manager, start_live_index_cancellable, windows};

    /// The SCM service name; must match the MSI's ServiceInstall entry.
    const SERVICE_NAME: &str = "filex-indexd";
    const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;
    /// `ERROR_FAILED_SERVICE_CONTROLLER_CONNECT` — returned by
    /// `StartServiceCtrlDispatcher` when the process wasn't launched by the
    /// SCM, i.e. it's a console/dev run.
    const NOT_STARTED_BY_SCM: i32 = 1063;

    /// Where the SCM service should write logs. LocalSystem's local data
    /// dir is `C:\Windows\System32\config\systemprofile\AppData\Local` —
    /// invisible to an admin looking for service logs — so point the
    /// service at `C:\ProgramData\filex\logs` instead, which is
    /// machine-wide and discoverable. `None` (falls back to the user dir)
    /// only if `ProgramData` is unset, which doesn't happen on a normal
    /// Windows install.
    fn service_log_base() -> Option<PathBuf> {
        std::env::var_os("ProgramData").map(PathBuf::from)
    }

    pub fn main() -> Result<()> {
        let _logging_guard = filex::logging::init_in("filex-indexd", service_log_base());
        filex::telemetry::install_panic_hook("filex-indexd");
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
        // Cancels an in-flight self-update download if the service is asked
        // to stop mid-check. Always present (not feature-gated) so the event
        // handler stays uniform; only the update *thread* needs `updater`.
        let update_cancel = filex::update::CancelFlag::new();

        // Stop/Shutdown set the flag and wake the pipe accept so the serve
        // loop returns and the LiveIndexes drop (snapshots save).
        let event_handler = {
            let shutdown = shutdown.clone();
            let update_cancel = update_cancel.clone();
            move |control| -> ServiceControlHandlerResult {
                match control {
                    ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                    ServiceControl::Stop | ServiceControl::Shutdown => {
                        shutdown.store(true, Ordering::Relaxed);
                        update_cancel.cancel();
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

        // One-shot self-update check on launch (distribution decision 7: no
        // timer). SCM path only — a console/dev run must not self-update.
        // Runs off-thread so it never delays serving; a stop cancels it.
        #[cfg(feature = "updater")]
        spawn_update_check(update_cancel.clone());

        // The SCM only hands ServiceMain the arguments from an explicit
        // StartService call — NOT the arguments baked into the service's
        // ImagePath. The MSI registers the service with its roots in the
        // ImagePath (`filex-indexd.exe C:\`), so those arrive on the
        // process command line instead. Prefer ServiceMain args (a manual
        // `sc start filex-indexd D:\`), then the ImagePath/process args,
        // then settings.json. Falling straight through to an empty
        // settings.json is what made the service start and immediately
        // stop with "no roots to index".
        let scm_args: Vec<PathBuf> = arguments.into_iter().skip(1).map(PathBuf::from).collect();
        let path_args = if scm_args.is_empty() {
            env_root_args()
        } else {
            scm_args
        };
        let roots = resolve_roots(path_args);
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
    /// — the same sources the UI uses. When nothing is configured, fall
    /// back to the platform default (all fixed drives on Windows) instead
    /// of an empty set, which is what previously made the service start and
    /// immediately stop with "no roots to index" on a fresh install.
    fn resolve_roots(path_args: Vec<PathBuf>) -> Vec<PathBuf> {
        if !path_args.is_empty() {
            return path_args;
        }
        let legacy = manager::default_roots_file();
        let configured = match filex::settings::default_settings_file() {
            Some(file) => match filex::settings::Settings::load(&file, legacy.as_deref()) {
                Ok(settings) => settings.roots,
                Err(err) => {
                    tracing::warn!("unusable settings file ({err:#}); using legacy roots.list");
                    legacy
                        .as_deref()
                        .map(manager::load_roots)
                        .unwrap_or_default()
                }
            },
            None => legacy
                .as_deref()
                .map(manager::load_roots)
                .unwrap_or_default(),
        };
        if configured.is_empty() {
            filex::drives::default_index_roots()
        } else {
            configured
        }
    }

    /// Whether to exclude OS/system folders (`C:\Windows`, …) from the
    /// index — mirrors the UI's `index_system_files` setting. Defaults to
    /// excluding them (the memory-saving default) when no settings file is
    /// present or readable.
    fn exclude_system_dirs() -> bool {
        filex::settings::default_settings_file()
            .and_then(|file| filex::settings::Settings::load(&file, None).ok())
            .map(|settings| !settings.index_system_files)
            .unwrap_or(true)
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

        let stopping = || shutdown.as_ref().is_some_and(|s| s.load(Ordering::Relaxed));
        let exclude_system = exclude_system_dirs();

        let mut live_indexes = Vec::new();
        let mut host_roots = Vec::new();
        for root in roots {
            if stopping() {
                break; // asked to stop before bootstrapping the rest
            }
            tracing::info!("indexing {}", root.display());
            match start_live_index_cancellable(&root, exclude_system, || {}, shutdown.clone()) {
                Ok(live) => {
                    host_roots.push((root, live.index.clone()));
                    live_indexes.push(live);
                }
                Err(err) => tracing::warn!("skipping {}: {err:#}", root.display()),
            }
        }
        // A stop during bootstrap: don't start serving, just drop what we
        // have (fully-bootstrapped roots save their snapshots).
        if stopping() {
            tracing::info!("stop requested during bootstrap; not serving");
            return Ok(());
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

    // --- Self-update (Windows, `updater` feature) --------------------------
    //
    // The service is LocalSystem, so it can apply an MSI silently: no UAC,
    // and (because the download comes via our own HTTPS client, not a
    // browser) no SmartScreen. The verify gate in `filex::update` is the
    // hard boundary — an MSI is only ever handed to msiexec after
    // `check_for_update` returns `Apply`, i.e. after signature verification.
    // See docs/design-distribution.md §3, §4.

    /// Where the Windows update manifest lives. **Empty until release infra
    /// (block 5) fills it** — an empty URL disables the self-updater, so the
    /// service still indexes normally; it just never self-updates.
    #[cfg(feature = "updater")]
    const MANIFEST_URL: &str =
        "https://github.com/NayanVR/filex/releases/latest/download/filex-windows.json";

    /// Hex Ed25519 public key that update payloads must verify against,
    /// from the CI-held keypair (block 0). Empty disables the self-updater.
    #[cfg(feature = "updater")]
    const UPDATE_PUBLIC_KEY: &str =
        "bfe47bb637e1f4cd98f68c236cfce1db17527c6432904e5195869a6e2903e42f";

    /// Run the on-launch update check off the serving thread.
    #[cfg(feature = "updater")]
    fn spawn_update_check(cancel: filex::update::CancelFlag) {
        std::thread::spawn(move || {
            if let Err(err) = check_and_apply_update(&cancel) {
                tracing::warn!("self-update check did not complete: {err:#}");
            }
        });
    }

    /// Check for a newer release and, if one verifies, hand it to msiexec.
    #[cfg(feature = "updater")]
    fn check_and_apply_update(cancel: &filex::update::CancelFlag) -> Result<()> {
        use filex::update::{self, CURRENT_VERSION, UpdateAction};

        if MANIFEST_URL.is_empty() || UPDATE_PUBLIC_KEY.is_empty() {
            tracing::debug!("self-update disabled: manifest URL or public key not configured");
            return Ok(());
        }

        match update::check_for_update(
            update::http_fetch,
            MANIFEST_URL,
            CURRENT_VERSION,
            UPDATE_PUBLIC_KEY,
            cancel,
        ) {
            Ok(UpdateAction::UpToDate) => {
                tracing::info!("filex-indexd is up to date ({CURRENT_VERSION})");
                Ok(())
            }
            Ok(UpdateAction::Apply { version, payload }) => {
                tracing::info!("update available: {CURRENT_VERSION} -> {version}");
                let path = stage_msi(&version, &payload)?;
                spawn_msiexec(&path)?;
                tracing::info!(
                    "handed MSI {version} to msiexec; the upgrade will restart the service"
                );
                Ok(())
            }
            Err(err) => Err(anyhow::anyhow!("{err}")),
        }
    }

    /// Write the verified MSI to a temp file for msiexec to consume.
    #[cfg(feature = "updater")]
    fn stage_msi(version: &str, bytes: &[u8]) -> Result<PathBuf> {
        use std::io::Write;
        let mut path = std::env::temp_dir();
        path.push(format!("filex-update-{version}.msi"));
        let mut file = std::fs::File::create(&path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(path)
    }

    /// Launch `msiexec` to apply the staged MSI, detached so it OUTLIVES
    /// this service.
    ///
    /// The MSI's `ServiceControl` stops filex-indexd mid-upgrade, so we must
    /// spawn without waiting and let the SCM stop us cleanly while msiexec
    /// swaps both binaries and starts the new service. `DETACHED_PROCESS`
    /// plus not waiting means dropping the `Child` here does not kill it.
    #[cfg(feature = "updater")]
    fn spawn_msiexec(msi_path: &std::path::Path) -> Result<()> {
        use std::os::windows::process::CommandExt;
        use std::process::Command;

        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

        let args = filex::update::msiexec_args(&msi_path.to_string_lossy());
        let child = Command::new("msiexec")
            .args(&args)
            .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
            .spawn()?;
        tracing::info!("spawned msiexec (pid {})", child.id());
        Ok(())
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

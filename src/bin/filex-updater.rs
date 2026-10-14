//! filex-updater — the Windows update service (LocalSystem, demand start).
//!
//! Started on demand by the app (or by the MSI right after an install),
//! serves `\\.\pipe\filex-updater`, and exits when idle. All decisions live
//! in `filex::updater::core`; this file is SCM and pipe wiring only. See
//! `docs/design-updater.md`.
//!
//! Usage:
//!   filex-updater                 run as the SCM service (the MSI's path)
//!   filex-updater --check-only    fetch + verify the latest release and
//!                                 report, installing nothing (diagnostics)

#[cfg(target_os = "windows")]
fn main() -> anyhow::Result<()> {
    service::main()
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!(
        "filex-updater is the Windows update service; macOS updates through \
         Homebrew and Linux through the release tarball."
    );
    std::process::exit(1);
}

#[cfg(target_os = "windows")]
mod service {
    use std::ffi::OsString;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use anyhow::Result;
    use windows_service::service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
        ServiceType,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
    use windows_service::{Error as ServiceError, define_windows_service, service_dispatcher};

    use filex::update::{CURRENT_VERSION, CancelFlag, Manifest, download_and_verify, http_fetch};
    use filex::updater::components::UPDATER_SERVICE;
    use filex::updater::core::{Config, Updater};
    use filex::updater::windows::effects::WindowsEffects;
    use filex::updater::windows::security::PIPE_SDDL;
    use filex::updater::windows::{pipe, scm};
    use filex::updater::{MANIFEST_URL, PIPE_NAME, UPDATE_PUBLIC_KEY};

    const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;
    /// `ERROR_FAILED_SERVICE_CONTROLLER_CONNECT`: not launched by the SCM.
    const NOT_STARTED_BY_SCM: i32 = 1063;

    fn config() -> Config {
        Config {
            manifest_url: MANIFEST_URL.into(),
            public_key: UPDATE_PUBLIC_KEY.into(),
            current_version: CURRENT_VERSION.into(),
        }
    }

    pub fn main() -> Result<()> {
        let _log = filex::diagnostics::logging::init_in("filex-updater", None);
        if std::env::args().nth(1).as_deref() == Some("--check-only") {
            return check_only();
        }
        match service_dispatcher::start(UPDATER_SERVICE, ffi_service_main) {
            Ok(()) => Ok(()),
            Err(ServiceError::Winapi(io)) if io.raw_os_error() == Some(NOT_STARTED_BY_SCM) => {
                eprintln!(
                    "filex-updater runs as a Windows service (installed by the Filex MSI).\n\
                     For diagnostics: filex-updater --check-only"
                );
                std::process::exit(2);
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Fetch and verify the latest release without installing anything.
    fn check_only() -> Result<()> {
        let cancel = CancelFlag::new();
        let json = http_fetch(MANIFEST_URL, &cancel).map_err(anyhow::Error::msg)?;
        let manifest = Manifest::parse(&String::from_utf8(json)?)?;
        println!("installed: {CURRENT_VERSION}");
        println!("latest:    {} ({})", manifest.version, manifest.url);
        let bytes = download_and_verify(http_fetch, &manifest, UPDATE_PUBLIC_KEY, &cancel)?;
        println!(
            "artifact:  {} bytes, SHA-256 and Ed25519 signature verified",
            bytes.len()
        );
        println!(
            "action:    {}",
            if manifest.is_newer_than(CURRENT_VERSION)? {
                "would install"
            } else {
                "up to date"
            }
        );
        Ok(())
    }

    define_windows_service!(ffi_service_main, service_main);

    fn service_main(_arguments: Vec<OsString>) {
        if let Err(error) = run_service() {
            tracing::error!("filex-updater failed: {error:#}");
        }
    }

    fn run_service() -> Result<()> {
        let stop = Arc::new(AtomicBool::new(false));
        let updater = Updater::new(WindowsEffects::from_current_exe()?, config());

        let event_handler = {
            let stop = stop.clone();
            let updater = updater.clone();
            move |control| match control {
                ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                ServiceControl::Stop | ServiceControl::Shutdown => {
                    stop.store(true, Ordering::SeqCst);
                    updater.cancel();
                    ServiceControlHandlerResult::NoError
                }
                _ => ServiceControlHandlerResult::NotImplemented,
            }
        };
        let status = service_control_handler::register(UPDATER_SERVICE, event_handler)?;
        let report = |state, accepted| {
            status.set_service_status(ServiceStatus {
                service_type: SERVICE_TYPE,
                current_state: state,
                controls_accepted: accepted,
                exit_code: ServiceExitCode::Win32(0),
                checkpoint: 0,
                wait_hint: Duration::from_secs(5),
                process_id: None,
            })
        };
        report(
            ServiceState::Running,
            ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
        )?;
        tracing::info!("filex-updater {CURRENT_VERSION} started");

        if let Err(error) = scm::apply_service_dacl(UPDATER_SERVICE) {
            tracing::warn!("could not grant users SERVICE_START: {error:#}");
        }
        updater.startup();

        let server = {
            let updater = updater.clone();
            let stop = stop.clone();
            std::thread::spawn(move || {
                let handler: pipe::Handler =
                    Arc::new(move |command, session| updater.handle(command, session));
                pipe::serve(PIPE_NAME, PIPE_SDDL, handler, stop)
            })
        };

        while !stop.load(Ordering::SeqCst)
            && !server.is_finished()
            && !updater.should_exit(Instant::now())
        {
            std::thread::sleep(Duration::from_millis(500));
        }
        tracing::info!("filex-updater stopping (phase {:?})", updater.phase());
        report(ServiceState::StopPending, ServiceControlAccept::empty())?;
        stop.store(true, Ordering::SeqCst);
        updater.cancel();
        pipe::wake(PIPE_NAME);
        match server.join() {
            Ok(Err(error)) => tracing::error!("updater pipe server failed: {error:#}"),
            Err(_) => tracing::error!("updater pipe server panicked"),
            Ok(Ok(())) => {}
        }
        report(ServiceState::Stopped, ServiceControlAccept::empty())?;
        Ok(())
    }
}

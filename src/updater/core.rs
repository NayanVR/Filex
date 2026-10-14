//! The updater service's brain: check → download → verify → hold → apply,
//! plus the pending-install handling a freshly started updater runs.
//!
//! Every OS interaction goes through [`Effects`], so the whole sequence —
//! including "a tampered payload never reaches the installer" — is tested
//! with a mock on any platform. The Windows implementation lives in
//! `updater::windows::effects`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};

use super::pending::{self, Outcome, PendingInstall};
use super::protocol::{Command, PROTOCOL, Phase, Response};
use crate::update::{CancelFlag, DownloadError, Manifest, UpdateError, download_and_verify};

/// Exit after this long without a request when holding nothing.
pub const IDLE_EXIT: Duration = Duration::from_secs(2 * 60);
/// Exit after this long without a request while holding a verified payload
/// (dropping it; an `Apply` after that re-downloads).
pub const READY_EXIT: Duration = Duration::from_secs(15 * 60);
/// A fresh "up to date" answer is reused for this long instead of refetching.
pub const RECHECK_AFTER: Duration = Duration::from_secs(10 * 60);

/// `ERROR_SUCCESS_REBOOT_REQUIRED` from msiexec: files are replaced at the
/// next boot.
const MSI_REBOOT_REQUIRED: i32 = 3010;

/// Everything the updater does to the outside world.
pub trait Effects: Send + Sync + 'static {
    /// Fetch a URL (manifest or artifact), polling `cancel`.
    fn fetch(&self, url: &str, cancel: &CancelFlag) -> Result<Vec<u8>, String>;
    /// Seconds since the Unix epoch.
    fn now_unix(&self) -> u64;
    fn read_pending(&self) -> Result<Option<PendingInstall>>;
    fn write_pending(&self, pending: &PendingInstall) -> Result<()>;
    /// Delete the pending record and every staged file.
    fn purge_staging(&self) -> Result<()>;
    /// Bring Filex down for an install requested from `session`.
    fn quiesce(&self, session: u32) -> Result<()>;
    /// Write `bytes` into the staging directory as `file_name`.
    fn stage(&self, file_name: &str, bytes: &[u8]) -> Result<PathBuf>;
    /// Read a staged file back (for the post-write re-verification).
    fn read_staged(&self, path: &Path) -> Result<Vec<u8>>;
    /// Run the installer on `msi` and wait for it. When the install replaces
    /// this service, the process is stopped before this returns.
    fn run_installer(&self, msi: &Path, version: &str) -> Result<i32>;
    /// Start the app in `session` with that user's own token.
    fn relaunch_app(&self, session: u32) -> Result<()>;
}

/// The trust anchors, all compiled in by the service binary.
#[derive(Debug, Clone)]
pub struct Config {
    pub manifest_url: String,
    pub public_key: String,
    pub current_version: String,
}

struct Held {
    manifest: Manifest,
    bytes: Vec<u8>,
}

struct Inner {
    phase: Phase,
    held: Option<Held>,
    /// Session that asked to install while no payload was ready yet.
    apply_session: Option<u32>,
    last_activity: Instant,
    up_to_date_at: Option<Instant>,
    worker: Option<JoinHandle<()>>,
}

/// The service core. Cheap to clone; clones share state.
pub struct Updater<E: Effects> {
    effects: Arc<E>,
    config: Arc<Config>,
    inner: Arc<Mutex<Inner>>,
    cancel: CancelFlag,
}

impl<E: Effects> Clone for Updater<E> {
    fn clone(&self) -> Self {
        Self {
            effects: self.effects.clone(),
            config: self.config.clone(),
            inner: self.inner.clone(),
            cancel: self.cancel.clone(),
        }
    }
}

/// A valid staged-installer name for `version`. Only a parsed semver gets
/// into a file name, so a hostile manifest can't steer the path.
pub fn staged_file_name(version: &str) -> Result<String> {
    let parsed = semver::Version::parse(version).context("manifest version is not semver")?;
    Ok(format!("Filex-{parsed}.msi"))
}

impl<E: Effects> Updater<E> {
    pub fn new(effects: E, config: Config) -> Self {
        Self {
            effects: Arc::new(effects),
            config: Arc::new(config),
            inner: Arc::new(Mutex::new(Inner {
                phase: Phase::Idle,
                held: None,
                apply_session: None,
                last_activity: Instant::now(),
                up_to_date_at: None,
                worker: None,
            })),
            cancel: CancelFlag::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // A panicking worker must not wedge the service: the state it left
        // is still a valid `Phase`.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn phase(&self) -> Phase {
        self.lock().phase.clone()
    }

    /// Abort an in-flight download (service stop).
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// Answer one request from a client in `session`.
    pub fn handle(&self, command: Command, session: u32) -> Response {
        let mut inner = self.lock();
        inner.last_activity = Instant::now();
        match command {
            Command::Hello => Response::Hello {
                protocol: PROTOCOL,
                version: self.config.current_version.clone(),
            },
            Command::Status => Response::Status(inner.phase.clone()),
            Command::Check => {
                if self.should_check(&inner) {
                    self.start_check(&mut inner);
                }
                Response::Status(inner.phase.clone())
            }
            Command::Apply => {
                match &inner.phase {
                    Phase::Ready { version } => {
                        inner.phase = Phase::Applying {
                            version: version.clone(),
                        };
                        self.spawn(&mut inner, move |this| this.apply(session));
                    }
                    Phase::Applying { .. } => {}
                    Phase::Checking | Phase::Downloading { .. } => {
                        inner.apply_session = Some(session);
                    }
                    Phase::Idle | Phase::UpToDate | Phase::Failed { .. } => {
                        inner.apply_session = Some(session);
                        self.start_check(&mut inner);
                    }
                }
                Response::Status(inner.phase.clone())
            }
        }
    }

    fn should_check(&self, inner: &Inner) -> bool {
        match inner.phase {
            Phase::Idle | Phase::Failed { .. } => true,
            Phase::UpToDate => inner
                .up_to_date_at
                .is_none_or(|at| at.elapsed() >= RECHECK_AFTER),
            _ => false,
        }
    }

    fn start_check(&self, inner: &mut Inner) {
        inner.phase = Phase::Checking;
        self.spawn(inner, |this| this.check());
    }

    fn spawn(&self, inner: &mut Inner, work: impl FnOnce(&Self) + Send + 'static) {
        let this = self.clone();
        inner.worker = Some(std::thread::spawn(move || work(&this)));
    }

    /// Wait for the current worker (tests, and the service's stop path).
    pub fn join_worker(&self) {
        let worker = self.lock().worker.take();
        if let Some(worker) = worker {
            let _ = worker.join();
        }
    }

    // --- check -------------------------------------------------------------

    fn check(&self) {
        let result = self.fetch_verified();
        let mut inner = self.lock();
        inner.last_activity = Instant::now();
        match result {
            Ok(None) => {
                inner.phase = Phase::UpToDate;
                inner.up_to_date_at = Some(Instant::now());
                inner.apply_session = None;
            }
            Ok(Some(held)) => {
                let version = held.manifest.version.clone();
                inner.held = Some(held);
                if let Some(session) = inner.apply_session.take() {
                    inner.phase = Phase::Applying { version };
                    drop(inner);
                    self.apply(session);
                } else {
                    inner.phase = Phase::Ready { version };
                }
            }
            Err(error) => {
                tracing::warn!("update check failed: {error}");
                inner.phase = Phase::Failed {
                    message: error.to_string(),
                };
                inner.apply_session = None;
            }
        }
    }

    /// Fetch the manifest and, if newer, the verified payload.
    fn fetch_verified(&self) -> Result<Option<Held>, DownloadError> {
        let manifest_bytes = self
            .effects
            .fetch(&self.config.manifest_url, &self.cancel)
            .map_err(|e| self.classify(e))?;
        let json = String::from_utf8(manifest_bytes).map_err(|_| {
            DownloadError::Manifest(UpdateError::Parse("manifest is not UTF-8".into()))
        })?;
        let manifest = Manifest::parse(&json).map_err(DownloadError::Manifest)?;
        if !manifest
            .is_newer_than(&self.config.current_version)
            .map_err(DownloadError::Manifest)?
        {
            return Ok(None);
        }
        self.lock().phase = Phase::Downloading {
            version: manifest.version.clone(),
        };
        let effects = self.effects.clone();
        let bytes = download_and_verify(
            |url, cancel| effects.fetch(url, cancel),
            &manifest,
            &self.config.public_key,
            &self.cancel,
        )?;
        Ok(Some(Held { manifest, bytes }))
    }

    fn classify(&self, message: String) -> DownloadError {
        if self.cancel.is_cancelled() {
            DownloadError::Cancelled
        } else {
            DownloadError::Network(message)
        }
    }

    // --- apply -------------------------------------------------------------

    fn apply(&self, session: u32) {
        let held = self.lock().held.take();
        let result = match held {
            Some(held) => self.install(session, &held),
            None => Err(anyhow!("no verified update is held")),
        };
        // Reaching here means this process was not replaced: msiexec failed
        // or never ran. Settle the pending record ourselves.
        let failure = match result {
            Ok(0) => None,
            Ok(MSI_REBOOT_REQUIRED) => Some("Restart Windows to finish updating Filex".to_string()),
            Ok(code) => Some(format!("the installer exited with code {code}")),
            Err(error) => {
                tracing::error!("update install failed: {error:#}");
                Some(format!("{error:#}"))
            }
        };
        self.settle_pending();
        let mut inner = self.lock();
        inner.last_activity = Instant::now();
        inner.phase = match failure {
            Some(message) => Phase::Failed { message },
            None => Phase::Idle,
        };
    }

    /// The ordered install: record → quiesce → stage → re-verify → run.
    fn install(&self, session: u32, held: &Held) -> Result<i32> {
        let version = &held.manifest.version;
        let file_name = staged_file_name(version)?;
        self.effects.write_pending(&PendingInstall {
            from_version: self.config.current_version.clone(),
            to_version: version.clone(),
            session,
            requested_at: self.effects.now_unix(),
        })?;
        if let Err(error) = self.effects.quiesce(session) {
            // Restart Manager inside msiexec is the backstop for anything
            // still holding a file; carry on.
            tracing::warn!("quiescing Filex before the install was incomplete: {error:#}");
        }
        let path = self.effects.stage(&file_name, &held.bytes)?;
        let staged = self.effects.read_staged(&path)?;
        held.manifest
            .verify_payload(&staged, &self.config.public_key)
            .context("staged installer failed re-verification")?;
        tracing::info!("installing Filex {version} from {}", path.display());
        self.effects.run_installer(&path, version)
    }

    // --- startup -------------------------------------------------------------

    /// Run once when the service starts: settle an install the previous
    /// updater handed to msiexec, and clear any leftover staging files.
    pub fn startup(&self) {
        if let Some(Outcome::NotInstalled { expected }) = self.settle_pending() {
            self.lock().phase = Phase::Failed {
                message: format!("Filex {expected} was not installed"),
            };
        }
    }

    /// Relaunch the app if a recent install is pending, then purge staging.
    /// Returns the install outcome when there was a record.
    fn settle_pending(&self) -> Option<Outcome> {
        let pending = match self.effects.read_pending() {
            Ok(pending) => pending,
            Err(error) => {
                tracing::warn!("unreadable pending-install record: {error:#}");
                None
            }
        };
        let outcome = pending.map(|pending| {
            let action = pending::startup_action(
                &pending,
                &self.config.current_version,
                self.effects.now_unix(),
            );
            tracing::info!(
                "pending install {} -> {}: {:?}",
                pending.from_version,
                pending.to_version,
                action.outcome
            );
            if let Some(session) = action.relaunch
                && let Err(error) = self.effects.relaunch_app(session)
            {
                tracing::warn!("relaunching Filex in session {session} failed: {error:#}");
            }
            action.outcome
        });
        if let Err(error) = self.effects.purge_staging() {
            tracing::warn!("purging update staging failed: {error:#}");
        }
        outcome
    }

    // --- lifetime ------------------------------------------------------------

    /// Whether the service should exit now: never while busy; otherwise after
    /// [`IDLE_EXIT`] without a request, or [`READY_EXIT`] while holding a
    /// payload.
    pub fn should_exit(&self, now: Instant) -> bool {
        let inner = self.lock();
        if inner.phase.is_busy() || inner.apply_session.is_some() {
            return false;
        }
        let limit = if matches!(inner.phase, Phase::Ready { .. }) {
            READY_EXIT
        } else {
            IDLE_EXIT
        };
        now.saturating_duration_since(inner.last_activity) >= limit
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::sign_artifact;
    use std::collections::HashMap;

    const MANIFEST_URL: &str = "https://example.test/filex-windows.json";
    const ARTIFACT_URL: &str = "https://example.test/Filex-2.0.0-x64.msi";
    const PRIVATE_KEY: &str = "0707070707070707070707070707070707070707070707070707070707070707";

    fn public_key() -> String {
        let key = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        key.verifying_key()
            .as_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn manifest_for(version: &str, artifact: &[u8]) -> String {
        let (sha256, signature) = sign_artifact(PRIVATE_KEY, artifact).unwrap();
        serde_json::to_string(&Manifest {
            version: version.into(),
            url: ARTIFACT_URL.into(),
            sha256,
            signature,
            minimum_version: None,
        })
        .unwrap()
    }

    #[derive(Default)]
    struct Mock {
        routes: Mutex<HashMap<String, Vec<u8>>>,
        calls: Mutex<Vec<String>>,
        pending: Mutex<Option<PendingInstall>>,
        staged: Mutex<HashMap<PathBuf, Vec<u8>>>,
        installer_exit: Mutex<i32>,
        /// Corrupt the staged bytes as they are written (models tampering
        /// between write and msiexec).
        corrupt_stage: Mutex<bool>,
        now: Mutex<u64>,
    }

    impl Mock {
        fn log(&self, entry: impl Into<String>) {
            self.calls.lock().unwrap().push(entry.into());
        }
    }

    impl Effects for Arc<Mock> {
        fn fetch(&self, url: &str, _: &CancelFlag) -> Result<Vec<u8>, String> {
            self.log(format!("fetch {url}"));
            self.routes
                .lock()
                .unwrap()
                .get(url)
                .cloned()
                .ok_or_else(|| format!("404 {url}"))
        }
        fn now_unix(&self) -> u64 {
            *self.now.lock().unwrap()
        }
        fn read_pending(&self) -> Result<Option<PendingInstall>> {
            Ok(self.pending.lock().unwrap().clone())
        }
        fn write_pending(&self, pending: &PendingInstall) -> Result<()> {
            self.log(format!("write_pending {}", pending.to_version));
            *self.pending.lock().unwrap() = Some(pending.clone());
            Ok(())
        }
        fn purge_staging(&self) -> Result<()> {
            self.log("purge");
            *self.pending.lock().unwrap() = None;
            self.staged.lock().unwrap().clear();
            Ok(())
        }
        fn quiesce(&self, session: u32) -> Result<()> {
            self.log(format!("quiesce {session}"));
            Ok(())
        }
        fn stage(&self, file_name: &str, bytes: &[u8]) -> Result<PathBuf> {
            self.log(format!("stage {file_name}"));
            let mut bytes = bytes.to_vec();
            if *self.corrupt_stage.lock().unwrap() {
                bytes[0] ^= 0xff;
            }
            let path = PathBuf::from(file_name);
            self.staged.lock().unwrap().insert(path.clone(), bytes);
            Ok(path)
        }
        fn read_staged(&self, path: &Path) -> Result<Vec<u8>> {
            self.staged
                .lock()
                .unwrap()
                .get(path)
                .cloned()
                .ok_or_else(|| anyhow!("not staged"))
        }
        fn run_installer(&self, msi: &Path, version: &str) -> Result<i32> {
            self.log(format!("install {} {version}", msi.display()));
            Ok(*self.installer_exit.lock().unwrap())
        }
        fn relaunch_app(&self, session: u32) -> Result<()> {
            self.log(format!("relaunch {session}"));
            Ok(())
        }
    }

    fn setup(release: Option<(&str, &[u8])>) -> (Updater<Arc<Mock>>, Arc<Mock>) {
        let mock = Arc::new(Mock::default());
        *mock.now.lock().unwrap() = 1_000;
        if let Some((version, artifact)) = release {
            let mut routes = mock.routes.lock().unwrap();
            routes.insert(
                MANIFEST_URL.into(),
                manifest_for(version, artifact).into_bytes(),
            );
            routes.insert(ARTIFACT_URL.into(), artifact.to_vec());
        }
        let updater = Updater::new(
            mock.clone(),
            Config {
                manifest_url: MANIFEST_URL.into(),
                public_key: public_key(),
                current_version: "1.0.0".into(),
            },
        );
        (updater, mock)
    }

    fn calls(mock: &Mock) -> Vec<String> {
        mock.calls.lock().unwrap().clone()
    }

    #[test]
    fn up_to_date_never_downloads_the_artifact() {
        let (updater, mock) = setup(Some(("1.0.0", b"same")));
        updater.handle(Command::Check, 1);
        updater.join_worker();
        assert_eq!(updater.phase(), Phase::UpToDate);
        assert_eq!(calls(&mock), vec![format!("fetch {MANIFEST_URL}")]);
    }

    #[test]
    fn newer_release_is_held_ready_without_touching_disk() {
        let (updater, mock) = setup(Some(("2.0.0", b"installer bytes")));
        updater.handle(Command::Check, 1);
        updater.join_worker();
        assert_eq!(
            updater.phase(),
            Phase::Ready {
                version: "2.0.0".into()
            }
        );
        assert!(calls(&mock).iter().all(|c| c.starts_with("fetch ")));
    }

    #[test]
    fn apply_runs_record_quiesce_stage_install_in_order() {
        let (updater, mock) = setup(Some(("2.0.0", b"installer bytes")));
        *mock.installer_exit.lock().unwrap() = 1603;
        updater.handle(Command::Check, 1);
        updater.join_worker();
        let response = updater.handle(Command::Apply, 7);
        assert_eq!(
            response,
            Response::Status(Phase::Applying {
                version: "2.0.0".into()
            })
        );
        updater.join_worker();
        let calls = calls(&mock);
        let after_fetch: Vec<_> = calls.iter().skip(2).map(String::as_str).collect();
        assert_eq!(
            after_fetch,
            vec![
                "write_pending 2.0.0",
                "quiesce 7",
                "stage Filex-2.0.0.msi",
                "install Filex-2.0.0.msi 2.0.0",
                // msiexec failed and we're still alive: give the user their
                // window back and clean up.
                "relaunch 7",
                "purge",
            ]
        );
        assert!(matches!(updater.phase(), Phase::Failed { .. }));
    }

    #[test]
    fn tampered_artifact_never_reaches_the_installer() {
        let (updater, mock) = setup(Some(("2.0.0", b"genuine")));
        mock.routes
            .lock()
            .unwrap()
            .insert(ARTIFACT_URL.into(), b"evil".to_vec());
        updater.handle(Command::Apply, 3);
        updater.join_worker();
        assert!(matches!(updater.phase(), Phase::Failed { .. }));
        let calls = calls(&mock);
        assert!(
            calls
                .iter()
                .all(|c| !c.starts_with("stage") && !c.starts_with("install")),
            "{calls:?}"
        );
    }

    #[test]
    fn bytes_changed_after_staging_are_not_installed() {
        let (updater, mock) = setup(Some(("2.0.0", b"installer bytes")));
        *mock.corrupt_stage.lock().unwrap() = true;
        updater.handle(Command::Check, 1);
        updater.join_worker();
        updater.handle(Command::Apply, 1);
        updater.join_worker();
        assert!(calls(&mock).iter().all(|c| !c.starts_with("install")));
        assert!(matches!(updater.phase(), Phase::Failed { .. }));
    }

    #[test]
    fn apply_before_ready_checks_then_installs() {
        let (updater, mock) = setup(Some(("2.0.0", b"installer bytes")));
        updater.handle(Command::Apply, 4);
        updater.join_worker();
        assert!(
            calls(&mock)
                .iter()
                .any(|c| c == "install Filex-2.0.0.msi 2.0.0")
        );
    }

    #[test]
    fn apply_with_nothing_newer_installs_nothing() {
        let (updater, mock) = setup(Some(("1.0.0", b"same")));
        updater.handle(Command::Apply, 4);
        updater.join_worker();
        assert_eq!(updater.phase(), Phase::UpToDate);
        assert!(calls(&mock).iter().all(|c| !c.starts_with("install")));
        assert!(updater.should_exit(Instant::now() + IDLE_EXIT));
    }

    #[test]
    fn fresh_up_to_date_answer_is_reused() {
        let (updater, mock) = setup(Some(("1.0.0", b"same")));
        updater.handle(Command::Check, 1);
        updater.join_worker();
        updater.handle(Command::Check, 1);
        updater.join_worker();
        assert_eq!(calls(&mock).len(), 1);
    }

    #[test]
    fn network_failure_is_reported_and_retryable() {
        let (updater, mock) = setup(None);
        updater.handle(Command::Check, 1);
        updater.join_worker();
        assert!(matches!(updater.phase(), Phase::Failed { .. }));
        updater.handle(Command::Check, 1);
        updater.join_worker();
        assert_eq!(calls(&mock).len(), 2);
    }

    #[test]
    fn startup_settles_a_successful_install() {
        let (updater, mock) = setup(None);
        *mock.pending.lock().unwrap() = Some(PendingInstall {
            from_version: "0.9.0".into(),
            to_version: "1.0.0".into(),
            session: 5,
            requested_at: 990,
        });
        updater.startup();
        assert_eq!(calls(&mock), vec!["relaunch 5", "purge"]);
        assert_eq!(updater.phase(), Phase::Idle);
    }

    #[test]
    fn startup_reports_a_rolled_back_install() {
        let (updater, mock) = setup(None);
        *mock.pending.lock().unwrap() = Some(PendingInstall {
            from_version: "1.0.0".into(),
            to_version: "2.0.0".into(),
            session: 5,
            requested_at: 990,
        });
        updater.startup();
        assert!(matches!(updater.phase(), Phase::Failed { .. }));
        assert_eq!(calls(&mock), vec!["relaunch 5", "purge"]);
    }

    #[test]
    fn startup_without_a_record_just_purges() {
        let (updater, mock) = setup(None);
        updater.startup();
        assert_eq!(calls(&mock), vec!["purge"]);
    }

    #[test]
    fn idle_exit_timing() {
        let (updater, _) = setup(Some(("2.0.0", b"installer bytes")));
        let now = Instant::now();
        assert!(!updater.should_exit(now));
        assert!(updater.should_exit(now + IDLE_EXIT + Duration::from_secs(1)));
        updater.handle(Command::Check, 1);
        updater.join_worker();
        // Holding a payload: stay up longer.
        let now = Instant::now();
        assert!(!updater.should_exit(now + IDLE_EXIT + Duration::from_secs(1)));
        assert!(updater.should_exit(now + READY_EXIT + Duration::from_secs(1)));
    }

    #[test]
    fn staged_names_only_come_from_semver() {
        assert_eq!(staged_file_name("1.4.0").unwrap(), "Filex-1.4.0.msi");
        assert_eq!(
            staged_file_name("1.4.0-rc.1").unwrap(),
            "Filex-1.4.0-rc.1.msi"
        );
        assert!(staged_file_name("..\\..\\evil").is_err());
        assert!(staged_file_name("1.0.0/../../x").is_err());
    }

    #[test]
    fn hello_reports_protocol_and_version() {
        let (updater, _) = setup(None);
        assert_eq!(
            updater.handle(Command::Hello, 1),
            Response::Hello {
                protocol: PROTOCOL,
                version: "1.0.0".into()
            }
        );
    }
}

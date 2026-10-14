//! The real [`Effects`] for the Windows service.
//!
//! Assumes the service binary lives in the install directory (`Program
//! Files\Filex`), whose ACL — inherited from Program Files — lets only
//! administrators and SYSTEM write. The staging directory `updates\` sits
//! inside it for exactly that reason; `%TEMP%` and `%ProgramData%` are
//! user-writable or pre-creatable and would invite file-swap and junction
//! attacks against a SYSTEM writer.

use std::io::Write;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use windows::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS};

use super::{process, scm, session};
use crate::update::CancelFlag;
use crate::updater::components::{self, COMPONENTS, QuiesceStep};
use crate::updater::core::Effects;
use crate::updater::pending::{self, PendingInstall};

/// How long the requesting user's app gets to quit on its own.
const APP_EXIT_GRACE: Duration = Duration::from_secs(10);
/// How long a sibling service gets to stop.
const SERVICE_STOP_TIMEOUT: Duration = Duration::from_secs(30);

pub struct WindowsEffects {
    install_dir: PathBuf,
    staging_dir: PathBuf,
    log_dir: Option<PathBuf>,
}

impl WindowsEffects {
    /// Effects rooted at the running executable's directory.
    pub fn from_current_exe() -> Result<Self> {
        let exe = std::env::current_exe().context("locating filex-updater.exe")?;
        let install_dir = exe
            .parent()
            .context("filex-updater.exe has no parent directory")?
            .to_path_buf();
        Ok(Self::new(install_dir))
    }

    pub fn new(install_dir: PathBuf) -> Self {
        Self {
            staging_dir: install_dir.join("updates"),
            install_dir,
            // LocalSystem's profile: admin-only, unlike ProgramData.
            log_dir: dirs::data_local_dir().map(|d| d.join("filex").join("logs")),
        }
    }

    fn pending_path(&self) -> PathBuf {
        self.staging_dir.join(pending::FILE_NAME)
    }

    fn msiexec() -> PathBuf {
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
        Path::new(&root).join("System32").join("msiexec.exe")
    }

    fn run_step(&self, step: &QuiesceStep, session: u32) -> Result<()> {
        match *step {
            QuiesceStep::AwaitAppExit { binary } => {
                let exited = process::wait_for_exit_in_session(
                    &self.install_dir,
                    binary,
                    session,
                    APP_EXIT_GRACE,
                )?;
                if !exited {
                    tracing::warn!("{binary} in session {session} did not exit in time");
                }
            }
            QuiesceStep::Terminate { binary } => {
                process::terminate_all(&self.install_dir, binary)?;
            }
            QuiesceStep::StopService { name } => {
                scm::stop_and_wait(name, SERVICE_STOP_TIMEOUT)?;
            }
        }
        Ok(())
    }
}

fn write_synced(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file =
        std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

impl Effects for WindowsEffects {
    fn fetch(&self, url: &str, cancel: &CancelFlag) -> Result<Vec<u8>, String> {
        crate::update::http_fetch(url, cancel)
    }

    fn now_unix(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    }

    fn read_pending(&self) -> Result<Option<PendingInstall>> {
        match std::fs::read(self.pending_path()) {
            Ok(bytes) => PendingInstall::from_json(&bytes).map(Some),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).context("reading the pending-install record"),
        }
    }

    fn write_pending(&self, pending: &PendingInstall) -> Result<()> {
        std::fs::create_dir_all(&self.staging_dir)?;
        write_synced(&self.pending_path(), &pending.to_json()?)
    }

    fn purge_staging(&self) -> Result<()> {
        match std::fs::remove_dir_all(&self.staging_dir) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                Err(error).with_context(|| format!("removing {}", self.staging_dir.display()))
            }
            _ => Ok(()),
        }
    }

    fn quiesce(&self, session: u32) -> Result<()> {
        let mut first_error = None;
        for step in components::quiesce_plan(COMPONENTS) {
            if let Err(error) = self.run_step(&step, session) {
                tracing::warn!("quiesce step {step:?} failed: {error:#}");
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    fn stage(&self, file_name: &str, bytes: &[u8]) -> Result<PathBuf> {
        std::fs::create_dir_all(&self.staging_dir)?;
        let path = self.staging_dir.join(file_name);
        write_synced(&path, bytes)?;
        Ok(path)
    }

    fn read_staged(&self, path: &Path) -> Result<Vec<u8>> {
        std::fs::read(path).with_context(|| format!("reading back {}", path.display()))
    }

    fn run_installer(&self, msi: &Path, version: &str) -> Result<i32> {
        let log = self.log_dir.as_ref().map(|dir| {
            let _ = std::fs::create_dir_all(dir);
            dir.join(format!("msiexec-{version}.log"))
        });
        let args = crate::update::msiexec_args(
            &msi.to_string_lossy(),
            log.as_ref().map(|p| p.to_string_lossy()).as_deref(),
        );
        // Detached: the MSI's ServiceControl stops this service mid-install,
        // and msiexec must outlive it.
        let mut child = std::process::Command::new(Self::msiexec())
            .args(&args)
            .creation_flags(DETACHED_PROCESS.0 | CREATE_NEW_PROCESS_GROUP.0)
            .spawn()
            .context("starting msiexec")?;
        tracing::info!("msiexec started (pid {}) for Filex {version}", child.id());
        let status = child.wait().context("waiting for msiexec")?;
        Ok(status.code().unwrap_or(-1))
    }

    fn relaunch_app(&self, session: u32) -> Result<()> {
        let app = components::app(COMPONENTS).context("no app component registered")?;
        session::launch(session, &self.install_dir.join(app.binary))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staging_and_pending_live_under_the_install_dir_and_purge_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let effects = WindowsEffects::new(dir.path().to_path_buf());
        assert_eq!(effects.read_pending().unwrap(), None);
        let record = PendingInstall {
            from_version: "1.0.0".into(),
            to_version: "2.0.0".into(),
            session: 1,
            requested_at: 5,
        };
        effects.write_pending(&record).unwrap();
        assert_eq!(effects.read_pending().unwrap(), Some(record));
        let staged = effects.stage("Filex-2.0.0.msi", b"msi").unwrap();
        assert_eq!(staged, dir.path().join("updates").join("Filex-2.0.0.msi"));
        assert_eq!(effects.read_staged(&staged).unwrap(), b"msi");
        effects.purge_staging().unwrap();
        assert!(!dir.path().join("updates").exists());
        // Purging twice is fine.
        effects.purge_staging().unwrap();
    }

    #[test]
    fn msiexec_comes_from_system32() {
        let path = WindowsEffects::msiexec();
        assert!(
            path.ends_with(r"System32\msiexec.exe"),
            "{}",
            path.display()
        );
    }
}

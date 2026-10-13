//! Windows plumbing for `filex-updater`: the pipe transport, SCM helpers,
//! process control, session relaunch, and the real `Effects`.

pub mod client;
pub mod effects;
pub mod pipe;
pub mod process;
pub mod scm;
pub mod security;
pub mod session;

#[cfg(test)]
mod tests {
    //! The service binary's wiring — pipe server in front of the core —
    //! exercised end to end with a scripted `Effects` (no SCM, no install).

    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use anyhow::Result;

    use super::pipe;
    use super::security::PIPE_SDDL;
    use crate::update::{CancelFlag, Manifest, sign_artifact};
    use crate::updater::core::{Config, Effects, Updater};
    use crate::updater::pending::PendingInstall;
    use crate::updater::protocol::{Command, Phase, Response};

    const MANIFEST: &str = "https://example.test/m.json";
    const ARTIFACT: &str = "https://example.test/a.msi";
    const KEY: &str = "0909090909090909090909090909090909090909090909090909090909090909";

    #[derive(Default)]
    struct Scripted {
        installs: Mutex<Vec<(PathBuf, String)>>,
        quiesced: Mutex<Vec<u32>>,
    }

    impl Effects for Arc<Scripted> {
        fn fetch(&self, url: &str, _: &CancelFlag) -> Result<Vec<u8>, String> {
            let artifact = b"the msi".to_vec();
            match url {
                MANIFEST => {
                    let (sha256, signature) = sign_artifact(KEY, &artifact).unwrap();
                    Ok(serde_json::to_vec(&Manifest {
                        version: "9.0.0".into(),
                        url: ARTIFACT.into(),
                        sha256,
                        signature,
                        minimum_version: None,
                    })
                    .unwrap())
                }
                ARTIFACT => Ok(artifact),
                _ => Err("404".into()),
            }
        }
        fn now_unix(&self) -> u64 {
            0
        }
        fn read_pending(&self) -> Result<Option<PendingInstall>> {
            Ok(None)
        }
        fn write_pending(&self, _: &PendingInstall) -> Result<()> {
            Ok(())
        }
        fn purge_staging(&self) -> Result<()> {
            Ok(())
        }
        fn quiesce(&self, session: u32) -> Result<()> {
            self.quiesced.lock().unwrap().push(session);
            Ok(())
        }
        fn stage(&self, name: &str, _: &[u8]) -> Result<PathBuf> {
            Ok(PathBuf::from(name))
        }
        fn read_staged(&self, _: &Path) -> Result<Vec<u8>> {
            Ok(b"the msi".to_vec())
        }
        fn run_installer(&self, msi: &Path, version: &str) -> Result<i32> {
            self.installs
                .lock()
                .unwrap()
                .push((msi.to_path_buf(), version.to_string()));
            Ok(1603)
        }
        fn relaunch_app(&self, _: u32) -> Result<()> {
            Ok(())
        }
    }

    fn status(name: &str) -> Phase {
        match pipe::call(name, Command::Status, None, Duration::from_secs(5)).unwrap() {
            Response::Status(phase) => phase,
            other => panic!("unexpected {other:?}"),
        }
    }

    fn wait_for(name: &str, done: impl Fn(&Phase) -> bool) -> Phase {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let phase = status(name);
            if done(&phase) {
                return phase;
            }
            assert!(Instant::now() < deadline, "stuck at {phase:?}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn check_then_apply_over_the_pipe_installs_in_the_callers_session() {
        let public_key: String = ed25519_dalek::SigningKey::from_bytes(&[9u8; 32])
            .verifying_key()
            .as_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let effects = Arc::new(Scripted::default());
        let updater = Updater::new(
            effects.clone(),
            Config {
                manifest_url: MANIFEST.into(),
                public_key,
                current_version: "1.0.0".into(),
            },
        );
        let name = format!(r"\\.\pipe\filex-updater-test-e2e-{}", std::process::id());
        let stop = Arc::new(AtomicBool::new(false));
        let server = {
            let (name, stop, updater) = (name.clone(), stop.clone(), updater.clone());
            std::thread::spawn(move || {
                let handler: pipe::Handler =
                    Arc::new(move |command, session| updater.handle(command, session));
                pipe::serve(&name, PIPE_SDDL, handler, stop)
            })
        };

        pipe::call(&name, Command::Check, None, Duration::from_secs(5)).unwrap();
        let ready = wait_for(&name, |p| !p.is_busy());
        assert_eq!(
            ready,
            Phase::Ready {
                version: "9.0.0".into()
            }
        );
        pipe::call(&name, Command::Apply, None, Duration::from_secs(5)).unwrap();
        let settled = wait_for(&name, |p| matches!(p, Phase::Failed { .. }));
        assert!(matches!(settled, Phase::Failed { message } if message.contains("1603")));

        let mut session = 0u32;
        // SAFETY: querying our own session id.
        unsafe {
            windows::Win32::System::RemoteDesktop::ProcessIdToSessionId(
                std::process::id(),
                &mut session,
            )
        }
        .unwrap();
        assert_eq!(*effects.quiesced.lock().unwrap(), vec![session]);
        assert_eq!(
            *effects.installs.lock().unwrap(),
            vec![(PathBuf::from("Filex-9.0.0.msi"), "9.0.0".to_string())]
        );

        stop.store(true, Ordering::SeqCst);
        pipe::wake(&name);
        server.join().unwrap().unwrap();
    }
}

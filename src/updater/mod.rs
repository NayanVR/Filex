//! `filex-updater`: the demand-started Windows service that installs
//! verified Filex releases silently. See `docs/design-updater.md`.
//!
//! Layout:
//! - [`components`] — what the MSI ships and how each part is quiesced
//!   (the extensibility point for future services);
//! - [`protocol`] — the closed app ↔ updater command set;
//! - [`core`] — check/download/verify/apply, behind a mockable `Effects`;
//! - [`pending`] — the record that survives the install;
//! - `windows` — pipe, SCM, process and session plumbing (Windows +
//!   `updater` feature only).
//!
//! Everything except `windows` is plain logic, tested on every OS.

pub mod components;
pub mod core;
pub mod pending;
pub mod protocol;

#[cfg(all(target_os = "windows", feature = "updater"))]
pub mod windows;

use crate::update::{UpdateAffordance, UpdateStatus};
use protocol::Phase;

/// The Windows update manifest, at GitHub's stable "latest release" URL.
pub const MANIFEST_URL: &str =
    "https://github.com/NayanVR/filex/releases/latest/download/filex-windows.json";

/// Hex Ed25519 public key release artifacts are signed with (the private
/// half is the CI secret `FILEX_SIGNING_KEY`). Rotation: FIL-12.
pub const UPDATE_PUBLIC_KEY: &str =
    "bfe47bb637e1f4cd98f68c236cfce1db17527c6432904e5195869a6e2903e42f";

/// The named pipe the service listens on.
pub const PIPE_NAME: &str = r"\\.\pipe\filex-updater";

/// What the app's banner shows for an updater phase. Checking, up-to-date
/// and failures stay quiet (a failed check retries next launch).
pub fn ui_status(phase: &Phase) -> UpdateStatus {
    match phase {
        Phase::Idle | Phase::Checking | Phase::UpToDate => UpdateStatus::Idle,
        Phase::Failed { .. } => UpdateStatus::Failed,
        Phase::Downloading { version } => UpdateStatus::Downloading {
            version: version.clone(),
        },
        Phase::Ready { version } => UpdateStatus::Available {
            version: version.clone(),
            affordance: UpdateAffordance::Restart,
        },
        Phase::Applying { version } => UpdateStatus::Installing {
            version: version.clone(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phases_map_to_banner_states() {
        assert_eq!(ui_status(&Phase::Checking), UpdateStatus::Idle);
        assert_eq!(
            ui_status(&Phase::Failed {
                message: "x".into()
            }),
            UpdateStatus::Failed
        );
        assert_eq!(
            ui_status(&Phase::Ready {
                version: "2.0.0".into()
            }),
            UpdateStatus::Available {
                version: "2.0.0".into(),
                affordance: UpdateAffordance::Restart
            }
        );
        assert_eq!(
            ui_status(&Phase::Applying {
                version: "2.0.0".into()
            }),
            UpdateStatus::Installing {
                version: "2.0.0".into()
            }
        );
    }

    #[test]
    fn public_key_is_a_valid_ed25519_key() {
        let bytes: Vec<u8> = (0..UPDATE_PUBLIC_KEY.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&UPDATE_PUBLIC_KEY[i..i + 2], 16).unwrap())
            .collect();
        let array: [u8; 32] = bytes.as_slice().try_into().unwrap();
        assert!(ed25519_dalek::VerifyingKey::from_bytes(&array).is_ok());
    }
}

//! The record that survives the install: written by the old updater just
//! before msiexec runs, read by whichever updater starts next (the new one
//! the MSI starts, or the old one after a rollback / failed msiexec).

use serde::{Deserialize, Serialize};

/// How long after the request a relaunch still makes sense. Past this, the
/// user has long moved on (or rebooted); bringing a window up unannounced
/// would be a surprise, so only the outcome is recorded.
pub const RELAUNCH_WINDOW_SECS: u64 = 10 * 60;

/// File name of the record inside the staging directory.
pub const FILE_NAME: &str = "pending.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingInstall {
    /// The version that was running when the install was requested.
    pub from_version: String,
    /// The version being installed.
    pub to_version: String,
    /// Windows session id of the user who clicked Restart.
    pub session: u32,
    /// Seconds since the Unix epoch when the install was requested.
    pub requested_at: u64,
}

/// Whether the install landed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The running updater is the target version.
    Updated,
    /// Still on another version: msiexec failed or rolled back.
    NotInstalled { expected: String },
}

/// What a starting updater does with a pending record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupAction {
    /// Session to relaunch the app in, if the request is recent enough.
    pub relaunch: Option<u32>,
    pub outcome: Outcome,
}

/// Decide what to do with a pending record. Pure — `current` is the running
/// updater's version and `now` the Unix time. The app is relaunched whatever
/// the outcome: the updater closed it, so it owes the user a window back.
pub fn startup_action(pending: &PendingInstall, current: &str, now: u64) -> StartupAction {
    let outcome = if same_version(&pending.to_version, current) {
        Outcome::Updated
    } else {
        Outcome::NotInstalled {
            expected: pending.to_version.clone(),
        }
    };
    // A record from the future (clock change) is treated as stale rather
    // than trusted indefinitely.
    let recent = now >= pending.requested_at && now - pending.requested_at <= RELAUNCH_WINDOW_SECS;
    StartupAction {
        relaunch: recent.then_some(pending.session),
        outcome,
    }
}

fn same_version(a: &str, b: &str) -> bool {
    match (semver::Version::parse(a), semver::Version::parse(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

impl PendingInstall {
    pub fn to_json(&self) -> anyhow::Result<Vec<u8>> {
        Ok(serde_json::to_vec_pretty(self)?)
    }

    pub fn from_json(bytes: &[u8]) -> anyhow::Result<Self> {
        Ok(serde_json::from_slice(bytes)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(to: &str, at: u64) -> PendingInstall {
        PendingInstall {
            from_version: "0.3.5".into(),
            to_version: to.into(),
            session: 2,
            requested_at: at,
        }
    }

    #[test]
    fn installed_target_relaunches_and_reports_updated() {
        let action = startup_action(&record("0.4.0", 1_000), "0.4.0", 1_030);
        assert_eq!(action.relaunch, Some(2));
        assert_eq!(action.outcome, Outcome::Updated);
    }

    #[test]
    fn failed_install_still_relaunches_the_closed_app() {
        let action = startup_action(&record("0.4.0", 1_000), "0.3.5", 1_030);
        assert_eq!(action.relaunch, Some(2));
        assert_eq!(
            action.outcome,
            Outcome::NotInstalled {
                expected: "0.4.0".into()
            }
        );
    }

    #[test]
    fn stale_or_future_records_do_not_relaunch() {
        let stale = startup_action(
            &record("0.4.0", 1_000),
            "0.4.0",
            1_000 + RELAUNCH_WINDOW_SECS + 1,
        );
        assert_eq!(stale.relaunch, None);
        assert_eq!(stale.outcome, Outcome::Updated);
        let future = startup_action(&record("0.4.0", 5_000), "0.4.0", 1_000);
        assert_eq!(future.relaunch, None);
    }

    #[test]
    fn version_comparison_is_semantic() {
        // Build metadata differs textually but not in precedence-equality
        // terms only when identical; a plain equal version must match.
        let action = startup_action(&record("1.0.0", 0), "1.0.0", 0);
        assert_eq!(action.outcome, Outcome::Updated);
    }

    #[test]
    fn round_trips_through_json() {
        let original = record("0.4.0", 42);
        let parsed = PendingInstall::from_json(&original.to_json().unwrap()).unwrap();
        assert_eq!(parsed, original);
        assert!(PendingInstall::from_json(b"{not json").is_err());
    }
}

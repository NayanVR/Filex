//! The binaries the Windows MSI ships, and how the updater quiesces each one
//! before handing the MSI to msiexec. This is the extensibility point for
//! future services: add an entry here, a WiX component in `wix/main.wxs`, and
//! a build line in `release.yml` — the drift tests below fail until all three
//! agree.

/// How a component is brought down before an install.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The GUI, one per interactive session. Asked to quit (it quits itself
    /// once it sees `Applying`); terminated after the grace period.
    App,
    /// A per-user background process. Terminated — it must be crash-safe
    /// (the index daemon's WAL is its contract).
    UserProcess,
    /// A machine-wide SCM service, stopped through the SCM.
    Service {
        /// SCM service name; must match the WiX `ServiceInstall`.
        name: &'static str,
    },
    /// The updater service itself. Never stopped by its own hand: the MSI's
    /// `ServiceControl` stops it once msiexec (detached) is running.
    Updater {
        /// SCM service name; must match the WiX `ServiceInstall`.
        name: &'static str,
    },
}

/// One shipped binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Component {
    /// Stable identifier, used in logs.
    pub id: &'static str,
    /// File name inside the install directory.
    pub binary: &'static str,
    pub kind: Kind,
}

/// The updater's SCM service name.
pub const UPDATER_SERVICE: &str = "filex-updater";

/// Everything the Windows MSI installs. Order matters only within a kind:
/// services stop in reverse order (later entries may depend on earlier ones).
pub const COMPONENTS: &[Component] = &[
    Component {
        id: "app",
        binary: "filex.exe",
        kind: Kind::App,
    },
    Component {
        id: "indexd",
        binary: "filex-indexd.exe",
        kind: Kind::UserProcess,
    },
    Component {
        id: "updater",
        binary: "filex-updater.exe",
        kind: Kind::Updater {
            name: UPDATER_SERVICE,
        },
    },
];

/// The app component — the binary relaunched after an update.
pub fn app(components: &[Component]) -> Option<&Component> {
    components.iter().find(|c| c.kind == Kind::App)
}

/// One step of bringing Filex down before an install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuiesceStep {
    /// Wait for the requesting session's app to exit on its own.
    AwaitAppExit { binary: &'static str },
    /// Terminate every process running this binary from the install dir.
    Terminate { binary: &'static str },
    /// Stop an SCM service and wait for it.
    StopService { name: &'static str },
}

/// The ordered quiesce plan: let apps exit, terminate what's left (apps,
/// then per-user processes), then stop services in reverse registry order.
/// The updater never appears — it must outlive the msiexec spawn.
pub fn quiesce_plan(components: &[Component]) -> Vec<QuiesceStep> {
    let of = |kind: fn(&Kind) -> bool| components.iter().filter(move |c| kind(&c.kind));
    let mut plan: Vec<QuiesceStep> = of(|k| *k == Kind::App)
        .map(|c| QuiesceStep::AwaitAppExit { binary: c.binary })
        .collect();
    plan.extend(of(|k| *k == Kind::App).map(|c| QuiesceStep::Terminate { binary: c.binary }));
    plan.extend(
        of(|k| *k == Kind::UserProcess).map(|c| QuiesceStep::Terminate { binary: c.binary }),
    );
    plan.extend(components.iter().rev().filter_map(|c| match c.kind {
        Kind::Service { name } => Some(QuiesceStep::StopService { name }),
        _ => None,
    }));
    plan
}

/// SCM names of every service the MSI installs, the updater included.
pub fn service_names(components: &[Component]) -> Vec<&'static str> {
    components
        .iter()
        .filter_map(|c| match c.kind {
            Kind::Service { name } | Kind::Updater { name } => Some(name),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_waits_for_apps_then_terminates_then_stops_services_in_reverse() {
        let components = [
            Component {
                id: "app",
                binary: "a.exe",
                kind: Kind::App,
            },
            Component {
                id: "d",
                binary: "d.exe",
                kind: Kind::UserProcess,
            },
            Component {
                id: "s1",
                binary: "s1.exe",
                kind: Kind::Service { name: "s1" },
            },
            Component {
                id: "u",
                binary: "u.exe",
                kind: Kind::Updater { name: "u" },
            },
            Component {
                id: "s2",
                binary: "s2.exe",
                kind: Kind::Service { name: "s2" },
            },
        ];
        assert_eq!(
            quiesce_plan(&components),
            vec![
                QuiesceStep::AwaitAppExit { binary: "a.exe" },
                QuiesceStep::Terminate { binary: "a.exe" },
                QuiesceStep::Terminate { binary: "d.exe" },
                QuiesceStep::StopService { name: "s2" },
                QuiesceStep::StopService { name: "s1" },
            ]
        );
    }

    #[test]
    fn updater_never_quiesces_itself() {
        let plan = quiesce_plan(COMPONENTS);
        assert!(plan.iter().all(|step| !matches!(
            step,
            QuiesceStep::Terminate {
                binary: "filex-updater.exe"
            } | QuiesceStep::StopService {
                name: UPDATER_SERVICE
            }
        )));
    }

    #[test]
    fn registry_has_one_app_and_one_updater_and_unique_binaries() {
        assert_eq!(app(COMPONENTS).map(|c| c.binary), Some("filex.exe"));
        let updaters = COMPONENTS
            .iter()
            .filter(|c| matches!(c.kind, Kind::Updater { .. }))
            .count();
        assert_eq!(updaters, 1);
        let mut binaries: Vec<_> = COMPONENTS.iter().map(|c| c.binary).collect();
        binaries.sort_unstable();
        binaries.dedup();
        assert_eq!(binaries.len(), COMPONENTS.len());
    }

    // --- Drift guards: registry ↔ installer ↔ release pipeline -------------

    fn repo_file(path: &str) -> String {
        let full = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(path);
        std::fs::read_to_string(&full).unwrap_or_else(|e| panic!("reading {path}: {e}"))
    }

    #[test]
    fn every_component_is_installed_by_the_msi() {
        let wxs = repo_file("wix/main.wxs");
        for c in COMPONENTS {
            assert!(
                wxs.contains(&format!("Name=\"{}\"", c.binary)),
                "wix/main.wxs installs no File named {}",
                c.binary
            );
        }
    }

    #[test]
    fn every_service_is_registered_and_controlled_by_the_msi() {
        let wxs = repo_file("wix/main.wxs");
        for name in service_names(COMPONENTS) {
            let install = wxs
                .split("<ServiceInstall")
                .skip(1)
                .any(|block| block.contains(&format!("Name=\"{name}\"")));
            assert!(install, "wix/main.wxs has no ServiceInstall for {name}");
            let control = wxs
                .split("<ServiceControl")
                .skip(1)
                .any(|block| block.contains(&format!("Name=\"{name}\"")));
            assert!(control, "wix/main.wxs has no ServiceControl for {name}");
        }
    }

    #[test]
    fn every_component_is_built_by_the_windows_release_job() {
        let release = repo_file(".github/workflows/release.yml");
        let windows_job = release
            .split("  linux:")
            .next()
            .expect("release.yml has a windows job before linux");
        let words: Vec<&str> = windows_job.split_whitespace().collect();
        for c in COMPONENTS {
            let bin = c.binary.trim_end_matches(".exe");
            assert!(
                words.windows(2).any(|w| w == ["--bin", bin]),
                "release.yml's windows job doesn't build {bin}"
            );
        }
    }
}

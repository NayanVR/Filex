//! What a launch asks for, before and after the window exists: the
//! single-instance hand-off, `--register-*` commands that run without a
//! window, pass-through of targets only the system file manager can show,
//! and landing the window on the requested folder.

use super::*;
use filex::launch::{Browse, ShellCommand};
use filex::single_instance::{self, Primary, Startup};

/// Become the UI process, or hand `browse` to the one already running.
/// `None` means this process should exit. A failed hand-off (stale
/// endpoint, a primary that is hung) falls back to running standalone:
/// a second window beats a launch that silently does nothing.
pub(super) fn claim_instance(browse: &Browse) -> Option<Option<Primary>> {
    match single_instance::claim_or_forward(
        &single_instance::default_directory(),
        browse,
        platform::allow_foreground,
    ) {
        Ok(Startup::Forwarded) => {
            tracing::info!("handed the launch to the running instance");
            None
        }
        Ok(Startup::Primary(primary)) => Some(Some(primary)),
        Err(err) => {
            tracing::warn!("single-instance hand-off failed, running standalone: {err:#}");
            Some(None)
        }
    }
}

/// Run a `--register-*` / `--unregister-*` command and return the exit
/// code. No window: the installer and scripts call these.
pub fn run_shell_command(command: ShellCommand) -> i32 {
    let _logging_guard = filex::diagnostics::logging::init("filex");
    let result = shell_command(command);
    match &result {
        Ok(()) => tracing::info!(?command, "shell integration updated"),
        Err(err) => {
            tracing::error!(?command, "shell integration failed: {err:#}");
            eprintln!("filex: {err:#}");
        }
    }
    i32::from(result.is_err())
}

fn shell_command(command: ShellCommand) -> anyhow::Result<()> {
    use filex::shell_integration as shell;
    let exe = std::env::current_exe()?;
    match command {
        ShellCommand::RegisterDefault => shell::register_default(&exe),
        ShellCommand::UnregisterDefault => shell::unregister_default(),
        ShellCommand::RegisterDialogs => shell::register_dialogs(&shell::dialog_shim_path(&exe)),
        ShellCommand::UnregisterDialogs => shell::unregister_dialogs(),
        ShellCommand::UnregisterAll => {
            // Both, even if the first fails: the uninstaller gets one shot.
            let default = shell::unregister_default();
            let dialogs = shell::unregister_dialogs();
            default.and(dialogs)
        }
    }
}

/// A target only the system file manager can show (Control Panel, the
/// Recycle Bin, …): pass the original arguments straight through.
pub fn hand_to_system_explorer(args: &[String]) {
    let _logging_guard = filex::diagnostics::logging::init("filex");
    if let Err(err) = platform::launch_system_file_manager(args) {
        tracing::warn!("couldn't hand {args:?} to the system file manager: {err}");
    }
}

impl Workspace {
    /// Land on what a launch asked for. At startup (`new_tab == false`)
    /// the first tab is replaced; a forwarded launch opens a new tab.
    /// `browse` was normalized off the UI thread, so nothing here stats.
    pub(super) fn apply_launch(&mut self, browse: Browse, new_tab: bool, cx: &mut Context<Self>) {
        let (dir, select) = match browse {
            Browse::Default | Browse::Foreign(_) => return,
            Browse::Open(dir) => (dir, None),
            Browse::ThisPc => (filex::drives::this_pc(), None),
            Browse::Select(item) => match item.parent() {
                Some(parent) => (
                    parent.to_path_buf(),
                    item.file_name().map(|n| n.to_string_lossy().into_owned()),
                ),
                None => (item, None),
            },
        };
        if new_tab {
            self.open_tab(dir.clone(), cx);
        }
        self.load_dir_selecting(&dir, select, cx);
        cx.notify();
    }
}

// On Windows, link as a GUI-subsystem binary so double-clicking filex.exe
// doesn't allocate a console window that lingers behind the app for the
// whole session. Release builds only — a debug run keeps its console so
// `RUST_LOG` output and panics stay visible during development.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod quick_look;
mod settings_store;
mod thumbnails;
mod ui;
mod workspace;

fn main() {
    #[cfg(windows)]
    if filex::platform_preview::dispatch_helper() {
        return;
    }
    use filex::launch::{Browse, Launch};
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let cwd = std::env::current_dir().unwrap_or_default();
    match filex::launch::parse(&args, &cwd, filex::launch::known_folder) {
        Launch::Shell(command) => std::process::exit(workspace::run_shell_command(command)),
        Launch::Pick => workspace::run_picker(),
        Launch::Browse(Browse::Foreign(args)) => workspace::hand_to_system_explorer(&args),
        Launch::Browse(browse) => workspace::run(browse),
    }
}

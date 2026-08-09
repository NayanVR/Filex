// On Windows, link as a GUI-subsystem binary so double-clicking filex.exe
// doesn't allocate a console window that lingers behind the app for the
// whole session. Release builds only — a debug run keeps its console so
// `RUST_LOG` output and panics stay visible during development.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod settings_store;
mod thumbnails;
mod ui;
mod workspace;

fn main() {
    workspace::run();
}

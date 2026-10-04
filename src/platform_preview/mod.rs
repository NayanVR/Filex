//! Platform preview policy and isolated Windows Shell integration.
//! Native providers never execute in the GPUI process or its COM apartments.
#[cfg(windows)]
mod native;
pub mod policy;
#[cfg(windows)]
mod process;
#[cfg(any(windows, test))]
mod protocol;
#[cfg(windows)]
pub use process::{Preview, thumbnail};

/// Reserved child-process entry point. Call before initializing GPUI.
#[cfg(windows)]
pub fn dispatch_helper() -> bool {
    let mode = std::env::args_os().nth(1);
    let Some(mode) = mode.and_then(|s| s.into_string().ok()) else {
        return false;
    };
    if mode != "--filex-thumbnail-helper" && mode != "--filex-preview-helper" {
        return false;
    }
    if let Err(error) = native::run(&mode) {
        let _ = protocol::write(
            &mut std::io::stdout().lock(),
            &protocol::Event::Error(format!("{error:#}")),
        );
        std::process::exit(1);
    }
    true
}

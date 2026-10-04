//! Space-bar full-file previews, separate from thumbnail generation.
#[cfg(not(windows))]
mod deferred;
#[cfg(not(windows))]
pub use deferred::Viewer;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::Viewer;

//! `filex_dialog_shim.dll`: shows Filex in place of the Windows common
//! file dialog.
//!
//! Registered per user as the in-proc server for `CLSID_FileOpenDialog`
//! and `CLSID_FileSaveDialog` (see `filex::shell_integration::windows`),
//! so `CoCreateInstance` in any unelevated app loads this DLL instead of
//! comdlg32. Every object we hand out is a *proxy* around the real dialog,
//! created straight from comdlg32's class factory: all calls are
//! forwarded to it, and only `Show` is intercepted to run `filex --pick`.
//! Any failure, and any request Filex cannot honour (custom controls),
//! falls back to the real dialog's `Show` — an app must never be left
//! without a working Save dialog because of Filex.
//!
//! The request/response JSON is `filex::picker`'s protocol; [`protocol`]
//! builds and parses it without depending on the filex crate.

// MSVC's LNK4104 ("export of DllGetClassObject should be PRIVATE") is the
// standard, harmless note for COM exports; CI denies warnings.
#![allow(linker_messages)]

pub mod protocol;

#[cfg(target_os = "windows")]
mod com;
#[cfg(target_os = "windows")]
mod dialog;
#[cfg(target_os = "windows")]
mod launch;

#[cfg(target_os = "windows")]
pub use com::{DllCanUnloadNow, DllGetClassObject};

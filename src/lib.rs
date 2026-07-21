//! filex — fast cross-platform file explorer.
//!
//! Library target so that logic modules are reachable from benches and
//! integration tests; the GPUI app lives in `main.rs`.

pub mod index;
pub mod listing;
pub mod logging;
pub mod ops;
pub mod recents;
pub mod selection;
pub mod settings;

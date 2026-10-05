//! filex — fast cross-platform file explorer.
//!
//! Library target so that logic modules are reachable from benches and
//! integration tests; the GPUI app lives in `main.rs`.

pub mod catalog;
pub mod diagnostics;
pub mod drives;
pub mod frecency;
#[cfg(feature = "index-v2-lab")]
pub mod index_lab;
pub mod listing;
pub mod magic;
pub mod ops;
pub mod recents;
pub mod search;
pub mod selection;
pub mod settings;
pub mod tags;
pub mod update;

pub mod daemon;
pub mod ingest;

#[cfg(feature = "app")]
pub mod platform_preview;

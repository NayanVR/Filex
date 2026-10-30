//! filex — fast cross-platform file explorer.
//!
//! Library target so that logic modules are reachable from benches and
//! integration tests; the GPUI app lives in `main.rs`.

pub mod archive;
pub mod catalog;
pub mod connect;
pub mod context_menu;
pub mod diagnostics;
pub mod drives;
pub mod favorites_import;
pub mod frecency;
#[cfg(feature = "index-v2-lab")]
pub mod index_lab;
pub mod launch;
pub mod listing;
pub mod magic;
pub mod network;
pub mod ops;
pub mod picker;
pub mod recents;
pub mod search;
pub mod selection;
pub mod settings;
pub mod shell_integration;
pub mod single_instance;
pub mod tags;
pub mod update;
pub mod updater;

pub mod daemon;
pub mod ingest;

#[cfg(feature = "app")]
pub mod platform_preview;

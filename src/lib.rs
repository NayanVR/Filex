//! filex — fast cross-platform file explorer.
//!
//! Library target so that logic modules are reachable from benches and
//! integration tests; the GPUI app lives in `main.rs`.

pub mod catalog;
pub mod drives;
pub mod frecency;
pub mod fuzzy;
#[cfg(feature = "index-v2-lab")]
pub mod index_lab;
pub mod listing;
pub mod logging;
pub mod magic;
#[cfg(feature = "observability")]
pub mod observability;
pub mod ops;
pub mod phrases;
pub mod recents;
pub mod search;
pub mod search_filter;
pub mod selection;
pub mod settings;
pub mod tags;
pub mod telemetry;
pub mod update;

pub mod daemon;
pub mod ingest;

#[cfg(feature = "app")]
pub mod platform_preview;

//! Logging, local crash-report queueing, and the opt-in Sentry transport.
pub mod logging;
#[cfg(feature = "observability")]
pub mod observability;
pub mod telemetry;

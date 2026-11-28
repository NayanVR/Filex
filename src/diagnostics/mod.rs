//! Logging, local crash-report queueing, and PostHog analytics/error
//! tracking (the `analytics` module's transport is the `analytics` feature).
pub mod analytics;
pub mod logging;
pub mod telemetry;

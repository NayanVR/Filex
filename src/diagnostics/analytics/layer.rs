//! A `tracing` layer that reports `error!` events as PostHog exceptions,
//! replacing `sentry-tracing`. Only the message is read: other fields
//! (often `path = %p.display()`) are ignored outright, and the message is
//! scrubbed by [`super::exception::scrub_text`]. Warn and below stay in the
//! local log file.
//!
//! Each callsite reports at most [`PER_CALLSITE`] times per process, so an
//! error inside a loop can't flood the pipeline.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Mutex;

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;

use super::{exception, names};

/// Reports per callsite per process.
const PER_CALLSITE: u32 = 5;

#[derive(Default)]
pub struct AnalyticsLayer {
    seen: Mutex<HashMap<tracing::callsite::Identifier, u32>>,
}

impl AnalyticsLayer {
    /// Whether this callsite may report again (and count it).
    fn admit(&self, id: tracing::callsite::Identifier) -> bool {
        let Ok(mut seen) = self.seen.lock() else {
            return false;
        };
        let count = seen.entry(id).or_default();
        *count += 1;
        *count <= PER_CALLSITE
    }
}

impl<S: Subscriber> Layer<S> for AnalyticsLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        if *meta.level() != Level::ERROR || !super::is_running() {
            return;
        }
        // Never report on ourselves (a failure to send would loop).
        if meta.target().starts_with("filex::diagnostics::analytics") {
            return;
        }
        if !self.admit(meta.callsite()) {
            return;
        }
        let mut message = MessageOnly::default();
        event.record(&mut message);
        let props = exception::log_error_props(meta.target(), meta.file(), meta.line(), &message.0);
        super::track_raw(names::EXCEPTION, props);
    }
}

/// Collects only the `message` field.
#[derive(Default)]
struct MessageOnly(String);

impl Visit for MessageOnly {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.0, "{value:?}");
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.0.push_str(value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::prelude::*;

    #[test]
    fn message_visitor_ignores_other_fields() {
        // Rendered through a real subscriber so the visitor sees what a
        // live `error!` produces.
        struct Capture(std::sync::Arc<Mutex<Vec<String>>>);
        impl<S: Subscriber> Layer<S> for Capture {
            fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
                let mut m = MessageOnly::default();
                event.record(&mut m);
                self.0.lock().unwrap().push(m.0);
            }
        }
        let out = std::sync::Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(Capture(out.clone()));
        tracing::subscriber::with_default(subscriber, || {
            let path = "/home/bob/secret.txt";
            tracing::error!(path, count = 3, "copy failed");
        });
        assert_eq!(out.lock().unwrap().as_slice(), ["copy failed"]);
    }

    #[test]
    fn callsites_are_rate_limited() {
        let layer = AnalyticsLayer::default();
        struct Probe(std::sync::Arc<Mutex<Option<tracing::callsite::Identifier>>>);
        impl<S: Subscriber> Layer<S> for Probe {
            fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
                *self.0.lock().unwrap() = Some(event.metadata().callsite());
            }
        }
        let id = std::sync::Arc::new(Mutex::new(None));
        let subscriber = tracing_subscriber::registry().with(Probe(id.clone()));
        tracing::subscriber::with_default(subscriber, || tracing::error!("x"));
        let id = id.lock().unwrap().clone().unwrap();
        let admitted = (0..10).filter(|_| layer.admit(id.clone())).count();
        assert_eq!(admitted, PER_CALLSITE as usize);
    }
}

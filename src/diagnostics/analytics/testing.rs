//! Capture events in tests instead of sending them.
//!
//! ```ignore
//! let capture = analytics::testing::Capture::start();
//! workspace.go_up(cx);
//! assert_eq!(capture.names(), ["folder_opened"]);
//! ```
//!
//! The capture is per thread, so parallel tests don't see each other's
//! events, and [`super::is_enabled`] reports true while it is active, so
//! code that skips building events when analytics is off still runs.
//! Outside tests the cost is one thread-local read per [`super::track`].

use std::cell::RefCell;

use serde_json::{Map, Value};

use super::event::Event;

thread_local! {
    static CAPTURED: RefCell<Option<Vec<Event>>> = const { RefCell::new(None) };
}

/// Whether a capture is active on this thread.
pub(super) fn active() -> bool {
    CAPTURED.with(|c| c.borrow().is_some())
}

/// Record `event` if a capture is active on this thread.
pub(super) fn capture(event: &Event) -> bool {
    CAPTURED.with(|c| match c.borrow_mut().as_mut() {
        Some(events) => {
            events.push(event.clone());
            true
        }
        None => false,
    })
}

/// An active capture; dropping it stops capturing.
pub struct Capture(());

impl Capture {
    pub fn start() -> Self {
        CAPTURED.with(|c| *c.borrow_mut() = Some(Vec::new()));
        Capture(())
    }

    /// The captured events' names, in order.
    pub fn names(&self) -> Vec<&'static str> {
        CAPTURED.with(|c| c.borrow().iter().flatten().map(|e| e.name).collect())
    }

    /// The captured events as `(name, properties)`.
    pub fn events(&self) -> Vec<(&'static str, Map<String, Value>)> {
        CAPTURED.with(|c| {
            c.borrow()
                .iter()
                .flatten()
                .map(|e| (e.name, e.props_json()))
                .collect()
        })
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        CAPTURED.with(|c| *c.borrow_mut() = None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::analytics::{catalog, is_enabled, track};

    #[test]
    fn captures_typed_events_on_this_thread_only() {
        assert!(!active());
        let capture = Capture::start();
        assert!(is_enabled());
        track(catalog::TabChanged {
            action: catalog::TabAction::Opened,
            tabs: 2,
        });
        std::thread::spawn(|| {
            track(catalog::TabChanged {
                action: catalog::TabAction::Closed,
                tabs: 1,
            })
        })
        .join()
        .unwrap();
        assert_eq!(capture.names(), ["tab_changed"]);
        let (_, props) = &capture.events()[0];
        assert_eq!(props["action"], "opened");
        assert_eq!(props["tabs"], 2);
        drop(capture);
        assert!(!active());
    }
}

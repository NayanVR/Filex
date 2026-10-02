//! Main-thread command boundary. No GPUI handles or platform callbacks belong
//! here: the scheduled driver runs only after the UI update has returned.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;

pub(super) trait Backend {
    fn is_visible(&self) -> bool;
    fn present(&self, paths: Vec<PathBuf>, selected: usize) -> anyhow::Result<()>;
    fn close(&self);
}

pub(super) enum Request {
    Present(Vec<PathBuf>, usize),
    Close,
}

pub(super) struct Driver<B> {
    backend: B,
    pending: RefCell<Option<Request>>,
    running: Cell<bool>,
    requested_visibility: Cell<Option<bool>>,
}

impl<B: Backend> Driver<B> {
    pub(super) fn new(backend: B) -> Self {
        Self {
            backend,
            pending: RefCell::new(None),
            running: Cell::new(false),
            requested_visibility: Cell::new(None),
        }
    }

    pub(super) fn is_visible(&self) -> bool {
        self.requested_visibility
            .get()
            .unwrap_or_else(|| self.backend.is_visible())
    }

    /// Returns true only when a new driver task needs scheduling. One pending
    /// command bounds memory even if native presentation pumps nested events.
    pub(super) fn submit(&self, request: Request) -> bool {
        let request = match request {
            Request::Present(paths, _) if paths.is_empty() => Request::Close,
            Request::Present(paths, selected) => {
                let selected = selected.min(paths.len() - 1);
                Request::Present(paths, selected)
            }
            Request::Close => Request::Close,
        };
        self.requested_visibility
            .set(Some(matches!(request, Request::Present(..))));
        *self.pending.borrow_mut() = Some(request);
        !self.running.replace(true)
    }

    /// Must run outside app/entity/window updates. Never hold a borrow across
    /// a backend call or an error callback: either can submit another command.
    pub(super) fn drain(&self, on_error: impl Fn(anyhow::Error)) {
        loop {
            let request = self.pending.borrow_mut().take();
            let Some(request) = request else { break };
            match request {
                Request::Present(paths, selected) => {
                    if let Err(error) = self.backend.present(paths, selected) {
                        // Failed presentation must release native callbacks and
                        // report closed before notifying the app. A newer user
                        // request remains pending and is processed normally.
                        self.backend.close();
                        if self.pending.borrow().is_none() {
                            self.requested_visibility.set(Some(false));
                            on_error(error);
                        }
                    }
                }
                Request::Close => self.backend.close(),
            }
        }
        self.requested_visibility.set(None);
        self.running.set(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;

    #[derive(Default)]
    struct Fake {
        visible: Cell<bool>,
        fail: Cell<bool>,
        calls: RefCell<Vec<String>>,
        during_present: RefCell<Option<Box<dyn FnOnce()>>>,
    }

    impl Backend for Rc<Fake> {
        fn is_visible(&self) -> bool {
            self.visible.get()
        }
        fn present(&self, paths: Vec<PathBuf>, selected: usize) -> anyhow::Result<()> {
            self.calls
                .borrow_mut()
                .push(format!("{}:{selected}", paths[0].display()));
            self.visible.set(true);
            let callback = self.during_present.borrow_mut().take();
            if let Some(callback) = callback {
                callback();
            }
            if self.fail.replace(false) {
                anyhow::bail!("injected presentation failure");
            }
            Ok(())
        }
        fn close(&self) {
            self.calls.borrow_mut().push("close".into());
            self.visible.set(false);
        }
    }

    fn present(name: &str) -> Request {
        Request::Present(vec![PathBuf::from(name)], usize::MAX)
    }

    #[test]
    fn submissions_are_deferred_coalesced_and_indices_clamped() {
        let backend = Rc::new(Fake::default());
        let driver = Driver::new(backend.clone());
        assert!(driver.submit(present("first")));
        for _ in 0..10_000 {
            assert!(!driver.submit(present("latest")));
        }
        assert!(backend.calls.borrow().is_empty());
        assert!(driver.is_visible());
        driver.drain(|e| panic!("{e}"));
        assert_eq!(*backend.calls.borrow(), ["latest:0"]);
    }

    #[test]
    fn close_cancels_pending_open_and_reopen_cancels_pending_close() {
        let backend = Rc::new(Fake::default());
        let driver = Driver::new(backend.clone());
        assert!(driver.submit(present("cancelled")));
        assert!(!driver.submit(Request::Close));
        assert!(!driver.is_visible());
        driver.drain(|e| panic!("{e}"));
        assert_eq!(*backend.calls.borrow(), ["close"]);
        assert!(driver.submit(Request::Close));
        assert!(!driver.submit(present("reopened")));
        driver.drain(|e| panic!("{e}"));
        assert_eq!(*backend.calls.borrow(), ["close", "reopened:0"]);
    }

    #[test]
    fn native_reentrancy_queues_close_without_recursive_presentation() {
        let backend = Rc::new(Fake::default());
        let driver = Rc::new(Driver::new(backend.clone()));
        let weak = Rc::downgrade(&driver);
        *backend.during_present.borrow_mut() = Some(Box::new(move || {
            let driver = weak.upgrade().unwrap();
            assert!(!driver.submit(present("superseded")));
            assert!(!driver.submit(Request::Close));
            assert!(!driver.is_visible());
        }));
        driver.submit(present("opening"));
        driver.drain(|e| panic!("{e}"));
        assert_eq!(*backend.calls.borrow(), ["opening:0", "close"]);
        assert!(!driver.is_visible());
        assert!(driver.submit(present("next")), "driver must become idle");
    }

    #[test]
    fn failure_cleans_up_before_reporting_and_callback_can_retry() {
        let backend = Rc::new(Fake::default());
        backend.fail.set(true);
        let driver = Driver::new(backend.clone());
        driver.submit(present("broken"));
        let errors = Cell::new(0);
        driver.drain(|_| {
            errors.set(errors.get() + 1);
            assert!(!driver.is_visible());
            assert!(!backend.visible.get());
            assert!(!driver.submit(present("retry")));
        });
        assert_eq!(errors.get(), 1);
        assert_eq!(*backend.calls.borrow(), ["broken:0", "close", "retry:0"]);
        assert!(driver.is_visible());
    }

    #[test]
    fn obsolete_failure_does_not_report_over_new_selection() {
        let backend = Rc::new(Fake::default());
        backend.fail.set(true);
        let driver = Rc::new(Driver::new(backend.clone()));
        let weak = Rc::downgrade(&driver);
        *backend.during_present.borrow_mut() = Some(Box::new(move || {
            weak.upgrade().unwrap().submit(present("new"));
        }));
        driver.submit(present("old"));
        driver.drain(|_| panic!("stale error"));
        assert_eq!(*backend.calls.borrow(), ["old:0", "close", "new:0"]);
    }

    #[test]
    fn empty_selection_closes_and_native_dismissal_is_observed() {
        let backend = Rc::new(Fake::default());
        let driver = Driver::new(backend.clone());
        driver.submit(present("first"));
        driver.drain(|e| panic!("{e}"));
        backend.visible.set(false); // native close button
        assert!(!driver.is_visible());
        driver.submit(Request::Present(Vec::new(), usize::MAX));
        driver.drain(|e| panic!("{e}"));
        assert_eq!(*backend.calls.borrow(), ["first:0", "close"]);
    }
}

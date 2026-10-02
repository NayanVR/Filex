//! QLPreviewPanel controller inserted into the Filex window's responder chain.
//! All AppKit work is main-thread-only. Quick Look owns document loading,
//! rendering, playback, and its native toolbar; Filex supplies only file URLs.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;

use anyhow::{Context as _, Result};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{
    ClassType, DefinedClass, MainThreadMarker, MainThreadOnly, Message, define_class, msg_send, sel,
};
use objc2_app_kit::{
    NSEvent, NSEventModifierFlags, NSEventType, NSResponder, NSView, NSWindow, NSWindowDelegate,
};
use objc2_foundation::{NSInteger, NSObject, NSObjectNSDelayedPerforming, NSObjectProtocol, NSURL};
use objc2_quick_look_ui::{
    QLPreviewItem, QLPreviewPanel, QLPreviewPanelDataSource, QLPreviewPanelDelegate,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

#[derive(Default)]
struct ControllerState {
    enabled: Cell<bool>,
    paths: RefCell<Vec<PathBuf>>,
    // Set only by begin/endPreviewPanelControl, never by pretending to own
    // the singleton. Clone this handle before sending reentrant AppKit calls.
    panel: RefCell<Option<Retained<QLPreviewPanel>>>,
    // Keep the disabled responder in its window's chain until the deferred
    // close has run. Quick Look may defer updateController itself too.
    retired_link: RefCell<Option<ResponderLink>>,
}

struct ResponderLink {
    // Temporary self-retain, released by endPreviewPanelControl/finish_closing.
    // AppKit's responder-chain link itself does not keep the controller alive.
    _controller: Retained<Controller>,
    window: Retained<NSWindow>,
    next: Option<Retained<NSResponder>>,
}

define_class!(
    #[unsafe(super = NSResponder)]
    #[thread_kind = MainThreadOnly]
    #[name = "FilexQuickLookController"]
    #[ivars = ControllerState]
    struct Controller;

    unsafe impl NSObjectProtocol for Controller {}
    unsafe impl NSWindowDelegate for Controller {}

    impl Controller {
        #[unsafe(method(filexFinishClosingPreview:))]
        fn finish_closing(&self, panel: &QLPreviewPanel) {
            // Dismissing during the same event as presentation can leave the
            // native panel open. Defer to the next run-loop turn, never closing a
            // viewer that was reopened or handed to another controller.
            let controller = unsafe { panel.currentController() };
            if !self.ivars().enabled.get()
                && controller.as_deref().is_none_or(|current| std::ptr::eq(current, self.as_ref()))
            {
                // Relinquish control while visible: Quick Look can defer
                // controller updates for a hidden panel, retaining a stale
                // controller after its owning Filex window has been dropped.
                unsafe { panel.updateController() };
                let current = unsafe { panel.currentController() };
                if !self.ivars().enabled.get()
                    && current.as_deref().is_none_or(|current| std::ptr::eq(current, self.as_ref()))
                {
                    panel.orderOut(None);
                }
            }
            // orderOut may animate asynchronously. Keep the responder and
            // its owner alive until endPreviewPanelControl releases control.
            let current = unsafe { panel.currentController() };
            if current.as_deref().is_none_or(|current| !std::ptr::eq(current, self.as_ref())) {
                self.unlink_retired();
            }
        }

        #[unsafe(method(acceptsPreviewPanelControl:))]
        fn accepts_panel(&self, _: Option<&QLPreviewPanel>) -> bool {
            self.ivars().enabled.get()
        }

        #[unsafe(method(beginPreviewPanelControl:))]
        fn begin_panel(&self, panel: &QLPreviewPanel) {
            let previous = self.ivars().panel.replace(Some(panel.retain()));
            drop(previous); // native release may reenter; no RefCell guard held
            // SAFETY: AppKit granted control. Viewer retains this controller
            // until it has detached both unretained panel references.
            unsafe {
                panel.setDataSource(Some(ProtocolObject::from_ref(self)));
                panel.setDelegate(Some(self.as_ref()));
            }
        }

        #[unsafe(method(endPreviewPanelControl:))]
        fn end_panel(&self, panel: &QLPreviewPanel) {
            // SAFETY: Called by Quick Look while relinquishing our control.
            unsafe {
                panel.setDataSource(None);
                panel.setDelegate(None);
            }
            let previous = self.ivars().panel.borrow_mut().take();
            drop(previous);
            self.unlink_retired();
        }
    }

    unsafe impl QLPreviewPanelDataSource for Controller {
        #[unsafe(method(numberOfPreviewItemsInPreviewPanel:))]
        unsafe fn item_count(&self, _: Option<&QLPreviewPanel>) -> NSInteger {
            self.ivars().paths.borrow().len() as NSInteger
        }

        #[unsafe(method_id(previewPanel:previewItemAtIndex:))]
        unsafe fn item_at(&self, _: Option<&QLPreviewPanel>, index: NSInteger)
            -> Option<Retained<ProtocolObject<dyn QLPreviewItem>>>
        {
            self.preview_item(index)
        }
    }

    unsafe impl QLPreviewPanelDelegate for Controller {
        #[unsafe(method(previewPanel:handleEvent:))]
        unsafe fn handle_event(&self, panel: Option<&QLPreviewPanel>, event: Option<&NSEvent>) -> bool {
            self.handle_key(panel, event)
        }
    }
);

impl Controller {
    fn unlink_retired(&self) {
        let link = self.ivars().retired_link.borrow_mut().take();
        if let Some(link) = link {
            // SAFETY: Both ends of the responder link stay retained until
            // cleanup. Do not overwrite another component's replacement.
            unsafe {
                if link.window.nextResponder().as_deref() == Some(self.as_super()) {
                    link.window.setNextResponder(link.next.as_deref());
                }
                self.setNextResponder(None);
            }
        }
    }

    fn preview_item(
        &self,
        index: NSInteger,
    ) -> Option<Retained<ProtocolObject<dyn QLPreviewItem>>> {
        let path = self
            .ivars()
            .paths
            .borrow()
            .get(usize::try_from(index).ok()?)?
            .clone();
        // URL conversion preserves spaces and non-UTF8 filesystem names;
        // no file contents are read on the UI thread.
        NSURL::from_file_path(&path).map(ProtocolObject::from_retained)
    }

    fn handle_key(&self, panel: Option<&QLPreviewPanel>, event: Option<&NSEvent>) -> bool {
        let (Some(panel), Some(event)) = (panel, event) else {
            return false;
        };
        if event.r#type() != NSEventType::KeyDown
            || event.modifierFlags().intersects(
                NSEventModifierFlags::Command
                    | NSEventModifierFlags::Control
                    | NSEventModifierFlags::Option,
            )
        {
            return false;
        }
        match event.keyCode() {
            49 | 53 => {
                // Space / Escape, when not already handled by Quick Look.
                self.close();
                true
            }
            123 | 124 => {
                let delta = if event.keyCode() == 123 { -1 } else { 1 };
                let count = self.ivars().paths.borrow().len();
                if count < 2 {
                    return false;
                }
                let last = count.saturating_sub(1) as NSInteger;
                // SAFETY: Delegate callbacks occur while we control panel.
                unsafe {
                    panel.setCurrentPreviewItemIndex(
                        panel
                            .currentPreviewItemIndex()
                            .saturating_add(delta)
                            .clamp(0, last),
                    );
                }
                true
            }
            _ => false,
        }
    }

    fn panel(&self) -> Option<Retained<QLPreviewPanel>> {
        self.ivars().panel.borrow().clone()
    }

    /// Snapshot the index before entering AppKit. reloadData may synchronously
    /// dismiss the panel and clear paths; never subtract from an unchecked len
    /// or pass a RefCell-backed expression directly as an Objective-C argument.
    fn selected_index(&self, selected: usize) -> Option<NSInteger> {
        if !self.ivars().enabled.get() {
            return None;
        }
        let last = self.ivars().paths.borrow().len().checked_sub(1)?;
        NSInteger::try_from(selected.min(last)).ok()
    }

    fn close(&self) {
        let was_enabled = self.ivars().enabled.replace(false);
        self.ivars().paths.borrow_mut().clear();
        if !was_enabled {
            // Explicit close and owner Drop can arrive in the same turn.
            // Scheduling dismissal twice can strand the animated native panel.
            return;
        }
        if let Some(panel) = self.panel() {
            // SAFETY: We own the panel. Detach unretained callbacks before
            // the Viewer can be dropped, then retain both objects for cleanup.
            unsafe {
                panel.setDataSource(None);
                panel.setDelegate(None);
                // Update while our disabled responder is still in the chain.
                // Removing it first can leave Quick Look's currentController
                // pointing at it: a search with no candidate need not end the
                // existing controller's session.
                panel.updateController();
                let object: &NSObject = self.as_super().as_ref();
                // Foundation retains the receiver until this main-run-loop
                // callback, including when close is called from Drop.
                object.performSelector_withObject_afterDelay(
                    sel!(filexFinishClosingPreview:),
                    Some(panel.as_ref()),
                    0.0,
                );
            }
        }
    }
}

pub struct Viewer {
    controller: Retained<Controller>,
    window: Retained<NSWindow>,
    next_responder: Option<Retained<NSResponder>>,
}

impl Viewer {
    pub fn new(window: &gpui::Window) -> Result<Self> {
        let mtm = MainThreadMarker::new().context("Quick Look requires the main thread")?;
        let handle = HasWindowHandle::window_handle(window)
            .map_err(|err| anyhow::anyhow!("No native window handle: {err}"))?;
        let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
            anyhow::bail!("Quick Look requires an AppKit window");
        };
        // SAFETY: GPUI's borrowed AppKit handle contains a live NSView. We
        // retain its window and only use AppKit on the main thread.
        let view = unsafe { handle.ns_view.cast::<NSView>().as_ref() };
        let window = view.window().context("Filex has no native window")?;
        let controller = Controller::alloc(mtm).set_ivars(ControllerState::default());
        // SAFETY: NSResponder's designated initializer, with initialized ivars.
        let controller: Retained<Controller> = unsafe { msg_send![super(controller), init] };
        // SAFETY: Both the inserted responder and its old successor are
        // retained for the entire lifetime of these unretained links.
        let next_responder = unsafe { window.nextResponder() };
        unsafe {
            controller.setNextResponder(next_responder.as_deref());
            window.setNextResponder(Some(&controller));
        }
        Ok(Self {
            controller,
            window,
            next_responder,
        })
    }

    pub fn is_visible(&self) -> bool {
        // Include the opening transition so a second Space can cancel it.
        // Native dismissal relinquishes control via endPreviewPanelControl.
        self.controller.ivars().enabled.get() && self.controller.panel().is_some()
    }

    pub fn show(&self, paths: Vec<PathBuf>, selected: usize) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        *self.controller.ivars().paths.borrow_mut() = paths;
        self.controller.ivars().enabled.set(true);
        let already_controls_panel = self.controller.panel().is_some();
        if !already_controls_panel {
            self.window.makeKeyAndOrderFront(None);
        }
        // SAFETY: Main-thread-only Viewer; this creates no decoder work itself.
        let Some(panel) = (unsafe { QLPreviewPanel::sharedPreviewPanel(self.controller.mtm()) })
        else {
            self.controller.close();
            anyhow::bail!("macOS could not create the Quick Look panel");
        };
        // Showing the panel causes Quick Look to ask the main window's
        // responder chain for a controller. A hidden panel may defer this.
        if !already_controls_panel {
            panel.makeKeyAndOrderFront(None);
        }
        unsafe { panel.updateController() };
        if !self.controller.ivars().enabled.get() {
            return Ok(()); // native dismissal during presentation
        }
        if self.controller.panel().is_none() {
            self.controller.close();
            if unsafe { panel.currentController() }.is_none() {
                panel.orderOut(None);
            }
            anyhow::bail!("macOS could not attach Quick Look to the Filex window");
        }
        // SAFETY: beginPreviewPanelControl granted ownership above.
        unsafe {
            // A same-turn reopen may still own the panel after close detached
            // these callbacks, without another beginPreviewPanelControl call.
            panel.setDataSource(Some(ProtocolObject::from_ref(&*self.controller)));
            panel.setDelegate(Some(self.controller.as_ref()));
            panel.reloadData();
            if let Some(index) = self.controller.selected_index(selected) {
                panel.setCurrentPreviewItemIndex(index);
            }
        }
        Ok(())
    }

    pub fn update(&self, paths: Vec<PathBuf>, selected: usize) {
        if !self.is_visible() {
            return;
        }
        if paths.is_empty() {
            self.close();
            return;
        }
        if *self.controller.ivars().paths.borrow() != paths {
            *self.controller.ivars().paths.borrow_mut() = paths;
            if let Some(panel) = self.controller.panel() {
                // SAFETY: Only the current controller has a panel handle.
                unsafe { panel.reloadData() };
            }
        }
        if let (Some(panel), Some(index)) = (
            self.controller.panel(),
            self.controller.selected_index(selected),
        ) {
            unsafe {
                if panel.currentPreviewItemIndex() != index {
                    panel.setCurrentPreviewItemIndex(index);
                }
            }
        }
    }

    pub fn close(&self) {
        self.controller.close();
    }
}

impl Drop for Viewer {
    fn drop(&mut self) {
        // The delayed close retains this controller; transfer its responder
        // link too. Unlinking immediately here prevents a deferred controller
        // update from finding our disabled responder and ending its session.
        *self.controller.ivars().retired_link.borrow_mut() = Some(ResponderLink {
            _controller: self.controller.clone(),
            window: self.window.clone(),
            next: self.next_responder.clone(),
        });
        self.close();
        if self.controller.panel().is_none() {
            self.controller.unlink_retired();
        }
    }
}

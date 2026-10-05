//! Opt-in live macOS lifecycle regression check; opens native windows.
//! Run: cargo run --example quick_look_smoke -- /path/to/sample.pdf

#[cfg(target_os = "macos")]
#[path = "../src/quick_look/mod.rs"]
mod native;

#[cfg(target_os = "macos")]
fn main() {
    harness::run();
}
#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("This native Quick Look check requires macOS.");
}

#[cfg(target_os = "macos")]
mod harness {
    use super::native;
    use gpui::{
        App, Application, Bounds, Context, FocusHandle, KeyBinding, Window, WindowBounds,
        WindowOptions, actions, div, prelude::*, px, size,
    };
    use objc2::MainThreadMarker;
    use objc2_app_kit::{
        NSApplication, NSEvent, NSEventModifierFlags, NSEventType, NSView, NSWindow,
    };
    use objc2_foundation::{NSPoint, NSString};
    use objc2_quick_look_ui::QLPreviewPanel;
    use std::{path::PathBuf, time::Duration};
    actions!(quick_look_smoke, [TogglePreview]);
    struct Harness {
        viewer: Option<native::Viewer>,
        updates: usize,
        focus: FocusHandle,
        fixtures: Vec<PathBuf>,
    }
    impl Render for Harness {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .key_context("PreviewSmoke")
                .track_focus(&self.focus)
                .on_action(cx.listener(|this, _: &TogglePreview, window, _cx| {
                    if this.viewer.is_none() {
                        this.viewer = Some(
                            native::Viewer::new(window, _cx.foreground_executor().clone(), |err| {
                                panic!("Native preview failed: {err}")
                            })
                            .unwrap(),
                        );
                    }
                    let viewer = this.viewer.as_ref().unwrap();
                    if viewer.is_visible() {
                        viewer.close();
                    } else {
                        let initialized = unsafe {
                            QLPreviewPanel::sharedPreviewPanelExists(
                                MainThreadMarker::new().unwrap(),
                            )
                        };
                        viewer.show(this.fixtures.clone(), 0);
                        // Regression: native creation here can run a nested
                        // event loop while the action still borrows GPUI.
                        if !initialized {
                            assert!(
                                !unsafe {
                                    QLPreviewPanel::sharedPreviewPanelExists(
                                        MainThreadMarker::new().unwrap(),
                                    )
                                },
                                "Opening must be queued outside the GPUI action"
                            );
                        }
                    }
                }))
                .child("Filex native Quick Look lifecycle test")
        }
    }
    fn panel() -> objc2::rc::Retained<QLPreviewPanel> {
        unsafe { QLPreviewPanel::sharedPreviewPanel(MainThreadMarker::new().unwrap()).unwrap() }
    }
    fn key_event(p: &NSWindow, code: u16, chars: &str) -> objc2::rc::Retained<NSEvent> {
        let chars = NSString::from_str(chars);

        NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(
            NSEventType::KeyDown,
            NSPoint::new(0., 0.),
            NSEventModifierFlags::empty(),
            0.,
            p.windowNumber(),
            None,
            &chars,
            &chars,
            false,
            code,
        ).unwrap()
    }
    fn send_key(code: u16, chars: &str) {
        let p = panel();
        NSApplication::sharedApplication(MainThreadMarker::new().unwrap())
            .postEvent_atStart(&key_event(&p, code, chars), false);
    }
    fn post_space(window: &NSWindow) {
        NSApplication::sharedApplication(MainThreadMarker::new().unwrap())
            .postEvent_atStart(&key_event(window, 49, " "), false);
    }
    fn native_window(window: &Window) -> objc2::rc::Retained<NSWindow> {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let handle = HasWindowHandle::window_handle(window).unwrap();
        let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
            panic!("Expected AppKit")
        };
        unsafe { handle.ns_view.cast::<NSView>().as_ref() }
            .window()
            .unwrap()
    }
    pub fn run() {
        // An assertion failure is a test failure, not an application crash.
        // Avoid macOS Crash Reporter dialogs during this opt-in check.
        std::panic::set_hook(Box::new(|info| {
            eprintln!("Native preview check failed: {info}");
            std::process::exit(1);
        }));
        let fixture_dir = tempfile::tempdir().unwrap();
        let first = fixture_dir.path().join("Quick Look Sample.txt");
        let second = fixture_dir.path().join("Second Sample.txt");
        std::fs::write(&first, "Filex Quick Look sample document.\n").unwrap();
        std::fs::write(&second, "Second selection for native lifecycle checks.\n").unwrap();
        let first = std::env::args_os()
            .nth(1)
            .map(PathBuf::from)
            .unwrap_or(first);
        let fixtures = vec![first.canonicalize().unwrap(), second];
        Application::new().run(move |cx: &mut App| {
            let bounds = Bounds::centered(None, size(px(650.), px(400.)), cx);
            cx.bind_keys([KeyBinding::new(
                "space",
                TogglePreview,
                Some("PreviewSmoke"),
            )]);
            let w = cx
                .open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(bounds)),
                        ..Default::default()
                    },
                    |_, cx| {
                        cx.new(|cx| Harness {
                            viewer: None,
                            updates: 0,
                            focus: cx.focus_handle(),
                            fixtures: fixtures.clone(),
                        })
                    },
                )
                .unwrap();
            cx.activate(true);
            // Match Filex's service probe: another foreground task updates the
            // app while Quick Look may pump a nested native event loop.
            cx.spawn(async move |cx| {
                loop {
                    gpui::Timer::after(Duration::from_millis(1)).await;
                    if w.update(cx, |h, _, _| h.updates += 1).is_err() {
                        break;
                    }
                }
            })
            .detach();
            cx.spawn(async move |cx| {
                gpui::Timer::after(Duration::from_secs(1)).await;
                let host = w
                    .update(cx, |h, window, _| {
                        window.focus(&h.focus);
                        native_window(window)
                    })
                    .unwrap();
                // Post a real AppKit event. Calling sendEvent from this async
                // task would run on the dispatch queue and mask reentrancy.
                post_space(&host);
                gpui::Timer::after(Duration::from_secs(2)).await;
                w.update(cx, |h, _, _| {
                    assert!(h.viewer.as_ref().unwrap().is_visible());
                    assert_eq!(unsafe { panel().currentPreviewItemIndex() }, 0);
                    println!("KEYBOARD_OPEN_OK concurrent_updates={}", h.updates);
                    h.viewer.as_ref().unwrap().update(fixtures.clone(), 1);
                })
                .unwrap();
                gpui::Timer::after(Duration::from_secs(1)).await;
                assert_eq!(unsafe { panel().currentPreviewItemIndex() }, 1);
                println!("UPDATE_OK");
                send_key(49, " ");
                gpui::Timer::after(Duration::from_secs(1)).await;
                w.update(cx, |h, _, _| {
                    assert!(
                        !h.viewer.as_ref().unwrap().is_visible(),
                        "Space must close the panel"
                    );
                    println!(
                        "SPACE_OK source_released={}",
                        unsafe { panel().dataSource() }.is_none()
                    );
                    h.viewer.as_ref().unwrap().show(fixtures.clone(), 0);
                })
                .unwrap();
                gpui::Timer::after(Duration::from_secs(1)).await;
                send_key(53, "\u{1b}");
                gpui::Timer::after(Duration::from_secs(1)).await;
                w.update(cx, |h, _, _| {
                    assert!(
                        !h.viewer.as_ref().unwrap().is_visible(),
                        "Escape must close the panel"
                    );
                    println!("ESCAPE_OK");
                    h.viewer.as_ref().unwrap().show(fixtures.clone(), 1);
                    h.viewer.as_ref().unwrap().close();
                    h.viewer.as_ref().unwrap().show(fixtures.clone(), 0);
                    assert!(h.viewer.as_ref().unwrap().is_visible());
                })
                .unwrap();
                gpui::Timer::after(Duration::from_secs(1)).await;
                w.update(cx, |h, _, _| {
                    assert!(
                        h.viewer.as_ref().unwrap().is_visible() && panel().isVisible(),
                        "Deferred close must not dismiss reopened viewer"
                    );
                    assert!(unsafe { panel().dataSource() }.is_some());
                    println!("SAME_TURN_REOPEN_OK");
                    let viewer = h.viewer.as_ref().unwrap();
                    for _ in 0..100 {
                        viewer.close();
                        viewer.show(fixtures.clone(), 0);
                        viewer.update(fixtures.clone(), usize::MAX);
                    }
                })
                .unwrap();
                gpui::Timer::after(Duration::from_secs(1)).await;
                w.update(cx, |h, _, _| {
                    assert!(panel().isVisible());
                    assert_eq!(unsafe { panel().currentPreviewItemIndex() }, 1);
                    println!("RAPID_REQUESTS_OK");
                    h.viewer.as_ref().unwrap().update(Vec::new(), 0);
                })
                .unwrap();
                gpui::Timer::after(Duration::from_secs(1)).await;
                w.update(cx, |h, _, _| {
                    assert!(!panel().isVisible(), "Empty selection must close the panel");
                    println!("EMPTY_SELECTION_OK");
                    h.viewer.as_ref().unwrap().show(fixtures.clone(), 0);
                    h.viewer.take();
                    assert!(unsafe { panel().dataSource() }.is_none());
                    println!("DROP_PENDING_OK");
                })
                .unwrap();
                gpui::Timer::after(Duration::from_secs(1)).await;
                assert!(!panel().isVisible());
                // Also drop a live owner, rather than only cancel a queued open.
                w.update(cx, |h, window, cx| {
                    let viewer =
                        native::Viewer::new(window, cx.foreground_executor().clone(), |err| {
                            panic!("Native preview failed: {err}")
                        })
                        .unwrap();
                    viewer.show(fixtures.clone(), 0);
                    h.viewer = Some(viewer);
                })
                .unwrap();
                gpui::Timer::after(Duration::from_secs(1)).await;
                assert!(panel().isVisible());
                w.update(cx, |h, _, _| {
                    h.viewer.take();
                })
                .unwrap();
                gpui::Timer::after(Duration::from_secs(1)).await;
                assert!(unsafe { panel().dataSource() }.is_none());
                assert!(unsafe { panel().delegate() }.is_none());
                assert!(unsafe { panel().currentController() }.is_none());
                println!("DROP_VISIBLE_OK");
                println!(
                    "FINAL visible={} source={} controller={}",
                    panel().isVisible(),
                    unsafe { panel().dataSource() }.is_some(),
                    unsafe { panel().currentController() }.is_some()
                );
                assert!(!panel().isVisible(), "Panel stays closed after owner drops");
                cx.update(|cx| cx.quit()).unwrap();
            })
            .detach();
        });
    }
}

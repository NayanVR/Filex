//! Application entry point: logging, telemetry, and the main window.

use super::*;

pub fn run() {
    let _logging_guard = filex::diagnostics::logging::init("filex");
    filex::diagnostics::telemetry::install_panic_hook("filex");
    // Sentry (UI process only; on-by-default, opt-out). The
    // `crash_reports` setting is consent and gates the whole integration,
    // so read it from disk before the app exists. The returned guard
    // flushes pending events on exit and must live for the whole run.
    // Builds without `observability` never link the SDK — which is how the
    // elevated `filex-indexd` service is built.
    #[cfg(feature = "observability")]
    let _sentry_guard = {
        let consent = filex::settings::default_settings_file()
            .and_then(|file| {
                let legacy = filex::ingest::default_roots_file();
                filex::settings::Settings::load(&file, legacy.as_deref()).ok()
            })
            .is_some_and(|settings| settings.crash_reports);
        filex::diagnostics::observability::init("filex", env!("CARGO_PKG_VERSION"), consent)
    };
    // A startup line at the default level, so a blank log file means "not
    // writing", not "nothing happened". Names the log directory, and
    // slow-op warnings land here without any RUST_LOG.
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        "filex starting — slow operations (>{SLOW_OP_MS}ms) log at warn; \
         set RUST_LOG=filex=debug for per-scan timing"
    );
    gpui_platform::application()
        .with_assets(ui::assets::Assets)
        .run(|cx: &mut App| {
            // Register the bundled UI font before anything renders.
            ui::fonts::register(cx);
            // A default theme so `cx.theme()` is valid from the first frame;
            // the workspace refines it against the real window appearance as
            // soon as a window exists (see the open-window closure below).
            cx.set_global(Theme::dark());
            cx.on_action(|_: &Quit, cx| cx.quit());
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();

            let bounds = Bounds::centered(None, size(px(1120.), px(760.)), cx);
            let titlebar = platform::titlebar_options();
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    window_min_size: Some(size(px(860.), px(520.))),
                    titlebar: Some(titlebar),
                    app_id: Some("dev.filex.app".into()),
                    ..Default::default()
                },
                |window, cx| {
                    cx.activate(true);
                    let workspace = cx.new(|cx| {
                        let workspace = Workspace::new(cx);
                        // Focus the workspace, not the search box, so the
                        // app starts on the file list and single-key
                        // shortcuts work at once. `/` moves focus in.
                        window.focus(&workspace.focus_handle, cx);
                        workspace
                    });
                    // A window now exists, so its OS appearance is known:
                    // resolve the theme against it, and keep it in sync as
                    // the user flips the OS between light and dark.
                    workspace.update(cx, |ws, cx| {
                        ws.appearance = window.appearance();
                        ws.apply_theme(cx);
                    });
                    window
                        .observe_window_appearance({
                            let workspace = workspace.downgrade();
                            move |window, cx| {
                                workspace
                                    .update(cx, |ws, cx| {
                                        ws.appearance = window.appearance();
                                        ws.apply_theme(cx);
                                    })
                                    .ok();
                            }
                        })
                        .detach();
                    workspace
                },
            )
            .expect("failed to open the main window");
        });
}

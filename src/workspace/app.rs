//! Application entry point: logging, telemetry, and the main window.

use super::*;
use filex::launch::Browse;

/// Run the browsing UI, landing on `browse`. A target is classified
/// (file vs folder) here, before any window exists, so neither the
/// hand-off nor the first frame waits on the file system.
pub fn run(browse: Browse) {
    let _logging_guard = filex::diagnostics::logging::init("filex");
    filex::diagnostics::telemetry::install_panic_hook("filex");
    // Sentry (UI process only; on-by-default, opt-out). The
    // `share_diagnostics` setting is consent and gates the whole integration,
    // so read it from disk before the app exists. The returned guard
    // flushes pending events on exit and must live for the whole run.
    // Builds without `observability` never link the SDK — which is how the
    // elevated `filex-indexd` service is built.
    #[cfg(feature = "observability")]
    let _sentry_guard = {
        let consent = filex::settings::default_settings_file()
            .and_then(|file| filex::settings::Settings::load(&file).ok())
            .is_some_and(|settings| settings.share_diagnostics);
        filex::diagnostics::observability::init("filex", env!("CARGO_PKG_VERSION"), consent)
    };
    let browse = browse.normalize();
    let Some(primary) = startup::claim_instance(&browse) else {
        return;
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
            init_app(cx);
            // Launches forwarded by later `filex <path>` runs arrive on the
            // single-instance thread; hop them onto the UI thread here.
            let (launch_tx, mut launch_rx) = futures::channel::mpsc::unbounded::<Browse>();
            if let Some(primary) = primary {
                primary.serve(move |browse| {
                    launch_tx.unbounded_send(browse).ok();
                });
            }
            let window =
                open_workspace_window(size(px(1120.), px(760.)), None, cx, |workspace, cx| {
                    workspace.apply_launch(browse, false, cx)
                });
            cx.spawn(async move |cx| {
                use futures::StreamExt as _;
                while let Some(browse) = launch_rx.next().await {
                    tracing::info!("forwarded launch");
                    let applied = window.update(cx, |workspace, window, cx| {
                        window.activate_window();
                        workspace.apply_launch(browse, true, cx);
                    });
                    if applied.is_err() {
                        break;
                    }
                }
            })
            .detach();
        });
}

/// App-wide setup shared by the browser and the picker: fonts, a first
/// theme, Quit, and the lifecycle logging.
pub(super) fn init_app(cx: &mut App) {
    // Register the bundled UI font before anything renders.
    ui::fonts::register(cx);
    // A default theme so `cx.theme()` is valid from the first frame;
    // the workspace refines it against the real window appearance as
    // soon as a window exists (see `open_workspace_window`).
    cx.set_global(Theme::dark());
    cx.on_action(|_: &Quit, cx| {
        tracing::info!("quit requested (Quit action)");
        cx.quit();
    });
    // Every way the app can go away is logged, so "the window
    // disappeared" can be told apart from a crash (no line at all).
    cx.on_window_closed(|cx, _| {
        let remaining = cx.windows().len();
        tracing::info!(remaining, "window closed");
        if remaining == 0 {
            cx.quit();
        }
    })
    .detach();
    cx.on_app_quit(|_| async {
        tracing::info!("filex exiting");
    })
    .detach();
}

/// Open a window holding a fresh [`Workspace`], wired to the OS
/// appearance. `setup` runs once the workspace exists, before the first
/// frame.
pub(super) fn open_workspace_window(
    window_size: gpui::Size<Pixels>,
    title: Option<SharedString>,
    cx: &mut App,
    setup: impl FnOnce(&mut Workspace, &mut Context<Workspace>) + 'static,
) -> gpui::WindowHandle<Workspace> {
    let bounds = Bounds::centered(None, window_size, cx);
    let mut titlebar = platform::titlebar_options();
    if title.is_some() {
        titlebar.title = title;
    }
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
                let mut workspace = Workspace::new(cx);
                setup(&mut workspace, cx);
                // Focus the workspace, not the search box, so the
                // app starts on the file list and single-key
                // shortcuts work at once. `/` moves focus in. A Save
                // picker starts in its file-name box instead.
                let focus = workspace.initial_focus(cx);
                window.focus(&focus, cx);
                workspace
            });
            // A window now exists, so its OS appearance is known:
            // resolve the theme against it, and keep it in sync as
            // the user flips the OS between light and dark.
            workspace.update(cx, |ws, cx| {
                ws.appearance = window.appearance();
                ws.apply_theme(cx);
            });
            // Losing focus mid-drag (Alt+Tab) hands the drag to the
            // OS; focus changes log at debug (RUST_LOG=filex=debug).
            workspace.update(cx, |_, cx| {
                cx.observe_window_activation(window, |ws, window, cx| {
                    let active = window.is_window_active();
                    tracing::debug!(active, "window activation");
                    if !active {
                        ws.deactivated_mid_drag(window, cx);
                    }
                })
                .detach();
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
    .expect("failed to open the main window")
}

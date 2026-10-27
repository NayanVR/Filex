//! Settings › Integration: the opt-in "default file explorer" and "system
//! Open/Save dialogs" switches. Their truth lives in the OS (registry on
//! Windows), not in the settings file, so every read and write goes
//! through `filex::shell_integration` on the background executor.

use super::*;
use filex::shell_integration::{self as shell, Status};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Integration {
    DefaultExplorer,
    Dialogs,
}

impl Workspace {
    /// Re-read what is registered. Called when the Integration page opens
    /// and after every change.
    pub(super) fn refresh_integration_status(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let status = cx
                .background_executor()
                .spawn(async { shell::status() })
                .await;
            this.update(cx, |this, cx| {
                this.integration_status = Some(status);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn set_integration(&mut self, which: Integration, on: bool, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let exe = std::env::current_exe()?;
                    match (which, on) {
                        (Integration::DefaultExplorer, true) => shell::register_default(&exe),
                        (Integration::DefaultExplorer, false) => shell::unregister_default(),
                        (Integration::Dialogs, true) => {
                            shell::register_dialogs(&shell::dialog_shim_path(&exe))
                        }
                        (Integration::Dialogs, false) => shell::unregister_dialogs(),
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                if let Err(err) = result {
                    tracing::warn!("integration change failed: {err:#}");
                    this.notice = Some(format!("couldn't change the integration: {err:#}").into());
                }
                this.refresh_integration_status(cx);
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn render_integration(&self, theme: &Theme, cx: &mut Context<Self>) -> gpui::Div {
        let support = shell::support();
        let status = self.integration_status.unwrap_or(Status::default());
        let mut group = ui::settings_pane::group(theme);
        if support.default_explorer {
            group = group.child(
                ui::settings_pane::toggle_row(
                    theme,
                    "default-explorer",
                    "Default file explorer",
                    "Open folders and drives in filex instead of File Explorer. \
                     Places filex can't show, like Control Panel, still open in File Explorer.",
                    status.default_explorer,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.set_integration(
                        Integration::DefaultExplorer,
                        !status.default_explorer,
                        cx,
                    );
                })),
            );
        }
        if support.dialogs {
            group = group.child(
                ui::settings_pane::toggle_row(
                    theme,
                    "system-dialogs",
                    "Use filex for Open and Save dialogs (experimental)",
                    "Other apps show filex when you open or save a file. Apps that add their \
                     own controls to the dialog, and apps running as administrator, keep the \
                     standard dialog.",
                    status.dialogs,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.set_integration(Integration::Dialogs, !status.dialogs, cx);
                })),
            );
        }
        if !support.default_explorer && !support.dialogs {
            group = group.child(ui::settings_pane::note(
                theme,
                "Not available on this platform yet.",
            ));
        }
        group
    }
}

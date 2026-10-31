//! The This PC page: every drive on one page, Windows' stand-in for `/`.
//! It's a location like any folder (tabs, history, Up and the path field
//! all carry it), but it lists [`Drive`]s from the background refresh
//! rather than reading a directory, and nothing can be pasted or
//! dropped onto it.

use super::*;

/// A drive tile's fixed width; tiles wrap to fill the pane.
const TILE_WIDTH: f32 = 260.;

impl Workspace {
    /// Whether the active tab is showing the This PC page.
    pub(super) fn at_this_pc(&self) -> bool {
        filex::drives::is_this_pc(&self.cwd)
    }

    /// Install the This PC page. Drives come from the shared refresh, so
    /// there is no listing to wait for; one refresh is kicked off so a
    /// drive plugged in since the last timer tick shows up now.
    pub(super) fn show_this_pc(&mut self, cx: &mut Context<Self>) {
        self.path_request = self.path_request.wrapping_add(1);
        self.path_loading = false;
        self.close_quick_look();
        self.cwd = filex::drives::this_pc();
        self.listed_dir = self.cwd.clone();
        self.entries.clear();
        self.load_error = None;
        self.selection.clear();
        self.renaming = None;
        self.pending_delete = None;
        self.sync_path(cx);
        self.refresh_drives(cx);
        self.refresh_preview(cx);
        cx.notify();
    }

    /// Re-enumerate drives once, off the UI thread.
    pub(super) fn refresh_drives(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let drives = cx
                .background_executor()
                .spawn(async { filex::drives::list_drives() })
                .await;
            this.update(cx, |this, cx| this.set_drives(drives, cx)).ok();
        })
        .detach();
    }

    /// Store a fresh drive list, repainting only when it changed.
    pub(super) fn set_drives(&mut self, drives: Vec<Drive>, cx: &mut Context<Self>) {
        if self.drives != drives {
            self.drives = drives;
            cx.notify();
        }
    }

    /// The drive tiles. At most 26 on Windows, so no virtualization.
    pub(super) fn render_this_pc(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = *cx.theme();
        if self.drives.is_empty() {
            return ui::pane::message_state(
                &theme,
                "icons/hard-drive.svg",
                "Looking for drives",
                "Your drives will appear here in a moment.",
            );
        }
        div()
            .id("this-pc")
            .flex()
            .flex_wrap()
            .content_start()
            .gap_3()
            .p_4()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .children(self.drives.iter().enumerate().map(|(ix, drive)| {
                let path = drive.path.clone();
                let selected = self.this_pc_selected == Some(ix);
                // Explorer's model: a click selects the drive, a double
                // click opens it.
                drive_tile(&theme, ix, drive, selected).on_click(cx.listener(
                    move |this, event: &ClickEvent, _window, cx| {
                        if event.click_count() >= 2 {
                            this.navigate(path.clone(), cx);
                        } else {
                            this.this_pc_selected = Some(ix);
                            cx.notify();
                        }
                    },
                ))
            }))
            .child(connect_tile(&theme).on_click(cx.listener(
                |this, _: &ClickEvent, window, cx| this.open_connect(None, None, window, cx),
            )))
            .into_any_element()
    }
}

/// The tile after the drives that opens "Connect to Server" (FIL-59).
fn connect_tile(theme: &Theme) -> gpui::Stateful<gpui::Div> {
    let hover = theme.hover;
    div()
        .id("this-pc-connect")
        .flex()
        .items_center()
        .gap_3()
        .w(px(TILE_WIDTH))
        .p_3()
        .rounded_xl()
        .border_1()
        .border_dashed()
        .border_color(theme.border)
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
        .child(ui::icon::ui_icon("icons/server.svg", theme.text_dim).size(px(32.)))
        .child(
            div()
                .flex()
                .flex_col()
                .gap_1p5()
                .child(div().text_sm().child("Connect to Server…"))
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.text_dim)
                        .child("Open a shared folder on the network"),
                ),
        )
}

/// One drive: icon, name, capacity bar, and free space, Explorer-style.
fn drive_tile(
    theme: &Theme,
    ix: usize,
    drive: &Drive,
    selected: bool,
) -> gpui::Stateful<gpui::Div> {
    let hover = theme.hover;
    div()
        .id(("this-pc-drive", ix))
        .flex()
        .items_center()
        .gap_3()
        .w(px(TILE_WIDTH))
        .p_3()
        .rounded_xl()
        .border_1()
        .border_color(if selected { theme.accent } else { theme.border })
        .bg(if selected {
            theme.selected
        } else {
            theme.stripe
        })
        .cursor_pointer()
        .when(!selected, move |s| s.hover(move |s| s.bg(hover)))
        .tooltip(ui::tooltip::text_tooltip(display_path(&drive.path), *theme))
        .child(ui::icon::ui_icon(drive_icon(drive.kind), theme.text_dim).size(px(32.)))
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .gap_1p5()
                .child(div().text_sm().truncate().child(drive.name.clone()))
                .when(drive.capacity.is_some(), |s| {
                    s.child(ui::sidebar::capacity_bar(theme, drive.used_fraction()))
                })
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.text_dim)
                        .truncate()
                        .child(drive_detail(drive)),
                ),
        )
}

/// A drive's glyph: a server for network shares, a disk otherwise.
pub(super) fn drive_icon(kind: DriveKind) -> &'static str {
    match kind {
        DriveKind::Network => "icons/server.svg",
        DriveKind::Fixed | DriveKind::Removable => "icons/hard-drive.svg",
    }
}

/// The line under a drive's name: "12 GB free of 1 TB", or "Not
/// responding" for a network drive that didn't report its size in time.
pub(super) fn drive_detail(drive: &Drive) -> String {
    match drive.capacity {
        Some(capacity) => format!(
            "{} free of {}",
            format_size(capacity.free_bytes),
            format_size(capacity.total_bytes)
        ),
        None => "Not responding".into(),
    }
}

/// The This PC subtitle and status line: "1 drive", "3 drives".
pub(super) fn drive_count(n: usize) -> String {
    format!("{n} drive{}", if n == 1 { "" } else { "s" })
}

#[cfg(test)]
mod tests {
    use filex::drives::{Capacity, Drive, DriveKind};

    use super::{drive_count, drive_detail, drive_icon};

    fn nas(capacity: Option<Capacity>) -> Drive {
        Drive {
            name: "Media (nas)".into(),
            path: "/mnt/media".into(),
            kind: DriveKind::Network,
            capacity,
            share: None,
        }
    }

    #[test]
    fn a_drive_that_answered_shows_its_free_space() {
        let capacity = Capacity {
            total_bytes: 2048,
            free_bytes: 1024,
        };
        let detail = drive_detail(&nas(Some(capacity)));
        assert!(detail.contains(" free of "), "{detail}");
    }

    #[test]
    fn a_silent_network_drive_says_so() {
        assert_eq!(drive_detail(&nas(None)), "Not responding");
    }

    #[test]
    fn network_drives_get_the_server_glyph() {
        assert_eq!(drive_icon(DriveKind::Network), "icons/server.svg");
        assert_eq!(drive_icon(DriveKind::Fixed), "icons/hard-drive.svg");
        assert_eq!(drive_icon(DriveKind::Removable), "icons/hard-drive.svg");
    }

    #[test]
    fn drive_count_is_singular_for_one() {
        assert_eq!(drive_count(1), "1 drive");
        assert_eq!(drive_count(0), "0 drives");
        assert_eq!(drive_count(3), "3 drives");
    }
}

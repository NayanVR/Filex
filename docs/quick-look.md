# Quick Look viewer

On macOS, select one or more files in browse or search results and press
**Space** to open the system Quick Look panel. Space or Escape closes it.
The native viewer supplies document pages, media controls, fullscreen, and
“Open with…” where supported by macOS or an installed preview extension.
The existing details sidebar remains on Cmd-I. The Quick Look binding can
be changed or disabled in Keyboard settings; it does not run in text fields.

The viewer receives the current selection and lead item. Clicking another
selection updates an open viewer; navigating to another directory closes it.
Unsupported or missing files use Quick Look's native unavailable/icon display.

## Implementation

`src/quick_look/macos.rs` implements an AppKit responder and Quick Look data
source. The controller is attached lazily to the GPUI window's responder chain,
and only changes panel state after macOS grants control. File URLs are created
on demand; Filex does not read or decode documents on the UI thread. Native
callbacks are detached before closing and before the workspace is released.
The public viewer queues native presentation, selection updates, and teardown
on the foreground executor, outside GPUI app/entity/window updates. Quick Look
can pump a nested macOS event loop; invoking it with a GPUI borrow held lets
other foreground tasks reenter the app and panic. One driver serializes native
operations, combining pending selection changes and cancelling pending opens
when the viewer is closed or dropped. Presentation errors return to the
workspace notice through its weak entity handle.
Panel dismissal runs on the next event-loop turn to avoid racing presentation;
reopening or handing control to another viewer cancels that dismissal.
The controller follows Apple's
[QLPreviewPanel lifecycle](https://developer.apple.com/documentation/quicklookui/qlpreviewpanel).

This is a full-file viewer, separate from `src/thumbnails.rs`. It does not
replace grid thumbnails or their cache. Framework dependencies are gated to
macOS and the `app` feature, so the index daemon does not link Quick Look.

The viewer interface leaves room for Windows preview-handler hosting and Linux
desktop preview services. Those backends are not implemented yet, and the
shortcut is only exposed on macOS.

## Validation

Run `cargo test --bin filex workspace::shortcuts::tests` for the focus, remapping,
and disabling checks. For an automated native lifecycle check in a macOS GUI
session, run `cargo run --example quick_look_smoke -- /path/to/sample.pdf`.
Omit the path to use temporary text fixtures. This posts a real AppKit Space
event to the GPUI window while another task continuously updates the app. It
asserts that initial native panel creation is deferred beyond the action, then
checks selection updates, Space/Escape, immediate reopen, clearing selection,
and owner cleanup. It exits when finished.

Check a PDF with multiple pages, an image, a text file, and an unsupported file:

- Space opens the selected file; Space, Escape, and the close button dismiss it.
- Reopen repeatedly, including immediately after closing.
- Select multiple files and switch between them in the native panel.
- Click another file while the panel is open; clear selection or change folders.
- Close Filex while the preview is opening; no detached preview should remain.
- Type spaces in search, path, rename, and tag inputs; no preview should open.

The native viewer's contents and available controls depend on the file type and
installed macOS providers. Thumbnail benchmark results do not measure this
full viewer's document-loading or playback performance.

See [Failure boundaries](resilience.md) for the command-driver contract,
thumbnail resource limits, fault-injection tests, and remaining process-level
failure risks.

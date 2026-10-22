# Windows thumbnails and Space-bar previews

The application uses its existing Rust decoder and 512-entry RAM cache for
ordinary images. JPEGs with at least 12 million pixels try Windows Shell first;
this is a tunable routing heuristic based on the benchmark's 2 MP and 24 MP
fixtures, not a measured crossover point. Documents, video, camera formats and
other supported extensions can request Shell thumbnails. Availability depends
on installed Windows codecs and providers. Failed requests retain file artwork.

## The preview window

Space opens a Filex window drawn by GPUI (`src/quick_look/preview_window.rs`):
a toolbar with Previous/Next (multi-selections, up to 64 around the lead),
the file name, position and size, and an Open button. Left/Right navigate,
Enter opens, Space and Escape close; the close box hides the window for reuse.
Changes to Filex's selection update the same window. It follows the Filex
theme, is owned by the Filex window (stays above it, no taskbar button of its
own) and opens centred on its display.

What fills the body depends on the file (`preview_window::route`):

- **Images** Filex can decode (`policy::rust_image`) are decoded in-process
  on the background executor, with the thumbnail decoder's limits (64 MiB
  input, 128 MiB allocation, 32768 px per axis) and a 3840 px output edge.
  Small images stay at their own size. Moving to another file drops the
  task, cancelling a decode still in flight.
- **Everything else** goes to the file's registered `IPreviewHandler`, hosted
  by the isolated helper and embedded in the window (below).
- **No handler, or a failure,** shows a card with the file's artwork, size and
  an Open in default app button. PDF and Office previews are not guaranteed
  on clean Windows installations.

### Embedding the handler

GPUI renders through DirectComposition with a topmost target and
`WS_EX_NOREDIRECTIONBITMAP`, so it composites *above* child windows and an
ordinary GDI child of its window never shows. The helper therefore creates a
`WS_EX_LAYERED` child surface (Windows 8+ child layering gives it its own
surface) inside the preview window, at the body's bounds in device pixels, and
loads the handler into it. The view paints nothing over that region once the
handler reports ready, so the handler shows through; until then it paints its
background and a spinner over it. Layout changes become `Resize` requests.

Requirements and consequences, all exercised by the smoke test:

- The helper executable must declare Windows 8+ (`supportedOS`) in its
  manifest, or `CreateWindowExW` fails for the layered child. `filex.exe`
  gets gpui-ce's `gpui.manifest.xml`; the smoke example links GPUI for the
  same reason.
- Paths go to handlers without the `\\?\` prefix (`listing::plain_path`):
  handlers that open by file name, such as PDF viewers, fail on it.
- A cross-process parent/child pair shares input state. Clicking into the
  handler gives it keyboard focus; a mouse-down anywhere on GPUI's chrome asks
  the helper (not the UI thread, which a hung handler could stall) to hand
  focus back. Handlers forward Escape/Space/Left/Right through
  `IPreviewHandlerFrame::TranslateAccelerator` only if they choose to; the
  built-in text handler keeps them while it has focus, as in Explorer's
  preview pane.
- Every native show/hide/owner call runs in a foreground task outside GPUI
  updates. Hidden windows get their initial placement from GPUI's
  `activate_window`, so the first show goes through it.

## Process and resource boundaries

- Native work runs in copies of `filex.exe` with reserved helper switches,
  before GPUI initializes. Each helper owns one STA and pumps Windows messages.
  No Shell objects or helper HWNDs are owned by GPUI's threads.
- Two warm thumbnail helpers, one preview helper per viewer, bounded IPC
  frames, and a single coalesced pending presentation (plus a separately
  coalesced resize) prevent an unbounded backlog. Images never start a helper.
- Thumbnail requests and stalled preview message loops have a 10-second
  deadline. A failed helper is killed, the view shows the fallback card, and
  the next native file starts a new one. Closing the window stops the helper,
  allowing up to one second for `Unload`; closing Filex stops it before the
  preview window is destroyed.
- Windows jobs terminate helpers if Filex exits, and cap each helper's committed
  memory at 512 MiB. This is crash/hang containment, **not a security sandbox**.
  COM servers started separately by Windows can live outside this job and its
  memory accounting. Driver or OS failures cannot be made crash-proof by this.
- Request identifiers reject stale completion events after rapid selection
  changes. Pipes run on dedicated threads so a blocked write cannot disable
  the supervisor's timeout or block the UI.

The macOS Quick Look adapter retains its deferred, reentrancy-safe execution.
Linux behavior is unchanged; this change adds no Linux native preview service.

## Verification

`cargo run --locked --example windows_preview_smoke` on an interactive Windows
session exercises production helpers: MTA thumbnails, real BGRA thumbnail
pixels, bad files, a native text handler embedded in a parent window (pixels
inside its surface, nothing outside it), the unavailable result, resize,
coalesced presentations, forwarded keys, hide/close/reopen, cleanup, and
injected child crashes, malformed frames and hangs. Fault injection exists only
in that example. CI runs it on Windows, plus the application and policy tests.
The GPUI view's selection, routing, device-pixel snapping and stale-request
handling are unit tests in `preview_window.rs`. API readiness alone is not a
measurement of first paint or scrolling smoothness.

The earlier `preview_benchmark` continues comparing Rust and raw Shell adapters.
Its numbers do not include the production helper's IPC and startup overhead.
Measure the hybrid path separately before claiming the same end-to-end latency.

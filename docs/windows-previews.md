# Windows thumbnails and Space-bar previews

The application uses its existing Rust decoder and 512-entry RAM cache for
ordinary images. JPEGs with at least 12 million pixels try Windows Shell first;
this is a tunable routing heuristic based on the benchmark's 2 MP and 24 MP
fixtures, not a measured crossover point. Documents, video, camera formats and
other supported extensions can request Shell thumbnails. Availability depends
on installed Windows codecs and providers. Failed requests retain file artwork.

Space opens a separate, resizable preview window. Previous/Next buttons and
Left/Right navigate the selected files (up to 64 around the lead selection).
Space, Escape and the close button dismiss it. Changes to Filex's selection
update the same window. Open in default app is explicit and launches outside
the helper's lifetime job.

Images use a fitted, aspect-preserving view with bounded decoding and alpha
composited onto the background. Other formats try their registered
`IPreviewHandler`: stream initialization first, then Shell item or file.
Handlers are activated with `CLSCTX_LOCAL_SERVER`; their DLLs are never loaded
into Filex. No installed handler, corrupt files, and unavailable files display
a fallback with the filename and an Open button. PDF and Office previews are
not guaranteed on clean Windows installations.

## Process and resource boundaries

- Native work runs in copies of `filex.exe` with reserved helper switches,
  before GPUI initializes. Each helper owns one STA and pumps Windows messages.
  No Shell objects or HWND ownership cross into GPUI's MTA background executor.
- Two warm thumbnail helpers, one full-view helper per viewer, bounded IPC
  frames, and a single coalesced pending selection prevent an unbounded backlog.
- Thumbnail requests and stalled preview message loops have a 10-second
  deadline. A failed helper is killed and the next request can create a new one.
  Closing allows up to one second for `Unload`, then terminates the helper.
- Windows jobs terminate helpers if Filex exits, and cap each helper's committed
  memory at 512 MiB. This is crash/hang containment, **not a security sandbox**.
  COM servers started separately by Windows can live outside this job and its
  memory accounting. Driver or OS failures cannot be made crash-proof by this.
- Image fallback limits: 64 MiB input, 128 MiB decoder allocation limit, 32768
  pixels per axis, and a 2048-pixel output edge. Some codec allocations remain
  best-effort limited inside the OS-enforced helper budget.
- Request identifiers reject stale completion events after rapid selection
  changes. Pipes run on dedicated threads so a blocked write cannot disable
  the supervisor's timeout or block the UI.

The macOS Quick Look adapter retains its deferred, reentrancy-safe execution.
Linux behavior is unchanged; this change adds no Linux native preview service.

## Verification

`cargo run --locked --example windows_preview_smoke` on an interactive Windows
session exercises production helpers: MTA callers, real BGRA thumbnail pixels,
bad files, a native text handler, image and unsupported-format fallbacks,
resize, coalesced selection, keyboard navigation, close/reopen, cleanup, and
injected child crashes, malformed frames and hangs. Fault injection exists only
in that example. CI runs it on Windows, plus the application and policy tests.
API readiness alone is not a measurement of first paint or scrolling smoothness.

The earlier `preview_benchmark` continues comparing Rust and raw Shell adapters.
Its numbers do not include the production helper's IPC and startup overhead.
Measure the hybrid path separately before claiming the same end-to-end latency.

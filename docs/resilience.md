# Failure boundaries

Filex does not promise to be crash-proof. These boundaries keep expected errors
local and prevent the reentrant application borrow that occurred when opening
Quick Look from a GPUI action.

## UI and native previews

- UI actions submit owned preview commands. Only the foreground driver calls
  presentation, reload, and teardown, after GPUI releases app/entity/window
  borrows. `cx.defer` alone is not this boundary: its callback still has an app.
- One driver and one pending command per viewer serialize native operations.
  New selections replace pending ones; closing or dropping cancels pending opens.
  Empty selections close; indices are clamped before reaching the backend.
- No `RefCell` guards survive calls into AppKit, URL creation, native object
  release, or application error callbacks. Native callbacks read snapshots and
  never enter a GPUI update. Selection length is checked again after reload,
  because native dismissal can clear it synchronously.
- A presentation error detaches callbacks before notifying the workspace. A
  superseded request cannot overwrite a newer selection with an old error.
  Error delivery uses a weak workspace reference; closed windows are ignored.
- Quick Look's data source and delegate are unretained native references. The
  controller retains its dependencies, detaches callbacks before release, and
  defers final dismissal to avoid closing a viewer that was just reopened.
  Close is idempotent: explicit close followed by owner destruction must not
  schedule two overlapping dismissals. A retiring controller keeps its responder
  link alive until native control ends, then restores the original chain.

`quick_look::driver` has no GPUI dependency and is tested with a backend that
submits commands synchronously during presentation. The public viewer owns
scheduling; platform backends stay private. Future native preview backends
must preserve this contract, including main-thread affinity where required.

## Thumbnail decoding

`thumbnails::Cache` owns both admission and results. At most two jobs per
workspace run simultaneously. Busy requests do not create tasks or a backlog;
completion notifies the view and currently visible rows try again. Cache
eviction preserves active jobs, so late completions cannot overfill the cache
or cause duplicate work. Completed entries, including failures, cap at 512.

Decoders run off the UI thread. The policy rejects nonregular files and files
above 64 MiB, dimensions above 32,768 pixels, and decoded output above the
128 MiB allocation budget. These limits deliberately favor falling back to an
icon over allocating a huge image just to display a 128-pixel thumbnail.
Codec-internal allocation limits are best-effort, not a process memory cap.

Only the isolated synchronous Rust decoder runs inside `catch_unwind`. It owns
its decoder state and shares no mutable UI state. An unwinding decoder panic
becomes a failed thumbnail and releases its admission slot; the normal panic
hook still records diagnostics. Do not wrap the app/event loop or native FFI
in a blanket panic catcher and continue with potentially inconsistent state.

## Verification and limits

- `cargo test --locked --bin filex`: deterministic reentrancy, failure/retry,
  coalescing, empty selections, capacity pressure, corrupt files, dimensions,
  and decoder panic tests, alongside existing application tests.
- `cargo run --locked --release --example quick_look_smoke`: local macOS GUI
  test using real Space events while app updates continue. Covers native
  lifecycle, rapid commands, pending-owner drop, and visible-owner drop.
- `cargo bench --locked --bench thumbnail_bench`: generated JPEG decode
  comparison and cache admission/lookup overhead. Warm local I/O, not a full
  directory load or a native Quick Look benchmark.
- CI builds and tests the app on Windows, Linux, and macOS. The native smoke
  example is compiled in macOS CI; running it requires an interactive desktop.

Local release benchmark (2026-10-02, generated 1600×1200 JPEG): previous decode
9.007 ms, bounded decode 9.024 ms; the measured intervals overlap. Busy admission
14.3 ns and ready-cache lookup 17.6 ns. This measures check overhead, not the
throughput change from limiting parallel decodes to two per workspace.

OOM, aborting panics, native exceptions, segmentation faults, and stuck codecs
cannot be recovered by a Rust panic catcher. Filex's own image decoder still
runs in the UI process's worker pool, so it is not a hard crash or timeout
boundary. A supervised decoder helper process with resource/time limits is
the next isolation step if needed. The existing index daemon is already a
separate process and the UI reconnects after service failures.

These changes focus on preview lifecycle and decoding. They are not a claim
that every filesystem/UI path has been audited; for example, ordinary browse
navigation still performs a synchronous directory read and needs a separate
asynchronous navigation change.

## Run the Windows benchmark on GitHub

After `.github/workflows/thumbnail-benchmark.yml` is on the default branch,
open **Actions → Windows Thumbnail Benchmark → Run workflow**, select the
branch, and run it. The workflow is manual so pushes do not spend benchmark
minutes. Its first build may take longer; subsequent runs reuse Rust caches.

Download **windows-thumbnail-benchmark** from the completed run's artifacts.
It contains the environment, benchmark log, scope description, and Criterion
result files. The workflow runs on Windows Server 2022 and needs no GUI or RDP.
Failures also upload available diagnostics. Results expire after 14 days.

This currently compares the old and bounded Filex decoders, plus cache
admission/lookup. It does **not** compare against Windows Shell thumbnails:
that native benchmark adapter still needs to be implemented. The fixture is
a generated JPEG and I/O is warm; do not interpret these numbers as full
directory-load, mixed-format, or cold-cache results.

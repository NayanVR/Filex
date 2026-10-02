# Windows preview benchmark

Run **Actions → Windows Preview Benchmark → Run workflow → main**. One invocation
builds the optimized worker and runs the whole suite. Download
`windows-preview-benchmark`, read `summary.md` and `coverage.json`, and open
`gallery.html`. Per-case JSON/logs retain raw samples, failures and screenshots.
The generated source files and 1,200-file scroll corpus are excluded from the
artifact to keep it small. The workflow can take 30–60 minutes on a cold build.

## What is measured

| Area | Workload | Evidence |
|---|---|---|
| Thumbnail coverage/latency | Actual Filex decoder versus `IShellItemImageFactory`, 128px, JPEG/PNG/WebP/BMP/GIF/TIFF, portrait, transparency, 24MP, PDF/DOCX/RTF/text/HTML/SVG/ZIP/WAV/MP4, corrupt/oversized files | 20 raw calls per format/backend/cache condition, p50/p95/p99, HRESULT/error and thumbnail PNG |
| Caches | New file identities, repeated extraction-permitted calls, explicit cache-only requests | Separate results; icon fallback forbidden on Shell requests |
| Memory/CPU | Fresh worker for every case; 10ms private-commit/RSS samples, Windows peak working set, before/after CPU and handle counters | Raw samples and snapshots; preview-handler PID counters when discoverable/readable |
| Scrolling | Real GPU-rendered GPUI window using production cards, filename layout, icons, details pane and bounded cache; 1,200 paths, two background jobs, cache cap 512 | Frame intervals, render-to-callback durations, slow/fast/revisit phases, thumbnail readiness, errors, cleanup memory and screenshot |
| UI comparison | Icons only, Filex thumbnails, experimental Shell thumbnails; three rounds with rotated order | Same geometry, path corpus, cache and concurrency policy; native adapter used only by the benchmark |
| Full native previews | Associated out-of-process `IPreviewHandler` in an HWND/message pump; ten open/resize/unload cycles per fixture | API latency, child HWND appearance, post-unload child presence, host/handler memory and screenshot |
| Failure containment | Every case in a child process with an external deadline | Timeout, process exit and partial checkpoints retained; later cases still run |

## Interpretation limits

- **Filex does not implement a Windows full-file viewer.** Its current thumbnail/
  details components are exercised; native full previews cannot be described as
  a like-for-like speed comparison with a nonexistent Filex viewer.
- The live UI workload uses the actual production components, not the whole
  Workspace. It excludes indexing, navigation, input dispatch and workspace
  chrome. Programmatic scrolling and frame callback intervals are not a test of
  physical display latency or Explorer scrolling.
- Server 2022 may lack a suitable graphics adapter, interactive desktop or a
  handler for a format. Unsupported formats, HRESULT failures, missing results
  and timeouts are reported, not turned into successful previews. No Office/PDF
  provider is installed just to improve coverage: provider inventory records the
  runner's actual configuration. Missing desktop/GPU support makes required UI
  cases fail and the workflow red while still publishing available evidence.
- A native API returning and a child HWND appearing do **not** establish first
  paint or complete rendering. Screenshots after a fixed settle period need
  visual review. The suite does not claim PDF pagination, media playback or
  input-interaction correctness from a screenshot.
- `first` copies each file before timing to get a new path/identity; it does not
  flush filesystem/codec caches. COM setup is outside decoder timings. `repeat`
  re-decodes in Filex but can use Shell caches. `cache` is a Filex in-memory lookup
  versus native cache retrieval, which can involve disk/COM and pixel copying.
  Never collapse these layers into one "X is faster" result.
- Shell output includes HBITMAP-to-CPU BGRA conversion so it can feed GPUI, as
  Filex output does. PNG evidence encoding and fixture copying are outside the
  timed decode. Thumbnail quality/aspect/transparency need visual review too.
- Per-worker memory includes runtime and UI overhead. External Shell thumbnail
  surrogates and GPU allocations are excluded; preview-handler memory is a
  separate PID snapshot when obtainable, not added blindly to host totals.
  Shared surrogate peaks may predate a case. Private-commit sampling can miss
  sub-10ms spikes; kernel working-set peaks supplement it. Retention after
  cleanup is not by itself proof of a leak.
- Small sample p95/p99 estimates are coarse. Shared runners and software graphics
  are noisy; compare distributions in the same run and review all three UI
  rounds. Instrumentation adds some work to every backend equally.

## Running locally

From an interactive Windows desktop, with Rust, Python 3 and ffmpeg on PATH:

```powershell
cargo test --locked --example preview_benchmark
cargo build --locked --release --example preview_benchmark
python scripts/preview_benchmark.py --exe target/release/examples/preview_benchmark.exe --output benchmark-results
```

The executable is a development example; native experimental adapters and extra
Windows API dependencies are not added to the shipped application. The parent
uses `taskkill /T` only for its own timed-out worker. COM-managed shared surrogates
are not forcibly killed. No documents from the user's machine are read.

API contracts: [Shell thumbnail flags](https://learn.microsoft.com/en-us/windows/win32/api/shobjidl_core/nf-shobjidl_core-ishellitemimagefactory-getimage),
[preview hosting](https://learn.microsoft.com/en-us/windows/win32/shell/preview-handlers),
[association discovery](https://learn.microsoft.com/en-us/windows/win32/api/shlwapi/nf-shlwapi-assocquerystringa).

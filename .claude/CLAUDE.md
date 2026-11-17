# Project: Filex — Fast Cross-Platform File Explorer

## What this is
A native, cross-platform (Windows/macOS/Linux) file explorer replacement built in Rust with GPUI (Zed's UI framework). Core value prop is **speed**: instant navigation, instant filename search, no lag on huge folders. It can optionally act as the system's default file explorer and Open/Save dialog. It is NOT a semantic/AI search tool — that is Phase 3, not part of this build.

## Current phase
**Phase 2: explorer capabilities, UI polish, drop-in replacement.** Phase 1 (fast browser + instant filename search) is done. The phase map lives in `docs/roadmap.md` (2a explorer features, 2b Windows, 2c telemetry, 2d UI overhaul, 2e search ranking, 2f Magic mode, 2g default explorer); Linear holds the live work.

**Phase 3 gate:** do not introduce vector embeddings, ML inference, or content-based search (OCR, PDF text extraction, etc.). Magic mode is allowed because it is strictly rule-based (grammar + phrases, no model); its deferred LLM fallback is behind the same gate (`docs/design-magic-mode.md` §4). If a task drifts toward any of this, stop and flag it.

## Tech stack & hard constraints
- **Language:** Rust stable (pinned in `rust-toolchain.toml`; nightly breaks gpui's `pathfinder_simd`), edition 2024.
- **UI framework:** GPUI via **gpui-ce** (github.com/gpui-ce/gpui-ce), the community fork of Zed's GPUI, package-renamed to `gpui` in Cargo.toml so code still says `use gpui::…`. Pre-1.0, and it diverges from Zed's upstream (e.g. `palette`-backed colours, `window.focus(&h, cx)`, `gpui_platform::application()`), so always check the version pinned in Cargo.toml before assuming an API shape. Keep the exact `=` pins: crates.io also carries stale gpui-ce 0.3.x releases.
- **Target platforms:** Windows, macOS, Linux — every feature must be designed with all three in mind, even if implemented one at a time. Never write Windows-only (or Mac-only) code without a clear `#[cfg(target_os = ...)]` boundary and a plan for the other platforms.
- **No webview, no Electron-style architecture, no JS.** Everything renders through GPUI's native GPU pipeline.
- Reference implementation for patterns/components: GPUI Component (github.com/longbridge/gpui-component) — check it before hand-rolling a widget GPUI doesn't ship with, especially virtualized lists/tables. It targets Zed's GPUI, so borrow patterns from it, not exact API calls.
- When GPUI docs are insufficient (they usually are), the fallback is reading gpui-ce's source (github.com/gpui-ce/gpui-ce, `crates/gpui`, or the pinned copy under `~/.cargo/registry`) — check it before guessing at an API. Zed's own source is only useful for history and patterns now.

## Architecture (orient here first)
One crate (+ `crates/filex-dialog-shim`, the Windows Open/Save dialog cdylib). `lib.rs` exposes GUI-free logic for tests/benches; UI-only modules hang off `main.rs`.

- **Binaries:** `filex` (GPUI UI), `filex-indexd` (per-user index daemon), `filex-updater` (Windows demand-start service that installs updates silently, `docs/design-updater.md`), `filex-sign`, `filex-index-lab` (benchmarks only).
- **Features:** `app` = GPUI and everything UI. `--no-default-features` builds the index library + daemon headless. `observability` = Sentry, UI only; the daemon must never link a telemetry SDK. `index-v2-lab` = measurements, never shipped.
- **UI process:**
  - `workspace/` is the `Workspace` root view; its `impl` is split by concern (`navigation`, `search`, `file_ops`, `tabs`, `menus`, `shortcuts`, `render*`, `services`, `magic`, `picker`, …).
  - `ui/` holds reusable widgets and the theme (`docs/design-system.md`).
  - `listing/` reads folders live. Browsing never depends on the daemon.
  - `ops.rs` applies reversible file ops with an undo `Journal`. Ops touch only the filesystem, then send the daemon a hint (`ipc::notify_applied`).
  - `thumbnails`, `quick_look`, and `platform_preview` handle previews. Windows Shell preview handlers run out of process.
- **Index daemon (index v2, `docs/design-index-v2.md`; §19 is what shipped):**
  - Watchers and UI hints feed a single writer: WAL, then overlay, then a published epoch (`daemon/writer.rs`).
  - Immutable mmapped `.fx2` segments are built in a disposable worker process (`daemon/builder.rs`), and the writer publishes them by swapping the manifest.
  - `catalog/` holds the columnar store, deduplicated names, and WAL. Paths are rebuilt from parent links, never stored.
  - `search/` holds the planner, FM-index + wavelet top-K, and `filter`/`phrases`.
  - `daemon/ipc.rs` is authenticated loopback. Interactive search is bounded and paged by epoch; Magic uses a separate exhaustive stream.
- **Platform code** lives in per-OS files inside the feature's module (`connect/{windows,macos,linux}.rs`, `listing/platform.rs`, `ingest/windows.rs`, `quick_look/{macos,windows}.rs`, `workspace/shell_windows.rs`, …).

## Per-OS indexing approach (don't conflate these)
- **Shipped (all OSes):** the per-user daemon bootstraps with the read-only `ingest` walk and keeps up through `notify`-based watchers. Linux adds one inotify watch per directory itself so excluded areas are never walked; macOS (FSEvents) and Windows (ReadDirectoryChangesW) watch subtrees natively and filter. Startup and watcher overflow **reconcile** the roots; there is no journal-exact replay yet.
- **Windows fast path (committed, not built):** MFT enumeration (`FSCTL_ENUM_USN_DATA`) + USN Journal, via a *separate privileged helper service* feeding the per-user daemon. The daemon itself stays unelevated (decision 2026-10-14, `docs/design-updater.md`). Prerequisite: FIL-33, which puts enumeration/watching behind an index-source trait. Any code touching it needs elevation, so surface that to the user clearly.
- **macOS:** Full Disk Access (TCC) governs what the walk can see; surface it rather than failing silently.
- `docs/indexing-architecture.md` is the historical v1 design (in-UI index, USN tiers, fanotify). It does not describe the shipped runtime.

## Performance is the product — non-negotiables
- UI thread must never block on I/O, indexing, or thumbnail generation. All of that goes through background tasks/executors.
- Virtualized rendering is mandatory for any file list — never render more rows than are visible in the viewport.
- Thumbnails are lazy and cancellable (if the user scrolls past before a thumbnail finishes, don't keep spending CPU on it).
- Interactive search retrieves the best visible results within a candidate budget; it never enumerates or scores the whole match set.
- Animations and micro-interactions are wanted (roadmap "performance AND polish"), but never on latency-critical paths: search-as-you-type, list scrolling, and keyboard navigation are never gated on an animation, and nothing layout-animates inside a list row.
- Any change that could plausibly add latency to search-as-you-type or folder navigation should be called out explicitly, not silently merged.

## Code conventions
- `anyhow::Result` and `?` for fallible code. No `unwrap()`/`expect()` outside of tests and genuinely unreachable states.
- Keep platform-specific code isolated in clearly named per-OS modules, not scattered `#[cfg]` blocks throughout shared logic.
- Favor small, focused functions and reusable `ui/` components over growing `Workspace` further. This is a long-running project across many sessions, and future-you (or future-Claude) needs to be able to re-orient quickly.
- Add a short doc comment on any function touching raw filesystem structures (MFT, USN Journal, FSEvents, inotify, the `.fx2`/WAL formats) explaining *what* it assumes about the OS/filesystem, since these are exactly the places where subtle platform assumptions cause bugs later.
- GPUI gotchas: `svg()` paints nothing without its own explicit `text_color`, so use the `ui::icon` helpers. `.truncate()` only shows `…` when the text has a definite width.

## Testing
- Every new module (especially anything under `daemon`, `catalog`, `search`, `ingest`) ships with unit tests alongside the code, not as an afterthought at the end of a session.
- Use GPUI's `#[gpui::test]` macro and `TestAppContext` for anything that touches GPUI's entity/window/view system — don't try to test UI logic by instantiating a real window.
- Pure logic (catalog/segment formats, WAL recovery, search planning and ranking, filters/phrases, Magic parsing, file ops) is tested with plain `#[test]` and no GPUI dependency — keep business logic decoupled from the UI framework specifically so it's easy to test in isolation.
- Platform watcher/enumeration code is hard to unit test against a real filesystem — prefer fixture data (`tests/fixtures`, synthetic event streams) so tests run in CI without raw disk access or elevation.
- When the index-source trait lands (FIL-33), write one behavioral suite that runs against every source implementation so platforms are held to the same contract.
- Regression tests are mandatory for any bug fix — if something broke, add a test that would have caught it before merging the fix.
- Performance-sensitive paths have `criterion` benches in `benches/` (search, segment, filter, listing, magic, tags, thumbnails). Add or extend one when you touch such a path, since a passing test doesn't tell you if something got slower.
- Don't let test coverage lag behind feature work across sessions — if a session adds indexing or search logic without tests, treat that as incomplete, not done.

### Commands
- Library/daemon (what CI runs on Linux + Windows): `cargo test --no-default-features --features index-v2-lab` and `cargo check --all-targets --no-default-features`
- App + lib (CI on all three OSes): `cargo build` then `cargo test --bin filex --lib`
- Windows compile check from the Mac (the full app can't cross-compile because `ring` needs a Windows C toolchain): `PATH="/opt/homebrew/opt/llvm/bin:$PATH" cargo check --target x86_64-pc-windows-msvc --no-default-features --bins`. The full app and the MSI are validated only in CI.

## Workflow expectations
- **Linear is the plan of record.** Team `Filex` (FIL), project `P-FIL-1`:
  https://linear.app/nayanvr/project/filex-015c98b86862 — check it for what
  to work on next. The `docs/*.md` files stay the design and decision
  record; Linear issues point at them rather than restating them. When work
  lands, close the issue; when a doc records a new open item, file one.
- Verify with `cargo build` / `cargo test`. Don't launch the GUI app unless asked; if runtime behaviour matters, ask the user to run it (logs are near-empty by default; `RUST_LOG=filex=debug` and `scripts/profile.py` are the real signals).
- After rebuilding `filex-indexd` in release, restart any running daemon. It spawns segment workers from the on-disk binary, and a format mismatch makes compaction livelock silently at high CPU.
- This is a solo/small-team project built incrementally across sessions — don't assume prior context beyond what's in this file and the current conversation. If something about scope or architecture seems ambiguous, ask rather than guessing and building the wrong thing.
- Push back if a request seems to skip ahead of the current phase (see "Current phase" above) or underestimates complexity — the goal is a realistic, shippable project, not scope creep.
- When in doubt about GPUI API shape, don't hallucinate a plausible-looking API — check gpui-ce's source (or GPUI Component for patterns) first, or flag that it needs verification.

## Out of scope reminders (Phase 3 — do not build these yet)
- Vector embeddings / semantic search
- Local ML model inference (including an LLM fallback for Magic mode)
- Content-based search (OCR, PDF parsing, etc.)
- Shell-extension integration — this is a standalone app, not an Explorer/Finder plugin. (Exception decided 2026-10-27: the opt-in "default file explorer" registration and the Open/Save dialog shim are in scope — see docs/design-default-explorer.md. Explorer/Finder plugins — context-menu handlers, property sheets, overlays — stay out.)

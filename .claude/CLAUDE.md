# Project: Filex — Fast Cross-Platform File Explorer

## What this is
A native, cross-platform (Windows/macOS/Linux) file explorer replacement built in Rust with GPUI (Zed's UI framework). Core value prop is **speed**: instant navigation, instant filename search, no lag on huge folders. This is NOT a semantic/AI search tool — that is an explicit future phase, not part of this build.

## Current phase
**Phase 1: Fast native browser + instant filename search.**
Do not introduce vector embeddings, ML inference, or content-based search (OCR, PDF text extraction, etc.) in this phase. If a task seems to be drifting toward that, stop and flag it rather than proceeding.

## Tech stack & hard constraints
- **Language:** Rust (latest stable)
- **UI framework:** GPUI (pre-1.0, breaking changes are expected between versions — always check the version pinned in Cargo.toml before assuming an API shape)
- **Target platforms:** Windows, macOS, Linux — every feature must be designed with all three in mind, even if implemented one at a time. Never write Windows-only (or Mac-only) code without a clear `#[cfg(target_os = ...)]` boundary and a plan for the other platforms.
- **No webview, no Electron-style architecture, no JS.** Everything renders through GPUI's native GPU pipeline.
- Reference implementation for patterns/components: GPUI Component (github.com/longbridge/gpui-component) — check it before hand-rolling a widget GPUI doesn't ship with, especially virtualized lists/tables.
- When GPUI docs are insufficient (they usually are), the fallback is reading Zed's own source (github.com/zed-industries/zed, `crates/gpui`) — check for this before guessing at an API.

## Per-OS indexing approach (don't conflate these)
- **Windows:** Master File Table (MFT) parsing for instant filename index, USN Journal for live incremental updates. Requires elevated/raw disk access — surface this requirement to the user clearly in any code touching it.
- **Linux:** No MFT equivalent — use direct filesystem metadata scan + inotify for live updates. Treat this as a separate implementation, not a port of the Windows approach.
- **macOS:** Investigate leveraging or working alongside Spotlight's existing metadata store; use FSEvents for live updates. Also a separate implementation.
- Each platform's indexer should sit behind a shared trait/interface so the rest of the app (UI, search logic) doesn't need to know which OS it's running on.

## Performance is the product — non-negotiables
- UI thread must never block on I/O, indexing, or thumbnail generation. All of that goes through background tasks/executors.
- Virtualized rendering is mandatory for any file list — never render more rows than are visible in the viewport.
- Thumbnails are lazy and cancellable (if the user scrolls past before a thumbnail finishes, don't keep spending CPU on it).
- Any change that could plausibly add latency to search-as-you-type or folder navigation should be called out explicitly, not silently merged.

## Code conventions
- Prefer explicit error handling (`Result`, `?`) over `unwrap()`/`expect()` outside of tests and genuinely unreachable states.
- Keep platform-specific code isolated in clearly named modules (e.g. `indexer::windows`, `indexer::linux`, `indexer::macos`) behind a common trait, not scattered `#[cfg]` blocks throughout shared logic.
- Favor small, focused functions over large ones — this is a long-running project across many sessions, and future-you (or future-Claude) needs to be able to re-orient quickly.
- Add a short doc comment on any function touching raw filesystem structures (MFT, USN Journal, FSEvents) explaining *what* it assumes about the OS/filesystem, since these are exactly the places where subtle platform assumptions cause bugs later.

## Testing
- Every new module (especially anything under `indexer::*`) should ship with unit tests alongside the code, not as an afterthought at the end of a session.
- Use GPUI's `#[gpui::test]` macro and `TestAppContext` for anything that touches GPUI's entity/window/view system — don't try to test UI logic by instantiating a real window.
- Pure logic (indexing, parsing MFT records, USN Journal event handling, search ranking) should be tested with plain `#[test]` and no GPUI dependency at all where possible — keep business logic decoupled from the UI framework specifically so it's easy to test in isolation.
- Platform-specific indexer code (MFT parsing, inotify, FSEvents) is hard to unit test directly against a real filesystem — prefer testing against fixture data (recorded/mocked MFT records, synthetic FS event streams) so tests run in CI without needing OS-specific raw disk access or elevated permissions.
- For the shared indexer trait/interface, write a single suite of behavioral tests that runs against all platform implementations, so Windows/Linux/macOS indexers are held to the same contract instead of drifting into inconsistent behavior.
- Regression tests are mandatory for any bug fix — if something broke, add a test that would have caught it before merging the fix.
- Performance-sensitive paths (virtualized list rendering, search-as-you-type latency) should have a benchmark (e.g. via `criterion`) in addition to correctness tests, since a passing test doesn't tell you if something got slower.
- Don't let test coverage lag behind feature work across sessions — if a session adds indexing or search logic without tests, treat that as incomplete, not done.

## Workflow expectations
- This is a solo/small-team project built incrementally across sessions — don't assume prior context beyond what's in this file and the current conversation. If something about scope or architecture seems ambiguous, ask rather than guessing and building the wrong thing.
- Push back if a request seems to skip ahead of the current phase (see "Current phase" above) or underestimates complexity — the goal is a realistic, shippable project, not scope creep.
- When in doubt about GPUI API shape, don't hallucinate a plausible-looking API — check GPUI Component or Zed's source first, or flag that it needs verification.

## Out of scope reminders (do not build these yet)
- Vector embeddings / semantic search
- Local ML model inference
- Content-based search (OCR, PDF parsing, etc.)
- Shell-extension integration — this is a standalone app, not a Explorer/Finder plugin
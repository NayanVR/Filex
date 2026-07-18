# filex — Roadmap (Phase 2 and beyond)

Written 2026-07-17, after Phase 1 completion. This is the working plan for
future sessions — read it alongside `.claude/CLAUDE.md` (conventions,
non-negotiables) and `docs/indexing-architecture.md` (index internals).

## Where Phase 1 ended

Everything in CLAUDE.md's Phase 1 scope is implemented, tested (96 unit
tests + criterion benches), and CI-validated on Windows + Linux runners
(macOS is validated on the dev machine by design — no macOS CI job):

- GPUI shell: virtualized browse + search lists, real text input
  (EntityInputHandler: cursor/selection/clipboard/IME), selection +
  keyboard nav, open-with-default-app, file-type glyphs, lazy image
  thumbnails.
- Index: multi-root, Everything-style core (FRN-keyed on Windows),
  sub-2ms searches at 200k entries via top-K scan, arena compaction.
- Live updates: FSEvents (macOS), fanotify→inotify with degraded
  reconcile (Linux), USN journal / RDCW tiers (Windows).
- Persistence: per-root snapshots, journal-checkpoint replay (FSEvents +
  USN), periodic saves via PersistNow markers.
- Windows service split: `filex-indexd` + named-pipe IPC; the UI runs in
  service mode when the daemon is present, falls back seamlessly.

Known deliberate limitations: non-UTF-8 names browse but aren't indexed;
emoji glyphs stand in for a real icon set; thumbnail decodes are
lazy-by-visibility but not hard-cancelled mid-decode; daemon runs via
Task Scheduler, not yet as an SCM service.

---

## Standing principle from Phase 2 on: performance AND polish

Decided 2026-07-17: alongside the founding "performance is the product"
rule, every Phase 2+ feature should also invest in **animations and
micro-interactions** — the app should feel alive, not just fast. GPUI
supports this natively (`AnimationExt::with_animation`, easings like
`pulsating_between`/`bounce`, SVG transforms via `Transformation`), so
use it:

- Prefer animated SVG assets (spinners, chevrons, state markers) over
  static glyphs as the icon set matures.
- Give interactions feedback: hover/press states, pulsing "building"
  markers, shimmer placeholders while thumbnails decode, smooth
  progress bars on file-operation jobs, eased expand/collapse on
  panels and menus.
- Hard boundary (unchanged): animations stay OFF the latency-critical
  paths — search-as-you-type, list scrolling, and keyboard navigation
  are never gated on or slowed by an animation, and nothing
  layout-animates inside a `uniform_list` row. When in doubt, measure.

## Standing principle from Phase 2 on: reusable components, reviewable code

Also decided 2026-07-17: Phase 2+ code is written to be **reviewed by a
human later**, not just to work. Concretely:

- **Extract reusable UI components** instead of growing `main.rs`
  (already ~1200 lines): a `src/ui/` module family — list rows, icon
  cell, sidebar sections/rows, panels, menus, buttons, the settings
  form controls — each a small, documented, self-contained building
  block styled from shared palette constants. The first Phase 2a
  session should begin by carving the existing render code into these
  before adding features on top.
- **One concern per commit**, with messages that explain why (the
  existing history is the standard to keep). Don't mix drive-by
  refactors into feature commits; land the refactor first, then the
  feature on top.
- **Keep logic out of the view layer** so it stays unit-testable (the
  existing lib/app split is the model — extend it: sort comparators,
  settings, the undo journal all belong in the lib with tests).
- Doc comments on every public component and on anything with a
  non-obvious constraint; consistent naming with the existing code.

## Phase 2a — Explorer capabilities (primary track, macOS-friendly)

Work in this order; each block is roughly one session.

### 0. Carve main.rs into `src/ui/` components (per the reviewability principle)

**Done 2026-07-18.** `src/ui/` now holds theme (shared palette),
list_row, icon, sidebar, top_bar, status_bar (index status lines are
pure, unit-tested functions), pane (empty states), and search_input
(moved into the family). main.rs kept the Workspace state/logic and
shrank to composition of these blocks. Pure refactor, no behavior
change.

### 1. Settings foundation (do first — everything after generates settings)

**Done 2026-07-18.** `filex::settings` (lib, serde/serde_json) persists
the JSON below at `<config_dir>/filex/settings.json`; first launch
migrates and immediately rewrites `roots.list` roots into it, and both
the UI and `filex-indexd` keep reading `roots.list` as fallback for
one version (drop the fallback + `manager::save_roots` next version).
`SettingsStore` (app entity) persists changes off-thread and emits
`Changed`; a settings pane (gear button / cmd-,) swaps into the main
area with toggle rows for the settings consumed today. Logging landed
too: `filex::logging`, tracing → daily-rotated files in
`<data_local_dir>/filex/logs`, all runtime `eprintln!`s replaced.
Pulled forward from block 2: the hidden-files toggle is fully wired
(`Entry.is_hidden`: dotfiles everywhere + FILE_ATTRIBUTE_HIDDEN +
UF_HIDDEN), and hidden files are now hidden by default — a deliberate
behavior change.

- `Settings` struct in the lib; persisted as **JSON** (decision:
  JSON, not TOML) at `<config_dir>/filex/settings.json` via
  `serde`/`serde_json` (approved dependency addition, confined to the
  settings module).
- Single source of truth: fold `roots.list` into it (migrate on first
  load, keep reading the old file as fallback for one version).
- Initial keys (grow as needed):

```json
{
  "version": 1,
  "roots": ["/Users/example"],
  "show_hidden_files": false,
  "sort": { "by": "name", "ascending": true, "directories_first": true },
  "confirm_delete": true,
  "delete_to_trash": true,
  "thumbnails_enabled": true
}
```

- Workspace subscribes to settings changes (same event pattern as
  SearchInput). Settings UI = popup/pane in gpui; check gpui-component
  for form widgets before hand-rolling (CLAUDE.md rule).
- Structured local logging in the same session: `tracing` +
  rotating file in the data dir, replacing scattered `eprintln!`s.

### 2. Sorting + breadcrumbs (small, warm-up for list UI work)

- Sort comparator enum (name/size/modified/kind) + clickable column
  headers; defaults come from settings. Note: `listing::Entry` needs
  mtime — fetch lazily or accept one stat per entry in browse (browse
  dirs are small; the index never stats).
- Breadcrumbs: replace the top-bar path text with clickable segments.
- Hidden-files toggle: already done (landed with block 1).

### 3. File operations (the big block; likely 2+ sessions)

- Ops: copy, move, rename, delete. Architecture note: ops never touch
  the index — watchers pick up the changes as deltas automatically.
- **Undo** via an operation journal storing inverses (move A→B ⇒ move
  B→A; copy ⇒ delete-the-copy). Delete must go to **trash** to be
  undoable: evaluate the `trash` crate before hand-rolling per-OS
  backends (macOS NSFileManager, Windows IFileOperation/Recycle Bin,
  Linux freedesktop trash spec).
- Long operations = background jobs with progress UI, cancellation,
  and conflict prompts (skip/overwrite/rename). Job queue lives beside
  the executor patterns already in main.rs.
- Rename-in-place UI can reuse the SearchInput element.

### 4. Context menus

- Right-click menu for rows (open, reveal, copy path, rename, delete,
  index-this-folder) and for the sidebar (remove root — currently
  impossible from the UI).
- Evaluate gpui-component's menu/popover first; hand-roll an anchored
  popover only if it doesn't fit.
- Menus/popovers are a natural first home for the polish principle:
  eased open/close, hover feedback.

## Phase 2b — Finish the Windows story (parallel track; needs Windows hardware or CI-driven iteration)

Sequence matters here:

1. **SCM service wrapper** for `filex-indexd` using the
   `windows-service` crate (StartServiceCtrlDispatcher, stop/shutdown
   control codes → clean LiveIndex drop so snapshots save).
2. **MSI installer** via WiX (`cargo-wix`): installs app + service with
   ServiceInstall/ServiceControl tables — the one-time-admin-consent
   moment that gives standard users the USN fast path forever. CI's
   Windows runner can build and smoke-test the MSI (install, query
   service, uninstall).
3. **Code signing** (Azure Trusted Signing) to pass SmartScreen, then a
   **winget** manifest. Auto-update (Velopack) later, when there are
   external users.
4. Run the `#[ignore]`d full-volume MFT bootstrap test on real hardware
   once (`cargo test -- --ignored`).

## Phase 2c — Telemetry (decided stance; implement late in Phase 2)

Privacy stance is settled — this app sees filenames, and filenames are
private:

1. Local structured logs first (ships with Phase 2a block 1).
2. Panic-hook crash logs, user-initiated report sharing.
3. Remote metrics **last, opt-in only, aggregates only** (search
   latency percentiles, index sizes, watcher tier used) — never paths,
   filenames, or query strings. Defer endpoint choice until there are
   external users.

## Phase 3 — Future (unchanged, still out of scope)

Semantic search / embeddings, content extraction (OCR, PDF), local ML
inference, shell integration. Do not start these; flag drift per
CLAUDE.md. Phase 2 quietly serves this phase anyway (settings surface,
service split, perf telemetry).

## Also on the radar (unscheduled)

- Real icon set replacing emoji glyphs (SVG assets, per-platform look;
  design them animation-ready — spinners, rotating chevrons, state
  transitions — per the polish principle above).
- Manual UX pass on macOS + release packaging (.app bundle) — dogfood
  feedback should reorder Phase 2a items freely.
- Thumbnail hard-cancellation + disk cache if profiling ever shows the
  soft approach hurting.
- gpui version bumps: gpui is pre-1.0; expect breaking changes on every
  upgrade and always re-check APIs against the pinned version's source.

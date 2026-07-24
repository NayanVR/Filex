# filex — UI Enhancement Roadmap ("premium" overhaul)

Written 2026-07-21, after Phase 2a completed. Companion to
`docs/roadmap.md` — that file stays the plan of record for engine and
platform work (Phase 2b Windows story, 2c telemetry); this one plans
the visual/UX overhaul toward a polished, Finder-class UI. Reference
target: the Atlas-style mockup shared 2026-07-21 (light theme, tab
bar, card grid + list views, right-hand preview/details panel, tags,
drives with capacity bars).

Everything here renders through GPUI's native pipeline — no webview,
ever (CLAUDE.md). Zed proves this level of UI is reachable at native
speed with the same stack.

## Ground rules (unchanged, restated because a visual overhaul is
where they die)

- Performance is the product: virtualized rendering everywhere a list
  or grid can grow; nothing layout-animates inside virtualized rows;
  search-as-you-type and scrolling never wait on decoding, theming,
  or animation. When in doubt, measure.
- Preview/thumbnail work is lazy, background, and cancellable.
- Reusable components in `src/ui/`, styled only through the theme;
  logic stays in the lib with tests.
- Every block below re-checks GPUI APIs against the pinned version's
  source before use (no hallucinated APIs).

## Build order

Ordered so each block compounds: theme and icons close most of the
visual gap alone and everything later is built in the new skin, not
retrofitted.

### 1. Theme system + light theme + typography

**Done 2026-07-21.** `ui::theme` is now a `Theme` struct of semantic
`Rgba` slots (bg, panel, hover, selected, stripe, border, text,
text_dim, accent, on_accent, accent_selection, warn, success) with two
variants — `Theme::dark()` (pixel-identical to the old constants) and
`Theme::light()` (white content, cyan-blue accent deepened to `#0e8fce`
so `on_accent` white reads on fills). It lives in GPUI global state,
reached via the `ActiveTheme` extension (`cx.theme()`); every `ui::*`
component and the search input takes `&Theme` and no component
references a raw hex value. New `theme` setting (`system`/`light`/`dark`,
default `system`) resolves against `window.appearance()` and re-resolves
live via `observe_window_appearance`; the settings pane gained an
Appearance segmented control (`choice_row`/`segmented`/`segment`) that
restyles the app instantly. Typography: Inter (four static weights,
OFL, in `assets/fonts/`) is embedded via `include_bytes!`, registered
at startup (`ui::fonts::register`), and set as the root `font_family`
so it cascades everywhere. Accent decision (2026-07-21): keep the
cyan-blue family across both themes rather than the mockup's violet.
Per-size/weight theme tokens were deferred — the existing `text_xs/sm`
helpers carry the scale for now.

- Replace the `ui::theme` constants with a `Theme` struct: semantic
  slots (bg, panel, hover, selected, stripe, border, text, text-dim,
  accent, warn, success), two built-in variants (dark = current
  palette, light = new), reachable from every component. The palette
  was kept in one file exactly for this migration.
- Settings: `"theme": "light" | "dark" | "system"`; follow the OS
  when `system` (gpui exposes window appearance — verify API).
- Bundle a UI font (Inter or similar, OFL-licensed) via gpui's asset
  source + font registration; sizes/weights become theme tokens too.
- Exit criteria: switching theme in the settings pane restyles the
  whole app live; no component references a raw hex constant.

### 2. Real icon set (replaces emoji glyphs)

**Done 2026-07-21.** Lucide SVGs (ISC, `assets/icons/`) served by an
embedded `AssetSource` (`ui::assets`, `Application::with_assets`);
gpui's `svg()` tints each coverage mask with the passed theme color.
`ui::icon` gained `file_icon(theme, kind)` (folder in the accent, curated
per-kind hues), `ui_icon(path, color)` for any themed glyph, and
`spinner(...)` (1s linear rotation). Converted: file-type marks, sidebar
places (house/hard-drive) + root status (dot/loader-circle/triangle-
alert, building animated), toolbar up/gear, the search magnifier, sort-
header chevrons, breadcrumb chevrons, the FDA banner, and the job-cancel
✕. Emoji glyphs where an icon inherits the button color use a plain
`svg()` (so active/hover tints cascade); the rest pass an explicit color.
`FileKind::glyph` (emoji) is retained as the documented fallback.

Already flagged in roadmap.md "on the radar" — this is the single
biggest premium jump after the light theme.

- UI glyphs (chevrons, gear, search, close, sort arrows, sidebar
  markers): Lucide or Phosphor, MIT-licensed, shipped as SVG assets;
  rendered with `svg()` + theme colors; designed animation-ready
  (spinners, rotating chevrons) per the polish principle.
- File-type icons: colored folder + per-kind marks (image, video,
  audio, archive, code, document, generic) in a consistent style;
  extend `FileKind` mapping. App-specific marks (PDF/AI/Sketch-style)
  only if a good licensed set exists — no trademark art.
- `ui::icon` grows an asset-backed API; the emoji path stays as
  fallback until every glyph is covered, then dies.

### 3. Multi-select

**Done 2026-07-21.** Lib-side `filex::selection::Selection` (set of
indices + anchor + lead), unit-tested in isolation: `select_one`
(click), `toggle` (cmd/ctrl-click), `range_to` (shift-click),
`select_all` (cmd-a, propagated from the empty search input like the
clipboard keys), `move_lead`/`extend_lead` (arrow / shift-arrow). The
workspace's `selected: Option<usize>` became `selection: Selection`,
cleared whenever the active list changes. Clipboard holds `Vec<PathBuf>`;
copy/cut/delete and the context menu act on the whole selection. Batch
undo: `ops::Journal` now stores `Vec<Vec<AppliedOp>>` (one user action =
one batch = one undo) with `ops::undo_batch`; multi-delete and multi-
paste each record one batch. Multi-paste runs sequentially off-thread as
one job, auto-resolving conflicts to the next free name (single paste
keeps the interactive dialog). The context menu drops single-only
actions (open/rename/reveal/index) past one selected row. Status bar
shows "N of M selected · <combined size>".

Prerequisite for grid view and batch file ops, so it lands before
both.

- Selection becomes a set + anchor: click = single, cmd/ctrl-click =
  toggle, shift-click = range, cmd-a = all (propagated from the empty
  search input like the clipboard keys).
- File ops, clipboard, delete, and the context menu accept multiple
  targets (journal entries become one user action = one undo).
- Status bar: "N of M selected" + combined size.
- Lib-side selection-model logic with unit tests.

### 4. Grid / card view + view toggle + zoom

**Done 2026-07-21.** `ui::grid` provides card scaffolds + a pure,
unit-tested `columns_for(width, cell)`; `render_grid` is a `uniform_list`
whose rows are strips of N cards (N from the window width via
`viewport_size`, reflowing on resize) — virtualized, never one element
per file. Cards show a large icon/thumbnail, wrapped name, and a
size/age line, reusing the list's selection + click + context-menu
paths. Settings gained `view` (list/grid) and `grid_zoom` (index into
four `CARD_SIZES` steps); a top-bar list⇄grid toggle and a grid-only
−/+ zoom stepper drive them, both persisted. `render_icon_cell`,
`file_icon`, and `thumbnail_icon` take an edge size now; `THUMBNAIL_EDGE`
rose 48→128 so cards are crisp (the list downscales). A real draggable
zoom slider (the mockup's bottom-right control) is deferred — the
stepper covers the same size steps.

- A second render mode next to the list: cards with a large icon or
  thumbnail, name, size/age line. Virtualized by chunking cards into
  fixed-height uniform_list rows of N columns (N from pane width ÷
  card size) — never one element per file.
- Toggle in the top bar (list ⇄ grid) + a zoom slider (card size
  steps); both persisted in settings, per the mockup's bottom-right
  control.
- Thumbnails reuse the existing lazy decode cache; grid just asks at
  a bigger edge size.

### 5. Preview / details panel

**Done 2026-07-21.** `ui::details` renders a right-hand panel for the
lead item: a large preview (the image thumbnail at 160px, else the big
file-type icon — reusing `render_icon_cell`, no PDF/design parsing) over
name, Kind (`FileKind::label`), Size, Modified, Created, image
Dimensions, and Where. Created time + dimensions are fetched lazily
off-thread per selection (`fetch_preview_meta`: one `std::fs::metadata`
+ `image::image_dimensions` header read), guarded against stale results
by comparing the lead path. Settings gained `preview_open` +
`preview_width`; a top-bar toggle (panel-right icon) and cmd-i/ctrl-i
drive it, width persisted. The grid's column count subtracts the open
panel's width so cards reflow. Drag-to-resize is deferred (width is
persisted at a fixed default for now); a larger dedicated preview decode
(vs. reusing the 128px thumbnail) is a possible refinement.

- Right-hand collapsible panel for the selected item: large preview,
  name, kind, size, created/modified, image dimensions.
- Previews: images via the existing decode path at panel size
  (background, cancellable); everything else shows the file-type icon.
  No content parsing of PDFs/design files — that is Phase 3 territory
  and stays out.
- Metadata beyond what browse already stats (created time, image
  dimensions) is fetched lazily per selection, off-thread.
- Toggle via top-bar button + keybinding; width persisted.

### 6. Tabs

**Done 2026-07-21.** Split across two commits per the plan. 6a
disentangled the selection into per-list `selection` (browse) +
`search_selection` (global). 6b added `TabSnapshot` (cwd, entries,
load_error, browse selection, scroll, back/forward history): the active
tab's state stays live on the Workspace so the browse code is untouched,
and `snapshot_active`/`restore_tab` swap it on switch (transient rename/
armed-delete are dropped, not saved). `open_tab`/`activate_tab`/
`close_tab` manage the `Vec<TabSnapshot>`; search stays global and result
activation targets the active tab. `ui::tabs` renders the strip (shown
only with 2+ tabs) with per-tab close ✕, middle-click close, and a "+".
Per-tab back/forward history with top-bar ‹ › buttons (dimmed when
empty). Keys: cmd-t/ctrl-t new, cmd-w/ctrl-w close (last tab closes the
window), ctrl-tab / ctrl-shift-tab cycle, cmd-[ / cmd-] (alt-left/right)
back/forward. Drag-reorder was left out (the "only if cheap" option).

- Multiple browse locations, one window: tab bar under the titlebar
  (mockup's top strip). Each tab owns cwd, selection, scroll,
  history; search stays global (Spotlight model) but result
  activation targets the active tab.
- cmd-t / cmd-w / ctrl-tab keybindings; middle-click close; "+" to
  open; drag-reorder only if cheap.
- Workspace state refactor: today's single cwd/selection moves into a
  per-tab struct — the meat of this block is that refactor, done as
  its own commit before the tab UI.

### 7. Sidebar upgrade: Favorites, Recents, Drives

**Done 2026-07-21.** Two commits. 7a: Favorites (settings-backed pins;
folder menu Pin/Unpin, per-item Move Up/Down/Unpin), Recents (new
unit-tested `filex::recents` — capped/deduped JSON log in the data dir,
recorded on navigate + file open, with a Clear row), and collapsible
sections (disclosure headers, collapsed state persisted; the section
list scrolls with the macOS FDA banner pinned below). 7b: Drives —
`filex::drives` enumerates mounts with capacity behind a per-OS boundary
(`/Volumes` + statvfs on macOS, `/proc/mounts` block devices + statvfs
on Linux, `GetLogicalDrives` + `GetDiskFreeSpaceExW` on Windows; pure
mount parsing + used-fraction unit-tested). The sidebar DRIVES section
shows capacity bars (warn-tinted past 90%) and free/total; clicking
navigates; refreshed on a 30s timer off-thread. Eased collapse animation
is left instant for now (the chevron swaps state).

- Favorites: user-pinned folders (settings-backed), add via context
  menu ("Pin to Sidebar") and drag later; reorder via menu first.
- Recents: recently opened files/folders from a small local usage log
  (data dir, capped, clearable — local-only per the telemetry
  stance).
- Drives: enumerate mounted volumes with free/total space behind a
  small per-OS trait (`statvfs` / `GetDiskFreeSpaceExW` /
  `NSFileManager`), shown with the mockup's capacity bars; clicking
  navigates; refresh on a slow timer, never per-frame.
- Sidebar sections become collapsible (eased, per polish principle).

### 8. Metadata tier (bigger; each its own design pass before code)

- **Tags**: real metadata, not UI. macOS: interop with Finder tags
  via xattr (they show in both apps). Windows/Linux: sidecar store in
  the data dir. Tag chips in the details panel + sidebar tag section
  + `tag:` search filter. Design doc: `docs/design-tags.md`.
  Progress: phases 1–4 done — portable `TagStore` + sidecar backend
  (1); Workspace store + path-key migration through the `ops::AppliedOp`
  hooks + startup prune (2); macOS Finder-interop backend writing the
  raw `_kMDItemUserTags` xattr, verified reading in Finder/`plutil` (3);
  details-panel chips + inline color-picker editor (4); `tag:` search
  token with a pure tokenizer + sidecar intersect, benched at 100k (5);
  collapsible sidebar TAGS section (6). **Complete** — all six phases
  landed; next block-8 item is Search filter chips.
- **Search filter chips** ("larger than 2MB", "modified today",
  `kind:image size:>2mb`): requires size/mtime in the index, which
  today stores names only by design. This is an index schema change
  with memory and freshness costs — design doc + benchmarks before
  building, per the perf rule. Design doc: `docs/design-search-chips.md`
  (written 2026-07-23; **full-v1 path chosen** — size/mtime go into the
  index). Progress: design ratified; **phase 1 done** — the pure
  `filex::search_filter` `key:value` grammar (`kind:`/`ext:`/`size:`/
  `modified:`/`tag:`, base-1024, keyword/relative/ISO dates) plus its
  wiring: `kind:`/`ext:` filter inside the index scan
  (`search_filtered`), chosen as "Option A" after benchmarking showed
  post-filtering under-returns past the result limit and can't do
  filter-only queries; the no-filter keystroke is provably unchanged.
  Phase 2 schema **done** — `size`/`mtime` + `FLAG_HAS_META` on
  `FileEntry`, `FORMAT_VERSION 2→3`, persistence, scan reads them.
  Benchmarking killed the doc's "macOS ~free via `getattrlistbulk`"
  assumption (that fast path isn't built; statting during the `jwalk`
  walk is ~6.7× slower bootstrap), so population is now **lazy
  background backfill on all OSes** (Option C): bootstrap stays
  names-only, size/mtime fill in after the index is searchable. Phase 2b
  **done** — a `MetaBackfiller` thread per `LiveIndex` stats entries in
  the background and populates them, so `size:`/`modified:` now work
  (converging over the first seconds of a fresh index). Phase 3 (live
  freshness) **done** — modify events now re-stat across FSEvents/
  inotify/fanotify/USN, so in-place edits stay current. Phase 5
  (Windows) **done** — the service backfills size/mtime the same way
  (via `start_live_index`), and the IPC search frame now carries the
  filters (JSON blob, protocol v2) so `size:`/`modified:` apply
  service-side. Only phase 6 remains — the removable **chip UI** (v1b);
  tokens-as-text already filter, so the whole grammar is usable now on
  all platforms.

## Explicitly out of scope

- **"Ask AI" / semantic search** — Phase 3 (user-confirmed
  2026-07-21). The search bar gets no AI affordances now.
- **Cloud drives (Google Drive, Dropbox), sharing avatars, comments,
  AirDrop** — collaboration/cloud services, not a local-first
  explorer. Off the table unless the product vision changes.
- **PDF / design-file content previews** — content parsing is Phase
  3; the details panel ships with image previews + icons only.

## Sizing notes

Blocks 1–2 close most of the visual gap and are roughly a session
each (block 2 is mostly asset curation). Blocks 3–7 are a session or
two each, in order. Block 8 items each start with a design doc, not
code. Phase 2b (Windows service/MSI) stays the parallel track in
roadmap.md and is not displaced by this file.

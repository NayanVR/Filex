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
  + `tag:` search filter. Needs a short design doc first (storage,
  rename/move behavior, watcher interaction).
- **Search filter chips** ("larger than 2MB", "modified today",
  `kind:image size:>2mb`): requires size/mtime in the index, which
  today stores names only by design. This is an index schema change
  with memory and freshness costs — design doc + benchmarks before
  building, per the perf rule. Until then, chips can cover what needs
  no index change (kind: from extension).

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

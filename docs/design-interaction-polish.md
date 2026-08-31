# Design: Interaction polish (block 9)

Status: **design ratified 2026-08-31, not yet implemented.** Companion
to `docs/ui-enhance-roadmap.md`, which closed blocks 1–8 (the *visual*
overhaul: theme, icons, grid, panel, tabs, sidebar, metadata tier).
This block is the **felt** overhaul — the difference between an app that
looks right in a screenshot and one that feels right under the hands.

## Decisions taken (2026-08-31, user-confirmed)

Recorded up front because three of them close questions this doc
originally left open:

1. **Type-ahead: route printable keys into the search box**, scoped to
   Current Dir (§B3). The bare-key shortcuts move; see the migration
   table there.
2. **Hover motion: chrome only** (§A3a). Toolbar, sidebar and tabs may
   fade; list rows and grid cards stay instant, per §A2.
3. **Next stretch: sequencing items 1–3 only** (§A1, §B1, §B7), then
   reassess before committing to the thumbnail and motion work.
4. **All three platforms are tuning targets.** No macOS-only behaviour;
   see the note in §B6.

The target is the quality bar of a first-party Apple app: every
keystroke and click acknowledged immediately, nothing that jumps, and no
interaction whose cost scales with folder size.

Two tiers, both scoped to what a user *feels* rather than what the
engine does:

- **Tier A — motion & feedback.** The app currently has two animations
  in ~29k lines and no pressed states at all. This is the largest
  perceived-quality gap.
- **Tier B — micro-behaviours.** Scroll strategy, grid keyboard
  navigation, thumbnail discipline, scrollbar fade. Individually small,
  collectively the difference between "jumpy" and "solid".

A separate, larger item — making `load_dir` non-blocking — is
deliberately **not** in this block. It is the biggest latency win in the
app but it is an architectural change to the navigation path, and it
wants its own design pass. It is tracked as block 10 at the end of this
doc so the ordering is on the record.

## Ground rules inherited from the roadmap

Restated because a motion pass is exactly where they die:

- **Nothing layout-animates inside virtualized rows.** This block's
  in-row work is *paint-only* — background, foreground, opacity. No
  animated size, padding, or position in a `uniform_list` row, ever.
- Preview/thumbnail work is lazy, background, and **cancellable**. §B4
  exists because the current implementation is none of the third.
- Every API is re-checked against the pinned version's source before
  use. This doc cites what was verified and, more importantly, what was
  found **not** to exist.

## What was verified against the pinned crates

Checked against `gpui 0.2.1` and `image 0.25.9` as pinned in
`Cargo.toml`. Recording the negative results matters more than the
positive ones, because three of them kill an obvious-looking approach:

| Question | Answer | Source |
| --- | --- | --- |
| Does `.active()` give a real pressed state? | **Yes.** Driven by `clicked_state.element`, i.e. actual mouse-down element state. | `gpui/src/elements/div.rs:2543` |
| What does `.active()` require? | A **stateful** element (`StatefulInteractiveElement`, declared at `div.rs:1040`). Every interactive component in `src/ui/` already takes an id and returns `Stateful<Div>` — verified across all 15 constructors. | `div.rs:1088` |
| Does GPUI have CSS-style transitions? | **No.** `with_animation` is a one-shot 0→1 progress ramp keyed on `ElementId`, holding `AnimationState { start: Instant }`, driven by `request_animation_frame`. It does **not** interpolate from a current value, so it cannot reverse mid-flight. | `elements/animation.rs:54,117,143,176` |
| Does `uniform_list` have a minimum-scroll strategy? | **No `Nearest` variant** — only `Top`/`Center`/`Bottom`. But non-strict `scroll_to_item` already no-ops when the item is visible, and applies the strategy only when it is not. | `elements/uniform_list.rs:84–92, 424–452` |
| Can we read the visible range at runtime? | **No.** `logical_scroll_top_index()` is `#[cfg(any(test, feature = "test-support"))]`. Release code cannot ask which rows are visible. | `uniform_list.rs:218` |
| Can background tasks be deprioritized? | **No.** `BackgroundExecutor::deprioritize` is `#[cfg(any(test, feature = "test-support"))]`. There is no production priority mechanism. | `executor.rs:377` |
| Can `image` decode at reduced size? | **No** scale-denominator / DCT-scaled decode is exposed. `thumbnail()` is a post-decode resample. | `image/src/imageops/sample.rs:613` |
| Can we get dimensions without decoding? | **Yes.** `ImageReader::into_dimensions()` reads the header only; `limits(Limits { max_image_width, max_image_height, max_alloc })` bounds a decode. | `image/src/io/image_reader_type.rs:302,137`; `io/limits.rs:33` |

Two consequences shape the whole block:

1. **No transitions + no reversal** means animated hover on list rows is
   not cheaply available and would fight the virtualized-row ground rule
   anyway. The motion budget goes to non-row chrome instead (§A3a, §A3).
2. **No task priority** means thumbnail/search contention must be solved
   with an explicit concurrency cap in our own code (§B4). We cannot ask
   the executor to yield.

---

# Tier A — motion & feedback

## A1. Pressed states (highest value, lowest risk)

**Problem.** There are zero `.active()` styles in the codebase. No
button, row, tab, menu item, or toolbar icon visually responds to
mouse-down. The action fires on release, so for the whole press the UI is
inert. This is the single most-felt "not a native app" tell, and it is
independent of everything else in this block.

**Why it's cheap.** All 15 interactive constructors already return
`Stateful<Div>` (they were written with ids for click handling), so
`.active()` is available today with no restructuring. There are 27
`.hover(` sites in `src/ui/`; each gets an `.active(` sibling.

**Plan.**

1. Add a `pressed` slot to `Theme` (`src/ui/theme.rs`), defined for both
   light and dark. Convention: `pressed` sits one step *past* `hover` in
   the same direction — on dark, slightly lighter; on light, slightly
   darker. For accent-filled controls (`confirm_button`, active `tab`,
   `magic_toggle`) it darkens the accent instead, so the fill reads as
   depressed rather than washing out.
2. Add `.active(...)` beside every `.hover(...)`. Paint-only: `bg`,
   `text_color`, `border_color`. **No** transform, scale, or size — that
   would violate the virtualized-row rule for `list_row` and `card`.
3. Rows and cards get it too. A file row that darkens under the finger
   before the selection resolves is most of what "responsive" means in a
   list.

**Exit criteria.** Every clickable surface in the app visibly changes on
mouse-down and restores on release or drag-out. No layout shift anywhere
during a press.

**Testing.** This is styling with no extractable logic, so it is a
visual-review item, not a unit-test item — consistent with how blocks 1–2
handled theme and icon work. The one thing worth asserting is that
`Theme::light()` and `Theme::dark()` both populate `pressed`, which the
existing theme tests can cover by construction.

## A2. The transition question — decided, not deferred

The instinct after A1 is "now animate the hover fade". Per the table
above, GPUI 0.2.1 cannot do this cleanly: `with_animation` restarts from
0 on every id change and cannot reverse, so a fast pointer sweep across a
list would leave rows mid-fade at wrong values, and driving it per row
would mean per-row animation state inside a virtualized list — precisely
what the ground rule forbids.

**Decision: do not animate hover or selection on list rows or grid
cards.** Instant hover is correct here, and is what Zed itself does with
the same framework. The perceived-smoothness win people attribute to
"animated hover" comes overwhelmingly from A1 (press feedback) and from
B1–B2 (not jumping), both of which are available now.

This is a decision, not a deferral — revisit only if GPUI gains real
transitions upstream.

## A3a. Hover motion on chrome (confirmed scope)

The §A2 "no" is scoped to **virtualized surfaces**. Toolbar buttons,
sidebar rows, drive rows and tabs are neither virtualized nor
per-row-stateful, so a hover fade there has neither problem, and is
confirmed in scope.

**Mechanism.** Key the animation's `ElementId` on the hover state and
choose the interpolation direction from it — `lerp(base, hover, delta)`
on enter, `lerp(hover, base, delta)` on leave. This works because the
animator closure decides what it interpolates; the framework only
supplies the 0→1 ramp.

**The honest caveat.** Because an animation restarts from its endpoint
rather than its current mid-value, flipping hover *during* a fade
produces a visible jump. Sweeping the pointer across a tight cluster
(the toolbar's five adjacent buttons) is exactly that case, and is not
rare. Two mitigations, in order:

1. Keep the duration short — 80–100ms, at the fast end of the menu's
   existing 120ms — so any jump is small and brief.
2. **Checkpoint during implementation:** if the artifact is visible on
   the toolbar cluster, restrict the fade to sidebar rows and tabs,
   where pointer sweep-through is far less common, and leave the
   toolbar instant. Judge this on-screen; it is not decidable on paper.

Pressed states (§A1) remain instant everywhere — a press must never lag
the finger.

## A3. Discrete motion where it actually pays

`with_animation`'s one-shot semantics are a *good* fit for discrete,
low-frequency, non-virtualized state changes, which is exactly where the
roadmap already noted missing motion. Three targets, in value order:

1. **Sidebar section collapse/expand.** Block 7 explicitly shipped this
   instant and flagged it: *"Eased collapse animation is left instant for
   now."* This is the most visible missing motion in the app.
2. **Details panel open/close** (`cmd-i`). Currently the panel pops in
   and the grid reflows in the same frame — a hard jump on a large
   surface.
3. **Modal / settings pane entry.** `ui::modal` already has a backdrop;
   a short scale-and-fade (~120ms, `ease_out_quint`) matches the existing
   context-menu treatment at `ui/menu.rs:54`, which is the one piece of
   motion in the app that already feels right.

Durations stay in the 100–150ms band the menu established. Anything
longer reads as sluggish rather than smooth.

**Constraint to honour.** The details panel animating its width forces a
grid-column recompute per frame (`render_lists.rs:238` derives `cols`
from viewport width). Either animate opacity/transform only and snap the
width once, or accept the reflow and confirm it is cheap. Prefer the
former.

**Testing.** Animation state is not meaningfully unit-testable; the
extractable part is the collapse *state* machine, which is already
settings-backed and covered.

---

# Tier B — micro-behaviours

## B1. Arrow-key scrolling recenters the list (5-line fix, large felt win)

**Problem.** `move_selection` (`workspace/navigation.rs:121`) scrolls
with `ScrollStrategy::Center`. Per `uniform_list.rs:424–452`, non-strict
scrolling no-ops while the item is visible, then applies the strategy
when it is not — so arrowing one row past the bottom edge **jumps the
list by half a viewport**. Finder scrolls exactly one row.

**Fix.** Choose the strategy from the direction of travel: `Bottom` when
`delta > 0`, `Top` when `delta < 0`. Verified at `uniform_list.rs:448–452`
that this yields exactly minimum-scroll-to-edge, with no new API and no
need for the (test-only) visible-range accessor.

The same applies at `navigation.rs:191` (`reveal`), where `Center` is
arguably right — revealing a search hit in its folder benefits from
context around it. Keep `Center` there; this fix is scoped to
`move_selection`.

**Testing.** Extract the direction→strategy choice as a pure function and
unit-test it, in the spirit of `columns_for`. The scroll behaviour itself
needs `#[gpui::test]` with `TestAppContext` if covered at all — and note
that `logical_scroll_top_index()` *is* available under `test-support`,
which is precisely what makes this assertable in a test but not in
release code.

## B2. Grid keyboard navigation is broken

**Problem.** `handle_key` (`workspace/input.rs:41–46`) handles only
up/down/enter, and `move_selection` moves by ±1 regardless of view mode.
In a 6-column grid, **Down moves one card sideways**. There is no
left/right binding at all, and no Home/End/PageUp/PageDown in either
view.

**The structural obstacle.** The column count is computed inside
`render_grid` (`render_lists.rs:238`) from `window.viewport_size()` and
then discarded. The key handler has no access to it, and cannot recompute
it (it has no `Window` in `handle_key`'s useful path and shouldn't
duplicate the layout math).

**Plan.**

1. Cache the last computed column count on `Workspace` as
   `grid_cols: Cell<usize>`. A `Cell` specifically, because `render_grid`
   takes `&self` — this avoids changing its signature or introducing a
   borrow conflict at the `render.rs:479` call site.
2. Extend `move_selection` to take a step derived from view mode: ±1 in
   list; ±1 (left/right) and ±`cols` (up/down) in grid.
3. Bind `left`/`right` in grid mode, and `home`/`end`/`pageup`/`pagedown`
   in both. Page size in list mode needs a visible-row count, which is
   not readable at runtime (see the table) — derive it from
   `viewport height / theme.row_height` at the key-handling site, which
   is close enough and is already how the grid derives its columns.
4. Clamp rather than wrap at the ends, and clamp the *row* on
   up/down so a short final row doesn't strand the lead.

**Testing.** The index arithmetic (`ix`, `cols`, `len`, direction →
new `ix`, with clamping and ragged-last-row handling) is pure and
belongs in `filex::selection` with plain `#[test]` coverage — same
treatment `Selection::move_lead` already gets. This is the part that
actually has edge cases; the binding is trivial.

## B3. Type-ahead — route printable keys into search (decided)

Finder and Explorer both jump to a file as you type letters at the list.
Filex has no equivalent.

**Decision (confirmed 2026-08-31).** Rather than a separate type-ahead
buffer, printable keys pressed at the list feed the **existing search
box, scoped to Current Dir**. This reuses the debounced, cancellable,
generation-guarded search path — the best-engineered code in the app —
and is strictly more capable than a prefix-jump: fuzzy, ranked, and
composable with the `kind:`/`size:`/`tag:` filters from block 8. It is
also the honest expression of what Filex *is*: a search-first explorer
where typing searches.

### Keybinding migration

The bare-key bindings at `mod.rs:1072–1081` are scoped `!SearchInput`
and collide with any type-ahead. Auditing them turned up that **most are
already redundant**, so the migration is cheaper than it first looks:

| Key | Action | Status | Resolution |
| --- | --- | --- | --- |
| `p` | `TogglePreview` | **Pure duplicate** of `cmd-i`/`ctrl-i` (`mod.rs:1030,1057`) | Drop. Zero functional loss. |
| `/` | `FocusSearch` | **Duplicate** of `cmd-f`/`ctrl-f` (`mod.rs:1074,1076`) | Drop as a shortcut; becomes a literal typed character. |
| `v` | `ToggleView` | No modifier equivalent | Replace — see below. |
| `?`, `shift-/` | `ToggleShortcuts` | No modifier equivalent | Rebind to `cmd-/` / `ctrl-/`. |

So only **one** binding (`v`) loses real functionality, and one (`?`)
needs a new home. `p` and `/` are pure redundancy being retired.

For `v`, prefer **explicit view actions over a toggle**: `cmd-1` = list,
`cmd-2` = grid, which is Finder's own convention and removes the "which
way does it flip?" ambiguity a blind toggle has. Keep the `ToggleView`
action itself for the top-bar button, which shows its current state and
so reads correctly as a toggle.

### Routing rules

- Route only **printable characters with no platform/control modifier**;
  arrows, `enter`, `esc`, `tab` and every accelerator are untouched.
- A **leading space is ignored** — it cannot start a meaningful query,
  and swallowing it keeps `space` free for a future Quick Look.
- The first routed keystroke focuses the search input, sets scope to
  **Current Dir**, and inserts the character; everything after is
  ordinary input, so IME, selection and clipboard keep working unchanged.
- `esc` already clears and leaves search, so the exit path exists.

### UI hints that must change with it

Two places advertise the retired keys and would become wrong:

- `ui/kbd.rs:100` — the shortcuts overlay lists *"Focus search — `/`"*.
- `render.rs:163` — the search box paints a `/` keycap as its affordance.

Both should read "type to search" rather than naming a key. The overlay
also needs new `cmd-1`/`cmd-2` and `cmd-/` rows.

**Testing.** The routing predicate (which key events become query text,
including the modifier and leading-space rules) is pure and belongs in
the lib with plain `#[test]`s. The focus-and-insert sequence touches the
input entity, so it wants `#[gpui::test]` with `TestAppContext`.


## B4. Thumbnail decode discipline (violates a stated ground rule today)

**Problem.** Three compounding issues in the same path:

1. `decode_thumbnail` (`thumbnails.rs:37`) calls `image::open()` — a
   **full-resolution decode** — then downscales. The module comment
   claims decodes "are milliseconds"; a 50MP JPEG is a ~200MB allocation.
2. `request_thumbnail` (`navigation.rs:44`) has **no concurrency cap and
   no cancellation**, despite the roadmap ground rule requiring
   "lazy, background, and cancellable". Fast-scrolling a folder of 500
   images spawns 500 full decodes.
3. Those decodes run on the **same background executor as search scans**,
   and GPUI 0.2.1 has no way to deprioritize them. So thumbnail churn
   directly inflates search-as-you-type latency — a thumbnail bug that
   presents as a search bug.

**What is and isn't available.** `image` 0.25 exposes no reduced-size
decode, so the full decode cannot simply be made cheap. It does expose
`into_dimensions()` (header-only) and `Limits`. The practical levers are
therefore admission control and cancellation, not faster decoding.

**Plan.**

1. **Cap in-flight decodes** at a small constant (start at 2; it must be
   well under `num_cpus` so search always has cores). A counter on
   `Workspace` plus a queue of pending paths is sufficient — no new
   dependency.
2. **Cancellation by epoch.** Bump a generation counter on every
   `load_dir` / query change; a decode whose epoch is stale is dropped
   without being applied, and pending queue entries for stale epochs are
   discarded before they start. This is the same generation pattern
   `run_search` already uses, and it is what makes the ground rule true
   rather than aspirational.
3. **Header pre-check.** `into_dimensions()` before decoding; skip
   anything above a pixel budget (and set `Limits` as a backstop) so one
   pathological file cannot stall a decode slot.
4. **LRU eviction** instead of the wholesale `self.thumbnails.clear()` at
   `navigation.rs:54`, which currently blanks and reloads every visible
   thumbnail the moment the cache tops out.

**Testing.** The admission/eviction/epoch logic is pure and should live
in `filex::thumbnails` (lib side, no GPUI) with plain `#[test]`s:
cap respected, stale epochs dropped, LRU order correct. Decoding itself
is tested against small fixture images, per the CLAUDE.md preference for
fixtures over live filesystem state. A `criterion` bench on
`decode_thumbnail` across a few representative sizes gives the number
that justifies the cap.

## B5. Preview panel decodes at the wrong size

`render_details_panel` (`render_menus.rs:878`) asks for a **160px** icon,
but the cache is populated at `THUMBNAIL_EDGE = 128`. The preview is
therefore an upscaled thumbnail — soft, and exactly what the comment at
`thumbnails.rs:19–21` says the sizing is meant to avoid. Block 5 already
flagged this: *"a larger dedicated preview decode … is a possible
refinement."*

**Plan.** A separate `PREVIEW_EDGE` (320px, so it stays crisp on a 2×
display) cached under a distinct key, decoded through the same capped,
cancellable path from §B4 — one entry at a time, since only the lead item
has a preview.

## B6. Scrollbar never fades

`ui/scrollbar.rs:85` paints the thumb at a constant 0.30 alpha. macOS
overlay scrollbars fade in on scroll and out after roughly a second of
stillness.

**Plan.** `ScrollbarState` is `Rc<Cell<Inner>>` (`scrollbar.rs:50`), so a
`last_activity: Option<Instant>` field can be added with no signature
churn anywhere. Fade the idle alpha to zero after ~1s of no scrolling and
no hover; hold it visible while dragging or hovering the track. This is
non-virtualized chrome, so `with_animation` is appropriate here.

**Cross-platform note (decision 4: all three OSes are tuning targets).**
Auto-hiding scrollbars are no longer a macOS-only convention — Windows 11
and GNOME/Adwaita both hide-when-idle by default — so **one shared
behaviour is correct on all three** and no `#[cfg(target_os)]` split is
warranted here. Per CLAUDE.md, resist adding one: a per-OS fade would be
platform-specific code with no platform-specific justification.

The same applies to §A1's pressed states. Windows conventionally uses a
stronger press tint than macOS, but Filex has its own visual identity
(block 1 chose its own accent over the mockup's), so a **single
`pressed` slot per theme variant** is the right call, tuned to look
right in both light and dark rather than forked per OS.

Two things genuinely *are* worth checking on each platform, because they
are OS-driven rather than style choices: scroll-wheel/trackpad event
cadence feeding the idle timer (a trackpad emits a long tail of small
deltas that must not read as "still scrolling" forever), and whether the
fade holds correctly while a native overlay scrollbar is also present.

## B7. Per-frame work in the row processors

Both list processors call `format_modified(modified, SystemTime::now())`
**inside the per-row closure** (`render_lists.rs`), so every visible row
issues a syscall and a `String` allocation every frame, plus another for
the size cell.

**Plan.** Hoist the `SystemTime::now()` to the top of the processor
closure — one call per frame instead of one per visible row. Small, but
it is the hottest loop in the app and the fix is free.

## B8. Browse list goes stale on external changes

`refresh_after_fs_change` (`workspace/roots.rs:132`) refreshes index
stats and re-runs the *search*, but never re-lists the current directory.
A file created by another app doesn't appear until manual refresh.

**Plan.** Re-list the cwd on the same throttled tick, reusing the
existing `FS_REFRESH_INTERVAL` throttle and the same-directory
scroll-preservation already implemented at `navigation.rs:24–31`.

**Ordering note.** This makes directory re-listing happen on a *timer*
rather than only on user action, which multiplies the cost of the
synchronous `read_dir_sorted` on the UI thread. **B8 should land after
block 10 (async `load_dir`), not before** — shipping it first would turn
a navigation-time stall into a recurring one.

---

## Sequencing

Ordered by felt-improvement per unit of risk, with dependencies
respected. **Items 1–3 are the committed next stretch** (decision 3);
everything below the line is planned but not yet authorised.

| # | Item | Size | Risk | Notes |
| --- | --- | --- | --- | --- |
| 1 | §A1 pressed states | S | Very low | Additive; no logic touched |
| 2 | §B1 scroll strategy | XS | Very low | ~5 lines |
| 3 | §B7 per-frame hoisting | XS | Very low | Free |
| — | *reassess here* | | | |
| 4 | §B4 thumbnail discipline | M | Medium | Also improves search latency |
| 5 | §B5 preview size | S | Low | Depends on B4's decode path |
| 6 | §B2 grid keyboard nav | M | Low | Pure logic + bindings |
| 7 | §B3 type-ahead | M | Medium | Ships with the §B2 key work |
| 8 | §B6 scrollbar fade | S | Low | First `with_animation` use |
| 9 | §A3a chrome hover fade | S | Medium | Sweep artifact needs an on-screen call |
| 10 | §A3 discrete motion | M | Medium | Panel reflow is the risk |
| 11 | §B8 stale browse list | S | Low | **After block 10** |

Two ordering notes changed by the ratified decisions:

- **§B3 moved up to sit beside §B2.** Both rewrite the same key-handling
  path (`workspace/input.rs` + the binding block at `mod.rs:1014–1081`),
  so doing them together avoids touching that code twice and avoids an
  intermediate state where the bare keys are half-migrated.
- **§A3a is sequenced before §A3** — it is the smaller and more
  reversible of the two motion items, and it is the one that tells us
  whether the sweep artifact is tolerable before more motion is built on
  the same mechanism.

### The committed stretch, concretely

Items 1–3 touch: `src/ui/theme.rs` (one new slot, two variants), the 27
`.hover(` sites across `src/ui/*.rs`, `workspace/navigation.rs:121`
(strategy selection), and the two row processors in
`workspace/render_lists.rs` (hoisting `SystemTime::now()`).

No public API changes, no new dependencies, no schema or settings
changes, and nothing that alters behaviour on a background thread. Exit
criteria are §A1's plus: arrowing through a long list never moves the
viewport by more than one row, and no per-row `SystemTime::now()` call
remains in a processor closure.


## Explicitly out of scope

- **Animated hover/selection on rows and cards** — decided against in
  §A2 on framework grounds, not deferred. Hover motion on non-virtualized
  chrome *is* in scope (§A3a); the exclusion is specific to rows and
  cards.
- **A classic prefix-jump type-ahead buffer** — rejected in §B3 in
  favour of routing into search.
- **Async `load_dir`** — the largest latency item in the app, but
  architectural. See block 10 below.
- **Drag-to-resize for the details panel** — still deferred from block 5;
  unrelated to smoothness.
- Anything from the Phase 3 list (semantic search, content previews).

## Block 10 (next, larger): the navigation path must stop blocking

Recorded here so the ordering is explicit, not to be built in this block.

`load_dir` (`navigation.rs:19`) calls `read_dir_sorted` **synchronously
on the UI thread**. That function (`listing.rs:59`) issues one `stat` per
entry and then sorts with `name_order` (`listing.rs:115`), which
allocates two `String`s per comparison via `to_lowercase()` — O(n log n)
allocations inside a blocking window. On a cold cache, a network mount,
or a large folder this is a multi-hundred-millisecond frozen window, and
it violates the CLAUDE.md non-negotiable that the UI thread never blocks
on I/O.

It is reached from `navigate`, `go_up`, `go_back`, `go_forward`, every
completed file op, **and every settings change** — the subscription at
`mod.rs:855` fires `load_dir` on any `SettingsEvent::Changed`, so typing
in the accent-colour hex field (`mod.rs:841`) re-stats the current
directory per keystroke. `Workspace::new` (`mod.rs:937`) also calls it
synchronously before first paint, alongside `Recents::load` and
`PlatformTags::load`.

The design pass should cover: a two-phase listing (names from `read_dir`
first, metadata backfilled — the same shape as the `MetaBackfiller`
pattern block 8 already established for the index), generation-guarded
async loads, holding the previous entries on screen so navigation never
flashes empty, scoping the settings subscription to the settings that
actually affect the listing, and a `criterion` bench on `read_dir_sorted`
at 1k/10k/100k entries so the improvement is measured rather than
asserted.

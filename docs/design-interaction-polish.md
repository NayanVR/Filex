# Design: Interaction polish (block 9)

Status: **design, not yet implemented.** Written 2026-08-31. Companion
to `docs/ui-enhance-roadmap.md`, which closed blocks 1–8 (the *visual*
overhaul: theme, icons, grid, panel, tabs, sidebar, metadata tier).
This block is the **felt** overhaul — the difference between an app that
looks right in a screenshot and one that feels right under the hands.

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
   anyway. The motion budget goes to non-row chrome instead (§A3).
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

## B3. Type-ahead — a product decision, flagged not assumed

Finder and Explorer both jump to a file as you type letters at the list.
Filex has no equivalent.

**The conflict.** `mod.rs:1079–1081` binds bare `v` (toggle view), `p`
(toggle preview), and `?` (shortcuts) scoped to `!SearchInput`. Any
type-ahead consumes exactly those keys. The two features cannot coexist
as bound.

**Recommendation (needs sign-off — it changes existing shortcuts).**
Rather than a separate type-ahead buffer, route printable keys at the
list into the **existing search box, scoped to Current Dir**. This reuses
the whole debounced/cancellable search machinery (which is already the
best-engineered path in the app), is strictly more capable than Finder's
prefix-jump (fuzzy, ranked, filterable), and is on-brand for a
search-first explorer. Cost: `v` and `p` move to modifier shortcuts.

The alternative — a classic prefix-match type-ahead buffer with a ~1s
reset, leaving `v`/`p` alone — is more conservative and less useful.

I recommend the first, but it is a product call about breaking two
documented shortcuts, so it should not be made inside an implementation
session. **Blocked on user decision.**

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

## Suggested sequencing

Ordered by felt-improvement per unit of risk, with dependencies
respected:

| # | Item | Size | Risk | Notes |
| --- | --- | --- | --- | --- |
| 1 | §A1 pressed states | S | Very low | Additive; no logic touched |
| 2 | §B1 scroll strategy | XS | Very low | ~5 lines |
| 3 | §B7 per-frame hoisting | XS | Very low | Free |
| 4 | §B4 thumbnail discipline | M | Medium | Also improves search latency |
| 5 | §B5 preview size | S | Low | Depends on B4's decode path |
| 6 | §B2 grid keyboard nav | M | Low | Pure logic + bindings |
| 7 | §B6 scrollbar fade | S | Low | First `with_animation` use |
| 8 | §A3 discrete motion | M | Medium | Panel reflow is the risk |
| 9 | §B3 type-ahead | M | — | **Blocked on decision** |
| 10 | §B8 stale browse list | S | Low | **After block 10** |

Items 1–3 are a single sitting and cover most of the perceived gap.

## Explicitly out of scope

- **Animated hover/selection on rows and cards** — decided against in
  §A2 on framework grounds, not deferred.
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

# Design: Fuzzy + frecency search ranking ("Tier 0")

Status: **blocks 1–3 implemented (uncommitted), block 4 not started** —
see "Commit sequence" at the end. Written 2026-07-26 as the gate before
code, per the Phase 2 convention (design doc first, one concern per
commit) and the perf rule: anything that could add latency to
search-as-you-type gets called out explicitly, with benchmarks. Updated
after implementation, so the decisions below record what was actually
built — including three that the tests overturned mid-flight.

## Why this exists (and what it replaces)

This is the outcome of a 2026-07-26 discussion about giving filex an "AI
feel". The considered option was local vector embeddings over the index.
It was rejected **for now** (not forever — see "Deferred: Tier 1"), on
these grounds:

- The index stores **filenames**, and filenames are poor embedding input
  (`IMG_2831.HEIC`, `invoice_q3_final_v2.pdf`). The vector is mostly noise.
- 384-dim f32 ≈ 1.5 KB/entry ⇒ ~1.5 GB for a 1M-entry index, against
  ~100 MB today, plus inference at bootstrap and on every rename.
- Query-side inference (5–20 ms for a real encoder) lands **directly on
  the keystroke path**, which CLAUDE.md makes non-negotiable.
- Embeddings only pay off over *content*, which needs OCR/PDF/docx
  extraction — explicitly Phase 3, and the larger 90% of that project.

The bet instead: most of the perceived intelligence in Raycast/Alfred/fzf
is **fuzzy matching + frecency**, not a model. That is what this doc
specifies. It is pure ranking logic — no new dependency, no index size
change, no model file.

## Goal & scope

1. **Subsequence/acronym matching** so `dsr` finds `Design System
   Review.pdf` and `srcidx` finds `src/index`, ranked below today's
   literal matches, never above them.
2. **Frecency**: files and folders the user actually opens rank up, using
   a decayed count of opens.
3. **Natural-language phrases → existing filters**: `photos from last
   summer` → `kind:image modified:2025-06-01..2025-08-31`, reusing the
   `search_filter` grammar and surfacing as normal chips.

Non-goals: embeddings, content search, learned ranking, per-user model
training, cross-session query logging (filenames and queries are private
— see the telemetry stance in `docs/roadmap.md`; everything here stays
local and nothing new is uploaded).

## What ranking does today

`VolumeIndex::search` (src/index/mod.rs) is a rayon scan over the entry
arena. Per live entry it runs one `memmem::Finder::find` over the folded
name and classifies the hit:

```rust
pub enum MatchKind { Exact, Prefix, WordBoundary, Substring }  // lower is better
```

The heap key is the tuple `(MatchKind, name_len, ix)`, kept in a bounded
per-chunk `TopK` (max-heap whose root is the *worst* kept item), merged
across rayon chunks and then across roots in `manager::search_all`, which
re-sorts by the same `(kind, name_len)`.

Two properties of that design drive everything below:

- **The scan never materializes a path.** `path_of()` walks parents and
  allocates; doing it per candidate would be orders of magnitude slower
  than the SIMD substring test. So *nothing that needs a path can happen
  inside the scan.*
- **The heap key must stay cheap and `Ord`.** It is compared on every
  push.

## Decision 1: two-stage ranking (scan, then re-rank top-K)

Frecency is keyed by **path**, which the scan cannot afford. So:

- **Stage A (scan)** — unchanged in shape: name-only matching, bounded
  `TopK`, but with `limit * OVERFETCH` candidates instead of `limit`.
  `OVERFETCH = 4` initially.
- **Stage B (re-rank)** — resolve paths for those `≤ 4 * limit`
  candidates only (this already happens in `search_all` to build
  `MergedHit`), apply the frecency boost, re-sort, truncate to `limit`.

Stage B is O(limit) with a hash lookup per candidate — microseconds
against a scan measured in milliseconds. This is the same two-stage shape
as the rejected embedding plan (cheap candidate generation, expensive
rerank on a bounded set), which is why it is worth naming: it is the
structure a future Tier 1 would slot into unchanged.

Overfetch is the only cost to the scan, and it is a heap-depth change,
not a scan-work change.

## Decision 2: fuzzy is a gated second pass, not a widened first pass

Subsequence matching matches far more entries than substring matching, so
folding it into the main `filter_map` would raise the heap push rate on
every keystroke — a latency regression on the common case (typing
`invoice`, which already has plenty of literal hits) in exchange for
nothing.

Instead: **run the fuzzy pass only if the literal pass returned fewer
than `limit` hits.** Then:

- The common case is byte-for-byte today's code path and today's latency.
- Fuzzy hits are pure filler below literal hits, which is also exactly
  the ranking we want.
- The worst case is two scans, and it only happens when the first scan
  found almost nothing — i.e. when the user is currently staring at an
  empty result list and would rather wait.

The gate is `hits < limit`, deliberately not `hits == 0`: `dsr` may match
one junk file literally, and that must not suppress the acronym hits.

Accepted trade-off: a query with ≥ `limit` literal hits never shows
acronym matches, even if one would have been the better answer. Revisit
only if it bites in dogfooding.

## Decision 3: the scoring model

`MatchKind` grows one variant, kept last so existing orderings are
untouched:

```rust
pub enum MatchKind { Exact, Prefix, WordBoundary, Substring, Fuzzy }
```

Within `Fuzzy`, quality varies a lot (`dsr` → `Design System Review` is
excellent; `dsr` → `dadsrock.mp3` is noise), so fuzzy hits carry a
`u16` **penalty** (lower is better, so it composes with the existing
"lower wins" key) built from fzf-style signals:

| signal | effect |
| --- | --- |
| char starts a word (after a separator, or a camelCase hump, or a letter→digit transition) | strong bonus |
| char immediately follows the previous match | same bonus (rewards runs) |
| gap between matches | small cost per skipped char |
| chars before the match starts | small cost |
| unmatched tail length | mild cost (subsumes `name_len`) |

**Two candidate alignments, one scale.** `src/fuzzy.rs` greedily aligns
the needle twice — once restricted to word starts (which is what *finds*
the acronym alignment a plain greedy walk would miss), once unrestricted
— and keeps the better score.

The first implementation instead made "acronym" a hard tier above
"greedy". Its own test killed it: `a_x_b_x_c.txt` is an acronym match for
`abc` and `abc.txt` is not, so the junk name won. The word-start and
consecutive-run bonuses have to be *alternatives on one scale*, not
tiers. This is why the boundary bonus is large relative to the gap cost:
an acronym match over a long name is mostly gap, and must still beat a
dense match on an unrelated name.

Matching operates on **chars, not bytes**, so multi-byte names neither
panic nor skew the gap math. Case folding is Unicode-aware but does *not*
strip diacritics — `u` does not find `ü` — matching how the index folds
names elsewhere.

### The ranking key is packed into one `u32`

The heap key is conceptually `(MatchKind, penalty, name_len)`, and
`Score` exposes it that way. Inside the top-K heap it is **packed into a
single `u32`** (`kind << 28 | penalty << 12 | name_len`), whose natural
integer ordering is exactly the field-wise ordering.

This is not premature cleverness — it is a benchmarked correction.
Carrying the fields as a struct grew the heap element from 8 to 12 bytes
and cost **~12% on every keystroke** (`single_char_e` 1.73 → 1.93 ms),
which is precisely the regression this doc forbids. Packed, the element
is 8 bytes again and the literal path returns to baseline. `name_len` is
clamped to 12 bits; filenames are ≤255 bytes on every filesystem filex
targets, and it only ever affects a tiebreak.

For the four literal kinds `penalty` is `0`, so their relative order is
exactly what it was (guarded by the pre-existing
`search_ranks_exact_then_prefix_then_boundary_then_substring` test).

## Decision 4: frecency needs a recents schema change

`recents.rs` today is a capped `Vec<PathBuf>`, `#[serde(transparent)]`,
CAP 20 — a *recents list*, with no counts and no timestamps. Frecency
needs both, and 20 entries is far too few to rank against.

New shape (same file, `recents.json`):

```rust
struct Visit { path: PathBuf, opens: u32, last_opened: i64 }  // unix secs
```

- **Backward compatible load**: an untagged enum accepts the legacy bare
  array and migrates it to `opens: 1` with `last_opened` = load time.
  A corrupt or missing file stays "empty, never an error" as today.
- **CAP rises** (200) and eviction becomes lowest-score-first rather than
  oldest-first, so a folder opened daily for a year is not evicted by a
  burst of one-offs. The recents *UI* keeps showing the most-recent 20 by
  timestamp — the ordering it wants is unchanged; only the store grows.

Score, a pure function (`frecency::score`):

```
score = opens * 0.5f32.powf(age_days / HALF_LIFE_DAYS)   // HALF_LIFE = 30
```

Exponential decay rather than Mozilla-style bucket weights: one constant,
no cliff edges, trivially testable.

Applied in stage B as a **rank boost, not a score override** — a frecent
file may jump above other hits *of the same `MatchKind`*, but never above
a better match class. The stage-B sort key is
`(kind, penalty * PENALTY_SCALE + name_len - boost)`: tier first, so no
boost can lift a hit out of its match class, then a combined value the
boost is subtracted from.

The obvious formulation — subtract the boost from `penalty` — is wrong,
and was caught while wiring it up: literal hits all have `penalty == 0`,
so `saturating_sub` clamps to zero and frecency would have silently
affected *only fuzzy hits*. The combined value exists so there is
headroom to reorder within a tier at all. `MAX_BOOST` is sized to
comfortably beat any `name_len` (so frecency wins the tiebreak between
two equally good literal matches) but only a few `PENALTY_SCALE` units
(so among fuzzy hits it overturns close calls, not clear ones).

**Two visit stores, one file.** `recents.json` now backs both the sidebar
list and ranking, so `CAP` is sized for ranking (200) while the sidebar
shows 20. A visit also records whether it was **directly opened**: parent
credit must count for ranking without making a folder the user never
opened appear under "recently opened". Both of those were bugs the first
implementation shipped and the tests caught — along with crediting `/`,
which is an ancestor of everything and therefore ranks nothing.

**Parent-directory credit**: opening `/a/b/c.pdf` also credits `/a/b` at
a fraction (0.25). Cheap, and it is what makes "the folder I live in
surfaces first" work without needing per-file history everywhere.

## Item 3: natural-language phrases → existing filters

A pure function `search_filter::expand_phrases(&str) -> (String, Vec<Filter>)`
runs **before** `parse_query` and rewrites recognized phrases into the
existing grammar. Strictly rule-based, no model:

| phrase | expansion |
| --- | --- |
| `photos`, `pictures`, `images` | `kind:image` |
| `screenshots` | `kind:image` + text `screenshot` |
| `videos`, `music`, `docs`, `pdfs` | `kind:*` / `ext:pdf` |
| `today`, `yesterday`, `this week`, `last week`, `last month` | `modified:…` |
| `last summer`, `in June`, `in 2025` | `modified:<range>` |
| `big`, `huge`, `tiny` | `size:>100mb` / `size:<1mb` |
| `from <phrase>`, `my`, `all` | dropped as filler |

Rules:

- **Every expansion becomes a visible chip.** The user sees
  `kind:image` + `modified:2025-06-01..2025-08-31` appear, and can remove
  either. Nothing is invisible or unexplainable — this is the whole
  reason it reads as smart rather than as magic that mysteriously hides
  files.
- **Only fires when the phrase is the whole token run**, and never when
  the same word is also plausible filename text with results: a file
  literally named `photos` must still be findable. Tie broken by keeping
  the residual text in the query as well.
- Anything unrecognized is left as literal text, exactly like today's
  unknown-`key:` fallback.

Sequenced **after** blocks 1–2 land, as its own commit; it is independent
of the ranking work and rides entirely on existing machinery.

## Perf budget & benchmarks (gate the merge)

Budget from `docs/indexing-architecture.md` §3: a full scan stays well
under one keystroke (~30 ms) at 200k entries.

`benches/search_bench.rs` grows cases, and merge requires:

1. **No regression on the literal path.** Existing short/long ×
   rare/common cases within noise of today. This is the load-bearing
   number: it is what Decision 2 buys.
2. **Fuzzy pass measured separately** — a query with no literal hits, so
   both passes run. Must stay inside budget.
3. **Overfetch cost** — `limit` vs `4 * limit` heap depth, to confirm
   `OVERFETCH = 4` is free.
4. A `frecency_rerank` group in `benches/search_bench.rs` (not a separate
   file — it wants the same synthetic index) comparing `search_all` with
   and without a populated table; the point is to prove stage B is not on
   the critical path.

### Where the service path stands

`index/ipc.rs` passes an **empty** frecency table: visit history is
private user data and the service runs as LocalSystem, so it is never
sent across the pipe. Service-backed search (Windows, once the MSI
installs `filex-indexd`) therefore ranks on match quality alone. Closing
that means returning `Score` in `RemoteHit` and re-ranking client-side —
a protocol change, deliberately not bundled here.

## Testing

Per CLAUDE.md, all of this is pure logic with no GPUI, so plain `#[test]`:

- `fuzzy`: subsequence correctness (match/no-match, non-ASCII, empty
  needle), and **ordering assertions on the signals** — `dsr` ranks
  `Design System Review.pdf` above `dadsrock.mp3`; consecutive runs beat
  scattered gaps; word-boundary starts beat mid-word.
- `frecency`: decay math at 0 / one half-life / many half-lives; eviction
  keeps the daily-opened entry over a burst of one-offs; legacy-format
  migration; corrupt file → empty.
- `index`: the existing ranking test still passes unchanged (that is the
  regression guard for Decision 3); fuzzy hits never outrank literal
  hits; the gate does not fire when literal hits ≥ limit.
- `expand_phrases`: table-driven over the phrase list, plus
  "unrecognized text is untouched" and "a file named `photos` is still
  findable".
- `manager::search_all`: merged multi-root ordering under the new key.

## Commit sequence

1. `frecency`: recents schema change + decay scoring + migration (no
   search changes yet — store and logic, fully tested). **Implemented.**
2. `fuzzy`: matcher + scoring, gated second pass, wired into `search` /
   `search_filtered`, benches. **Implemented.**
3. `rerank`: stage B in `search_all` (+ the IPC path in `index/ipc.rs`),
   overfetch, frecency boost applied. **Implemented.**
4. `expand_phrases`: NL → filters, chips wired in the UI. **Not started.**

Status 2026-07-26: blocks 1–3 are implemented on the working tree with 47
new tests (229 lib tests green, clippy clean, Windows target checks).
Uncommitted pending the benchmark sign-off below.

### Benchmark results

**Paired measurement**, both sides run back-to-back on an idle machine in
the same thermal state (stash → `--save-baseline` → restore →
`--baseline`). This methodology matters: interleaved runs on a warm
machine produced swings up to +250% on *unmodified* code, i.e. pure
noise that looked exactly like a regression.

| case | before | after | change |
| --- | --- | --- | --- |
| `single_char_e` | 1.667 ms | 1.658 ms | −2.0% (noise) |
| `search_plus_paths` | 1.194 ms | 1.241 ms | +2.7% (noise) |
| `common_stem_report` | 986 µs | 1.038 ms | **+3.7%** (p < 0.05, real) |
| `rare_needle` | 514 µs | 3.307 ms | +537% (**by design**) |

Two findings:

- **The literal path costs ~3.7% on one case.** Small, reproducible, and
  attributable to packing the ranking key per matching entry (a shift/or
  and a `min`) where the old code built a tuple. It is a *deliberate
  trade*: the unpacked struct was ~12% (measured, then fixed), so 3.7% is
  what is left after the fix, and the absolute figure — 1.04 ms at 200k
  entries — sits ~30× inside the one-keystroke budget. Flagged rather
  than buried, per the CLAUDE.md rule about latency on search-as-you-type.
- **`rare_needle` is 6.4× slower**, exactly the cost decision 2 predicted:
  no literal hits means both passes run. Still 3.3 ms, ~10× inside
  budget, and only when the result list would otherwise be empty.

## Deferred: Tier 1 (embeddings), and the bar it must clear

Not started. If it is revisited, the discussion settled on the cheap
shape: **static embeddings** (model2vec/potion-style token→vector lookup
+ mean pooling — no ONNX Runtime, no GPU story to solve three times),
binary/int8 quantized, flat SIMD scan rather than HNSW, opt-in per
folder, computed in `filex-indexd`, rendered as a *second* async section
below instant literal results.

**Gate before any of that is built**: take ~200 real queries against a
real disk and measure recall against the ranking this doc specifies. If
the delta is small, the 1.5 GB index and the content-extraction pipeline
are not worth it.

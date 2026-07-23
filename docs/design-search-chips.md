# Design: Search filter chips (Phase 2d, block 8 item 2)

Status: **design, not yet implemented.** Written 2026-07-23 as the gate
the roadmap requires before any code (see `docs/ui-enhance-roadmap.md`
block 8, "Search filter chips", and the perf rule: an index schema change
wants "design doc + benchmarks before building"). This settles the filter
grammar, the index schema change (size/mtime), how those fields get
populated and kept fresh per-OS, where predicates run in the query path,
the IPC/service story, the chip UI, and the benchmarks that gate the
merge — so implementation follows a plan rather than discovering the hard
parts (they are the freshness and Windows-population problems) mid-code.

## Goal & scope

Let a user narrow search with structured filters — `kind:image`,
`ext:pdf`, `size:>2mb`, `modified:today` — shown as removable **chips**
in the search field, composable with each other and with filename text
and the existing `tag:` filter (`report tag:work kind:pdf size:>1mb`).
This is block 8's second metadata item and the **first general
`key:value` filter grammar**; the `tag:` tokenizer from item 1
(`tags::parse_tag_query`) is its seed and gets generalized here.

**Scope decision (2026-07-23, user-confirmed): full v1 — put size + mtime
in the global index.** The alternative (ship only name-derivable filters
like `kind:`/`ext:` and defer size/modified) was rejected; v1 delivers the
whole chip set. That makes this a genuine reversal of a core indexing
decision (see next section), so the schema change, its per-OS population,
the live-freshness gap, and mandatory benchmarks are the heart of this
doc, not the grammar.

Non-goals (v1): saved/smart searches, boolean OR / negation
(`kind:image OR kind:video`, `-kind:folder`), regex, content filters
(still Phase 3 — no OCR/PDF text). These are notably absent from the
grammar below on purpose; the parser is written so they can slot in.

## What the index stores today (and why this is a real change)

`docs/indexing-architecture.md` §1/§3 make a deliberate choice: **the
index stores names only.** The live struct confirms it:

```rust
struct FileEntry {                 // src/index/mod.rs
    name: NameRef,                 // (u32 offset, u16 len) into name_pool
    name_lower: NameRef,           // folded copy, for search
    parent: EntryId,
    flags: u8,                     // dir / tombstone
    native_key: u64,               // FRN (win) / 0 elsewhere
}
```

No size, no mtime. The architecture is explicit that this is the point:
the Linux/macOS bootstrap reads `getdents64`/`getattrlistbulk` for
name + type and **does not `stat` every file**; "sizes/mtimes are fetched
lazily when a result is displayed" (~40 visible rows, not 2M). Storing
them globally trades that away, which is exactly why the roadmap gated
this on benchmarks. This doc keeps the name-only fast path honest by
measuring the regression, not hand-waving it.

## The filter grammar

One tokenizer splits a raw query into filename text + a set of typed
filters; unrecognized `key:` tokens fall back to literal text so the
field never rejects input.

```rust
// filex::search_filter  (new module; tags::parse_tag_query folds into it)
pub enum Filter {
    Tag(String),                       // existing tag: (item 1)
    Kind(FileKind),                    // kind:image  — name-derivable
    Ext(String),                       // ext:pdf     — name-derivable
    Size(NumRange<u64>),               // size:>2mb, size:1mb..5mb
    Modified(TimeRange),               // modified:today, modified:<7d
}
pub struct Query { pub text: String, pub filters: Vec<Filter> }
pub fn parse_query(raw: &str) -> Query;   // pure, unit-tested
```

- **Value grammar** (shared by `size:` and `modified:` and any future
  numeric key): a leading comparator `>`, `<`, `>=`, `<=`, a bare value
  (exact/`==`), or an inclusive `A..B` range.
- **`size:`** units `b`/`kb`/`mb`/`gb`/`tb`, case-insensitive, integer or
  decimal (`1.5gb`). **Decision: base-1024** (`1mb == 1048576`), matching
  Finder's "Size" column and most tools' expectations for filtering; the
  UI can label it MiB internally but chips read "MB". *(Confirmed
  2026-07-23; see Confirmed decisions.)*
- **`modified:`** keywords `today`, `yesterday`, `week` (last 7 days),
  `month`, `year`; relative `<7d`/`>30d`/`<2h`; absolute ISO dates
  `2026-01-01` and ranges `2026-01-01..2026-02-01`. Evaluated against a
  captured "now" so a query is deterministic within one run.
- **`kind:`** maps to the existing `FileKind` (image/video/audio/archive/
  code/document/folder/other) — derived from the extension already in the
  name, **needs no schema field**. `ext:pdf` is a raw extension match,
  also name-only.
- **Multiple filters AND** (matches `tag:`'s rule). Same key twice ANDs
  too (`size:>1mb size:<9mb` ≡ a range); the parser may fold these.
- Matching is case-insensitive; `parse_query` lowercases keys/enums.

`kind:`/`ext:` working with **zero index change** matters: they ship the
moment the grammar lands, independent of the risky schema work, and are
the fallback if size/mtime population proves too costly on some OS.

## Index schema change: size + mtime

```rust
struct FileEntry {
    // …existing…
    size: u64,      // bytes; 0 for directories
    mtime: i64,     // unix seconds (i64 tolerates pre-1970 / far future)
}
```

- **Memory.** +16 B/entry over today's ~60–80 B. At 2M files that's
  +32 MB (~+25%); at 10M, +160 MB. Acceptable but not free — flag it in
  the footprint bench. A later packing (`mtime` as `u32` seconds → +12 B,
  or size as `u48`) is noted as future tightening, not v1.
- **Persistence.** `persist.rs` serializes each entry field-by-field and
  pins `FORMAT_VERSION = 2`. Adding `size`/`mtime` to the per-entry
  record **bumps it to 3**; an older snapshot is rejected and rebuilt
  (the bootstrap is seconds — acceptable, same as any format change).
- **API.** `insert`/`insert_with_key` gain the two fields (default
  0/0 where a source can't supply them — see per-OS population); the
  browse-side listing already stats and can pass real values.

## Populating size/mtime per OS (the asymmetric part)

This is where "full v1" earns its cost. The three bootstrap backends give
metadata very differently:

- **macOS — nearly free.** The walker already uses `getattrlistbulk(2)`,
  which returns *any requested attrs* per batch. Add `ATTR_FILE_DATALENGTH`
  + `ATTR_CMN_MODTIME` to the request list; the bytes ride along in the
  same syscalls. Expected bootstrap-time delta: small. **Benchmark to
  confirm**, but this is the cheap OS.
- **Linux — a real cost.** `getdents64` yields name + `d_type` only; size/
  mtime need a `statx` **per entry**. On NVMe with the parallel walker
  this may still be low-single-digit seconds at ~1M files, but it is a
  regression to the "don't stat every file" rule and must be measured.
  Mitigations if the bench is bad: (a) `statx` with
  `STATX_SIZE|STATX_MTIME` only and `AT_STATX_DONT_SYNC` (cheapest); (b)
  best-effort — bootstrap without, backfill lazily (below); (c) keep it,
  accept a slower first index (it persists, so it's a one-time cost per
  volume). **Decision: measure (a) first; fall back to (b) if it blows the
  budget.**
- **Windows — the thorny one.** The elevated `FSCTL_ENUM_USN_DATA` path
  returns `USN_RECORD`s with name/FRN/parent/flags **but no size or
  mtime**, and the architecture explicitly forbids parsing raw `$MFT`
  bytes. Options:
  1. **Lazy/best-effort (recommended).** Bootstrap leaves size/mtime = 0
     ("unknown"); the USN *journal* then carries real values forward for
     any file that changes (its reasons include data-extend/truncation and
     basic-info change), and a low-priority background pass backfills
     resting files via `GetFileInformationByHandle` (FRN → handle) as
     cycles allow. `size:`/`modified:` filters treat 0/unknown as
     "excluded from a positive match" and the UI can note partial coverage
     while backfill runs.
  2. Stat-per-FRN at bootstrap — correct but slow at 2M and re-introduces
     the cost the USN fast path exists to avoid.
  3. Read `$STANDARD_INFORMATION`/`$DATA` sizes via the MFT — fastest but
     against the architecture's "don't parse raw MFT" rule; out of scope.
- **Generic walker fallback** (network/FAT/unindexed): `std::fs::metadata`
  per entry, same best-effort stance.

**Consequence to confirm:** size/mtime are **authoritative on macOS,
measured-then-decided on Linux, and best-effort (lazily converging) on
Windows** in v1. This asymmetry is the biggest open decision (below):
accept it, or stage `size:`/`modified:` behind macOS/Linux and ship only
`kind:`/`ext:` on Windows until backfill is proven.

## Freshness: the live-update gap

Today's `FsDelta` (src/index/watcher.rs) has **no metadata-change event** —
`Upsert{path,is_dir}` / `Remove` / `Rescan` / `NativeUpsert` /
`NativeRemove` / `PersistNow`. A file edited in place (same name, new
size/mtime) produces no delta the index acts on, so its stored size/mtime
would go stale. The native watchers *do* observe these changes; we're just
dropping them today:

- **macOS FSEvents**: `kFSEventStreamEventFlagItemModified` /
  `ItemInodeMetaMod`.
- **Linux inotify**: `IN_MODIFY` / `IN_ATTRIB`.
- **Windows USN**: `USN_REASON_DATA_EXTEND|DATA_TRUNCATION|
  BASIC_INFO_CHANGE`.

**Design: reuse `Upsert`, don't add a variant.** An in-place modify emits
`Upsert{path}` (or `NativeUpsert{key,…}` on Windows), and the upsert
handler re-reads size/mtime for a file (it already re-indexes). This keeps
the delta enum small and the writer path uniform. Cost control matters —
a file being written emits a *storm* of modifies:

- The writer already debounces ~50 ms batches; collapse repeated
  upserts of the same key within a batch to one re-stat.
- A re-stat on modify is one syscall off the writer thread, never the UI
  thread — consistent with the concurrency model.
- Windows journal upserts carry the FRN; re-stat by handle, or take
  size/mtime straight from the USN record where the reason provides them
  (avoids the syscall entirely for data-change reasons).

Restart catch-up already exists (USN/FSEvents replay, Linux reconcile
walk); metadata changes ride the same replay since they're now Upserts.

## Query integration

- **Where predicates run.** Filters evaluate **inside the index scan**,
  not as an after-the-fact pass in the UI, so filtered-out entries never
  materialize a path. `manager::search_all` (returning `MergedHit{name,
  path, is_dir, name_len}`) grows a `&[Filter]` param; the per-chunk rayon
  scan checks the predicate on each `FileEntry` (fields are inline in the
  arena — cache-friendly, like the name scan).
- **Text + filters**: name-match first (the existing fast scan cuts the
  set), then filter survivors — cheap. **Filter-only** (no text, e.g.
  `kind:image size:>2mb`): a full arena scan applying the predicate,
  ranked by name; at 2M this is the tag-bench shape (~1 ms measured for the
  analogous tag intersect).
- **`kind:`/`ext:`** read the entry name (already resolved for matches);
  **`size:`/`modified:`** read the new fields; **`tag:`** stays the sidecar
  intersect from item 1, applied as today (it's not in the index).
- **Windows service mode (IPC).** Search runs in `filex-indexd`, which
  owns the index — so the *parsed filters* must cross the pipe and be
  applied service-side (the client can't; it has no index). Extend the
  versioned IPC search frame to carry the encoded `Vec<Filter>` (the
  `tag:` filter, being sidecar/client-side, is applied on the client
  after results return, as it is now). Bump the IPC protocol version.

## Chip UI

The mockup shows filters as removable chips in the search field. Two
slices:

- **v1a — tokens as text (functional first).** Typing `size:>2mb` filters
  immediately; it lives as literal text in the input. Zero new input
  machinery, ships with the grammar. This is the honest MVP.
- **v1b — chips (the polish).** Recognized `key:value` tokens render as
  pills inside the search field (label + ✕), removable by click or
  backspace; `tag:` chips get the tag's color dot (reusing
  `ui::details::tag_dot_color`). This touches `ui::search_input` — the
  most interaction-heavy piece — so it is its own commit after the grammar
  and index work land and are benched.

**Decision: build v1a with the grammar, then v1b.** Don't gate the index
work behind chip rendering.

## Benchmarks (mandatory, per the perf rule)

Gate the merge on three criterion benches (mirroring `search_bench` /
`tag_bench`):

1. **Footprint** — index bytes at 2M entries before vs after the two
   fields; assert the delta ≈ 16 B/entry and record absolute MB.
2. **Bootstrap** — walk+populate time at ~1M entries, per backend where
   testable (macOS `getattrlistbulk` with vs without the extra attrs;
   Linux walk with vs without `statx`). This is the number that decides
   the Linux population strategy.
3. **Filter query** — filter-only and text+filter latency at 2M
   (`size:>2mb`, `modified:<7d`, `kind:image`); budget: well under one
   keystroke (~a few ms), same bar as `tag_bench`.

No regression to plain filename search-as-you-type: the predicate is off
the path unless a filter token is present (same stance as `tag:`).

## Testing plan

- **Pure/portable**: `parse_query` (comparators, units, dates, ranges,
  multi-filter AND, unknown-key fallback, `tag:` still works); the size
  and time predicate evaluators; `FileKind`/`ext` mapping — plain
  `#[test]`, fixture-driven, no GPUI, no index.
- **Index**: insert/serialize/deserialize round-trip carrying size/mtime
  across the v3 format; the in-scan filter over a synthetic index; a
  metadata-change `Upsert` updating size/mtime in place.
- **Per-OS population**: fixture-level where possible (a recorded
  `getattrlistbulk` attr buffer decode; a USN record with a data-change
  reason). Live population is dev-machine-only, same as today's watcher
  tests.
- **Behavioral trait suite**: the metadata-change contract runs against
  every watcher impl so platforms don't drift (CLAUDE.md's single-suite
  rule).

## Phasing (suggested implementation order)

1. `search_filter` grammar module (`parse_query` + predicate evaluators),
   `tag:` folded in — pure, tested. `kind:`/`ext:` wired into
   `search_all` (works immediately, no schema change).
2. Schema: `size`/`mtime` on `FileEntry`, `FORMAT_VERSION` bump,
   `insert*` + persistence + tests. macOS `getattrlistbulk` population +
   footprint/bootstrap benches.
3. Live freshness: modify→`Upsert`→re-stat across the three watchers,
   with debounce coalescing; behavioral suite.
4. `size:`/`modified:` predicates in the scan + filter-query bench; Linux
   `statx` population (decided by bench #2).
5. Windows population (lazy USN + background backfill) + IPC filter
   plumbing + protocol bump.
6. Chip UI: v1a tokens-as-text (with step 1), then v1b removable chips in
   `ui::search_input`.

## Confirmed decisions (2026-07-23)

- **Full v1 — size/mtime go in the global index** (not just name-derivable
  `kind:`/`ext:`). The schema change, per-OS population, freshness, and
  benchmarks above are in scope.
- **Size units: base-1024** (`1mb == 1048576`). Chips read "MB"/"GB" but
  mean MiB/GiB, matching Finder's Size column.
- **Windows size/mtime: best-effort / lazy.** `size:`/`modified:` are
  available on every OS; on Windows the bootstrap leaves them unknown and
  they converge via USN-forward updates + a background backfill pass, with
  a visible "indexing metadata…" state while it runs. (Not macOS+Linux-
  only.)
- **Date grammar: keyword + relative + ISO** — `today`/`yesterday`/`week`/
  `month`/`year`, relative `<7d`/`>30d`/`<2h`, ISO `2026-01-01` and `A..B`
  ranges. No natural-language ranges ("last month") in v1.
- **Chips: text-first, chips fast-follow.** v1a ships the grammar as
  literal text tokens (`size:>2mb` filters immediately); v1b adds the
  removable pill chips in `ui::search_input` as the next commit. Chips are
  **not** required in the first user-visible cut.

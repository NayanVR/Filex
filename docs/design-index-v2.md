# filex — index v2 clean-slate architecture

Status: implemented replacement, revised 2026-09-13 from local measurements.
The production application and daemon use v2. Section 19 records the concrete
implementation choices and supersedes earlier alternatives. Release qualification
is tracked in [index-v2-progress.md](index-v2-progress.md); measurements that have
not run are not treated as passing. The v1 design is historical.

## 1. Decision summary

Replace the UI-owned, full-arena search architecture with a persistent search
daemon built around:

1. one deduplicated, columnar file catalog;
2. immutable memory-mapped search segments;
3. an FM-index over unique normalized names for arbitrary substring lookup;
4. wavelet matrices for ranked, distinct top-K retrieval from match ranges;
5. compressed `NameId -> FileId` postings;
6. a small mutable overlay and write-ahead log for live changes;
7. a query planner that chooses exact, prefix, boundary, substring, metadata,
   or bounded-fuzzy retrieval;
8. a separate streaming API for exhaustive operations such as Magic plans.

The governing rule is:

> Interactive search retrieves the best visible results. It does not enumerate
> or score the entire matching population.

The suffix-array experiment in `benches/suffix_probe.rs` validates the central
latency claim. At 505,000 synthetic names, retrieving 500 distinct matches took
roughly 3-28 microseconds depending on selectivity, versus 1.6-2.6 milliseconds
for the v1 scan. Its 4-5x memory cost is not acceptable, but most of that cost is
from the deliberately naive representation: eight-byte suffix records, boxed
names, an absolute previous-occurrence array, and a power-of-two segment tree.
The implementation replaces those structures rather than shipping the prototype.

## 2. Product contract

### 2.1 Required behavior

- Search filenames across millions of files while the user types.
- Support exact, prefix, word-boundary, and arbitrary substring matching.
- Support one- and two-character queries without falling back to a corpus scan.
- Preserve file/directory identity across rename and move when the platform
  provides a stable native key.
- Reflect ordinary filesystem changes within 150 ms at p95.
- Keep browsing and file operations usable if the search daemon is unavailable.
- Keep all filename, path, tag, and query data local.
- Support current-directory scope and structured metadata filters.
- Recover durable catalog changes after a crash and reconcile offline changes.
  Journal-only recovery is deferred; the portable adapters reconcile on restart.

### 2.2 Explicit non-goals

- Content indexing is not part of index v2.
- Interactive search does not promise an exact total count on every keystroke.
- Arbitrary regex and wildcard expressions are not on the fast path.
- Global subsequence fuzzy matching is not implicit.
- Full paths are not stored or indexed as duplicated strings.
- The UI process does not own or publish whole-volume indexes.

### 2.3 Resource and latency gates

All measurements use an optimized build, the real developer snapshot, and a
fixed checked-in query/relevance fixture that contains no private paths.

| Metric | Gate at approximately 2 million files |
|---|---:|
| Existing-index daemon startup | < 1 second |
| Exact lookup p95 | < 1 ms |
| Prefix lookup p95 | < 3 ms |
| Literal substring p50 / p95 | < 3 ms / < 10 ms |
| Bounded fuzzy fallback p95 | < 20 ms |
| Update visibility p95 | < 150 ms |
| Search latency during compaction | < 2x normal p95 |
| Daemon warm RSS | <= 250 MiB |
| UI RSS excluding thumbnail cache | <= 100 MiB |
| Persistent index | <= 450 MiB |
| Peak construction / compaction RSS | <= 1.5 GiB; revised in §19 |
| Idle CPU after the watcher settles | effectively 0% |

Failure to meet a gate is a design result, not an invitation to weaken the
benchmark. The plan must be revised before integration continues.

## 3. Process topology

Every platform runs one persistent index owner:

| Platform | Process form |
|---|---|
| Windows | per-user `filex-indexd.exe`, started by the UI |
| macOS | per-user process; optional launchd agent |
| Linux | per-user process; optional systemd user service |

The GPUI application is a thin client over a versioned local protocol:

```text
filex UI
  |-- Search(query, scope, filters, limit, epoch_hint)
  |-- StreamMatches(query, scope, filters)
  |-- Status()
  |-- AddRoot / RemoveRoot
  `-- HintFilesystemChange(operation_id, affected_paths)

filex-indexd
  |-- query planner and executors
  |-- catalog writer
  |-- filesystem watchers
  |-- WAL and recovery
  `-- segment builder and compactor
```

Normal file operations remain in the user process. The UI sends a successful
operation as an immediate hint so search visibility does not wait for the OS
watcher; the eventual watcher event is deduplicated by native identity and
operation ID. The daemon exposes no privileged file-operation API.

The UI falls back to live directory browsing when the daemon is absent. Global
search reports a clear unavailable/rebuilding state rather than silently
starting a second in-process index.

## 4. Logical data model

### 4.1 File catalog

`FileId` identifies a filesystem object inside the catalog. It is separate from
the platform key because inode/FRN reuse and non-native fallback roots require a
generation-safe identity.

```text
FileId -> NameId
FileId -> ParentFileId
FileId -> RootId
FileId -> NativeKey + native generation
FileId -> flags (directory, hidden, symlink, tombstone)
FileId -> optional size
FileId -> optional mtime
```

Columns are stored separately. Queries touching only names and type must not
fault size or timestamp pages into memory.

### 4.2 Deduplicated name dictionary

Search indexes unique normalized basenames, not file occurrences:

```text
NameId 42 -> "package.json"
NameId 91 -> "index.js"

NameId 42 -> sorted/compressed postings [FileId, FileId, ...]
```

Normalization is versioned and deterministic:

- Unicode normalization form chosen once after corpus measurement;
- Unicode-aware case folding for search;
- original bytes/display spelling retained separately;
- non-UTF-8 names remain browsable and are addressable by exact raw bytes, but
  are excluded from normalized fuzzy search unless a reversible encoding is
  adopted.

Several case-sensitive names may share one folded search key. Postings preserve
their distinct `FileId` and display spelling.

Before implementation, measure:

- total file count;
- unique raw basename count;
- unique folded basename count;
- total and unique normalized bytes;
- duplicate-name frequency distribution;
- suffix and word-boundary counts.

These figures determine whether deduplication justifies the extra indirection.

### 4.3 Tree representation

Full paths are never stored per descendant. The catalog keeps parent links and a
compressed child lookup structure:

```text
(ParentFileId, NameId, raw-name discriminator) -> FileId
```

Paths are materialized only for returned results. A directory rename changes the
directory record; descendants keep their parent relations and need no search
rewrite.

The initial implementation uses sorted child ranges to enumerate small scopes
and verifies parent chains for text-first queries and moved subtrees. The tested
DFS representation remains available for later optimization; it is not a second
production scope executor.

## 5. Immutable search segment

An immutable segment contains:

```text
segment-N/
  header                 format, normalization version, checksums, sequence range
  catalog.*              hot column files
  names.text             optional extracted/display-name data
  names.fm               FM-index over unique folded names
  names.exact            exact dictionary / minimal perfect hash
  names.sorted           lexicographic NameId order
  names.prefix-rank      top-K support over sorted names
  names.docs             suffix-position -> ranked NameId wavelet matrix
  names.boundaries       start/word-boundary bitvectors
  names.boundary-docs    boundary-only wavelet matrix
  postings               NameId -> compressed FileId lists
  metadata.*             cold columns and filter indexes
  tombstones             deleted/replaced IDs relative to older segments
```

The implementation packs these logical sections into one aligned `.fx2` file
per base generation. Files are immutable, checksummed, and mapped read-only. Publishing a
new generation atomically swaps a small manifest containing segment IDs and the
highest incorporated WAL sequence. There is no deep clone of an index snapshot.

Old files remain alive until queries holding their generation finish, then are
deleted by a reaper.

## 6. Literal search algorithm

### 6.1 FM-index input

Concatenate unique normalized names with a reserved separator:

```text
report.pdf<SEP>invoice.txt<SEP>design-system.fig<SEP>
```

The FM-index uses backward search to turn any query into a contiguous suffix
range `[L, R)`. Range discovery depends mainly on query length, not the number of
files. The separator prevents a match from crossing filename boundaries.

### 6.2 Distinct ranked retrieval

Parallel to suffix order, store the ranked `NameId` for the suffix's source
name in a wavelet matrix. Given `[L, R)`, it returns the best distinct names
present in that range without enumerating all occurrences.

`NameId` search rank is static inside a segment and may include:

- filename length;
- directory/file prior;
- user versus system location prior;
- hidden/system penalties;
- aggregate historical usage at segment-build time.

For each returned name, its posting list is sorted by the best static file-level
rank. A heap merges postings across names. Retrieval stops once the next unseen
name's best possible score cannot beat the current Kth file.

Dynamic frecency is not encoded into the immutable wavelet matrix. A bounded hot
set of recent/frequent files is searched separately and merged with static
results.

### 6.3 Match tiers

Run tiers in strict order and stop when the visible budget is satisfied:

1. exact name;
2. filename prefix;
3. word-boundary substring;
4. arbitrary literal substring;
5. bounded fuzzy/acronym fallback.

Exact lookup uses `names.exact`. Prefix lookup performs two binary searches in
`names.sorted` and retrieves ranked names from that interval. The FM-index serves
arbitrary substring lookup.

During construction, suffix positions that begin a name or word component are
marked in succinct bitvectors. `rank` maps a full FM range into the corresponding
boundary-only document sequence, from which `names.boundary-docs` retrieves the
best boundary matches. Match-tier precedence is therefore structural rather than
rediscovered by scoring millions of candidates.

## 7. Fuzzy behavior

Remove implicit global subsequence scanning. Replace it with bounded mechanisms:

1. an acronym dictionary generated from word and camel-case boundaries;
2. a symmetric-delete spelling dictionary over filename components, normally
   capped at edit distance one;
3. optional explicit fuzzy mode over the candidates produced by those indexes;
4. the small hot/mutable set, which is always cheap to scan.

Fuzzy results run only when literal tiers return fewer than a small usefulness
threshold. A destructive or bulk Magic command never includes a fuzzy-only match.

## 8. Metadata indexes and query planning

Store coarse compressed indexes for fields that can substantially reduce a
candidate set:

```text
extension -> bitmap
kind      -> bitmap
size      -> logarithmic bucket bitmaps
mtime     -> day/week/month bucket bitmaps
tag       -> bitmap/posting list
root      -> bitmap/range
```

Exact values stay in cold columns and verify bucket boundaries.

The planner obtains cheap cardinality estimates before choosing an execution
order:

```text
"report ext:pdf"
  estimated text candidates: 80,000
  estimated PDF candidates:    4,000
  => filter first, then verify text

"very-specific-name ext:pdf"
  estimated text candidates:       3
  estimated PDF candidates:    4,000
  => text first, then check PDF bitmap
```

The planner has a hard work budget. If filtering removes too many overfetched
top-K results, it widens retrieval incrementally rather than requesting an
unbounded population immediately.

## 9. Interactive versus exhaustive APIs

Interactive search defaults to 100 results and may expose a capped count such as
`100+`. It is latency-bound and always cancellable.

```text
Search(query, scope, filters, limit=100) -> ranked page
```

Magic and explicit export/count operations use a separate streaming API:

```text
StreamMatches(query, scope, filters) -> ordered batches + progress + exact count
```

The exhaustive path may enumerate an FM range or filtered catalog, but it runs as
a visible job with cancellation and never shares the interactive query budget.
The plan builder consumes a stable index epoch and verifies every target against
the filesystem before mutation.

## 10. Mutable overlay, WAL, and visibility

Watchers and UI operation hints normalize changes into identity-oriented deltas.
The single catalog writer processes each delta as:

```text
assign sequence
  -> append and checksum WAL record
  -> update mutable catalog
  -> update mutable name/posting structures
  -> update tombstones
  -> publish query epoch
```

The mutable overlay deliberately uses simple structures:

- hash maps for identity and exact lookup;
- compact sorted vectors or a radix tree for prefix lookup;
- a direct SIMD scan for substring search;
- bitmaps/maps for metadata filters.

Its size is bounded, initially at the smaller of 25,000 changed files or 16 MiB.
When it crosses the threshold, freeze it, immediately open a new writable
overlay, and build an immutable segment in the background.

Queries merge results from immutable segments, the current overlay, and the hot
frecency set, then subtract tombstones and stale generations.

## 11. Compaction and quality of service

The initial writer maintains one unified base and bounded overlays. It compacts
a frozen view into a new base, one build at a time. Large enumeration runs spool
records to disk. Publication replaces the view only after durable manifest and
WAL writes. Frozen overlays are flushed into small delta segments, which are
merged among themselves and only occasionally into a new base; see
`docs/index-v2-maintenance.md` (FIL-30).

Priority order is explicit:

1. interactive query;
2. WAL append and mutable-overlay visibility;
3. watcher normalization;
4. frozen-overlay segment build;
5. compaction;
6. reconciliation and optional metadata backfill.

One worker is reserved for interactive requests and one for exhaustive streams.
One separate worker builds segments; one thread owns catalog mutation. Queues and
overlays are bounded. The optional service definitions request background
priority. Battery-aware scheduling and an explicit I/O token bucket are deferred.
No subsystem uses an unconstrained global Rayon pool.

Backpressure rules:

- watcher duplicates are coalesced by native identity;
- metadata-only changes collapse to the newest value;
- queue overflow emits a bounded subtree reconcile request;
- search work is cancelled at tier and segment boundaries;
- a newer query supersedes older interactive work immediately.

## 12. Recovery and correctness

The manifest records:

- format and normalization versions;
- active segment IDs;
- root configuration (v2 portable roots always require restart reconciliation);
- highest incorporated WAL sequence;
- catalog epoch;
- checksums.

Startup validates the latest generation, falling back to the preceding valid
manifest, then replays later WAL records. Watchers register before reconciliation
starts. A restart, watcher overflow, or explicit reconcile request rebuilds the
configured roots. Complete corrupt WAL frames are quarantined and trigger
reconciliation; only a torn trailing frame is trimmed in place.

Compaction writes to temporary segment paths, fsyncs completed files and their
directory, then atomically replaces the manifest. A crash before manifest swap
leaves unreachable temporary files; a crash after swap leaves a complete active
generation.

Every returned `FileId` carries enough generation information to reject identity
reuse. Before opening or mutating a result, Filex resolves and verifies its
current native identity.

## 13. Features deliberately removed or narrowed

| v1/general behavior | v2 decision |
|---|---|
| Full arena scan per query | Remove |
| UI-owned live indexes | Remove |
| Deep-cloned published snapshots | Remove |
| Independent full snapshot per root | Replace with unified catalog + `RootId` |
| Implicit global subsequence fuzzy pass | Remove |
| 500 eagerly ranked interactive rows | Default to 100; page incrementally |
| Exact total count per keystroke | Capped/optional asynchronous count |
| Magic built from interactive results | Separate stable exhaustive stream |
| Eagerly hot size/mtime for all files | Cold memory-mapped columns |
| Complete paths duplicated in search text | Do not index |
| Fuzzy evidence for destructive commands | Prohibit |
| Arbitrary regex on the fast path | Explicit slow mode only |
| Content indexing | Out of scope |

These removals are part of the v2 cutover, not optional follow-ups. Temporary v1
code may exist only on the implementation branch as a test oracle. It is not a
runtime fallback and must be deleted before the cutover is considered complete.

## 14. Source-tree replacement map

The implementation is a replacement, not an additional layer over `VolumeIndex`.
Build the new modules next to v1 long enough to validate them, switch all callers
in one cutover, and delete the superseded modules in that same development cycle.

### 14.1 Target source layout

```text
src/catalog/
  mod.rs                 FileId/NameId model and catalog snapshots
  columns.rs             memory-mapped hot/cold columns
  names.rs               normalized-name dictionary and postings
  tree.rs                parents, children, DFS/subtree representation
  wal.rs                 sequenced log, recovery, and checksums
  manifest.rs            atomic generation publication

src/search/
  mod.rs                 public query/result API
  query.rs               parsing and normalized query model
  planner.rs             cardinality estimates and execution choice
  exact.rs               exact dictionary
  prefix.rs              sorted-name range retrieval
  fm.rs                  FM-index abstraction and implementation
  wavelet.rs             ranked/distinct range retrieval
  boundary.rs            filename-start and word-boundary indexes
  fuzzy.rs               acronym and symmetric-delete lookup
  filters.rs             metadata bitmaps and verification
  overlay.rs             mutable names, postings, and tombstones
  ranking.rs             static score and hot-frecency merge

src/daemon/
  mod.rs                 cross-platform daemon runtime
  ipc.rs                 versioned local protocol
  writer.rs              single catalog writer and visible epochs
  segments.rs            freeze/build/publish lifecycle
  compaction.rs          bounded background compaction
  recovery.rs            manifest validation and WAL replay

src/ingest/
  mod.rs                 normalized filesystem delta API
  bootstrap.rs           per-root initial enumeration coordination
  macos.rs               macOS enumerator/FSEvents adapter
  linux.rs               Linux enumerator/watcher adapter
  windows.rs             Windows MFT/USN/RDCW adapter
```

The exact file split may change during implementation, but ownership boundaries
must remain: the catalog does not know GPUI, search structures do not call the
filesystem, ingest does not mutate indexes directly, and the daemon is the only
writer.

### 14.2 Existing code to delete or replace

| Existing path/symbol | Required disposition |
|---|---|
| `src/index/mod.rs::VolumeIndex` | Delete after the search and catalog cutover |
| `src/index/manager.rs` search/merge functions | Replace with `search::planner` and segment merge |
| `src/index/watcher.rs::SharedIndex` and `IndexWriter` | Delete; immutable generations plus one daemon writer replace them |
| `src/index/persist.rs` v1 snapshot format | Delete after v2 rebuild/cutover; do not maintain a permanent reader |
| `src/index/ipc.rs` | Replace with the cross-platform v2 protocol |
| `src/index/walker.rs` | Move useful enumeration code into `ingest::bootstrap`; delete index-building responsibilities |
| `src/index/{macos,linux,windows,usn}.rs` | Retain only tested OS enumeration/event decoding, move it under `ingest`, and delete v1 index coupling |
| `src/bin/filex-indexd.rs` | Rewrite as the cross-platform daemon entry point with per-user startup on Windows |
| `workspace::RootSlot` / local `LiveIndex` ownership | Delete; workspace stores daemon status and query client only |
| `workspace/search.rs` local manager calls | Replace with cancellable IPC requests and paged results |
| `workspace/roots.rs` local bootstrap/watch lifecycle | Replace with daemon root-management RPC |
| v1 search/filter/manager benchmarks | Replace with catalog, planner, segment, recovery, and end-to-end daemon benches |
| `arc-swap` dependency | Remove unless another non-index subsystem proves a need |
| global search Rayon pool | Remove; daemon owns explicitly bounded workers |

Pure, still-correct logic such as query syntax, OS event parsing, file operations,
and UI components may be reused. Reuse requires moving it behind the new boundary;
it is not a reason to preserve v1 orchestration.

### 14.3 Data migration policy

Do not translate `.fxidx` into the new format. The old snapshot duplicates names
and lacks several v2 invariants, so importing it would complicate correctness and
retain an obsolete parser.

On first v2 launch:

1. preserve user settings, roots, tags, favorites, and recents;
2. create an empty v2 catalog and begin platform-native enumeration;
3. serve an explicit `Building index` state while browsing remains available;
4. atomically activate the first complete v2 generation;
5. remove v1 `.fxidx` files after successful activation;
6. never write the v1 format again.

If v2 construction fails, keep diagnostic files and retry; do not silently revive
the old engine. Development builds can retain the v1 oracle behind a compile-time
test feature, but release binaries must not link it.

### 14.4 Cutover definition of done

The replacement is complete only when:

- every global query goes through the daemon;
- no GPUI entity owns a whole-volume index;
- no release binary contains `VolumeIndex` search or v1 persistence code;
- no startup path loads `.fxidx`;
- v1 index dependencies unused elsewhere are removed from `Cargo.toml`;
- old benchmarks and tests are replaced or moved to historical fixtures;
- `docs/indexing-architecture.md` is marked historical and points here;
- clean installs and upgrades exercise the same v2 runtime path.

## 15. Implementation and validation program

### Stage 0 — corpus and workload measurements

Add a read-only tool that loads the real v1 snapshot and reports aggregate data
only:

- file, directory, unique-name, and unique-folded-name counts;
- normalized byte totals and length distribution;
- duplicate-name distribution;
- suffix and boundary counts;
- hot metadata cardinalities;
- estimated packed-SA, FM, wavelet, and posting sizes.

Collect a privacy-safe query-shape histogram containing lengths, tier reached,
filter kinds, candidate counts, latency, and cancellation status—but never query
text or paths.

### Stage 1 — standalone exact/literal laboratory

Build outside the app:

- deduplicated names;
- concatenated text;
- packed 32-bit suffix array as the correctness oracle;
- exact and prefix tables;
- word-boundary bitvectors;
- wavelet matrix over ranked `NameId`s;
- compressed postings;
- exhaustive scan oracle for result-set and rank validation.

Do not start with an FM-index. First prove ranked distinct retrieval and memory
accounting with the simpler suffix array.

Pass gates:

- exact literal recall: 100%;
- top-100 order matches the ranking oracle;
- 505,000-name search structures: <= 100 MiB;
- real-snapshot p95 literal latency: <= 10 ms;
- no private corpus data is written into repository fixtures.

### Stage 2 — FM-index replacement

Replace packed suffix offsets with at least two FM-index representations and
measure:

- final disk and resident sizes;
- construction wall time and peak RSS;
- cold and warm query latency;
- rank-operation cache behavior;
- Unicode normalization correctness;
- extraction/locate sampling requirements.

Keep the packed suffix array if an FM-index misses the latency gate or saves less
than 25% of total search-structure memory. Compression is not automatically a
win when it creates irregular memory access.

### Stage 3 — WAL and mutable segments

Implement deterministic delta replay, tombstones, frozen overlays, atomic
manifest publication, and crash-injection tests. Verify create, delete, rename,
directory move, native-key reuse, watcher overflow, and checkpoint invalidation.

Pass gates include 10,000-event bursts without interactive p95 exceeding twice
the quiet p95 and restart convergence without a full rescan when the checkpoint
is valid.

### Stage 4 — temporary oracle comparison

In development builds only, run v2 beside a read-only v1 oracle without serving
v1 results to the UI. For sampled local queries, compare aggregate result
IDs/ranks, never persist raw query text, and record:

- missing/extra literal results;
- top-10/top-100 rank disagreement;
- latency by tier;
- update visibility;
- daemon RSS and mapped bytes;
- segment count and compaction amplification.

The oracle is compiled out and its call sites are deleted when these comparisons
pass. It does not become a shipped fallback.

### Stage 5 — application cutover

Switch the application to v2 daemon results and remove local indexing from the
workspace. Keep browsing independent. Exercise clean installation, upgrade,
corrupt-index rebuild, daemon crash/restart, and permission-loss paths. A failed
daemon produces an explicit unavailable state, not an in-process v1 fallback.

### Stage 6 — delete v1 and ship

Delete v1 after the local correctness cutover, and require cross-platform release
qualification before publishing. Remove v1 full scans, per-root
snapshots, in-process live indexes, deep snapshot publication, implicit global
fuzzy scanning, compatibility IPC, and the v1 snapshot reader. Ship only after a
repository search and dependency audit prove the old runtime is absent.

## 16. Ordered implementation work

The implementation followed the storage/search gates before UI integration.
The revised sequence below distinguishes code completion from release validation:

1. Add the Stage 0 aggregate corpus analyzer and record a private local baseline.
2. Export a privacy-safe synthetic/relevance corpus for reproducible CI.
3. Rework `suffix_probe` to use deduplicated concatenated names and packed `u32`
   suffix offsets.
4. Replace its segment-tree RMQ with ranked distinct retrieval via a wavelet
   matrix; validate exact top-100 ordering against an exhaustive oracle.
5. Implement the catalog IDs, columns, name dictionary, compressed postings, and
   tree representation as production modules with round-trip/property tests.
6. Implement exact, prefix, boundary, and packed-suffix substring executors plus
   the query planner.
7. Implement and compare FM-index candidates; select packed suffix or FM strictly
   from Stage 2 gates, then delete the losing implementation from production.
8. Implement metadata indexes, hot frecency overlay, acronym lookup, and bounded
   typo correction.
9. Freeze and document the on-disk segment format and crash-safe manifest.
10. Implement the WAL, mutable overlay, segment freeze/build, compaction, and
    fault-injection tests.
11. Implement the cross-platform daemon protocol and runtime, then connect OS
    ingestion adapters.
12. Add `Search` and `StreamMatches` clients to the UI and move Magic to the
    exhaustive stream.
13. Run the temporary v1 oracle comparison until correctness and ranking gates
    pass.
14. Cut the UI over, delete every item in the replacement map, remove unused
    dependencies, and delete v1 snapshot files after first successful activation.
15. Run release, upgrade, crash, churn, memory-pressure, and cross-platform test
    matrices; ship only when all definition-of-done checks pass.

## 17. Selected decisions

- NFC plus full non-Turkic case folding, normalization version 1; pinned crates.
- Exact raw native names retained, including Unix invalid UTF-8 and Windows
  unpaired UTF-16. Unfoldable names participate in catalog/metadata operations,
  not normalized literal or fuzzy lookup.
- Monotonic `u64` file IDs; no slot reuse. Native identity combines volume/device,
  inode/file key and creation generation where the filesystem exposes it.
- Segment-local name rank: folded byte length, folded lexical order, then file ID.
  Tier precedes that rank; a bounded recently opened set is merged within tiers.
- Wavelet BWT FM rank, 512-bit checkpoints and delta-varint postings.
- Tags remain a client sidecar, resolved before result truncation.
- One unified base, 25,000-entry / 16 MiB overlay thresholds; up to two bounded
  layers during builds. Overflow requests reconciliation and does not claim
  complete watcher coverage.
- No implicit global subsequence or regex scan. Fuzzy candidates come from
  bounded acronym and symmetric-delete dictionaries.

## 18. Stop conditions

Abandon or substantially revise this architecture if any of the following holds
after Stage 2:

- unique-name deduplication is materially weaker than expected;
- total ranked retrieval plus filename lookup exceeds the combined search budget (150 MiB on the measured 2.13-million-entry corpus);
- the total catalog plus search engine exceeds the 250 MiB warm-RSS gate;
- compressed index queries miss the 10 ms p95 gate under cold or write-heavy
  conditions;
- segment construction regularly interferes with foreground work;
- filter-aware ranking cannot preserve result quality without large unbounded
  overfetch;
- platform recovery cannot provide trustworthy live results.

The fallback is not to keep or revive v1. It is to reduce the v2 design—most
likely to a packed suffix array over deduplicated names with a bounded mutable
overlay—and repeat the gates with narrower product guarantees before cutover.


## 19. Measurement-driven implementation revision

The user authorized revising the plan and proceeding through architectural
blockers. The original Stage 2 relative-size condition rejected an otherwise
successful FM compression: FM lookup uses 23.43 MiB while ranking uses 65.88 MiB.
The combined search budget is therefore 150 MiB on the measured 2.13-million-entry
corpus. The selected implementation, including bounded fuzzy dictionaries, uses
135.18 MiB and saves 31.47% against the packed suffix representation. The sampled
BWT alternative remains laboratory-only; production never retains suffixes for
queries or links v1.

The first production implementation deliberately uses one base generation and
serialized full compaction. Its warm mapped-query process uses approximately
150 MiB, but building and compacting are different workloads. Measured peaks
are approximately 1.03 GiB for construction and 1.12 GiB for compaction with the
old mapped generation alive. The original `<1.5x steady state` compaction target
is not met. Replace that target with an explicit **1.5 GiB construction and
compaction budget at approximately two million entries**. This is a material
tradeoff, not a claim that construction has the same memory cost as searching.
Low-memory and larger-corpus qualification remains a release requirement; partial
compaction is the next architectural step if the absolute budget is exceeded.
The 250 MiB warm-daemon target remains unchanged.

The September 12–13 cleanup moves segment construction into a disposable child
process. A continuing in-process owner retained over 1 GiB after compaction even
after releasing its old generation; this was not captured by the earlier fresh
query-process measurement. The owner now spools records, launches the companion
daemon in worker mode, and validates its output after the worker exits. A lifetime
pipe and explicit kill/reap handling cover owner death and cancellation. The
worker never publishes a manifest or edits the WAL. Construction memory is
released with the process. The budget applies to the **combined owner and worker
peak**, not just the owner. Spooling adds temporary disk use and build latency;
the validation report records both costs. The segment format remains `FXSEG002`.

Portable `notify` adapters replace the v1-coupled native adapters. They never
claim journal-exact recovery: startup and overflow reconcile configured roots.
Watch coverage and unreadable subtrees are surfaced explicitly. Full native
journal replay, elevated machine-wide endpoints, battery scheduling, and an I/O
rate limiter are deferred. The per-user daemon uses authenticated loopback IPC;
it serves only that user's filesystem permissions. Windows MSI installs the
sibling executable rather than registering an elevated SCM search service.
Windows update discovery moves to the UI; installation opens the release page
instead of silently invoking an elevated installer from a service.

Interactive requests default to 100 rows and use bounded, progressive retrieval.
Pages carry an epoch and expose partial work limits; refining text or scope is
required when the interactive candidate budget is exhausted. The UI pages within
the same epoch, cancels superseded work, and reconnects to the daemon without
owning an index. Magic uses a separate stable, exhaustive stream, stops at its
review cap, and revalidates native identity before acting. Normal search-result
opens and mutations also validate identity.

The file format and operational details are documented in
[index-v2-progress.md](index-v2-progress.md). Available local tests and platform
cross-checks are required before handoff. Windows/Linux execution, installer
upgrade runs, cold-device latency, and sustained low-memory/churn qualification
remain explicit release checks rather than invented passing measurements.

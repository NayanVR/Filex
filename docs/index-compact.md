# Compact index format and search

The compact index changes the representation rather than dropping search
features. It preserves NFC/full case folding, original-name boundary unions,
exact/prefix/boundary/substring rank, bounded fuzzy candidates, native names,
64-bit identities, metadata filters, the WAL, and two recovery generations.

## Search unique names, not every character position

The preceding FM representation stores a suffix-to-name wavelet matrix, a BWT
matrix, and additional boundary-ranking structures. Together those structures
occupy about 90 MiB in the saved 2.13-million-entry corpus. They buy much lower
literal-query latency than this application requires.

New indexes retain the ranked unique-name dictionary, name-to-file postings,
lexical prefix lookup, and the small prefix-ranking matrix. They replace the
large substring and boundary structures with transposed gram signatures:

- Each group contains 64 consecutive names in static rank order.
- A 4,096-bucket signature records all one-, two-, and three-byte grams in a
  group. Each gram sets two hash positions. The transposed layout makes a query
  an intersection of small contiguous bit vectors.
- A separate 1,024-bucket signature records grams at original word boundaries.
  Boundary retrieval intersects this signature with the full-query signature;
  testing just the first gram was too permissive for rare long queries.
- Candidates are visited in name-rank order and checked against actual name
  bytes. Collisions can increase work but cannot create returned false matches.
- Boundary candidates are exhausted, or fill the requested page, before
  substring candidates are considered. The original boundary bit vector is
  retained because case folding can erase CamelCase boundaries.
- One- and two-byte queries use their corresponding signatures. Unicode queries
  use normalized UTF-8 bytes and are verified against the complete string.
  This is not a three-character-minimum search interface.

The two accelerators occupy about 6.7 MiB on this corpus. The query planner uses
admitted groups as a conservative name-count estimate; it does not treat that
estimate as an exact hit count. Literal recall and static rank are unchanged.
Signature geometry, hash version and required gram bits are validated on load.

## Block-packed catalog columns

Numeric columns use independently decodable blocks of 256 entries. A block
stores its minimum value and the number of bits needed for each delta. Constant
blocks have no value payload. Signed timestamps use an order-preserving unsigned
mapping, retaining the entire i64 domain. File IDs and native identities retain
the entire u64 domain, including large IDs and gaps after deletion.

Reads decode one value from the mapped block; loading does not expand columns
into full-width arrays. All widths, offsets, lengths, additions and integer
ranges are validated before query access. Catalog lookup ordering, permutations,
parent relationships and tree validation remain in place.

Compression happens before name-search construction so the builder does not
retain all full-width columns alongside its normalization dictionaries. The
builder no longer creates the large suffix array or substring/BWT matrices.

## Size-first text and postings (format 4)

Raw native filenames and normalized search text use independent 64 KiB DEFLATE
pages on disk. Logical byte offsets remain stable, including names that cross
page boundaries. Text is stored plain if compression would increase its size.
The portable disk codec uses flate2's pure Rust backend
([backend documentation](https://docs.rs/flate2/1.1.9/flate2/)).

Query access uses a different representation. The normalized search dictionary
is decoded once at load (22.05 MiB for the saved corpus). Broad candidate scans
read its bytes directly; paying decompression costs repeatedly made them too
slow. Native names stay compressed: validation converts their pages once into
in-memory LZ4 blocks, decoded through a 16-page LRU (about 1 MiB). Encoded blocks
occupy one anonymous mapping, avoiding allocator retention
from growing and shrinking large buffers. It reserves virtual address space for
the worst case but touches only the encoded payload. The block codec uses safe
encoding and checked, safe decoding
([LZ4 API](https://docs.rs/lz4_flex/0.14.0/lz4_flex/block/index.html)).
Neither runtime representation is serialized, so existing format-4 files gain
the speedup simply by loading them in the updated daemon, without a rebuild.

A query cursor reads candidate names without copying each string or taking the
page-cache lock for each candidate. Crossing page boundaries remains supported.
Unfiltered catalog queries avoid unnecessary name decoding for empty predicates;
native path and record construction reuse the already-owned decoded bytes.

The reader validates page offsets, exact decoded lengths, stream completion, and
input consumption before exposing a segment. Decompression output is limited to
one page plus a byte to detect oversized data. At startup, child-order validation
decodes the raw pool once into temporary memory; repeatedly decoding randomly
ordered child names made validation unnecessarily slow. That temporary uses an
anonymous mapping, released on drop before fresh file mappings are installed,
so the allocator cannot retain the whole buffer afterward. Allocator cache
relief also releases other unused startup buffers on macOS and glibc Linux.
The native-name pool is never expanded in full during normal queries. The
normalized search dictionary deliberately remains decoded for fast scanning.

Name and posting offsets use the same block-packed columns as catalog metadata.
Coarse metadata postings also encode consecutive ordinals as runs. Their exact
cardinalities are stored separately so the query planner cannot confuse a tiny
compressed run with a tiny result set. Sparse name/fuzzy postings retain delta
varints. Every run, addition, count, and list boundary is checked on load.

The size-first policy has no 10 ms search gate. Measurements below describe the
latency cost; correctness, ranking, native bytes and recovery remain mandatory.

## Construction and recovery

The isolated builder process remains: exiting it releases allocator caches.
Its version-2 scratch spool has a JSON header followed by binary records. Each
record has 66 bytes of metadata and the original name bytes. Optional size and
mtime presence is explicit. Names and records remain length-bounded; truncated
records fail the whole build. The worker also accepts the earlier version-1
JSON-lines request. This scratch codec does not change WAL serialization.

The worker tracks spool offsets while reading and preserves buffered reads for
adjacent records. It seeks only when ID sorting changes the input order. For an
unchanged validated base (including overlays that only append new IDs), record
enumeration reads known catalog slots directly instead of reconstructing every
path. Changes to existing IDs or roots retain the ancestor-validation path.

New segment files start with `FXSEG004`. Readers accept `FXSEG002`, `FXSEG003`,
and `FXSEG004`; legacy numeric spans and FM search structures are decoded without
eager conversion. A normal rebuild writes the new representation. Manifest and
IPC versions remain 2, and generation filenames retain the `.fx2` suffix; the
header identifies the segment layout. Older binaries cannot read `FXSEG004`.
An old manifest plus WAL remains available during conversion under the existing
two-generation protocol. Once both generations are compact, downgrading requires
rebuilding with the older application.

## Validation and reproduction

The laboratory's `verify OLD NEW` mode compares every record, every ranked name,
and every name-to-file and metadata posting. It checks all 37 fixture queries against the
exhaustive old-index oracle at limits 1, 100, 202 and 1,000, compares complete
substring recall, and compares fuzzy candidates. Reports contain aggregate
counts only. `mapped` measures the query engine with catalog reads and result
paths, excluding IPC and UI debounce. `profile` additionally separates planning,
name retrieval and fuzzy costs. `mixed` rotates the 37 fixtures across 20 rounds;
`typing` does the same for their 166 Unicode-safe prefixes. These modes avoid
repeating a single query to warm it before measuring the next query.

```sh
cargo build --locked --release --no-default-features --features index-v2-lab \
  --bin filex-index-lab --bin filex-indexd
target/release/filex-index-lab compact OLD_SEGMENT NEW_SEGMENT
target/release/filex-index-lab verify OLD_SEGMENT NEW_SEGMENT
target/release/filex-index-lab mapped NEW_SEGMENT
target/release/filex-index-lab mixed NEW_SEGMENT
target/release/filex-index-lab typing NEW_SEGMENT
```

Tests cover small and large limits, complete recall, overlapping matches,
normalization and boundary unions, signature collisions, mapped round trips,
bit widths 1–64, constant/empty columns, signed extrema, malformed columns,
missing signatures, old-format migration, binary-spool truncations, worker
lifetime, corruption recovery and existing daemon/IPC behavior.

Private corpus files and raw measurement output stay under ignored `target/`.
The archived corpus has zero native-identity columns inherited from its export;
its compression ratio must not be generalized to every live filesystem. The
benchmarks use warm filesystem caches and do not represent cold-device latency
or the full UI/watchers workload.

## Indexed Magic retrieval, September 30, 2026

Magic uses the search engine with exhaustive stopping rules. Its stream now
intersects complete metadata postings in temporary file-ordinal bitmaps. Each
bitmap costs one bit per base entry (about 260 KiB for this corpus) and is never
persisted. Multiple size/date buckets are unioned within a predicate; predicates
are intersected. Exact numeric comparisons reject bucket-edge candidates before
native filenames are decoded.

For nonempty text, metadata sets of at most 4,096 candidates receive direct
literal checks; otherwise complete filename postings are intersected with the
metadata set. Loose signature estimates cannot override this bound and send a
large metadata set through native-name decoding. A single-byte query estimated
to cover at least half the dictionary, without metadata candidates, streams
directly so the preview can stop early without materializing a huge posting set.
There is no 16,384-candidate limit or truncation on literal posting retrieval.
Block signatures are verified against normalized names. Legacy dictionaries
remain supported with cancellable dictionary enumeration.

Small folder scopes retain complete child-range enumeration. Large scopes use
the indexed planner and exact final path checks. Tag membership is also an exact
final path predicate; resolving just one ID per allowed path could miss transient
duplicate paths. Broad unfiltered commands still need catalog enumeration.

The base set excludes every changed ID. The newest overlay version of every
changed record is merged back in stable FileId order, so updates that newly match
are included, obsolete versions are excluded, and tombstones remain effective.
Final path reconstruction rejects descendants of deleted folders. Cancellation
is checked during planning and streaming, and a cancelled query never emits a
successful completion marker. Fuzzy matching and interactive result/work limits
do not enter the exhaustive stream.

The UI still waits for completion before building an executable plan, and still
refuses plans over 1,000 matches. This change does not alter confirmation, identity
checks, the typing debounce, IPC, or the disk layout. It takes effect when the
updated daemon is loaded; no format-4 rebuild is required.

The `filex-index-lab magic SEGMENT` command compares the previous stream and the
indexed stream on twelve public query shapes, three runs each, including
large folder scopes. It follows the UI's
preview cap, compares the first 1,001 IDs/paths/tiers, and reports completion,
records examined and time to first hits. Output contains aggregate values only.
The previous implementation is available only under `index-v2-lab`, not in
production builds. This measures engine streaming, excluding IPC and the UI.

### Magic benchmark results

Same saved 2,129,454-entry format-4 corpus; median of three warm measurements
per query shape, in milliseconds. Both implementations run in the same release
binary with the current text-pool runtime, isolating the retrieval change from
the earlier decompression improvements. No compilation or other validation
processes overlapped these measurements. They are not p95 measurements.

| Selection | Matches | Previous stream | Indexed stream |
|---|---:|---:|---:|
| PDF files | 697 | 1,072.10 ms | 1.29 ms |
| PDF files larger than 10 MiB | 21 | 1,089.11 ms | 0.095 ms |
| Images modified since January 1, 2024 | >1,000 | 9.90 ms | 6.11 ms |
| Files larger than 1 GiB | 36 | 995.93 ms | 0.55 ms |
| Screenshot PNG files | 94 | 1.55 ms | 0.90 ms |
| Names containing `.rs`, larger than 1 MiB | 107 | 1,016.21 ms | 7.47 ms |
| Directory names containing `config` | 864 | 33.19 ms | 13.51 ms |
| Broad single-character literal | >1,000 | 4.43 ms | 3.06 ms |
| PDF files, scoped to the corpus root folder | 697 | 1,094.05 ms | 19.07 ms |
| `config` directories, scoped to the corpus root folder | 864 | 51.43 ms | 31.34 ms |

Two additional no-match extension/date queries fall from about one second to
0.03–0.04 ms. Every fixture has identical preview IDs, paths and tiers. Selective
numeric/posting queries examine only their matching records instead of all
2.13 million entries. Large scopes still spend time attempting bounded subtree
enumeration before choosing the indexed path. Move/copy destination-directory
I/O, plan construction, IPC, and UI debounce are not included in these timings.

All **277 regular tests** pass with one test thread. New full-scan oracle tests
cover intersected filters, numeric bucket edges, missing metadata, non-ASCII
extensions/native bytes, duplicate paths, layered updates, tombstones, deleted
ancestors, complete results beyond interactive limits, cancellation and stale
epochs. The 10,000-file churn/IPC test also passes. All-target compilation,
Windows/Linux cross-checks, formatting and Clippy pass with the previously
recorded lint allowances.

Raw measurements: `target/magic-indexed-corpus-final.jsonl`; tests:
`target/magic-indexed-all-tests-final.txt` and `target/magic-indexed-churn.txt`.
The release binaries remain in `target/index-v4-build/`. The running application's
daemon and live index have not been replaced. The saved segment remains
71,122,454 bytes; these changes do not modify the persisted representation.

## Latency recovery, September 28, 2026

The disk format remains **71,122,454 bytes (67.83 MiB)** for the same 2,129,454
entries. The runtime representation above recovers query speed by keeping the
normalized dictionary decoded and using faster compressed native-name pages.
It deliberately spends more RAM than the earlier size-first implementation.

| Measurement | Earlier format-4 runtime | Current runtime |
|---|---:|---:|
| One base segment | 67.83 MiB | 67.83 MiB |
| Slowest fixture's p95, 37 fixtures × 50 samples | 93.28–105.61 ms | 4.23–4.48 ms |
| Warm query-process RSS, repeated fixtures | 47.25–47.98 MiB | 71.75–72.28 MiB |
| Validated load, warm filesystem cache | 3.30–3.50 s | 3.07–3.09 s |

Each range covers three separate process runs. The earlier column is the saved
September 27 measurement, not a simultaneous comparison under identical machine
load. The current measurements ran without overlapping compilation or other
benchmark processes. They include query planning, catalog lookup and reconstructed
result paths; IPC and the application's 50 ms typing debounce are excluded.

Additional three-run workloads exercise changing queries without first warming
each query repeatedly:

| Workload | Queries per run | Aggregate p50 | Aggregate p95 | Aggregate p99 | Warm RSS |
|---|---:|---:|---:|---:|---:|
| Mixed fixtures | 740 | 1.15–1.17 ms | 3.30–3.38 ms | 4.15–4.32 ms | 71.84–72.42 MiB |
| Typing prefixes | 3,320 | 1.49–1.54 ms | 2.85–2.97 ms | 6.05–6.22 ms | 75.61–76.48 MiB |

First-pass p95 is 3.42–3.64 ms for mixed fixtures and 2.88–3.02 ms for typing
prefixes. The slowest individual prefix has p95 6.59–7.05 ms. These are query-engine
measurements on the saved corpus, not a guarantee for every query or live volume.

The full-corpus comparison passes: every record, all 702,708 ranked names, every
name and metadata posting, the 37 fixture queries at four limits, complete
substring recall and fuzzy candidates match the original FM corpus. All **275
regular tests** pass with one test thread, including compressed-page corruption,
Unicode, native bytes, migration and daemon recovery. The explicit 10,000-file
churn/IPC test also passes (54 concurrent queries); its latency is not used in
the tables because validation/build checks overlapped that test.

One earlier parallel suite run hit an owner-lock restart race in
`self_exclusion_bursts_and_corrupt_generation_fallback` (`os error 35`). The
isolated rerun and subsequent full serial suites pass. This does not establish
that the parallel-test race is fixed; the server's locking code is unchanged.

The isolated production worker rebuilds the saved format-4 corpus in **18.93
seconds** and produces a byte-for-byte identical segment, verified with `cmp`.
The rebuild ran after the other validation processes completed. Its output is
`target/index-v4-latency-rebuild.jsonl`; the segment is
`target/index-v4-latency-rebuilt.fx4`.

Formatting, diff checks, all-target compilation, Windows/Linux cross-checks and
Clippy pass. Clippy retains the two previously documented allowances for
`manual_is_multiple_of` and `question_mark`. Cross-checks are compilation only;
Windows/Linux runtime qualification remains outstanding.

Measurements are in `target/index-v4-latency-{mapped,mixed,typing}-{1,2,3}.jsonl`;
verification and test outputs are `target/index-v4-latency-verify.jsonl`,
`target/index-v4-latency-tests.txt` and `target/index-v4-latency-churn.txt`.
The binaries remain isolated under `target/index-v4-build/`; the running
application and live index were not replaced. Existing format-4 indexes gain
the runtime changes when loaded by the updated daemon, with no index rebuild.

## Earlier size-first format-4 results, September 27, 2026

Same saved corpus: 2,129,454 entries and 702,708 normalized names. Format 4 is
**71,122,454 bytes (67.83 MiB)**, or **33.4 bytes per entry**. That is another
**37.1% reduction** from format 3 and **78.5% below** the original FM segment.

| Measurement | Format 3 | Format 4 |
|---|---:|---:|
| One base segment | 107.84 MiB | 67.83 MiB |
| Two equally sized retained bases | 215.69 MiB | 135.66 MiB |
| Raw native-name text, including compressed page offsets | 21.42 MiB | 7.80 MiB |
| Normalized text, including compressed page offsets | 22.05 MiB | 6.75 MiB |
| Both name-offset columns | 5.38 MiB | 2.28 MiB |
| Metadata postings | 9.92 MiB | 3.88 MiB |
| Name-to-file postings, including offsets | 7.48 MiB | 5.75 MiB |
| Warm query-process RSS | 61.5–61.6 MiB | 47.25–47.98 MiB |
| Slowest fixture's p95 query time, across three runs | 5.25–5.75 ms | 93.28–105.61 ms |

The query runs each use 50 samples for every one of the 37 fixtures. Between 29
and 30 fixtures have p95 at or below 30 ms; the median fixture p95 is 12.47–13.09 ms.
The slowest scans deliberately pay decompression costs to keep the index small.
These are query-engine timings including catalog lookup and reconstructed paths,
excluding IPC and the application's 50 ms typing debounce. Startup validation
and mapping take 3.30–3.50 seconds in the three runs, with warm filesystem caches.

An initial prototype retained a temporary validation allocation in one of three
runs (71.5 MiB RSS versus about 48 MiB). Releasing allocator caches after validation
removed that variation in the three final runs. This remains a macOS measurement;
Windows and Linux are compile-checked, not runtime-qualified here.

The full-record, ranked-name, file-posting, metadata-posting, literal-rank,
substring-recall and fuzzy comparison against the original FM corpus passes. All 274 regular tests pass,
including compressed text crossing page boundaries, non-UTF-8 native bytes,
Unicode search, run cardinality/overflow rejection and old-format migration.
The explicit 10,000-file churn/IPC test also passes (50 concurrent queries,
20.712 ms p95 on that smaller fixture). The binary spool and WAL/recovery
generation policy remain unchanged.

A rebuild from the compressed format takes **24.53 seconds** and produces a
byte-for-byte identical segment, checked against the first format-4 build. This
uses the isolated production worker and retains the old view during construction.
The first conversion measured 53.41 seconds while test compilation overlapped;
that initial timing is not an isolated performance result.

Final query measurements are in `target/index-v4-validated-mapped-{1,2,3}.jsonl`.
The format-4 corpus and verification outputs are `target/index-v4-final.fx4` and
`target/index-v4-final-verify.jsonl`. Rebuild and churn outputs are
`target/index-v4-rebuild.jsonl` and `target/index-v4-churn.txt`. The benchmark
binaries use the isolated
`target/index-v4-build/` directory; the running application's release binary and
live index were not replaced. Existing live generations adopt format 4 after a
rebuild with the updated daemon.

## Earlier format-3 results, September 26–27, 2026

The saved corpus contains 2,129,454 entries including its root and 702,708 unique
folded names. The previous-format segment and the new segment contain identical
records and postings; all 37 fixture queries pass the rank, recall and fuzzy
comparisons described above.

| Measurement | Previous format | Compact format |
|---|---:|---:|
| One base segment | 315.44 MiB | 107.84 MiB |
| Two equally sized retained bases, before updates | ~630.89 MiB | ~215.69 MiB |
| Scratch spool record payloads, excluding the small header | 417.95 MiB JSONL | 176.06 MiB binary |
| Warm owner before 10,000 additions | 137.59 MiB | 61.4 MiB |
| Owner after the additions | 152.62 MiB | 71.3 MiB |
| Owner after first compaction and old-view release | 158.44 MiB | 81.5 MiB |
| Owner after second compaction and old-view release | 150.80 MiB | 77.5 MiB |
| Owner high-water mark | 535.19 MiB | 217.0 MiB |
| Sampled combined owner + worker peak | 1.28 GiB | 575.2 MiB |
| First / second isolated compaction | 26.71 / 26.28 s | 23.77 / 23.22 s |

Disk and spool reductions are 65.8% and 57.9%, respectively. Storage counts are
exact for this corpus. The old lifecycle figures are the September 12–13 baseline
in `index-v2-progress.md`; the compact probe repeats its 37-query warmup,
10,000-addition workload and two compactions without restarting the owner. This
is a storage/query process probe, not a complete live-volume daemon test. The
combined peak is sampled every 100 ms and may miss brief peaks; the owner
high-water mark is separately obtained with `getrusage`.

Initial single-signature prototypes passed top-100 tests but exceeded the 10 ms
target when the daemon requested 202 candidates. Separate boundary signatures,
intersection with full-query signatures and two hashes per gram addressed that
case. Ranked duplicate elimination also uses a set instead of quadratic scans.
Three final warm query-process runs, each with 50 samples per fixture query,
reported worst per-query p95 values of **5.752, 5.432 and 5.251 ms**. These timings
include catalog reads and reconstructed paths, exclude IPC/UI debounce, and meet
the former 10 ms target (now removed). The historical FM query-engine result was 0.375 ms: the
compact representation deliberately trades some latency for space. Warm query
RSS was 61.5–61.6 MiB across the three runs.
The final 10,000-file churn/IPC test passed with 208 queries and 6.649 ms p95; this is
the existing 10,000-file fixture, not a two-million-file live watcher test.

Raw outputs: `target/index-v3-final-{build,verify}.jsonl`,
`target/index-v3-final-mapped-*.jsonl`, `target/index-v3-churn.txt` and
`target/index-v3-lifecycle/compact-*`. The lifecycle runner and process sampler
are retained in `target/index-v3-lifecycle/{src/main.rs,monitor.py}`. All outputs
contain aggregate metrics; the underlying private corpus remains untracked.

Final checks include 269 regular library/integration tests, the explicit churn test,
all-target compilation, and Windows/Linux library cross-checks. Windows checking
uses the installed LLVM resource compiler on `PATH`. Clippy passes with warnings
denied except for two pre-existing lint classes introduced by the current
toolchain (`manual_is_multiple_of` and `question_mark`); unrelated source files
were not changed to address those warnings. Runtime/installer qualification on
Windows and Linux remains separate from cross-compilation.

The final rebuild enumeration/spool cleanup also produced a byte-for-byte
identical real-corpus segment (`target/index-v3-validated.fx3`), compared with
the compact segment that passed full record, posting and query verification.

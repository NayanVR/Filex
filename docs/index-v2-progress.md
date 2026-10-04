# Index v2 implementation and validation

The later compact-format implementation and validation are described in
[index-compact.md](index-compact.md). The measurements below remain the historical
FM-format baseline.

The application and daemon now use v2. The v1 runtime, snapshot reader, local UI
indexes, and global scan fallback have been removed. The revised contract is
[design-index-v2.md](design-index-v2.md), especially §19. These are local results
from macOS/arm64 on 2026-09-10–13; this document does not certify a release on
platforms that have only been cross-compiled.

## Implemented behavior

- A deduplicated, mapped catalog with monotonic file IDs, native identities,
  raw-name child lookup, parent links, cold metadata columns, and compressed
  extension/kind/size/mtime postings. Native names round-trip without lossy path
  conversion through IPC or persistence.
- NFC/full non-Turkic normalization version 1, wavelet BWT FM lookup, ranked
  distinct exact/prefix/boundary/substring retrieval, bounded acronym/typo
  candidates, and a 512-entry recently opened overlay. Packed suffix lookup and
  exhaustive ranking oracles compile only for tests or `index-v2-lab`.
- Cardinality-based filter/text choice, bounded small-scope enumeration, and
  projected catalog reads. Name-only search does not read size/mtime columns.
  Updated names are ranked separately from stale immutable postings. A bounded
  heap materializes paths only for overlay candidates that can enter the page.
- One catalog writer and builder, checksummed WAL before visibility, immutable
  view swaps, 25,000-entry / 16 MiB overlay thresholds, bounded queues and pending
  paths, coalesced hints/events, serialized compaction, and startup/overflow
  reconciliation. Enumeration spools records rather than retaining all paths.
- Authenticated per-user loopback IPC on macOS, Linux and Windows. Separate
  interactive and exhaustive workers prevent a slow stream from occupying the
  interactive worker. A file lock prevents multiple owners. The database is
  excluded before notification enqueue and during enumeration.
- UI daemon startup/reconnection, 100-row epoch-bound pages, explicit partial
  results, tag filtering before truncation, current-folder scope, and stable
  exhaustive Magic streams. Opens and mutations verify native identity; Magic
  preflights the plan and checks each operation again before applying it. Ordinary
  epoch changes do not repeatedly cancel a running Magic stream.
- Companion-daemon packaging for Linux/macOS and per-user Windows startup.
  Windows MSI no longer registers an elevated SCM endpoint. Windows update
  discovery stays available in the UI, with installation through the release page. Optional launchd and
  systemd user definitions are under `packaging/`.

## Persistence contract

`segment-<generation>.fx2` is an immutable little-endian container. The 64-byte
header begins with `FXSEG002` and stores the JSON metadata offset and length.
Numeric/text blocks start at 64-byte boundaries; metadata describes typed spans,
root records, sequence, search structures and normalization version. SHA-256 over
all preceding bytes is stored in the final 32 bytes. Load validates checksums,
spans, widths, dictionaries, parent references/cycles, posting references, and
rank structures. Fresh mappings replace validation mappings so validation does
not force cold columns to remain resident.

Numbered `manifest-<generation>.json` files contain a checksummed payload with
format version, sequence, next file ID, segment filename and roots. Files and the
manifest are synced before publication; directory syncing is used on Unix.
The latest two valid generations are retained. Older mappings remain alive for
query views. Startup rejects invalid generations and tries the preceding one;
unreferenced builder scratch files are cleaned up.

`updates.wal` contains `[u32 payload length][JSON transaction][SHA-256]` frames.
Transactions contain a sequence, next ID and identity-oriented deltas. Only a
torn trailing frame is trimmed. Complete corruption is quarantined and causes
reconciliation. Checkpointing atomically replaces the WAL with the tail required
by the oldest retained manifest. Failed builds retry and never load v1.

The local protocol uses version 2, an 8 MiB frame limit, and an endpoint capability
in the private database directory. Paths use reversible native-byte encoding.
Search defaults to 100 hits; a page is limited to 1,000 rows. Interactive base
retrieval examines at most 20,000 candidates and reports partial work limits.
Magic has a separate 256-row streaming protocol with scanned progress and a final
exact count. Portable notification roots always reconcile after restart; no
journal cursor or journal-exact recovery is claimed.

## Search measurements and earlier in-process construction

The private corpus contains 2,129,453 entries plus its root, with 702,708 distinct
folded names and 22,414,722 normalized text bytes. Synthetic fixtures contain no
private paths. Earlier measurements established NFC versus NFKC: NFKC saved only
157 bytes, so compatibility distinctions are preserved with NFC.

| Measurement | Local result |
|---|---:|
| 505,000 synthetic names, packed search including fuzzy dictionaries | 86.86 MiB |
| Synthetic literal workload, worst per-query p95 | 22.25 μs |
| Real names, selected FM search structures including fuzzy dictionaries | 135.18 MiB |
| Real names, FM saving against packed suffix search | 31.47% |
| Real names, FM range/top-100 comparison, worst per-query p95 | 46.29 μs |
| Complete persisted real-corpus segment | 315.44 MiB |
| Mapped catalog query, 100 rows with reconstructed paths, worst p95 | 0.375 ms |
| Mapped query process after workload | 149.92 MiB RSS |
| Real-corpus construction and save | 16.14 s |
| Construction process peak RSS | 1.03 GiB |
| Full compaction with the preceding mapped generation alive | 18.51 s |
| Compaction process peak RSS | 1.12 GiB |
| 10,000-file churn, selective-query IPC p95 | 4.35 ms |
| 10,000-file churn, broad-query IPC p95 | 4.72 ms |

Literal comparisons validate complete recall and exact top-100 agreement with an
exhaustive oracle outside the timed region. Mapped-query timings include catalog
filtering, ranking and path materialization; they use 50 samples per fixture
query, and report the worst per-query p95. They exclude IPC. The separate churn
runs include IPC and verify exhaustive convergence to all 10,000 created files.
They are not measurements of a two-million-file live watcher workload.

The mapped query process is not the complete daemon with watchers and active
compaction. Warm RSS is measured after the workload; peak RSS uses `/usr/bin/time
-l`. File-system page caches were not purged. Construction timings exclude OS
enumeration of a live volume. No claim is made about cold-device latency or UI
RSS. The original relative compaction-memory gate failed; §19 explicitly revises
it to a 1.5 GiB absolute build/compaction budget at this corpus size.

## Validation and reproduction

### Direct comparison with main's v1

Measured on 2026-09-11 against local `main` commit `8a76ec9`, using its
`benches/search_bench.rs` corpus: 2,000 directories and 200,000 files. The archived
main library and current v2 library were compiled in release mode without default
features, using their respective lockfile dependency versions. Both receive the
same queries and result limits, with no filters, scope, or recent-open history.
V1 uses `manager::search_all`; v2 uses `daemon::query::search` over a mapped
segment. Both materialize result paths. Timings exclude construction, IPC,
watchers, and UI debounce.

Each case has 20 warmups and 100 individually timed calls per process run. The
table reports the median of three run-level p95 values; version order alternates
between runs. All v2 pages completed without a partial-work flag. Matching hit
counts do not imply identical ranking, since v2 changes the ranking contract.

| Query | Limit | V1 p95 | V2 p95 | Speedup | Hits, v1 / v2 |
|---|---:|---:|---:|---:|---:|
| `e` | 100 | 2.016 ms | 0.124 ms | 16.27× | 100 / 100 |
| `re` | 100 | 1.577 ms | 0.104 ms | 15.22× | 100 / 100 |
| `report` | 100 | 1.244 ms | 0.101 ms | 12.32× | 100 / 100 |
| `invoice_010` | 100 | 0.928 ms | 0.116 ms | 8.00× | 100 / 100 |
| `e` | 500 | 3.091 ms | 0.687 ms | 4.50× | 500 / 500 |
| `re` | 500 | 2.593 ms | 0.617 ms | 4.21× | 500 / 500 |
| `report` | 500 | 1.908 ms | 0.614 ms | 3.11× | 500 / 500 |
| `invoice_010` | 500 | 0.972 ms | 0.113 ms | 8.63× | 100 / 100 |
| `invoice_0100_0` | 500 | 2.600 ms | 0.0145 ms | 178.81× | 10 / 10 |
| `zqxjw` | 500 | 2.173 ms | 0.00229 ms | 948.01× | 0 / 0 |
| `rpt` | 500 | 3.615 ms | 0.00217 ms | Not comparable | 500 / 0 |

Sparse and empty queries benefit especially from removal of v1's global fuzzy
scan. This changes recall: `rpt` is a subsequence of `report`, but is not one of
v2's bounded acronym/one-edit matches in this corpus. Its timing is not counted as
a speedup. These measurements supersede the earlier rough 72–117× estimate based
on benchmarks with different result limits. The 31.47% search-structure saving
above compares two v2 representations, **not v1 versus v2 memory**. No comparable
v1/v2 daemon RSS result has been established.

Local harnesses, the exact archived main source, per-run JSONL, and aggregate
`summary.json` are retained under `target/main-v2-comparison/` (ignored build
artifacts). The runner sources are `v1/src/main.rs` and `v2/src/main.rs` there;
`main-commit.txt` records the full baseline commit. Rebuild either with
`cargo build --locked --offline --release --manifest-path
target/main-v2-comparison/v1/Cargo.toml --target-dir target` (substitute `v2` for
the new version), then run `target/release/compare-v1` or `compare-v2`.

### Direct memory and disk comparison before process isolation

Measured on 2026-09-12 using the saved 2,129,453-entry corpus plus its root. The
original v1 snapshot was no longer present after the app's cutover. Main's own
`VolumeIndex` insertion and persistence APIs reconstructed a temporary v1 snapshot
from the saved record export, preserving names, tree relationships, native keys,
and known metadata. A separate process then loaded that snapshot for measurement.
V2 loaded the previously built segment from the same export. The live application
database was not modified by this measurement.

Both release-mode processes warmed the same 37-query fixture, 20 calls per query,
with 100-row limits and path construction. Each then added the same 10,000 names
in ten batches, publishing each batch and holding the previous query view through
publication. Batches were at least 150 ms apart. V1 used `SharedIndex::write` and
`publish`; v2 appended synced WAL transactions and published cloned overlays over
the mapped base. This exercises index storage/publication, without filesystem
enumeration, notification adapters, IPC, or UI overhead. These are isolated process
measurements, not complete daemon or application totals.

| Measurement | Main's v1 | V2 |
|---|---:|---:|
| One persisted base, before updates | 167.32 MiB | 315.44 MiB |
| Warm process RSS, before updates | 402.75 MiB | 135.69 MiB |
| RSS after first 1,000 additions, previous reader held | 805.39 MiB | 140.28 MiB |
| RSS after 10,000 additions and reader release | 1,924.27 MiB | 151.31 MiB |
| Peak through v1 snapshot save / v2 compaction publication | 2.04 GiB | 1.46 GiB |
| V2 RSS after compaction and old-view release | — | 1.16 GiB |

For this workload, warm RSS fell 66.3% and ordinary-update RSS fell 92.1%. V1
deep-copies its complete index on publication; RSS includes allocator-retained
pages and is not a count of live logical index copies. V2's 10,000-record overlay
had a 1.64 MiB internal size estimate and a 2.01 MiB WAL, while actual RSS grew
15.62 MiB. Overlay byte thresholds are estimates, not RSS caps.

**The pre-cleanup compaction path retained substantial memory.** Rebuilding the updated v2 segment
took 17.35 seconds. Construction/save reached 1.25 GiB RSS, and reopening and
publishing reached a process peak of 1.46 GiB. RSS remained 1.16 GiB two seconds
after dropping the preceding mapping and invoking the production allocator-cache
release operation. A `vmmap` summary showed approximately 814 MiB resident in
empty large-allocation regions and 170 MiB in empty small-allocation regions.
This suggests allocator retention rather than that amount of live catalog data;
it still consumes resident memory. A preceding eight-query run reproduced the
pattern (1.43 GiB peak, 1.09 GiB afterward). The 250 MiB warm target has therefore
**not been demonstrated after compaction in a continuing process**. The earlier
149.92 MiB fresh mapped-query result must not be presented as the daemon's memory
through its whole lifecycle. This finding motivated the worker isolation described below. The new measurements
cover repeated compaction in a continuing owner process; live-volume watcher and
startup qualification still need separate runs.

Disk space grows: one v2 base is 88.5% larger than the reconstructed v1 snapshot.
V2 retains two valid generations for recovery, so the base files occupy about
631 MiB once two similarly sized generations exist. During a later compaction,
two retained generations plus a new output need about 946 MiB, before WAL,
manifests, enumeration scratch, or extra generations retained by active readers.
The updated v2 segment measured 317.77 MiB. V1's updated snapshot was 168.13 MiB;
atomic replacement temporarily needs both its previous snapshot and new output.

RSS uses `ps`; peaks use macOS `getrusage` and `/usr/bin/time -l`. The inherited
environment had `MallocNanoZone=0`; neither harness enabled malloc debugging.
No page-cache purge or low-memory pressure was induced. Harnesses are
`target/main-v2-comparison/{v1,v2}/src/bin/memory.rs`; raw measurements are
`{v1,v2}-memory.jsonl`, resource summaries are `{v1,v2}-memory-resource.txt`, and
the allocator summary is `v2-memory-vmmap.txt` in that directory. All are ignored
local artifacts; outputs contain aggregate measurements.

### Cleanup and isolated construction, September 12–13

The production writer now uses a disposable segment worker. Recovery,
filesystem-change normalization and build coordination have separate modules;
the unused mutable-catalog/column/tree prototypes are removed. Named constants
cover record flags, fuzzy limits and writer thresholds. Persisted child/native
lookups must be ordered permutations, root lookups must be complete, endpoint
replacement is atomic, and malformed build input fails the whole build. The
search-field debounce remains **50 ms**.

The process worker sorts a private record spool and saves a durable output. The
owner waits for worker exit, validates the result, and publishes through the
existing manifest/WAL protocol. Cancellation kills and reaps the worker; loss of
the owner's lifetime pipe terminates it. Unit and integration tests cover these
boundaries. A maintainer-oriented code map and invariants are in
[index-v2-maintenance.md](index-v2-maintenance.md).

The same 2.13-million-entry, 37-query, 10,000-addition lifecycle probe now compacts
twice without restarting the owner. After each publication it drops the preceding
query view and measures RSS. The second cycle found no retained-memory growth.

| Measurement | Isolated-worker result |
|---|---:|
| Warm owner before changes | 137.59 MiB |
| Owner after 10,000 additions | 152.62 MiB |
| Owner after first compaction and old-view release | 158.44 MiB |
| Owner after second compaction and old-view release | 150.80 MiB |
| Owner process peak across the run (`getrusage`) | 535.19 MiB |
| Worker peak sampled every 100 ms | 1.04 GiB |
| Combined owner + worker peak sampled every 100 ms | 1.28 GiB |
| First / second isolated compaction | 26.71 s / 26.28 s |
| Latest 10,000-file churn IPC p95, 168 queries | 5.05 ms |

This fixes the measured post-compaction retention (previously 1.16 GiB). The
250 MiB target is met by this isolated owner **after** the old query view is
released; temporary validation/publication RSS is higher. The sampled combined
peak includes both processes rather than hiding worker memory. Sampling can miss
short peaks; the owner high-water mark is also recorded separately. This remains
a storage/query lifecycle probe, with no full live-volume watcher overhead.

The tradeoff is extra I/O and latency: the earlier in-process updated-corpus
compaction took 17.35 seconds. The isolated path also writes a temporary JSONL
spool in addition to the new segment. Recovery generations and the on-disk
`FXSEG002` layout are unchanged, so the earlier base-size comparison still holds;
temporary total disk usage must additionally allow for the record spool.

Raw results are retained in `target/main-v2-comparison/` as
`v2-isolated-repeat-memory.jsonl`, `v2-isolated-repeat-memory-resource.txt`,
`v2-isolated-repeat-process-tree.json` and `v2-isolated-repeat-memory-vmmap.txt`.
`monitor-repeat-memory.py` contains the process-tree sampler. The preceding
single-cycle isolated run is retained under the `v2-isolated-` prefix. No live
application database or settings were changed by these probes.

Local checks pass: the regular suite, explicit churn test, all-target compile,
strict Clippy for the production library, and Windows/Linux library cross-checks.
Cross-platform runtime and installer qualification remain release requirements.

### Implementation checks

Local coverage includes 241 library tests with the laboratory enabled, 42 app
tests, 10 regular v2 integration tests, 2 worker-process tests, 3 existing
integration tests, and the explicit 10,000-file churn test (298 regular tests). Checks cover raw path encoding, corruption/torn
WAL frames, checkpoint tails, mapped round trips, epoch expiry, ranked pagination
against exhaustive streams, overlay renames, tombstones, filtering, scoping,
identity rejection, database self-exclusion, corrupt-generation fallback, and
forced daemon-process termination with offline changes before restart.

```sh
cargo test --locked --no-default-features --features index-v2-lab
cargo test --locked --bin filex
cargo check --locked --all-targets --features index-v2-lab
cargo check --locked --no-default-features --target x86_64-pc-windows-msvc
cargo check --locked --no-default-features --target x86_64-unknown-linux-gnu
cargo bench --locked --no-default-features --features index-v2-lab --bench suffix_probe
cargo test --locked --release --no-default-features --features index-v2-lab \
  --test index_v2 ten_thousand_changes_converge_during_queries -- --ignored --nocapture
```

The feature-gated laboratory accepts an immutable v2 segment for `analyze`,
`literal`, `fm` and `mapped` modes. `compact INPUT OUTPUT` retains the earlier
in-process rebuild for comparisons. `compact-isolated INPUT OUTPUT` uses the
production worker and holds the old mapping through the build. `build-json RECORDS OUTPUT` builds an isolated corpus of
sorted catalog records rooted at `/corpus`; it is a measurement helper, not a
migration or production ingestion path. No v1 decoder is retained in the repo.

```sh
cargo run --release --locked --no-default-features --features index-v2-lab \
  --bin filex-index-lab -- mapped "$V2_SEGMENT"
cargo run --release --locked --no-default-features --features index-v2-lab \
  --bin filex-index-lab -- fm "$V2_SEGMENT"
```

## Release qualification still required

Windows and Linux library cross-checks pass; their runtime tests, GPUI builds and
MSI install/uninstall jobs are configured in CI and have not been executed by this
local session. A real version-upgrade MSI run, full live-volume startup/idle RSS,
permission restoration, sustained compaction latency, cold-device queries and
low-memory/large-corpus runs remain release checks. Native journal replay,
size-tiered partial compaction and battery/I/O scheduling are explicitly deferred
by the revised design. Root counts in the sidebar describe the last compacted
base and can lag live changes until compaction.

# Maintaining index v2

The daemon owns the index. The application sends queries and filesystem hints;
it does not keep another catalog. Search still waits **50 ms** after the last
edit before sending a request.

## Where the code lives

| Responsibility | Source |
|---|---|
| IPC, authentication, query queues and notification registration | `src/daemon/server.rs`, `ipc.rs` |
| Durable state changes and generation publication | `src/daemon/writer.rs` |
| Selecting a generation and replaying its WAL | `src/daemon/recovery.rs` |
| Turning filesystem paths into identity-preserving changes | `src/daemon/changes.rs` |
| Enumeration, record spooling and the isolated segment worker | `src/daemon/builder.rs` |
| Immutable base, delta segments and overlays visible to a query | `src/daemon/view.rs` |
| Ranked pages, filtering and exhaustive streams | `src/daemon/query.rs` |
| Temporary exhaustive candidate intersections | `src/search/candidates.rs`, `src/catalog/segment/mod.rs` |
| Catalog validation and mapped files | `src/catalog/segment/persist.rs`, `storage.rs` |
| Packed columns, compressed text pages and posting runs | `src/catalog/columns.rs`, `pool.rs`, `postings.rs` |
| Compact gram-block search and bounded fuzzy matches | `src/search/` |
| Search input, cancellation, pagination and Magic integration | `src/workspace/search.rs` |

There is one production catalog representation: `Segment`. It uses the
[compact layout](index-compact.md), the only layout readers accept. The earlier
mutable catalog, FM search structures, column prototype and DFS tree prototype
have been removed. The `index-v2-lab` feature keeps measurement tools, such as
block-size tuning.

## Updating and compacting

The writer appends and syncs a WAL transaction before publishing its changes.
Queries hold an immutable `View`, so a new publication does not change a running
query's records. An updated name is considered separately from its old base
posting. Interactive work limits produce an explicit partial result.

A generation is a base segment plus up to four **delta segments**. A delta
is an ordinary `Segment` holding only records changed since the level below it,
plus sorted tombstones for deleted IDs (`Segment::tombstones`, which is `Some`
only for a delta). A query reads an ID from the newest level that holds it:
overlays first, then deltas from newest to oldest, then the base
(`View::shadowed`). Every level is searched through its own index. A delta's
records may have parents in older levels, and validation allows for that.

`next_build` in `writer.rs` picks the next build:

- **Flush:** write the overlays as a new delta. This happens at 4,096 records
  or about 4 MiB. Below that, it waits until the overlay is 5 minutes old
  **and** no change has arrived for 30 s, or until it is 30 minutes old.
- **Delta merge:** if a flush would make a fifth delta, every delta is merged
  with the overlays into one delta instead. The cost depends on the deltas'
  size, not the base's.
- **Merge:** rebuild the base from every level once the deltas hold more than
  1/8 as many entries as the base, and at least 4,096.
- **Reconcile:** walk every root into a new base, as before.

One coordinator prepares a build at a time. Enumeration preserves existing
native identities where possible; a move does not require rewriting every
descendant's path.

**Decision (FIL-27, 2026-10):** the earlier rule, "any pending change and 30
seconds since the last build", rebuilt back to back on a real machine: a full
build took about 90 s and something always changed within 30 s. The daemon never
idled, and each cycle wrote a new ~50 MB segment.

**Decision (FIL-30, 2026-10):** FIL-27 fixed that loop by letting the overlay
grow for 30 minutes to 6 hours. Every query scans the overlay entry by entry,
so that cost grew with it: up to 7 ms per query at the 25,000-record limit.
Flushing into indexed deltas keeps the overlay small without rewriting the
base. `search_overlay_820k` in `benches/segment_bench.rs`, on an 820k-entry
base:

| Pending changes | Overlay | 1 delta | 4 deltas |
|---|---|---|---|
| 5,000, rare name | 1.1 ms | 7 µs | 16 µs |
| 5,000, common name | 1.7 ms | 0.5 ms | 0.9 ms |
| 25,000, rare name | 6.0 ms | 7 µs | 16 µs |
| 25,000, common name | 7.2 ms | 0.4 ms | 1.2 ms |

With no pending changes, a common-name query takes 0.37 ms. Most of the extra
cost of four deltas is path materialization, which checks each delta for every
ancestor. Each level's ranked pass skips a candidate that can't beat the page
it already has, before materializing its path.

The costs:

- A flush adds a file. A delta merge rewrites every delta, so under a steady
  trickle every fourth build rewrites the merged delta. That is bounded by
  1/8 of the base, against a whole-base rewrite before.
- Status file counts (`View::root_counts`) are recomputed once per
  publication from the base and deltas: one lookup per delta entry, plus each
  descendant of a deleted directory, since a delete tombstones only the
  directory. Pending overlay changes join the count at the next flush.
- The watcher queue holds 16,384 commands. At 256, any burst of file events
  (a checkout, an unzip) overflowed it, and an overflow forces a reconcile of
  every root. The queue is allocated up front, at about 40 bytes per slot.

A directory that the catalog has never seen, such as a `mkdir` or a tree moved
in from outside a root, is listed in place, up to 10,000 entries per batch
(`changes::new_trees`). It used to trigger a reconcile, which re-walks every
root. Reconcile is still used for watcher overflow, larger new trees,
unreadable directories and added or removed roots. During a reconcile, each walked entry
is looked up under the parent ID the walk already holds. The old code resolved
every path from the root, decoding name pages at each level, and that took most
of a rebuild's CPU.

A stopping daemon cancels an in-flight build immediately. The walk checks the
stop flag before every entry. The writer waits at most 2 s for the builder
before abandoning it, which is safe because only the writer publishes and
startup removes scratch files. Each published generation logs its build mode,
duration, record count and byte size at `info` level.

Construction runs in a short-lived `filex-indexd` worker process. The coordinator
spools a versioned JSON header and binary records into its private database directory.
The worker sorts `(file ID, file offset)` pairs, builds an immutable segment and
syncs it. Exiting the worker releases its construction allocations, including
allocator caches that previously remained resident in the long-lived daemon.

The owner holds the worker's stdin pipe open as a lifetime signal. Normal
cancellation kills and reaps the child; owner death closes the pipe and the child
exits. Failed builds leave the current generation available. Scratch files are
removed on failure and startup. Worker stderr is captured locally, with a bounded
error excerpt returned to the writer.

After a successful build, the owner validates the segment's checksum, columns,
lookup ordering, tree and root references. Only then can the writer durably
publish a manifest and expose a new query epoch. Manifest version 3 lists a
generation's deltas. Only the current manifest and worker-request versions are
read; an index written by an older build is rebuilt by a reconcile. Two valid manifests are kept for recovery. A segment file is deleted
only when neither manifest references it and no live query view maps it
(`retire`), because consecutive generations share their base.

Keep these boundaries when changing the implementation:

- The worker never edits manifests, the WAL, settings or the live endpoint.
- File IDs are monotonic and are never reused after deletion.
- Mapped generation files stay immutable for the lifetime of their readers.
- A malformed record fails the build; it is never silently omitted.
- Changing the persisted layout or normalization requires an explicit format
  decision. Worker-request versioning is separate from segment and IPC versions.
- Measure both owner and worker memory. A small owner process does not imply a
  small combined peak while construction runs.

## Local checks

Build both binaries when testing changes to the daemon/worker protocol. An
already running daemon continues to use its previous executable until restarted.

```sh
cargo build --locked --release --no-default-features --bin filex-indexd
cargo run --locked --release --bin filex
```

The regular suite includes actual child-process construction, owner-disconnect
handling, daemon crash recovery, corruption checks and ranked pagination:

```sh
cargo test --locked --features index-v2-lab
cargo check --locked --all-targets --features index-v2-lab
cargo clippy --locked --no-default-features --lib --features index-v2-lab -- -D warnings
cargo test --locked --release --no-default-features --features index-v2-lab \
  --test index_v2 ten_thousand_changes_converge_during_queries -- --ignored --nocapture
```

`filex-index-lab compact-isolated INPUT OUTPUT` uses the production worker path;
`compact` retains the in-process path for measurement comparisons only. Synthetic
fixtures belong in `tests/fixtures/index-v2`. Private corpus exports and local
benchmark results stay under ignored `target/` paths.

See [the validation report](index-v2-progress.md) for measured resource costs and
platform release checks that still require execution.

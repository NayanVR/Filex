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
| Immutable base plus changed records visible to a query | `src/daemon/view.rs` |
| Ranked pages, filtering and exhaustive streams | `src/daemon/query.rs` |
| Temporary exhaustive candidate intersections | `src/search/candidates.rs`, `src/catalog/segment.rs` |
| Catalog validation and mapped files | `src/catalog/segment.rs`, `storage.rs` |
| Packed columns, compressed text pages and posting runs | `src/catalog/columns.rs`, `pool.rs`, `postings.rs` |
| Compact gram-block search, legacy FM readers and bounded fuzzy matches | `src/search/` |
| Search input, cancellation, pagination and Magic integration | `src/workspace/search.rs` |

There is one production catalog representation: `Segment`. New generations use
the [compact layout](index-compact.md); preceding FM generations remain readable.
The earlier mutable
catalog, separate column prototype and DFS tree prototype have been removed.
Alternate search representations remain under the `index-v2-lab` feature.

## Updating and compacting

The writer appends and syncs a WAL transaction before publishing its changes.
Queries hold an immutable `View`, so a new publication does not change a running
query's records. An updated name is considered separately from its old base
posting. Interactive work limits produce an explicit partial result.

The writer freezes changes at 25,000 records or an estimated 16 MiB, or after
30 seconds when there are pending changes. One coordinator prepares a build at a
time. Enumeration preserves existing native identities where possible; a move
does not require rewriting every descendant's path.

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
publish a manifest and expose a new query epoch. Two valid generations are kept
for recovery; older mappings stay alive while query views reference them.

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

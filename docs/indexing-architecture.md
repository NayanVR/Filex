# filex — Volume Indexing Architecture (Phase 1)

Goal: instant (sub-10ms perceived) filename/path search across whole volumes at
millions-of-files scale, with live incremental updates, never blocking the UI thread.

This covers three layers:

1. **Bootstrap indexer** — builds the full index per volume (per-OS strategy)
2. **Live watcher** — feeds incremental updates into the index (per-OS strategy)
3. **Index core** — OS-agnostic in-memory structure + query engine + persistence

---

## 1. Bootstrap indexing per OS

### Windows — USN/MFT enumeration (the "Everything" approach)

Do **not** parse the raw `$MFT` bytes off the volume. The supported, equally fast
route is the USN enumeration ioctl, which walks the MFT for you:

- Open the volume: `CreateFileW("\\\\.\\C:", GENERIC_READ, FILE_SHARE_READ|WRITE, ...)`
- Enumerate every file record: `DeviceIoControl(FSCTL_ENUM_USN_DATA, MFT_ENUM_DATA_V1)`
  in a loop, reading batches of `USN_RECORD_V2/V3`.
- Each record gives: file name, **FileReferenceNumber (FRN)**, **ParentFileReferenceNumber**,
  and attribute flags (directory bit). That's the entire tree in one flat stream —
  a full C: drive with ~2M files enumerates in ~1–3 seconds.
- Build the tree by linking FRN → parent FRN. Full paths are *not* stored; they're
  reconstructed by walking parent links (see §3).

**Privilege requirements (flagging as requested):**

- Opening a volume handle for `FSCTL_ENUM_USN_DATA` / reading the USN journal
  requires **elevation** (Administrator) or the `SE_BACKUP_NAME`/Backup Operators
  privilege. There is no way around this for whole-volume enumeration.
- Everything solves this with a **split-process design**: a small Windows service
  runs elevated and owns volume handles + the index writer; the UI runs unelevated
  and talks to it over a named pipe. We should plan the same shape eventually, but
  for Phase 1 it is fine to just relaunch the whole app elevated (manifest
  `requireAdministrator` or on-demand `runas`) and note the service split as a
  known follow-up. The index-core API should be process-splittable from day one
  (i.e., updates flow through a channel, not direct method calls).
- NTFS only. FAT/exFAT/network volumes fall back to the generic recursive scan below.

Crate: `windows` (windows-rs) exposes all the ioctls and structs directly.

### Linux — parallel filesystem walk (no MFT equivalent)

Research conclusion: there is **no user-accessible per-volume file table** on Linux.
ext4's inode table isn't exposed; `plocate`/`mlocate` also just walk the tree.
So the bootstrap is a maximally parallel recursive scan:

- Parallel directory walker over each mount point (the `jwalk` crate, or the
  `ignore` crate's parallel walker — both saturate SSDs with N threads).
- Use `getdents64` results only (name + `d_type`) — **do not `stat` every file**
  during bootstrap. `d_type` gives dir-vs-file on ext4/btrfs/xfs; sizes/mtimes are
  fetched lazily when a result is displayed (the UI shows them for ~40 visible rows,
  not 2M).
- Skip pseudo-filesystems (`/proc`, `/sys`, `/dev`, `/run`) and other-device mounts
  by comparing `st_dev` at directory boundaries.
- Expected: ~1M files in low single-digit seconds on NVMe.

No special privileges needed for the user's own files; indexing other users' homes
or all of `/` wants root but is not a Phase 1 requirement.

### macOS — bulk enumeration, not Spotlight

Research conclusion: Spotlight's metadata store (`.store.db`) is private and
undocumented; querying via `NSMetadataQuery`/`MDQuery` is async, rate-limited, and
can't enumerate everything reliably. **Bypass Spotlight** and enumerate directly:

- Recursive walk using **`getattrlistbulk(2)`** — Apple's bulk-metadata syscall
  (what `fts`/Spotlight's own importer use). It returns name + object type + any
  requested attrs for many entries per syscall, dramatically fewer syscalls than
  `readdir`+`stat`. On APFS this is the fastest supported enumeration path.
- Same parallel-walker structure as Linux (share the generic walker; the
  `getattrlistbulk` fast path is a cfg(target_os = "macos") specialization).
- Skip firmlinks/`/System/Volumes/Data` double-visits (track `st_dev` + visited
  device/inode pairs at mount boundaries).

**Privilege requirements:** no elevation, but **Full Disk Access** (TCC) is needed
to see `~/Library/Mail`, Messages, other users' dirs, etc. Without it the walk
silently gets permission errors — surface a "grant Full Disk Access" onboarding
prompt keyed off hitting `EPERM` in known-protected paths.

### Generic fallback (all OSes)

Network shares, FAT volumes, unindexed roots: the same parallel walker, capped and
on-demand. The index core doesn't care which backend produced entries.

---

## 2. Live update layer per OS

Design rule: watcher threads **never touch the index directly**. Each OS watcher
normalizes native events into a common enum and sends batches over a channel:

```rust
enum FsDelta {
    Created { parent: EntryKey, name: OsString, is_dir: bool },
    Removed { key: EntryKey },
    Renamed { key: EntryKey, new_parent: EntryKey, new_name: OsString },
    Rescan  { root: PathBuf },          // watcher overflowed; reconcile subtree
}
```

A single **index-writer thread** drains the channel, applies deltas in batches
(debounced ~50ms), then publishes a new index snapshot (§3). The UI is only ever
notified "index generation changed"; it never blocks.

### Windows — USN Journal (the good one)

- `FSCTL_READ_USN_JOURNAL` on the same volume handle, blocking-read loop on a
  dedicated thread. Events carry FRN + parent FRN + reason flags (create, delete,
  rename old/new) — they map 1:1 onto `FsDelta` with **no path lookup needed**.
- The journal is **persistent**: store `(UsnJournalID, NextUsn)` with the on-disk
  index snapshot. On startup, if the journal ID matches and the USN range is still
  present, replay only the gap — startup catch-up in milliseconds without a rescan.
  If the journal was truncated/recreated: full re-enumerate (it's ~seconds anyway).

### macOS — FSEvents with file-level events

- One `FSEventStream` on the volume root with `kFSEventStreamCreateFlagFileEvents`,
  scheduled on its own thread's runloop (or a dispatch queue).
- FSEvents also has **persistent event IDs**: store the last seen
  `FSEventStreamEventId` in the snapshot and create the stream with
  `sinceWhen = saved_id` on startup — same replay-the-gap trick as USN. If the
  event database was purged (`kFSEventStreamEventIdSinceNow` mismatch /
  `MustScanSubDirs` at root), rescan.
- Caveats: FSEvents can coalesce (flag `MustScanSubDirs` on a dir → send
  `Rescan { root }`), and rename events arrive as pairs that need matching by
  event ID adjacency + inode check. Handle both in the normalizer.

### Linux — fanotify when possible, inotify budgeted fallback

- **Preferred: `fanotify`** with `FAN_MARK_FILESYSTEM` + `FAN_CREATE | FAN_DELETE |
  FAN_MOVED_FROM | FAN_MOVED_TO | FAN_REPORT_FID | FAN_REPORT_DFID_NAME`
  (kernel ≥ 5.9): one mark covers the entire filesystem, events carry parent
  file-handle + name. Requires **`CAP_SYS_ADMIN`** — available if the user installs
  a privileged helper or runs a system service; detect and use when present.
- **Fallback: `inotify`**, which is per-directory — watching a whole volume means
  one watch per directory. Budget it: watch the N most-recently-visited /
  hottest directories (LRU up to ~512k watches, bounded by
  `fs.inotify.max_user_watches`), and run a **periodic low-priority reconcile walk**
  (compare mtimes of directories against the index) to catch changes in unwatched
  regions. This mirrors what most Linux indexers (tracker, recoll) end up doing.
- No persistent journal exists on Linux — after restart, the reconcile walk is the
  catch-up mechanism (it's a cheap dir-mtime-only walk, much faster than the
  bootstrap scan).

---

## 3. Index core (OS-agnostic)

### In-memory layout

The Everything-style layout, optimized for scan speed and small footprint:

```rust
struct EntryId(u32);                    // index into `entries`

struct FileEntry {
    name: NameRef,                      // (u32 offset, u16 len) into name_pool
    name_lower: NameRef,                // same, into folded pool (see below)
    parent: EntryId,                    // root points to itself
    flags: EntryFlags,                  // dir bit, hidden, symlink, tombstone
    native_key: u64,                    // FRN (win) / inode (unix) → delta lookups
}

struct VolumeIndex {
    entries: Vec<FileEntry>,            // arena; EntryId = position, never moves
    name_pool: Vec<u8>,                 // all names, concatenated (append-only)
    name_pool_lower: Vec<u8>,           // case-folded copy for search
    by_native_key: HashMap<u64, EntryId>, // FRN/inode → entry (delta application)
    children: HashMap<EntryId, Vec<EntryId>>, // only for browse-mode listing
    generation: u64,
}
```

- **No full paths stored.** A path is materialized by chasing `parent` links —
  only done for the ≤100 results actually shown. This is the single biggest
  memory win: ~60–80 bytes/entry total → **2M files ≈ 150–200 MB worst case,
  realistically ~100 MB** including the hash maps.
- Deletes mark a tombstone flag (arena slots are recycled via a free list);
  renames rewrite `name`/`parent` in place. `generation` bumps on every batch.

### Query engine: parallel linear scan (yes, really)

Everything's core insight: at this scale, **substring search doesn't need an
index**. `name_pool_lower` for 2M files is ~40 MB of contiguous bytes.
`memchr::memmem` scans at multiple GB/s per core; chunked across a rayon pool,
a full scan is **~5–10 ms**, well under a keystroke. Structure:

- Query thread pool (separate from the writer). Each keystroke submits a query
  tagged with the current `generation` and an `AtomicBool` cancel token; a newer
  keystroke cancels in-flight scans (checked per chunk).
- Case-insensitive by scanning the folded pool with a folded needle; the match
  hit index maps back to `EntryId` via a sorted `(pool_offset → EntryId)` lookup
  (binary search over entry order, since the pool is append-ordered).
- **Path-component queries** (`src/ main`) filter candidates by walking parent
  links only for name-matches — cheap because name matching already cut the set.
- Ranking, computed only on matches: prefix match > word-boundary > substring;
  then dirs/files, then name length. Truncate to top ~1000 for the UI.
- Later (not Phase 1): a trigram bitmap in front of the scan if profiling says we
  need it. Don't build it speculatively.

### Concurrency model

```
UI thread (GPUI) ──query──▶ search pool ──top-K results──▶ cx via channel
                                 │ reads
                                 ▼
                        VolumeIndex snapshot (ArcSwap)
                                 ▲ publishes
                                 │
 watcher threads ──FsDelta──▶ writer thread ◀── bootstrap scanner threads
```

- Readers get a consistent snapshot via `arc_swap::ArcSwap<VolumeIndex>`; the
  writer applies a batch to its private copy-on-write head and publishes.
  (Phase 1 can start with `RwLock` + short write batches; swap in `ArcSwap` or an
  im-style persistent structure if writer stalls show up. The interface —
  `index.snapshot()` — hides the choice.)
- GPUI integration: queries are spawned with `cx.background_executor()`; results
  come back through `cx.spawn` and set state + `cx.notify()`. **The UI thread
  never takes an index lock.**

### On-disk persistence

- Snapshot file per volume in the platform data dir: header (format version,
  volume identity, journal checkpoint = `UsnJournalID+NextUsn` / FSEvents ID /
  Linux walk timestamp) + the arenas dumped near-verbatim (they're already
  flat `Vec<u8>`/`Vec<FileEntry>` — serialization is essentially `write()`).
- Startup: mmap/read snapshot → replay journal gap (Win/mac) or start reconcile
  walk (Linux) → UI is searchable immediately, converging within seconds.
- Snapshot rewritten on clean shutdown + every N minutes from the writer thread.

---

## Phase-1 build order

1. Index core + generic parallel walker (works on all OSes, no privileges) —
   gets search working end-to-end behind a trait.
2. macOS FSEvents watcher (dev machine is macOS).
3. Windows USN enumeration + journal watcher (elevated single-process for now).
4. Linux inotify-budget watcher + reconcile walk; fanotify as an upgrade.
5. Persistence + journal-checkpoint catch-up.

Key crates: `windows`, `fsevent-sys`/`core-foundation`, `inotify`, `nix`
(fanotify), `jwalk` or `ignore` (walker), `memchr`, `rayon`, `arc-swap`.

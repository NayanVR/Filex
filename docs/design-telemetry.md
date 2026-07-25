# Design: Telemetry — crash reporting (Phase 2c)

Status: **design, settled 2026-07-25.** Implements the "Panic-hook crash
logs + user-initiated report sharing" layer of `docs/roadmap.md` Phase 2c.
The remote-metrics layer (search-latency/index-size aggregates) stays
deferred per the roadmap and is out of scope here.

## Goal & scope

Get crash reports to the developer with **zero per-crash user friction**,
without betraying the project's privacy stance (`logging.rs`: "filenames
are private, nothing leaves the machine"). The tension is resolved by:

- **One-time opt-in, then silent** (confirmed 2026-07-25). The user answers
  one prompt ever; after that every crash is captured *and* uploaded in the
  background with no further interaction.
- **Scrubbing** every report of path-shaped data before it can be queued
  for upload — the privacy backstop that lets crash data leave the machine
  at all.

Non-goals: remote metrics/aggregates, session/usage analytics, any
always-on data collection. Only crashes, only with consent, only scrubbed.

## Privacy invariants (non-negotiable)

- **Nothing is uploaded without `crash_reports == Some(true)`.** Unset (not
  yet asked) and `Some(false)` never transmit. Crashes are still captured
  *locally* regardless (they never leave the machine without consent).
- **Never captured:** the search query string, index contents, tag names,
  recents, or any browsed path. Crash reports carry only: timestamp, app
  version, OS + arch, thread name, panic message, backtrace.
- **Always scrubbed before queueing:** the user's home directory → `~`, and
  absolute path-shaped substrings (Unix `/a/b…`, Windows `X:\a\b…`) →
  `<path>`. Over-scrubbing is acceptable — losing a file:line beats leaking
  a path. Symbol names (`filex::index::…`, `::`-separated) are kept.
- **No endpoint shipped by default.** The upload URL is an empty-by-default
  config constant; an empty URL disables upload entirely, so a build with
  no endpoint set transmits nothing even with consent.

## The pipeline

```
 panic ──▶ capture ──▶ scrub ──▶ <data_local_dir>/filex/crash-reports/<ts>.json (queue)
                                          │
 next launch (UI), if consent ──▶ drain ─┘──▶ POST endpoint ──▶ delete on 2xx
```

1. **Capture** (`telemetry::install_panic_hook`): a chained
   `std::panic::set_hook` builds a `CrashReport`, scrubs it, and writes it
   **synchronously** to the queue dir (a `crash-<unixmillis>.json` file), so
   it survives an abort even if the async log worker never flushes. Also
   mirrors to `tracing::error!` best-effort. Installed in **both** `filex`
   and `filex-indexd` (the service runs unattended — its panics matter
   most).
2. **Scrub** (`telemetry::scrub`): pure, unit-tested; applied to the panic
   message and backtrace at capture time, so the queued file is already
   clean (defence in depth — even a leaked queue file has no paths).
3. **Consent**: `Settings.crash_reports: Option<bool>`. `None` until asked;
   a one-time first-run prompt in the UI sets it. The prompt is
   non-blocking and dismissible (dismiss = ask again next launch, since
   `None` is preserved until an explicit choice).
4. **Upload** (UI process only): on launch, if `Some(true)`, a background
   task lists the queue dir and POSTs each report to the endpoint; a `2xx`
   deletes the file (at-least-once; an offline/failed upload just stays
   queued for next launch). Never on the UI thread. The service never
   uploads (no consent context) — it only enqueues; the UI drains the
   shared queue dir.

## Transport (confirmed 2026-07-25: custom POST, configurable endpoint)

- A minimal HTTP `POST <endpoint>` of the report JSON (`Content-Type:
  application/json`), via a light blocking client (`ureq`) on a background
  thread. No third-party crash SaaS — data goes only to infrastructure the
  owner controls, matching the app's lean, self-hosted ethos.
- Endpoint is a single constant sourced from the `FILEX_CRASH_ENDPOINT`
  build/runtime env (empty ⇒ disabled). Standing up a receiver is a trivial
  "accept JSON, append to storage" service; deferred until wanted, and the
  client is a no-op until the URL is set.
- A bounded queue (cap the number of retained crash files, drop oldest) so
  a crash-looping install can't fill the disk.

## What gets captured (the `CrashReport`)

```rust
struct CrashReport {
    schema: u32,          // format version
    app: &str,            // "filex" | "filex-indexd"
    version: &str,        // CARGO_PKG_VERSION
    os: &str, arch: &str, // std::env::consts
    unix_millis: u128,
    thread: String,       // thread name, or "unnamed"
    message: String,      // scrubbed panic message
    location: Option<String>, // file:line of the panic site (scrubbed)
    backtrace: String,    // scrubbed
}
```

Serialized with serde_json. `schema` lets a receiver tolerate future
additions.

## Testing plan

- Pure/portable: `scrub` (home dir, Unix/Windows path shapes, that symbol
  names survive), `CrashReport` (de)serialization, queue read/write/prune —
  plain `#[test]`, no panic, no network.
- Panic hook: a `#[test]` that installs the hook, triggers a panic in a
  `catch_unwind`, and asserts a scrubbed report file lands in a temp queue
  dir.
- Upload: the drain/delete-on-success logic tested against a fake transport
  (a trait/closure), so no real network in tests. The endpoint being empty
  ⇒ upload is a no-op is asserted.
- No perf concern: capture is off the hot path (only on panic); upload is
  once per launch on a background thread.

## Phasing

1. `telemetry` module: `scrub` + `CrashReport` + the local queue
   (write/list/prune), pure and tested. No UI, no network.
2. `install_panic_hook`, wired into both binaries.
3. `Settings.crash_reports` + the one-time first-run consent prompt (UI).
4. Background uploader (`ureq` POST, `FILEX_CRASH_ENDPOINT`), drained on
   launch when consented; fake-transport tests.

## Confirmed decisions (2026-07-25)

- **Consent: one-time opt-in, then silent.** Not silent-without-consent
  (would exfiltrate end-user data with no consent) and not local-only
  (the developer wants reports to actually arrive).
- **Transport: custom POST to an owner-controlled endpoint**, configurable
  via `FILEX_CRASH_ENDPOINT`, empty-by-default (disabled). Not Sentry (no
  third-party crash cloud / heavy dep).
- **Remote metrics/aggregates stay deferred** (roadmap) — this pass is
  crash reporting only.

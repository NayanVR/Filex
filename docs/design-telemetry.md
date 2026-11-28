# Design: Telemetry — product analytics and error tracking (Phase 2c)

Status: **PostHog, built 2026-11-28 (not yet in production).** Replaces Sentry entirely (crash
reporting, error events, measurements, release health and feedback).
History is at the end.

## Goal

Learn what to improve without collecting anything personal:

- **Error tracking:** every panic (UI and daemon) and every `error!` log,
  grouped into issues, with stack frames.
- **Product analytics:** which features are used and how (navigation,
  search, Magic, file ops, menus, shortcuts, settings, onboarding, updates),
  plus funnels and retention per anonymous install.
- **Performance from real machines:** folder load time, search latency,
  startup time, memory, index size.

## Privacy invariants (non-negotiable)

1. **No PII, enforced by types.** Every product event is a struct declared
   in `catalog.rs`, and `track` accepts nothing else. Field types must
   implement `schema::Field`, which exists only for numbers, bools,
   durations, `&'static str` ids and closed label enums (plus lists,
   tallies and `Option`s of those). A `String` or `PathBuf` field does not
   compile, and a `compile_fail` doctest proves it. So a file name, path,
   query, tag, folder name, URL or anything the user typed *cannot* become
   a property. Lists (roots, favorites, excluded names, icon choices,
   shortcuts) are reported as **counts**.
2. **Free text is scrubbed twice.** Only three things carry text: panic
   messages, `error!` log messages and feedback the user chose to send.
   They go through `telemetry::scrub` (absolute paths, home dir) at capture,
   then `analytics::exception::scrub_text`, which also removes quoted strings,
   bare file names (`taxes.pdf`), relative paths, emails and URLs.
   Other `tracing` fields (`path = %p.display()`) are never read.
3. **Anonymous.** Events are sent with `process_person_profile: false`, so
   PostHog keeps no person profile. The `distinct_id` is a random UUIDv4
   install ID (`<data_local_dir>/filex/analytics-id`). It is never derived
   from the machine, user or hardware, and it is **deleted when the user
   turns sharing off**, so opting back in is a new, unlinked identity. A
   per-process random `session_id` groups one run.
4. **No location.** `$geoip_disable: true` on every event. The PostHog
   project **must** also have *Settings → Project → "Discard client IP
   data"* enabled, so the request IP is not stored at all (owner setup, below).
5. **No email.** The feedback dialog no longer asks for one, and feedback
   text is scrubbed of emails.
6. **Gated three ways.** Nothing is sent unless the build has the
   `analytics` feature **and** a project key (`FILEX_POSTHOG_KEY`, baked by
   release CI) **and** `share_diagnostics` is on. Without the feature, there
   is no HTTP transport in the binary.
7. **The daemon and updater link no telemetry transport.** They are built
   without `analytics`. The daemon still *queues* its own panics to disk
   (pure code in `telemetry.rs`), and the UI uploads them.

## Consent

`Settings.share_diagnostics` (unchanged field name, so existing settings
files keep their choice). It is **on by default (opt-out)**, as it was for
Sentry. The Windows MSI seeds it from its Privacy page
(`FILEX_CRASH_REPORTS=0` opts out unattended). Settings → Privacy → "Share
anonymous usage data" turns it off, and the change applies immediately:
collection stops, the buffer is dropped and the install ID is deleted.
Turning it on starts analytics live (off the UI thread) and drains queued
crashes. Pressing *Send* in the feedback dialog is explicit consent for
that one message, even with sharing off. It then goes under a throwaway ID.

> **Open item for the owner:** the opt-out default now covers usage
> analytics, not just crash reports. It's defensible because nothing
> personal is collected (above), but it is a wider scope than what users
> agreed to before. Consider a one-time notice for existing installs, or
> switching usage events to opt-in while keeping crashes opt-out.

## Architecture

```
 UI code ── fa::track(ev::FolderOpened { .. }) ──▶ mpsc ──▶ worker "filex-analytics"
   (typed struct from catalog.rs; channel send)            │ batch 20 / 30 s, retry w/ backoff
 tracing error! ── AnalyticsLayer ─▶ track_raw ────────────┤ ≤ 2000 per event name per session
                                                           │ buffer ≤ 1000 (oldest dropped)
 panic (UI or daemon) ─▶ telemetry queue (disk) ─▶ drain_crashes (next launch)
                                                           ▼
                         POST {host}/i/v1/analytics/events (ureq + rustls, Bearer key)
 quit ─▶ worker adds app_session_ended, writes unsent ─▶ analytics-spool.jsonl ─▶ next run
```

### The typed catalog

`src/diagnostics/analytics/catalog.rs` is the single source of truth:

```rust
labels! {
    /// How a folder was reached.
    pub enum NavVia { Up = "up", Back = "back", /* … */ }
}
events! {
    /// A folder's rows landed on screen.
    FolderOpened => "folder_opened" {
        via: NavVia,
        location: Location,
        /// Navigation to first paint.
        load_ms: Duration,
        entries: usize,
        entries_bucket: CountBucket,
    }
}
// call site:
fa::track(ev::FolderOpened { via, location, load_ms, entries, entries_bucket });
```

- `labels!` makes closed vocabularies. A typo is a compile error, and the
  allowed values appear in the docs.
- `events!` makes the struct, its `AnalyticsEvent` impl (wire name and
  properties in declaration order, `None` omitted) and a schema entry.
- **`docs/analytics-events.md` is generated** from the schema (doc comments
  included). The `catalog_doc_is_current` test fails when it is stale.
  Regenerate with `FILEX_BLESS=1 cargo test --lib catalog_doc_is_current`.
  Other tests check that names are unique snake_case and every event has a
  doc.
- `wire::SCHEMA_VERSION` (sent as `schema_version`) is bumped on a breaking
  change to an event's shape, so dashboards can split old and new data.
- Adding an event: declare it in `catalog.rs`, call `fa::track` at the
  site, bless the docs. Reviewers read one file to know what is sent.

### Delivery

- **Spool, not a blocking flush.** At quit the worker adds
  `app_session_ended`, writes unsent events to
  `<data_local_dir>/filex/analytics-spool.jsonl` (temp file plus rename) and
  stops. Quit waits at most 250 ms, for local disk only. The next run sends
  the spool first, so an offline session isn't lost either. Turning sharing
  off discards the buffer and deletes the spool and install ID. A launch
  without consent deletes them too.
- **Quota guard.** At most 2000 events of one name per session. The rest
  are counted in the summary's `dropped` tally, so a runaway loop shows up
  instead of burning the quota.
- **Retries.** 408/429/5xx and transport errors back off from 5 s to 5 min.
  Other 4xx responses drop the batch.

### Attribution

Navigation passes its cause explicitly: `navigate(path, NavVia::Sidebar…)`,
`load_dir_via(path, NavVia::Back)`. `load_listing` records
`(request, via)`, and only the latest request reports when it lands
(superseded loads never do). Plain `load_dir` (reloads after file ops) is
reported as `other` only if it lands on a different folder. This replaced a
mutable "next load's cause" field, which could leave a stale label behind
when `navigate` returned early for the current folder.

### Seeing and testing events

- `FILEX_ANALYTICS=log` logs every batch (`RUST_LOG=filex=info`) instead of
  sending it. No key, project or `analytics` feature needed.
  `FILEX_ANALYTICS=off` disables analytics.
- `RUST_LOG=filex::analytics_log=debug` also shows each event as it is queued.
- `analytics::testing::Capture::start()` captures events on the current
  thread. `is_enabled()` reports true while a capture is active, so tests
  can assert what a workflow emits (`capture.names()`, `capture.events()`).

### Files

| File | Role | Compiled |
|---|---|---|
| `catalog.rs` | **every event and vocabulary**, the generated-docs test | always |
| `schema.rs` | `Field`, `Label`, `Tally`, the `labels!`/`events!` macros, docs renderer | always |
| `event.rs` | internal wire form (`Event`, `Prop`); crate-private constructors | always |
| `wire.rs` | capture-v1 envelope, UUIDv7, super-properties, `SCHEMA_VERSION` | always |
| `exception.rs` | `$exception` from crashes and logs, `scrub_text`, backtrace parsing | always |
| `identity.rs` | install ID create/forget | always |
| `snapshot.rs` | settings → `settings_snapshot` and `setting_changed` | always |
| `pipeline.rs` | worker, batching, retry, caps, summaries, spool, `LogTransport` | always |
| `testing.rs` | thread-local `Capture` | always |
| `client.rs` | `ureq` transport, key/host config | `analytics` |
| `layer.rs` | `tracing` layer: `error!` → `$exception` | `analytics` |
| `mod.rs` | `init`, `track`, `count`, `shutdown`, `disable`, `drain_crashes`, `send_feedback` | always |
| `workspace/analytics.rs` | UI glue: launch, shortcuts, search-session accumulator, shared helpers | app |

**Why not the `posthog` crate:** it pulls in reqwest and tokio. filex already
ships ureq + rustls for updates. The wire format matches the official SDKs:
`posthog` 1.7 (`capture_event.rs`, `error_tracking.rs`, `endpoints.rs`) and
Python 8.4 (`capture_send.py`).

**Wire format:** `POST /i/v1/analytics/events`, Bearer auth,
`{created_at, batch: [{event, uuid (v7), distinct_id, timestamp, session_id,
options: {process_person_profile: false}, properties}]}`.

**Exceptions:** `$exception_list: [{type, value, mechanism: {type, handled,
synthetic}, stacktrace: {type: "raw", frames}}]`. Frames are `{function,
filename, lineno, lang: "rust", platform: "native", in_app, synthetic,
client_resolved: true}`, outermost first. `$exception_fingerprint` is an
FNV-1a hash of compile-time facts (panic location + innermost in-app
function, or the log callsite), never the message. Each log callsite
reports at most 5 times per process.

**Latency:** `track` and `count` are a struct build plus a channel send. The
per-keystroke cost on the search path is `chars().count()` and an `Option`
update. Folder navigation adds one send. HTTP only happens on the worker.

## Event catalog

The complete list is generated: **[analytics-events.md](analytics-events.md)**.
Super-properties on every event: `$lib` = `filex`, `$lib_version`, `$app_name`,
`$app_version`, `$os`, `arch`, `environment` (`debug`/`release`),
`$geoip_disable`, `schema_version`. Keyboard shortcuts are counted per action
name and reported in `usage_summary` / `app_session_ended`, not sent one by
one.

Not instrumented yet: the Open/Save dialog process (`filex --pick`) and the
connect-server dialog's outcome.

## Owner setup (one time)

1. Create a PostHog project. In *Settings → Project*, enable **"Discard
   client IP data"**, and leave person profiles off (events are anonymous anyway).
2. Add the repository secret **`FILEX_POSTHOG_KEY`** (the project API key,
   `phc_…`). Remove the old `FILEX_SENTRY_DSN` secret.
3. For an EU project (or a reverse proxy), add the repository *variable*
   **`FILEX_POSTHOG_HOST`**, e.g. `https://eu.i.posthog.com`. It defaults to
   `https://us.i.posthog.com`.
4. Optional: in Error tracking, mark `filex::*` frames as in-app (the SDK
   already flags them) and set alerts on new issues.

Local testing: `FILEX_ANALYTICS=log RUST_LOG=filex=info cargo run` to see
events with no project, or `FILEX_POSTHOG_KEY=phc_… cargo run --features
analytics` to send them. Runtime env vars override the baked-in values.
Debug builds tag events `environment: debug`.

## Testing

- Catalog: generated docs are current; names unique snake_case and
  documented; typed events serialize labels and omit `None`; helper
  mappings (`CountBucket`, `FileKind`, `Location`, `IoError`) give closed
  labels; a `compile_fail` doctest rejects a `String` field.
- Pure, no network: envelope shape and anonymity, `schema_version`, UUIDs,
  `scrub_text`, backtrace parsing, exception shape and fingerprint
  stability, install-ID lifecycle, settings snapshot and diff, the search
  session summary, op batch kinds.
- Pipeline with a recording transport: shutdown spools instead of posting
  and the next run sends the spool; revoked consent discards; corrupt spool
  lines are skipped; retry vs reject; bounded buffer; per-event cap with a
  `dropped` tally.
- Transport against a loopback HTTP server: path, Bearer header, content
  type, body; a refused connection is a retry.
- `testing::Capture` is per-thread and makes `is_enabled()` true.

## History

- **2026-07-25:** custom POST uploader (`FILEX_CRASH_ENDPOINT`), crashes only,
  on by default with a Settings toggle. "Not Sentry."
- **2026-08-08:** Sentry became the sole transport: crashes drained from the
  disk queue, `sentry-tracing` errors and breadcrumbs, measurement events,
  release-health sessions, feedback with an optional email.
- **2026-10-10:** the Windows MSI asks on its Privacy page.
- **2026-11-28:** Sentry removed. PostHog handles error tracking *and*
  product analytics through an in-tree ureq client. The disk crash queue,
  `scrub`, the panic hook and the `share_diagnostics` consent carry over.
  New: typed, PII-proof event properties, anonymous install ID, live
  consent toggle, and the email field removed from feedback.
- **2026-11-28 (same day, pre-production):** architecture pass. A typed
  catalog with macros and generated docs is now the only way to emit, plus
  explicit navigation attribution (fixing a stale-label bug), a disk spool
  instead of a blocking quit flush, per-event caps, `FILEX_ANALYTICS=log`, a
  test capture and `schema_version`. `feature_used` is split into specific
  events (`context_menu_used`, `toolbar_button_clicked`, `tab_changed`,
  `magic_mode_toggled`, `search_scope_changed`, `tag_saved`,
  `index_unavailable`), and the settings snapshot moved from
  `app_launched` to its own `settings_snapshot` event.

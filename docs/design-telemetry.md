# Design: Telemetry — product analytics and error tracking (Phase 2c)

Status: **PostHog, shipped 2026-11-28.** Replaces Sentry entirely (crash
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

1. **No PII, enforced by types.** A product event is an `Event` whose
   properties are `Prop`s, which can only be a number, a bool, or a
   `&'static str` label (or a list or tally of labels). There is no
   `From<String>`, so a file name, path, query, tag, folder name, URL or
   anything the user typed *cannot* become a property. Every label is a
   literal visible at its call site. Lists (roots, favorites, excluded
   names, icon choices, shortcuts) are reported as **counts**.
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
 UI code ── analytics::track(Event) ──▶ mpsc ──▶ worker thread "filex-analytics"
   (channel send, never blocks)                  │ batches of 20 / every 30 s
                                                 │ retry w/ backoff (5 s → 5 min)
 tracing error! ── AnalyticsLayer ─▶ track_raw ──┤ buffer ≤ 1000, oldest dropped
                                                 ▼
 panic (UI or daemon) ─▶ telemetry queue (disk) ─▶ drain_crashes (next launch)
                                                 ▼
                              POST {host}/i/v1/analytics/events  (ureq + rustls)
                              Authorization: Bearer <project key>
```

| File | Role | Compiled |
|---|---|---|
| `diagnostics/analytics/event.rs` | `Event`/`Prop` (the PII guard), catalog `names`, `bucket` | always |
| `diagnostics/analytics/wire.rs` | capture-v1 envelope, UUIDv7, super-properties | always |
| `diagnostics/analytics/exception.rs` | `$exception` from crashes and logs, `scrub_text`, backtrace parsing | always |
| `diagnostics/analytics/identity.rs` | install ID create/forget | always |
| `diagnostics/analytics/snapshot.rs` | settings snapshot and `setting_changed` diff | always |
| `diagnostics/analytics/pipeline.rs` | worker, batching, retry, session summary; `Transport` trait | always |
| `diagnostics/analytics/client.rs` | `ureq` transport, key/host config | `analytics` |
| `diagnostics/analytics/layer.rs` | `tracing` layer: `error!` → `$exception` | `analytics` |
| `diagnostics/analytics/mod.rs` | public API: `init`, `track`, `count`, `shutdown`, `disable`, `drain_crashes`, `send_feedback` | always (no-ops without a transport) |
| `workspace/analytics.rs` | UI helpers: launch, shortcuts, search session, op/file/index helpers | app |

**Why not the `posthog` crate:** it pulls in reqwest and tokio. filex already
ships ureq + rustls for updates, so the client is in-tree (≈600 lines with
tests). The wire format matches the official SDKs: `posthog` 1.7
(`capture_event.rs`, `error_tracking.rs`, `endpoints.rs`) and Python 8.4
(`capture_send.py`).

**Wire format:** `POST /i/v1/analytics/events`, Bearer auth,
`{created_at, batch: [{event, uuid (v7), distinct_id, timestamp, session_id,
options: {process_person_profile: false}, properties}]}`. 2xx = delivered;
408/429/5xx/transport errors = retry; other 4xx = drop.

**Exceptions:** `$exception_list: [{type, value, mechanism: {type, handled,
synthetic}, stacktrace: {type: "raw", frames}}]`. Frames are `{function,
filename, lineno, lang: "rust", platform: "native", in_app, synthetic,
client_resolved: true}`, outermost first. `$exception_fingerprint` is an
FNV-1a hash of compile-time facts (panic location + innermost in-app
function, or the log callsite), never the message, so issues stay stable
across occurrences and toolchain bumps. Each log callsite reports at most 5
times per process.

**Threading / latency:** `track` and `count` are a channel send behind an
atomic `is_enabled()` check. The per-keystroke cost on the search path is
`chars().count()` plus an `Option` update. Folder navigation adds one
`mem::replace` and a send. HTTP only happens on the worker. At quit,
`shutdown` waits **up to 1 s** for the final POST (only when analytics is
on). The app is already exiting, and GPUI's own 200 ms quit budget is too
short for a request.

## Event catalog

Super-properties on every event: `$lib` = `filex`, `$lib_version`,
`$app_name`, `$app_version`, `$os` (PostHog's names), `arch`,
`environment` (`debug`/`release`), `$geoip_disable`.

| Event | When | Properties |
|---|---|---|
| `app_launched` | first window open | `launch` (default, open_folder, select_item, this_pc, foreign), `new_install`, `startup_ms`, `onboarding_pending`, settings snapshot (`setting_*` enums/bools, `*_count`) |
| `app_session_ended` | quit | `duration_s`, `events` (tally by name), `counters` (shortcut tally) |
| `usage_summary` | every 30 min | `duration_s`, `since_last_summary_s`, `counters` |
| `resource_sample` | every 15 min | `rss_mb`, `index_roots`, `tabs` |
| `index_status` | first daemon answer; each finished build | `trigger`, `building`, `roots`, `files`, `files_bucket`, `skipped`, `error` (bool) |
| `folder_opened` | a folder's rows land | `via` (launch, up, back, forward, open, search_result, reveal, path_bar, breadcrumb, sidebar_place, sidebar_favorite, sidebar_drive, recent, this_pc, connect_server, magic, picker, new_tab, refresh, other), `location` (local, home, root, network), `load_ms`, `entries`, `entries_bucket` |
| `folder_load_failed` | a load errored | `via`, `location`, `load_ms`, `error` (io kind) |
| `search_session` | search opened/cleared/applied | `outcome` (opened, revealed, cleared, magic_applied, magic_command, magic_go), `rank`, `duration_ms`, `edits`, `max_query_len`, `final_query_len`, `responses`, `first_response_ms`, `mean_response_ms`, `max_response_ms`, `results`, `results_bucket`, `zero_result_responses`, `errors`, `pages_loaded`, `filters` (tag/kind/ext/size/modified), `phrases`, `magic`, `scope`, `hit_limit`, `literal_won` |
| `magic_plan_shown` | first plan per search | `verb`, `matches`, `ok`, `refusal` (PlanError id) |
| `magic_plan_applied` | confirm | `verb`, `selected`, `unchecked`, `local`, `conflicts` |
| `magic_instant` | app command / go | `kind` (app, go), `command` (AppCommand id), `new_tab`, `target` |
| `file_op` | batch reaches the undo journal; single-op failure | `op` (move, copy, rename, trash, create, mixed), `source` (paste, drop, delete, magic, single, new_folder, compress), `ok`, `items`, `requested`, `kinds` (tally), `duration_ms`, `error` (io kind) |
| `undo` | undo finished | `items`, `kinds`, `ok`, `error` |
| `file_opened` | open with default / chosen app | `kind` (image, video, audio, archive, code, document, other), `source` (browse, search, open_with), `app` (default, chosen), `ok` |
| `feature_used` | UI affordances | `feature`: context_menu (`command`, `target`, `selected`), toolbar_button (`action`), quick_look (`items`), tab_opened/tab_closed (`tabs`), settings_opened, magic_toggle (`on`), search_scope (`scope`), drag_out (`items`), tag_saved (`edited_existing`), tag_removed, sidebar_tag_search, index_unavailable (`crash_looping`), analytics_enabled |
| `setting_changed` | any settings change (store diff) | `setting`, `value` (label, bool or list length) |
| `onboarding_step` | step change | `from`, `to`, `on_step_ms` |
| `onboarding_finished` | flow closed | `completed`, `last_step`, `imported_favorites`, `offered_favorites`, `default_app`, `magic_live` |
| `update` | update flow | `stage` (available, copy_command, open_url, restart, dismissed, failed) |
| `feedback_submitted` | Send pressed | `message` (scrubbed, ≤ 5000 chars), `length` |
| `$exception` | panic (drained) / `error!` | PostHog exception props; crashes add `process`, `thread`, `panic_location` and the crashing `$app_version`; logs add `log_target` |

Keyboard shortcuts are counted, not sent one by one. Each keystroke that
resolves to an app action bumps a counter named after the action
(`filex::GoUp`), reported in `usage_summary` / `app_session_ended`.
Search-box text editing keys are excluded.

Not instrumented yet: the Open/Save dialog process (`filex --pick`), which
doesn't start analytics, and the connect-server dialog's outcome.

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

Local testing: `FILEX_POSTHOG_KEY=phc_… cargo run --features analytics`.
Runtime env vars override the baked-in values. Debug builds tag events
`environment: debug`.

## Testing

- Pure, no network: `Prop` serialization, buckets, UUIDv7/v4, envelope
  shape and anonymity, super-property override, `scrub_text`, backtrace
  parsing, crash/log exception shape and fingerprint stability, install-ID
  lifecycle (including rejecting a non-UUID file), settings snapshot and
  diff (lists as counts, custom accent hidden), search-session summary, AppCommand,
  PlanError and menu-command ids.
- Pipeline with a recording transport: batching, retry vs reject, bounded
  buffer, session summary, global start/track/shutdown, crash drain
  (timestamp and version kept, files deleted).
- Transport against a loopback HTTP server: path, Bearer header, content
  type, body; refused connection is a retry.
- The `tracing` layer reads only `message` and rate-limits per callsite.

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

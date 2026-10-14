# Design: `filex-updater` — the Windows update service

Status: **built 2026-10-14** (branch `feat/filex-updater-service`). Supersedes
the "installation opens the release page" decision in
[design-index-v2.md](design-index-v2.md) §"Portable adapters" for Windows, and
replaces §3 of [design-distribution.md](design-distribution.md) (the v1
"`filex-indexd` service updates itself" flow, removed in v0.2.0).

## Decision

Windows gets a **dedicated, demand-started LocalSystem service,
`filex-updater`, whose only job is installing verified Filex releases.** The
index daemon stays per-user. Compared options (session of 2026-10-14):

| | Updater service (chosen) | `filex-indexd` as SYSTEM (v1) |
|---|---|---|
| Silent updates | yes | yes |
| Search runs as | the user | SYSTEM — every result needs an ACL check |
| Mapped drives / per-user shares | visible | invisible to SYSTEM |
| Code with SYSTEM rights | download + verify + msiexec | indexer, watchers, query engine, updater |
| Idle cost | 0 (exits when idle) | always-on service |

Fast MFT/USN indexing, if measurements ever demand it, comes back as a
**separate** privileged helper ("option 3"), not by elevating the daemon. The
updater is written so that adding such a service is a registry entry plus a
WiX block (see *Components*).

macOS (Homebrew) and Linux (tarball) are unchanged: notice-only banner.

## Topology

```
filex.exe (user) ──\\.\pipe\filex-updater──> filex-updater (LocalSystem, demand start)
     │                                          │ fetch manifest + MSI (HTTPS)
     │ Shutdown (loopback IPC)                  │ verify SHA-256 + Ed25519
     ▼                                          │ terminate Filex processes
filex-indexd (user)                             │ msiexec /i … /qn /norestart (detached)
                                                ▼
                         new filex-updater (started by the MSI) relaunches filex.exe
                         in the requesting session (WTSQueryUserToken)
```

## Flow

1. App launch → app starts the service through the SCM (users are granted
   `SERVICE_START` on it) → `Check` over the pipe.
2. Service fetches the manifest. Newer → download into memory, verify. State
   `Ready{version}`. The verified payload stays **in memory**; nothing touches
   disk until apply.
3. Banner: "Filex X is ready — Restart". On Restart the app sends `Apply`. If
   the service lost the payload (idle exit), `Apply` re-runs the check and
   applies when ready (`apply_requested`).
4. State `Applying` → the app asks its daemon to shut down and quits.
5. Service writes `updates\pending.json` (target version + requesting session),
   waits up to 10 s for the requester's app to exit, terminates any Filex
   process still running from the install directory (any session — the daemon
   is crash-safe by its WAL contract), stops other Filex services, writes the
   MSI to `updates\`, and spawns `msiexec` **detached**.
6. The MSI's `ServiceControl` stops and replaces the updater, then starts the
   new one (`Start="install" Wait="no"`). On start the service reads
   `pending.json`: relaunches `filex.exe` in that session (only if the record is
   under 10 minutes old), records Updated/Failed, and purges `updates\`.
7. If msiexec exits while the old service is still alive (e.g. 1618 "another
   install in progress"), nothing was replaced: the service runs the same
   pending-record handling itself.

## Security rules

- **No client-supplied URLs, paths or arguments.** The protocol is `Hello`,
  `Status`, `Check`, `Apply`. Manifest URL and Ed25519 public key are compiled
  in (`filex::updater::{MANIFEST_URL, UPDATE_PUBLIC_KEY}`).
- **Verify before anything runs:** `download_and_verify` gate; the bytes
  handed to msiexec are the exact verified bytes (written by SYSTEM into an
  admin-only directory; re-hashed after the write).
- **Staging under `<install dir>\updates`**, which inherits Program Files'
  admin-only ACL. Not `%TEMP%` or `%ProgramData%`: standard users can
  pre-create folders in ProgramData and plant junctions.
- **No downgrades:** only `manifest.version > CURRENT_VERSION`.
- **Pipe:** explicit DACL — SYSTEM/Administrators full, `INTERACTIVE` read +
  write-data only (no `FILE_CREATE_PIPE_INSTANCE`, so users can't add
  instances), network denied, `PIPE_REJECT_REMOTE_CLIENTS`,
  `FILE_FLAG_FIRST_PIPE_INSTANCE` (squatting fails the service loudly). The
  client checks that the pipe's server PID is the service's PID.
- **Service DACL** is set by the service itself on every start (SYSTEM full,
  Administrators full, INTERACTIVE query + start) — no WiX extension needed.
- **Relaunch uses the user's own (filtered) token**, so the app and the
  daemon it spawns never run elevated.
- Logs go to the SYSTEM profile (`…\systemprofile\AppData\Local\filex\logs`),
  never to a user-writable directory.

## Components (the extensibility point)

`filex::updater::components::COMPONENTS` lists every binary the MSI ships and
how the updater quiesces it before an install:

| Kind | Quiesce | Example |
|---|---|---|
| `App` | asked to quit; terminated after the grace period | `filex.exe` |
| `UserProcess` | terminated (must be crash-safe) | `filex-indexd.exe` |
| `Service` | stopped through the SCM | future MFT helper |
| `Updater` (self) | never stopped by itself; the MSI stops it | `filex-updater.exe` |

Tests fail if `wix/main.wxs` lacks a `File` for any component, a
`ServiceInstall`/`ServiceControl` for any service, or if `release.yml` doesn't
build a component's binary. Adding a service = one registry entry + one WiX
component + one release build line.

## Idle behaviour

Busy (checking, downloading, applying) keeps the process alive. Otherwise it
exits after 2 minutes without a request, or 15 minutes when holding a `Ready`
payload. Nothing runs between app launches (distribution decision 7: check on
launch only).

## Fallbacks

If the service is missing (dev build, pre-updater install) or can't be
started, the app falls back to the notice-only banner that opens the
releases page. Installs older than the first release carrying the updater
must be updated by hand once.

## Validation

- Pure logic (state machine, components, protocol, pending record, msiexec
  args) has plain `#[test]`s on every OS. The service core runs against a
  mock effects layer, including verify-failure-never-installs.
- The pipe server and client are tested in-process on Windows with a
  test-only pipe name.
- **Not validated locally** (needs admin / WiX): service install, the MSI
  round-trip, session relaunch. The CI `msi` job checks the service is
  installed demand-start and removed on uninstall; the first real
  update round-trip must be done by hand on a Windows machine (see the
  checklist in `packaging/README.md`).

## Open items

- Manual round-trip on real Windows (FIL-31) — the acceptance test.
- Ed25519 key rotation (FIL-12) — a compromised key still means a SYSTEM-level
  install; a second embedded key would allow rotation.
- "Apply when Filex is closed" (update without a click) — FIL-32.
- Authenticode check of the MSI once releases are code-signed (FIL-9), as a
  second gate next to Ed25519.
- Index-source trait in the daemon (FIL-33) — prerequisite for an MFT helper.
- Other users' sessions: their Filex is terminated without warning during an
  install. Acceptable for now (rare); a per-session notice would need the
  service to message other sessions.

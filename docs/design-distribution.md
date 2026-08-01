# Design: Distribution & auto-update

Status: **Decided 2026-08-01, not yet built beyond the existing MSI.**
Written out of a distribution-strategy session. Captures how Filex ships
to users and how they get updates on each platform. The Windows MSI
(`wix/main.wxs`) already exists from roadmap Phase 2b; everything else
here — the auto-updater, payload signing, macOS packaging, release CI —
is new work planned below.

The governing constraint this session settled on: **no paid code
signing.** We accept the one-time OS trust prompt that costs us (one
SmartScreen on first Windows install; one Gatekeeper prompt on first
macOS launch) rather than pay for a certificate, and we design the update
path so that cost is paid *once per user, ever* — never on updates.

## Decisions (this session)

1. **No paid signing, either platform.** Not $99 Apple notarization, not
   a Windows cert (Azure Trusted Signing ~$120/yr was the cheap option
   and was declined). Revisit only when a non-technical userbase makes
   the first-launch prompts cost real installs.
2. **macOS → Homebrew tap, ad-hoc signed, unsigned/unnotarized.** Updates
   via `brew upgrade`. Frictionless install/upgrade for a technical
   userbase; one Gatekeeper prompt on first launch is the only cost.
3. **Windows → WiX MSI (already built), elevated `filex-indexd` service,
   coupled updates, unsigned.** One SmartScreen at first install; every
   update thereafter is **silent** (see §3). The blue "Windows protected
   your PC" wall keys off Mark-of-the-Web from *browser* downloads, so a
   service-driven download never triggers it.
4. **Coupled updates.** One artifact per platform updates UI + service
   together, atomically. No independent versioning of UI vs service — the
   MSI major-upgrade (Windows) / cask (macOS) is the unit.
5. **Update payloads are signed with our own Ed25519 key**, verified
   before apply. This is **non-negotiable** and separate from OS code
   signing — see §4. The Windows updater runs as **LocalSystem**, so an
   unverified payload is a SYSTEM-level RCE on every user's machine.
   Implementation uses **raw Ed25519 (`ed25519-dalek`), a detached
   signature over the artifact bytes, hex-encoded in the manifest** —
   chosen over the minisign envelope so verification is self-contained
   (no external tooling) and fixtures are reproducible in tests.
6. **Linux → first-party tarball** on GitHub Releases. No AppImage for
   now, no distro packages — a `.tar.gz` users extract. Update path is
   manual re-download (or community packages later), not silent — Linux
   has no elevated service assumed here.
7. **Update check runs on launch, not on a timer.** One check when the
   app/service starts; no recurring poll. Keeps it simple and off any
   hot path.
8. **`minimum_version` is not enforced yet.** The field stays in the
   manifest schema (cheap forward-compat) but clients don't act on it for
   now — no forced-reinstall path in v1.

## Current state — what already exists

- `wix/main.wxs`: perMachine MSI built via `cargo wix` (WiX **v3** via
  cargo-wix — the plan stays on v3, no migration). Has `MajorUpgrade`
  (`afterInstallInitialize`, downgrade-blocked), and a `FilexIndexd`
  component with `ServiceInstall` (LocalSystem, auto-start) +
  `ServiceControl` (start on install, stop on both, remove on uninstall).
  x64 only.
- `filex-indexd` service + named-pipe IPC; UI runs in service mode when
  the daemon is present, falls back seamlessly (roadmap Phase 2b).

So the **coupled MSI and the elevated service are done.** What's missing:
the updater, the manifest, payload signing, macOS packaging, and release
automation.

## Architecture: the update channel

A single static **manifest** (JSON) served over **HTTPS** (GitHub
Releases assets to start; a CDN later if traffic warrants). Both
platforms read the same schema; each release publishes one manifest per
platform (or one manifest with per-platform entries).

```json
{
  "version": "1.4.0",
  "url": "https://.../Filex-1.4.0.msi",
  "sha256": "<hex>",
  "signature": "<ed25519 sig over the sha256, base64>",
  "minimum_version": "1.0.0"
}
```

`minimum_version` is a reserved escape hatch for later: if a future
release changes something the in-place updater can't bridge (e.g. an
IPC-contract break the coupled MSI can't reconcile), bumping it would let
clients below it fall back to "download a fresh installer." **Not enforced
in v1** — the field is written but clients ignore it for now (decision 8).

## §3 Windows auto-update — service-driven, silent

The `filex-indexd` service (already LocalSystem) owns the update loop.
Because it is already elevated and downloads via its own HTTP client:

- **No UAC** — the service is already SYSTEM; `msiexec` inherits that, no
  interactive elevation.
- **No SmartScreen** — the MSI arrives via the service's HTTP client, not
  a browser, so it carries no Mark-of-the-Web; the reputation wall never
  fires.

Flow:

1. On service start, check the manifest once (decision 7 — no timer).
2. If `version > current`, download the MSI to a temp path.
3. **Verify** `sha256` and the **Ed25519 signature** (§4). Abort on any
   mismatch — do not touch `msiexec`.
4. Run `msiexec /i Filex-x.y.z.msi /qn` (quiet). The `MajorUpgrade`
   element makes this **transactional**: old version removed + new
   installed as one operation, **auto-rolled-back on failure** — this is
   the "doesn't break" guarantee, provided by Windows Installer, not
   hand-rolled.
5. `ServiceControl` stops the running service during the upgrade and
   restarts it after, solving the can't-replace-my-own-running-exe
   problem. The UI, if open, is signalled to restart into the new build.

Coupled by construction: the MSI carries both `filex.exe` and
`filex-indexd.exe`, so one major-upgrade lands both together.

## §3b macOS updates — Homebrew, not a self-updater

macOS has **no elevated service** (FSEvents needs no privilege), so there
is nothing to drive a silent update — and shipping a Sparkle-style
self-replacing `.app` would *fight* the package manager. So:

- **Primary path: `brew upgrade`.** CI bumps the cask's `version` +
  `sha256` on each release (§5); users get it on their next upgrade.
- **Optional in-app notice:** a non-blocking "a new version is available —
  run `brew upgrade filex`" banner when the app sees a newer manifest.
  This is a *notice*, not a silent apply — recommended, low priority.
- Set `auto_updates true` in the cask only if we later ship a real
  in-app updater, so brew doesn't fight it. Not now.

This asymmetry (Windows silent, macOS via brew) is deliberate and
consistent with the OS-privilege difference, not an inconsistency to fix.

## §4 Payload signing & verify (shared, testable)

The `filex::update` lib module holds the **pure, network-free**
verification logic, unit-tested against fixtures (per CLAUDE.md: business
logic decoupled from the UI, tests alongside). **Built — block 1 done**
(`src/update.rs`, 12 fixture tests). Shape:

- **Ed25519 keypair** (`ed25519-dalek`). **Public key embedded** in both
  binaries; **private key lives only in CI secrets**, never in the repo
  or on dev machines.
- CI signs each release artifact's **full bytes** with the private key
  and writes the hex `signature` into the manifest.
- `Manifest::verify_payload(data, public_key_hex)` recomputes the
  download's `sha256` as a fast corruption pre-check, then verifies the
  Ed25519 signature over the artifact bytes with `verify_strict`. **Both
  must pass** before any install step runs; the signature is the
  authoritative check (the hash is convenience/corruption-catching).
- All of `sha256`, `signature`, and the public key are lowercase **hex**
  in the manifest.
- Serve manifest + artifact over HTTPS.

Verification is the safety boundary that replaces the OS code signing we
declined. It must be tested like indexer code: fixture manifests (valid
sig, tampered hash, wrong key, downgrade, malformed) with a `#[test]`
each, no network.

## §5 Release CI

On a version tag:

1. Build `filex.exe` + `filex-indexd.exe` (x64), `cargo wix` → MSI.
2. Build macOS `.app` bundle, **ad-hoc sign** (`codesign -s -`), package
   `.dmg`/`.tar.gz`.
3. Compute `sha256` of each artifact; **Ed25519-sign** each hash with the
   CI-held private key.
4. Publish artifacts + write the per-platform **manifest** to the release.
5. **Bump the Homebrew cask** (`version` + `sha256`) in the tap repo via
   a commit/PR from CI.

Note: CI has Windows + Linux runners today but no macOS runner (macOS is
validated on the dev machine by design). **Decision: add a macOS runner
for release builds** (decision below) — the `.dmg`/`.tar.gz` ad-hoc sign
and packaging run there, not on the dev machine, so releases are
reproducible from CI. This is release-only; day-to-day macOS *testing*
stays on the dev machine.

The Linux job also produces the **`.tar.gz`** (decision 6) alongside the
MSI and mac artifacts.

## Implementation plan (session-sized blocks)

Ordered so each block is independently testable and lands one concern.

0. **Tap + key setup. — TOOLING DONE 2026-08-01; manual steps pending.**
   `filex-sign keygen` generates the keypair; `packaging/README.md` has the
   one-time checklist (secrets `FILEX_SIGNING_KEY` / `TAP_GITHUB_TOKEN`,
   filling the embedded constants, creating the `homebrew-filex` tap).
   These are **manual, owner-only** steps (the private key must never enter
   a transcript or the repo), so the constants ship as empty placeholders
   that safely disable self-update until filled.

1. **`filex::update` core lib. — DONE 2026-08-01** (`src/update.rs`).
   Manifest (de)serialization, semver compare, Ed25519 `verify_payload`
   over artifact bytes with a SHA-256 pre-check — pure, no I/O, 12 fixture
   unit tests covering authentic/tampered/corrupted/wrong-key/malformed
   cases. `minimum_version` parsed but unenforced (decision 8).

2. **Downloader + integrity gate. — DONE 2026-08-01** (`src/update.rs`).
   `download_and_verify(fetch, manifest, key, cancel)` is the gate: the
   network is **injected** (mirroring `telemetry::drain`), so the lib
   stays network-free and the gate is unit-tested with mock fetchers —
   authentic, tampered-source, network-error, and three cancellation
   cases (6 tests). `CancelFlag` (cloneable `Arc<AtomicBool>`) aborts
   mid-download. The concrete streaming HTTPS fetcher `http_fetch` lives
   behind the new **`updater` feature** (opt-in, `ureq`), polls the
   cancel flag between chunks, and caps the body at 500 MB.

   **Invariant change:** the Windows index service was previously
   network-free (Cargo.toml note). Silent service-driven updates require
   it to make outbound HTTPS — a deliberate exception, contained by the
   rule that nothing is installed unless `download_and_verify` returns
   `Ok`. The `updater` feature keeps the network opt-in, so the default
   library build is still network-free.

3. **Windows apply path. — DONE 2026-08-01** (`src/update.rs` +
   `src/bin/filex-indexd.rs`). On service start (SCM path only — dev/
   console runs don't self-update), an off-thread check runs
   `check_for_update(http_fetch, …)`; on `Apply` it stages the verified
   MSI to `%TEMP%` and spawns `msiexec /i /qn /norestart` **detached**, so
   msiexec outlives the service that the MSI's `ServiceControl` is about to
   stop. Cancellation is tied to service Stop/Shutdown via `CancelFlag`.
   `MANIFEST_URL`/`UPDATE_PUBLIC_KEY` are empty placeholders → self-update
   is disabled until block 5 fills them (fail-safe: the service still
   indexes).

   The "verify fails ⇒ msiexec never runs" guarantee is enforced in the
   **testable** lib pipeline (`check_for_update` returns `Apply` only after
   the verify gate) and covered by `check_rejects_tampered_artifact_without_applying`.

   **Validation gap:** the feature-gated glue (`stage_msi`, `spawn_msiexec`,
   `check_and_apply_update`) compiles only on Windows *with* `updater`, and
   that build can't be cross-checked from macOS — `updater` pulls
   `ureq→rustls→ring`, whose C build needs the Windows SDK headers.
   Locally validated: (a) all update *logic* on the host (23 tests), (b)
   the always-on service glue on the `x86_64-pc-windows-msvc` target with
   `updater` off. The `updater`-on Windows compile + a real MSI
   major-upgrade round-trip must run on **Windows CI** (block 5).

4. **UI surface. — DONE 2026-08-01** (`src/update.rs`, `src/ui/update_banner.rs`,
   `src/main.rs`). A GPUI-free status model (`UpdateStatus` /
   `UpdateAffordance` / `banner_content`) drives a slim, dismissible banner
   above the status bar; wording is in tested pure functions (5 tests).
   macOS/Linux run a notice-only manifest check on launch
   (`check_for_newer_version` — no artifact download; the package manager
   installs) and show the banner: macOS copies `brew upgrade filex`, Linux
   opens the releases page. Off the latency-critical paths (a detached
   background task, mirroring `spawn_drive_refresh`). `UPDATE_MANIFEST_URL`
   is an empty placeholder → the check no-ops until block 5.

   **Validated:** full app compiles on macOS with `updater`; pure model +
   presentation covered by tests (344 total). **Not runtime-verified** (the
   app isn't launched here — the user verifies rendering/behaviour).
   **Pending:** the Windows UI↔service path. The model has the
   `Downloading`/restart states, but wiring the banner to `filex-indexd`'s
   progress over the named pipe is future work — on Windows the banner
   stays `Idle` for now.

5. **Release CI. — DONE 2026-08-01** (`.github/workflows/release.yml`,
   `src/bin/filex-sign.rs`, `packaging/`). Tag-triggered (`v*`): Windows
   builds + signs the MSI (reusing the `msi` job's WiX setup), Linux a
   `.tar.gz`, macOS an ad-hoc-signed `Filex.app` tarball; each artifact is
   Ed25519-signed by `filex-sign` (shares `filex::update`'s exact code, so
   the format can't drift), published to the GitHub Release alongside a
   `filex-<os>.json` manifest, and the Homebrew cask is bumped in the tap.
   The client fetches the manifest at the stable `releases/latest/download/…`
   URL. A macOS release runner was added here (decision this session).

   **Validated here:** `filex-sign` keygen→sign→verify round-trips (2
   tests + a live CLI run); YAML parses; builds under both feature sets.
   **Runs on GitHub's runners** once pushed — the actual release, the
   `updater`-on Windows compile, and the MSI round-trip happen there, not
   on the Mac. Single-arch macOS (arm64) for now; Intel/universal deferred.

6. **Linux tarball. — DONE 2026-08-01** (folded into `release.yml`'s
   `linux` job): builds `filex`, packages `filex-*-linux-x86_64.tar.gz`,
   Ed25519-signs it, and attaches it + `filex-linux.json` to the release.
   No AppImage, no silent updater (the launch check shows a banner that
   opens the releases page). Decision 6.

## Resolved this session (2026-08-01)

- **Linux:** first-party **`.tar.gz`** on GitHub Releases; no AppImage,
  no distro packages, no silent update.
- **macOS release runner:** **add a macOS CI runner** for release builds;
  packaging + ad-hoc sign run there.
- **Update cadence:** **check on launch only**, no timer; `minimum_version`
  present in the schema but **not enforced** in v1.

## Open items

- **Manifest hosting — DECIDED:** GitHub Releases, fetched at the stable
  `releases/latest/download/filex-<os>.json` URL (always the newest
  release). Revisit a CDN only if bandwidth/latency warrants.
- **Network failure handling:** the on-launch check currently fails silent
  and retries next launch (no backoff), which is the intended default;
  revisit only if a flaky-network case argues otherwise.
- **macOS arch:** release builds arm64 only (single runner arch); an
  Intel/universal build is deferred until there's demand.
- **Windows UI↔service update status:** wire `filex-indexd`'s download/
  ready state to the UI banner over the named pipe (block 4 left this
  `Idle` on Windows).
- **Ed25519 key management:** exact storage (CI secret store), and a
  rotation story if the key is ever compromised (embed a second/rollover
  public key?).
- **First SmartScreen mitigation (optional):** unsigned MSI reputation
  builds slowly with download volume; if the first-install wall becomes a
  real drop-off, Azure Trusted Signing (~$120/yr) is the pre-decided
  switch — no WiX changes, just a CI signing step.

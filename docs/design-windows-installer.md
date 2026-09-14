# Design: Premium Windows install experience

Status: **Planned 2026-08-31, not yet built.** Extends
[`design-distribution.md`](design-distribution.md) — that doc settled *how*
Filex ships and updates; this one is about how the Windows install **feels**
from download link to first window. No decision here contradicts it; two are
amendments, called out as such.

## The honest starting point

What a Windows user sees today, in order:

1. GitHub Releases page → click `Filex-0.1.1-x64.msi`.
2. Browser attaches Mark-of-the-Web → SmartScreen's blue **"Windows
   protected your PC"** wall. *More info → Run anyway.*
3. Yellow UAC prompt: **"unknown publisher"** (unsigned, perMachine).
4. `wix/main.wxs` has **no `<UIRef>`** — there is no installer UI at all.
   A bare progress bar appears and vanishes. No welcome, no branding, no
   confirmation, no "Launch Filex".
5. Nothing visibly happened. The app is in the Start menu with a **generic
   default icon** — there is no `.ico` anywhere in this repo, and neither
   `filex.exe` nor `filex-indexd.exe` embeds an icon or a VERSIONINFO
   resource. Add/Remove Programs shows a blank tile.
6. The user finds and launches Filex manually.

Steps 4–6 are ours to fix outright. Steps 2–3 are the loud ones and they are
*not* fixable without a certificate — see decision 1.

## Decisions

1. **Stay unsigned; mitigate, don't pretend.** (Confirmed 2026-08-31,
   upholds `design-distribution.md` decision 1.) The blue wall and the
   yellow UAC prompt stay on first install. We buy back what we can with
   **winget as the recommended install path** and **copy that pre-frames the
   prompt** rather than letting it ambush people. Ceiling stated plainly:
   because reputation accrues per exact binary hash for unsigned files,
   *every* release restarts SmartScreen reputation from zero. The
   pre-decided escape hatch is unchanged — Azure Trusted Signing, a CI-only
   change, flip it when first-install drop-off becomes real.
2. **Ship a themed WiX Burn bootstrapper (`Filex-x.y.z.exe`) as the
   user-facing installer.** Single branded window, sidebar art, one
   progress phase, "Launch Filex" on completion. Burn elevates once for the
   whole chain, so this is still **one** UAC prompt, not two. The MSI
   becomes an internal/enterprise artifact that the bundle wraps.
3. **AMENDMENT to `design-distribution.md` §3: the silent updater installs
   the bundle, not the MSI.** `filex-indexd` will download
   `Filex-x.y.z.exe` and run it `/quiet /norestart` instead of
   `msiexec /i … /qn`. This is not cosmetic — see "Why the updater must
   switch" below; keeping the msiexec path would silently corrupt the
   bundle's Add/Remove Programs entry on every update. The manifest `url`
   points at the `.exe`. Nothing about signing/verification changes:
   `download_and_verify` signs and checks *bytes*, and is indifferent to
   file type.
4. **Icons and version resources are block 1, before any chrome.** A
   beautiful installer that plants a generic-icon shortcut is not premium.
   This is also the cheapest item on the list.
5. **No shell-extension work.** No "Open with Filex" context-menu verb, no
   folder-handler registration, no default-file-manager takeover. Out of
   scope per CLAUDE.md, and it is exactly the kind of thing that reads as
   *invasive* rather than premium.
6. **The installer chooses nothing the user must think about.** No feature
   tree, no component checkboxes, no install-directory picker
   (`%ProgramFiles%\Filex`, fixed). `DisableModify` in ARP. The only
   decision offered is "Install" and, at the end, "Launch".

## Why the updater must switch (decision 3, the non-obvious part)

A Burn bundle registers *itself* in Add/Remove Programs and marks its
chained MSI hidden. Our MSI uses `Product Id="*"` with `MajorUpgrade`, so a
new version gets a **new ProductCode**. If the service kept running
`msiexec /i` directly on the inner MSI:

- the new MSI installs and the old one is removed — the app itself is fine;
- but the bundle's ARP entry still advertises the **old version**, and its
  cached chain still references a ProductCode that no longer exists;
- uninstalling from Settings then runs the stale bundle, finds its package
  "absent", reports success, and **leaves the new install orphaned** with no
  ARP entry at all.

Running the bundle `/quiet` instead makes Burn's own `UpgradeCode` the unit
of upgrade, which is what it is designed to do, and keeps one coherent ARP
entry across every update. Cost is small and contained: `spawn_msiexec` in
`src/update.rs` becomes `spawn_installer`, with the argv construction unit
tested like the rest of that module.

## Blocks

Ordered so each is independently landable and testable. Blocks 1–2 are
worth doing even if 3+ slip.

### Block 0 — Branding assets *(needs you, not me)*

There is no Filex mark in the repo. I can convert and wire up whatever
exists, but I can't invent the identity. What the rest of the plan consumes:

| Asset | Size | Used by |
|---|---|---|
| `assets/branding/filex.ico` | multi-res 16/32/48/64/128/256 (256 PNG-compressed) | exe resources, MSI `ARPPRODUCTICON`, shortcut, bundle |
| `assets/branding/logo.png` | 64×64 | Burn theme header |
| `assets/branding/sidebar.png` | 165×312 | Burn `HyperlinkSidebarLicense` theme |

One source SVG plus a resize/convert step generates all three; the input is
the blocker. Interim unblock if you want the rest to proceed: a plain
wordmark on the app's accent colour, replaced later.

### Block 1 — Icon + VERSIONINFO in both binaries

`build.rs` + the `winresource` crate (target-gated, dev-time only) embedding
the icon and a proper version block into `filex.exe` and `filex-indexd.exe`:
FileDescription, ProductName, CompanyName, LegalCopyright, FileVersion from
`CARGO_PKG_VERSION`. Fixes the taskbar icon, the Alt-Tab icon, the
Start-menu tile, and right-click → Properties → Details, all of which are
currently blank or generic.

Validation: `cargo check --target x86_64-pc-windows-msvc --no-default-features --bins`
locally (per the existing Windows cross-check habit); resource presence
asserted on the Windows CI runner.

### Block 2 — MSI polish (`wix/main.wxs`)

Small, self-contained, no new tooling:

- `<Icon>` + `ARPPRODUCTICON` so Add/Remove Programs and the Start tile
  show the mark.
- `ARPURLINFOABOUT`, `ARPHELPLINK`, `ARPNOMODIFY=1` (there is one feature —
  a Modify button that leads nowhere is a papercut).
- `Shortcut Icon=` explicitly, plus `<Shortcut>` `Description` already
  present.
- `MSIFASTINSTALL=7` — skips System Restore point creation. Speed is the
  product; this is a couple of seconds of the install, free.
- Uninstall cleanup of the **index snapshot cache** (`RemoveFolder` /
  `util:RemoveFolderEx` on `…\AppData\Local\filex\index`). Note the service
  runs as LocalSystem, so its snapshots live under
  `C:\Windows\System32\config\systemprofile\AppData\Local\filex\index` —
  potentially gigabytes, invisible to the user, orphaned forever today.
  User *settings* (`<config_dir>\filex\settings.json`) are deliberately
  left behind, which is the platform convention.

### Block 3 — The Burn bundle (`wix/bundle.wxs`)

WiX v3 Burn — same toolchain generation as today, no v4 migration:

- `<Bundle>` with `Name`, `Manufacturer`, `Version`, `UpgradeCode` (new
  GUID, distinct from the MSI's), `IconSourceFile`, `AboutUrl`, `HelpUrl`,
  `DisableModify="yes"`.
- `BootstrapperApplicationRef` →
  `WixStandardBootstrapperApplication.HyperlinkSidebarLicense`, with
  `bal:WixStandardBootstrapperApplication` supplying `LogoFile`,
  `ThemeFile`, `LocalizationFile`, and `LicenseUrl=""` (no EULA gate — see
  open items).
- `<Chain><MsiPackage SourceFile="…Filex.msi" Visible="no" /></Chain>`.
- Custom `theme.xml` + `theme.wxl`: a copy of the stock hyperlink-sidebar
  theme restyled to Filex's palette (dark surface, accent progress bar,
  Inter where the theme engine allows it), and copy rewritten — "Installing
  Filex" and a one-line "Filex is setting up its index service — this takes
  a few seconds" instead of Windows Installer boilerplate, plus a
  **checked** "Launch Filex" on the success page.
- Requires `-ext WixBalExtension` at both candle and light.

Honest ceiling, so nobody is surprised: BAL themes are Win32-drawn with a
fixed bitmap layout. This lands somewhere near a 2019 Chrome installer —
clean, branded, coherent — not a Win11-native Mica/rounded surface. Getting
past that ceiling means hand-writing a managed/custom BA, which is a
disproportionate amount of code for a window shown once per user. Marked as
a deliberate ceiling, not an oversight.

### Block 4 — Release CI (`.github/workflows/release.yml`)

The `windows` job grows a step: after `cargo wix --no-build`, run
`candle`/`light` with `WixBalExtension` over `bundle.wxs` to produce
`Filex-<version>-x64.exe`. Both artifacts are Ed25519-signed by `filex-sign`
and published; **`filex-windows.json`'s `url` now points at the `.exe`**
(decision 3). The MSI stays attached to the release for enterprise/silent
deployment, unreferenced by the manifest.

### Block 5 — Updater switch

`src/update.rs`: `stage_msi` → `stage_installer` (extension-aware temp
name), `spawn_msiexec` → `spawn_installer` running the bundle with
`/quiet /norestart`. Pure argv/staging construction gets unit tests
alongside the existing 23; the verify-before-apply guarantee is untouched
and still covered by
`check_rejects_tampered_artifact_without_applying`. Same validation gap as
`design-distribution.md` block 3: the feature-gated Windows glue only
compiles under `updater` on Windows, so the real round-trip
(install bundle → tag a release → confirm silent self-update → confirm ARP
still shows one coherent entry) has to run on Windows CI or hardware. That
round-trip is the acceptance test for this block.

### Block 6 — winget (the mitigation that carries decision 1)

`winget install Filex` is the install path we actually recommend in the
README and on the release page: no browser download, no Mark-of-the-Web,
hash-verified against the manifest. Submit to `microsoft/winget-pkgs` with
`InstallerType: burn` and `/quiet` switches; automate subsequent version
bumps with `wingetcreate update` in the release workflow (needs a PAT +
fork, same shape as the existing `TAP_GITHUB_TOKEN` step).

**To verify before we promise it in docs:** whether an unsigned Burn bundle
installed via winget genuinely avoids the SmartScreen wall on a clean
Windows VM. It should — winget doesn't attach MotW — but that claim goes in
user-facing copy, so it gets tested, not assumed.

### Block 7 — The words around the download

Premium here means being upfront rather than hiding the wart:

- `docs/install-windows.md` — winget first, direct download second, with a
  short, unapologetic "Filex isn't code-signed yet. Windows will show a
  blue warning; click *More info → Run anyway*." Screenshot of the exact
  click.
- A release-notes template carrying the same two lines, since most people
  arrive at the Releases page, not the docs.
- `packaging/README.md` updated for the two Windows artifacts and the new
  manifest target.

### Block 8 — First run

The installer's last act launches the app, so the first window *is* part of
the install experience. `status_bar.rs` already renders
`indexing 1/3 roots · N files`, so the machinery exists; this block is
verification plus whatever gap shows up on a real cold install — that the
first launch reads as "Filex is getting ready, here's progress" rather than
"Filex is empty and possibly broken", and that the service-mode handshake
either succeeds or falls back visibly.

## Open items

- **Block 0 is the critical path.** Everything visual downstream is blocked
  on a mark.
- **EULA:** the sidebar theme reserves space for a license link. Filex has
  no LICENSE file in the repo. Either add one and link it, or drop the link
  from the theme — a blank "License" hyperlink is worse than none.
- **Bundle `UpgradeCode`:** new GUID, minted once, then permanent. Getting
  this wrong later means users with two ARP entries.
- **Per-user install:** not offered. The elevated service makes perMachine
  structural, so "install without admin" would mean shipping a degraded
  no-service mode. Not planned; noted because someone will ask.
- **arm64 Windows:** the MSI and bundle are x64-only, matching CI. Same
  deferral as the macOS Intel build.
- **Signing, still:** if it ever gets bought, it changes only CI (sign the
  MSI *and* the bundle, in that order — Burn requires re-signing the bundle
  after `insignia` detaches/reattaches the engine). Nothing in blocks 1–8
  needs redoing.

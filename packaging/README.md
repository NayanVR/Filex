# Packaging & release setup

How Filex is distributed and the one-time setup the release pipeline needs.
Full design: [`docs/design-distribution.md`](../docs/design-distribution.md).

| Platform | Artifact | Install | Update |
|----------|----------|---------|--------|
| macOS | ad-hoc-signed `Filex.app` in `filex-*-macos.tar.gz` | Homebrew cask (`brew install --cask …`) | `brew upgrade` |
| Windows | `Filex-*-x64.msi` (app + `filex-indexd` service) | run the MSI (one SmartScreen prompt) | silent, service-driven |
| Linux | `filex-*-linux-x86_64.tar.gz` | extract & run | re-download |

The release pipeline is [`.github/workflows/release.yml`](../.github/workflows/release.yml),
triggered by a `v*` tag.

## One-time setup (block 0)

### 1. Generate the signing keypair

Every update payload is signed with our own Ed25519 key — the boundary
that replaces the OS code signing we don't pay for. **Run once, locally:**

```bash
cargo run --no-default-features --bin filex-sign -- keygen
```

- Copy `private_key` into the repo's Actions secret **`FILEX_SIGNING_KEY`**.
  It must never be committed or pasted anywhere public.
- Put `public_key` into the two embedded constants (see step 2).

### 2. Fill the embedded constants

Currently empty placeholders (which safely disable self-update):

- `src/bin/filex-indexd.rs` → `UPDATE_PUBLIC_KEY` = the `public_key` hex,
  and `MANIFEST_URL` =
  `https://github.com/<owner>/filex/releases/latest/download/filex-windows.json`
- `src/main.rs` → `Workspace::UPDATE_MANIFEST_URL` =
  `https://github.com/<owner>/filex/releases/latest/download/filex-<macos|linux>.json`
  (and update the Linux releases URL in `platform_affordance`).

The `latest/download/...` URL always resolves to the newest release, so it
never changes between versions.

### 3. Create the Homebrew tap + token

- Create a public repo **`<owner>/homebrew-filex`** (the tap).
- Create a PAT with `contents:write` on it, stored as the Actions secret
  **`TAP_GITHUB_TOKEN`**. The release pipeline writes `Casks/filex.rb` there
  (see [`homebrew/filex.rb`](homebrew/filex.rb) for the shape).
- Update `TAP_REPO` in `release.yml` if the owner differs.

Users then install with:

```bash
brew install --cask <owner>/filex/filex
```

## Cutting a release

```bash
git tag v1.4.0 && git push origin v1.4.0
```

The pipeline builds all three artifacts, signs each with `FILEX_SIGNING_KEY`,
publishes the GitHub Release with the artifacts + `filex-<os>.json`
manifests, and bumps the cask. Once step 2's constants are filled, existing
installs pick the update up on next launch (silent on Windows; a banner
elsewhere).

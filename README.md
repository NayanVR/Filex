# Filex

A fast, native file explorer for Windows, macOS and Linux, with instant
filename search. Filex is written in Rust and renders on the GPU through
[GPUI](https://github.com/gpui-ce/gpui-ce). There is no web view.

- Browse folders of any size without lag. Only the visible rows are rendered.
- Search every file name on your drives as you type. A background index
  service (`filex-indexd`) keeps the index current as files change.
- Previews, thumbnails, tags, favorites, keyboard-first navigation, and themes.

## Install

Download the latest build from
[Releases](https://github.com/NayanVR/Filex/releases/latest).

- **Windows:** run `Filex-<version>-x64.msi`. The installer asks whether to send
  crash reports (see [Privacy](#privacy)).
- **macOS (Apple Silicon):** `brew install --cask NayanVR/filex/filex`. Builds
  are not notarized, so the first launch shows a Gatekeeper prompt.
- **Linux (x86_64):** extract `filex-<version>-linux-x86_64.tar.gz`, put `filex` and
  `filex-indexd` together on your `PATH`, and optionally install the bundled
  desktop entry and icon (see [packaging/README.md](packaging/README.md)).

Filex checks for new releases itself. Every update is verified against an
Ed25519 signature before it is installed.

## Uninstall

- **Windows:** Settings → Apps → Installed apps → Filex → Uninstall, or
  `msiexec /x Filex-<version>-x64.msi`.
- **macOS:** `brew uninstall --cask filex`.
- **Linux:** delete `filex`, `filex-indexd`,
  `~/.local/share/applications/dev.filex.app.desktop` and
  `~/.local/share/icons/hicolor/256x256/apps/filex.png`. If you set up the
  optional systemd unit, run `systemctl --user disable --now filex-indexd` and
  remove `~/.config/systemd/user/filex-indexd.service`.

Uninstalling leaves your settings and index in place. To remove them as well,
delete the `filex` folder in both locations:

| | Settings | Index, tags, recents, logs |
|---|---|---|
| Windows | `%APPDATA%\filex` | `%LOCALAPPDATA%\filex` |
| macOS | `~/Library/Application Support/filex` | `~/Library/Application Support/filex` |
| Linux | `~/.config/filex` | `~/.local/share/filex` |

## Privacy

Filex sends optional crash reports and diagnostics to Sentry, with file names
and paths removed, and checks GitHub for updates. Nothing else leaves your
computer. Read the full [privacy policy](PRIVACY.md).

## Code signing policy

Windows releases are code-signed. Free code signing provided by
[SignPath.io](https://about.signpath.io/), certificate by
[SignPath Foundation](https://signpath.org/).

Only binaries built by this repository's
[release workflow](.github/workflows/release.yml) from tagged source are signed.
Every signing request is approved manually.

**Team roles**

- Committers and reviewers: [NayanVR](https://github.com/NayanVR)
- Approvers: [NayanVR](https://github.com/NayanVR)

**Privacy:** see the [privacy policy](PRIVACY.md). This program does not
transfer information to other networked systems except as described there.

## Building from source

```sh
cargo build --release --locked --no-default-features --bin filex-indexd
cargo run --release --locked --bin filex
```

See [packaging/README.md](packaging/README.md) for the index daemon's options and
[docs/](docs/) for design notes.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT), at your option.

Unless you explicitly state otherwise, any contribution you intentionally submit
for inclusion in this project, as defined in the Apache-2.0 license, is dual
licensed as above, without any additional terms or conditions.

# Per-user index daemon

Release packages include both `filex` and `filex-indexd` in the same executable
directory. The application starts the sibling daemon automatically and reconnects
if it exits. Browsing remains usable while indexing is unavailable.

For development:

```sh
cargo build --locked --release --no-default-features --bin filex-indexd
cargo run --locked --release --bin filex
```

For a separate daemon with explicit roots and an isolated database:

```sh
cargo run --no-default-features --bin filex-indexd -- \
  --user --data-dir /path/to/database /path/to/root
```

Use an absolute database path. It is excluded from both enumeration and watcher
updates. Without explicit roots, settings and the platform defaults are used.
The database is under the user's local data directory at `filex/index-v2`.

The daemon also launches short-lived copies of its own executable to build
segments. Keep the app and daemon from the same build together. The worker has no
listening endpoint and cannot publish a generation; the owner validates and
publishes its output. See [index maintenance](../docs/index-v2-maintenance.md) for
the process lifecycle and development checks.

Optional supervisors:

- Linux: copy `systemd/filex-indexd.service` into
  `~/.config/systemd/user/`, adjust `ExecStart`, and enable it with
  `systemctl --user enable --now filex-indexd`.
- Linux desktop launcher: the release tarball includes `dev.filex.app.desktop` and
  `filex.png`. After putting `filex` on your `PATH`, copy them to
  `~/.local/share/applications/dev.filex.app.desktop` and
  `~/.local/share/icons/hicolor/256x256/apps/filex.png` respectively.
- macOS: copy `launchd/dev.filex.indexd.plist` into `~/Library/LaunchAgents/`,
  adjust the application path if necessary, and load it with
  `launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/dev.filex.indexd.plist`.
- Windows: the MSI installs the executables. The UI launches the daemon under the
  interactive user's account. The v2 MSI does not install an SCM service; a major
  upgrade removes the old service through the previous MSI's uninstall actions.

The endpoint capability stays in the private user database directory. The daemon
performs no privileged file operations. Initial enumeration and restart recovery
can take time; notification overflow requests reconciliation. Full journal-only
recovery and elevated machine-wide search are not part of this version.

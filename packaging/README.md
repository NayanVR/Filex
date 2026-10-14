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
  interactive user's account. The only SCM service is `filex-updater`
  (demand-start, LocalSystem), which installs verified updates silently; see
  [the updater design](../docs/design-updater.md). A major upgrade from a v1
  build removes the old `filex-indexd` service through the previous MSI's
  uninstall actions.

The daemon's endpoint capability stays in the private user database directory. The daemon
performs no privileged file operations. Initial enumeration and restart recovery
can take time; notification overflow requests reconciliation. Full journal-only
recovery and elevated machine-wide search are not part of this version.

## Windows updater: manual round-trip check

CI proves the service installs, boots and uninstalls, but not a real update.
Before relying on it, run this once on a Windows machine (admin needed for the
first install only):

1. Install release N (the first one that ships `filex-updater`).
2. From an elevated prompt, confirm the service and that users may start it:
   `sc qc filex-updater` (DEMAND_START, LocalSystem) and
   `sc sdshow filex-updater` (contains `(A;;CCLCSWRPLOCRRC;;;IU)`).
3. Diagnostics without installing: `"C:\Program Files\Filex\filex-updater.exe" --check-only`
   prints the latest release and whether its signature verifies.
4. Publish release N+1, open Filex as a standard user: the banner goes
   "Downloading…" then "Filex N+1 is ready — Restart".
5. Click Restart: Filex closes, no UAC prompt appears, and Filex reopens on N+1
   within about a minute. Settings > Apps lists Filex at N+1.
6. Logs: `C:\Windows\System32\config\systemprofile\AppData\Local\filex\logs\`
   (`filex-updater.*` and `msiexec-<version>.log`).
7. `C:\Program Files\Filex\updates\` is gone afterwards.

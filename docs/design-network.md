# Network locations

Status: **slice 1 landed (2026-10-30)**: network drives show up and can be
opened by address. Connecting and disconnecting shares are the next slices
(see "Plan").

## Goal

Treat network shares as first-class places: see the shares this machine has
mounted, open one by typing its address, and (next) connect to a new share or
disconnect one from inside Filex. All three platforms, one model.

Non-goals for Phase 1: browsing the LAN for servers (SMB browsing/mDNS
discovery), indexing network shares by default, a built-in SMB/NFS client
(Filex always goes through the OS's own mount), FTP/SFTP/cloud drives as
their own protocol stacks, and "Share with ›" access management (FIL-43).

## Ground rules

- **Never block on the network where the UI or the shared executor waits.**
  A share that stops answering makes `statvfs` / `GetDiskFreeSpaceExW` /
  `readdir` block for the SMB timeout, and an NFS hard mount blocks until the
  server returns. Anything that touches a mounted share either runs in a
  cancellable background task (listing, which already does) or under a time
  limit (capacity probes).
- **Parsing is pure.** Addresses are parsed and matched to mounts without
  I/O, against the mount snapshot the background drive refresh already holds.
- **The OS mounts, Filex doesn't.** Credentials, Kerberos, keychains and the
  OS's own auth prompts stay the OS's job.

## Model (`src/network.rs`, `src/drives.rs`)

- `ShareAddress { protocol: Smb | Nfs, host, user, segments }` parses what
  people type (`\\server\share\dir`, `smb://user@server/share/dir`,
  `nfs://server/export/dir`) and what mount tables report (`//server/share`
  for cifs/smbfs, `server:/export` for NFS). It's not a path: bare
  `//server/share` typed on Unix stays an ordinary path.
- `Drive` gained `kind: Fixed | Removable | Network`, `capacity:
  Option<Capacity>` (`None` = the share didn't answer in time) and `share:
  Option<ShareAddress>` (what a network drive points at).
- `ShareAddress::locate` maps an address to the deepest mount covering it
  (SMB names compare case-insensitively, NFS paths case-sensitively), so
  `smb://nas/Media/Movies` opens `/mnt/media/Movies` when `//nas/Media` is
  mounted there.

### Per-OS enumeration

| | Where mounts come from | How "network" is decided | Name |
|---|---|---|---|
| Windows | drive letters (`GetLogicalDrives`) | `GetDriveTypeW == DRIVE_REMOTE` | Explorer's `Movies (\\nas\Media) (Z:)`, target from `WNetGetConnectionW` |
| macOS | `/Volumes/*` | `getfsstat(MNT_NOWAIT)` local flag | `share (server)` |
| Linux | `/proc/mounts` | filesystem type (`cifs`, `nfs4`, `fuse.sshfs`, …) | `share (server)` |

None of the three sources contact a server: the WNet connection table, the
kernel's cached `statfs` (`MNT_NOWAIT`) and `/proc/mounts` are all local.
Previously Linux dropped every network mount (it kept only `/dev/*`
sources), and Windows skipped a mapped drive whose server was down and named
reachable ones like local disks (volume label, or "Local Disk").

### The capacity probe

Network drives get their capacity from `bounded_probe`: the syscall runs on
its own thread and the refresh waits at most 2 s. When it times out, the
mount point is remembered and no new probe starts for it until the stuck one
returns, so a dead mount costs one parked thread, not one per 30 s refresh.
The drive stays listed as "Not responding", without a capacity bar.

A refresh with several dead mounts can take 2 s each the first time; that's
the background refresh, not the UI.

### Typed addresses

The location field resolves a share address before path rules:

- Windows: SMB addresses become UNC paths and open directly (no mapping
  needed). NFS addresses need a mounted drive letter.
- macOS/Linux: the address maps into the mount that covers it; otherwise the
  field says `Media (nas) isn't connected. Mount it first, then open it
  here.` Slice 2 turns that into a Connect action.

## Plan

1. **Show and open mounted shares** — done (this change).
2. **Connect to a share** ("Connect to Server…" in the sidebar/This PC and
   the not-connected error). Per OS, behind one `connect(address) ->
   Result<PathBuf>` trait that runs off the UI thread and is cancellable:
   - Windows: `WNetAddConnection2W` with `CONNECT_INTERACTIVE |
     CONNECT_PROMPT` (the OS credential prompt), optional drive letter;
     without a letter, just open the UNC path once connected.
   - macOS: NetFS `NetFSMountURLAsync` (system auth UI, Keychain), mounting
     under `/Volumes`.
   - Linux: GVfs (`gio mount smb://…`), user-space and root-free; the share
     appears under `$XDG_RUNTIME_DIR/gvfs/`. `mount.cifs` needs root and
     stays out. Credentials: GVfs asks via the desktop's mount-operation
     agent; with no agent, report it rather than prompting in-app.
3. **Disconnect / eject**: `WNetCancelConnection2W`, `unmount(2)` via
   `diskutil unmount` semantics, `gio mount -u`. Context menu on network
   drives; refuse while a file operation targets the share.
4. **Linux GVfs mounts in the drive list**: `fuse.gvfsd-fuse` is one mount
   for all GVfs shares; list its `smb-share:server=…,share=…` children as
   drives and parse them into `ShareAddress` so typed addresses find them.
5. **Recent servers**: remember connected addresses (no credentials) for the
   Connect dialog.

Nested network mounts (FIL-62, done): on Linux, `IndexFilter` reads the
network mount points from `/proc/mounts` and the walk and live events skip
any that sit strictly inside a root, mount point included, so a share
mounted under `~` (sshfs at `~/remote`, cifs under `~/mnt`) isn't crawled.
A mount added as a root itself is indexed normally. The filter is read
when a walk starts and per event batch, so a share mounted later is
skipped from the next walk on.

Watches (FIL-66, done): on Linux the daemon registers one inotify watch
per directory itself (`daemon::watch`), pruned by the same `IndexFilter`,
instead of notify's recursive mode, which walked the whole tree, nested
network mounts and excluded folders included. A directory created or moved
in gets its watches from a registrar thread; a filter change or a queue
overflow resyncs them. macOS and Windows keep their native recursive
watches, which don't walk the tree.

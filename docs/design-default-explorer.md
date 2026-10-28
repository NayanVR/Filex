# Default file explorer + system Open/Save dialogs

Status: in progress (started 2026-10-27).

| Piece | State |
| --- | --- |
| Launch args (Explorer syntax), single-instance hand-off | Done, tested |
| Windows default-explorer registration + Settings › Integration | Done. Tried live 2026-10-27 on Windows 11: folders and drives open in Filex (forwarded as tabs), unregister restores the registry byte-for-byte. **Win+E still opens Explorer** |
| Picker mode (`filex --pick`) | Done; Open / Save / Select Folder, type filter (listing and search), overwrite prompt, owned by and centered over the asking window |
| Windows dialog shim (x64) | Done; verified end to end through the built DLL (`examples/show_dialog.rs`): open, multi-select, folder, save to a new file, overwrite (Replace / No), cancel. Tried in real apps 2026-10-28 (see the table below) |
| MSI: ship the shim, unregister on uninstall | Done; the CI install smoke test turns both switches on and checks uninstall removes them |
| 32-bit shim, Linux, macOS | Not started |

Picker latency (release, time to visible window, 2026-10-27, 5 runs):
Filex median 339 ms (cold first run 1018 ms) vs the system dialog's
478 ms (cold 707 ms). No need to route picks through the running
instance for now.

Picker keys: Save starts in the name box with the stem selected; Enter
saves (and answers "Replace?" with Replace), Escape backs out of the
prompt, then cancels. With nothing selected, Enter accepts the current
folder (Select Folder). Picker processes skip the app-lifetime chores
(update check, crash upload, welcome tour, tag pruning): a Save dialog
must never make network requests.

## Scope decision (2026-10-27)

`docs/roadmap.md` Phase 3 and CLAUDE.md list "shell integration" as out of
scope. The owner decided on 2026-10-27 to open up two specific pieces of it:

1. **Filex as the default file explorer** — a "full takeover": folders,
   drives, This PC and Win+E open in Filex instead of Explorer (Win+E
   turned out not to be reachable through the registry; see §1).
2. **Replacing the OS Open/Save pickers** that other apps show.

Still out of scope: Explorer/Finder *plugins* (context-menu handlers,
property sheets, icon overlays, preview handlers) — Filex stays a
standalone app that the OS hands work to, not code that lives inside
Explorer.

Both features are **opt-in toggles, off by default, fully reversible**,
and live in Settings › Integration. Nothing is registered by the
installer.

## Shared groundwork

- `filex::launch` parses the command line, Explorer's own syntax
  included (`/select,<path>`, `/e,`, `/root,`, `/n`, `shell:` names,
  `::{CLSID}`), so anything that used to call `explorer.exe <args>`
  works unchanged once the verb points at `filex.exe`. Anything Filex
  cannot show (Control Panel, Recycle Bin, other virtual folders) is
  handed to the real `explorer.exe` with the original arguments.
- `filex::single_instance` forwards a launch to the running UI over a
  loopback TCP endpoint (same pattern as the index daemon:
  `endpoint.json` with port + random token in the per-user data dir; a
  lock file elects the primary). Targeted launches open a new tab in the
  running window and bring it to the front; a plain launch just
  activates it. Latency: one file read + a localhost connect before
  GPUI starts — well under the window-creation cost it saves.

## 1. Default explorer

### Windows (`shell_integration::windows`)

Per-user only (HKCU\Software\Classes overrides the merged HKCR view), no
elevation:

| Key | Effect |
| --- | --- |
| `Directory\shell` (Default) = `filex` | Double-click / Enter on folders |
| `Directory\shell\filex\command` = `"<exe>" "%1"` | |
| `Drive\shell` (Default) = `filex` + command | Drives |
| `CLSID\{52205fd8-5dfb-447d-801a-d0b52f2e83e1}\shell\opennewwindow\command` = `"<exe>"` | ShellExecute of the File Explorer launcher with `opennewwindow` |

`Folder\shell` is deliberately left alone: it covers virtual shell
folders Filex cannot display. Unregistering deletes the keys we wrote,
restores the previous `Directory\shell`/`Drive\shell` default verb, and
removes only the parent keys registration itself created (recorded as
`CreatedKeys` under `HKCU\Software\Filex\ShellBackup`). Many machines
already have empty `Directory\shell` / `Drive\shell` keys, and those stay.

Drives arrive as `"D:\"` through `"%1"`, which the MSVC argument rules
turn into `D:"`; `launch` maps a trailing `"` back to `\`.

**Win+E** (checked live on Windows 11, 2026-10-27): the hotkey does not
go through the launcher's registry verbs, so it keeps opening Explorer.
Taking it over would need a resident low-level keyboard hook, which is a
separate decision (it runs in every session, and AV tools watch for it).

The MSI's uninstall runs `filex.exe --unregister-shell` for the
uninstalling user (not on upgrades), so folders never point at a deleted
binary. **Known gap:** other users on the same machine who enabled the
toggle keep a dangling verb after a per-machine uninstall.

**Not interceptable without hooking Explorer** (and so not done):
`SHOpenFolderAndSelectItems` (browsers' "Show in folder"), apps that
spawn `explorer.exe` by full path, the taskbar's File Explorer pin.

### Linux

`xdg-mime default filex.desktop inode/directory`, plus the
`org.freedesktop.FileManager1` D-Bus service (`ShowFolders`,
`ShowItems`) that browsers use for "Show in folder". Not started — needs
a D-Bus dependency decision (zbus pulls an async runtime).

### macOS

Finder cannot be replaced system-wide. What is possible: a Services
menu item / "Open in Filex", and handling `open -a Filex <folder>`. Not
started.

## 2. System Open/Save dialogs

### Windows: per-user COM class override

Apps create the common file dialog with
`CoCreateInstance(CLSID_FileOpenDialog / CLSID_FileSaveDialog)` (and
`GetOpenFileName`/`GetSaveFileName` route through the same classes on
Vista+ unless the app supplies a hook or template). A per-user
`HKCU\Software\Classes\CLSID\{…}\InprocServer32` overrides the machine
registration for every non-elevated process, so a small in-proc DLL
(`filex-dialog-shim`, its own cdylib with no GPUI) is loaded instead —
no injection, no hooks.

The shim is a **proxy**: it creates the real dialog object straight from
`comdlg32.dll`'s class factory and forwards every call to it, recording
the state Filex needs (options, title, folder, file name, filters,
default extension). Only `Show` is intercepted: it spawns
`filex.exe --pick`, writes a `picker::PickRequest` to its stdin and
reads a `picker::PickResponse` from stdout. If anything goes wrong —
filex.exe missing, crash, malformed reply — or the app asked for
something Filex does not implement (`IFileDialogCustomize` controls,
`FOS_PICKFOLDERS` is supported but event-sink-driven dialogs are not),
it falls back to the real dialog's `Show`. A broken Filex must never
leave an app without a working Save dialog.

Elevated processes ignore HKCU class overrides and keep the system
dialog. 32-bit apps need the x86 build of the shim registered under
`WOW6432Node` (follow-up).

Apps that customize the dialog (`IFileDialogCustomize` — e.g. an
encoding drop-down) get the system dialog: the proxy forwards those
calls and marks itself customized. Apps that `Advise` an event sink get
`OnFileOk` after a Filex pick; `S_FALSE` from it falls back to the real
dialog.

Real apps, registered for real on Windows 11 (2026-10-28):

| App | Result |
| --- | --- |
| WinForms `OpenFileDialog` / `SaveFileDialog` (.NET Framework) | Filex; open, multi-select, save with default extension |
| WPF `Microsoft.Win32.SaveFileDialog` | Filex, once the proxy implemented `IOleWindow` (WPF casts to it inside `OnFileOk` and silently keeps the *initial* file name if that fails) |
| Chrome (Ctrl+O, `<input type=file>`) | Filex, owned by Chrome's window. Chrome answers `OnFileOk` with `E_NOTIMPL`, which means "no handler", not a refusal |
| Legacy `GetOpenFileName` / `GetSaveFileName` | System dialog: comdlg32 builds it internally and never reaches the COM class. Safe, just not replaced |
| Windows 11 Notepad (packaged/MSIX) | System dialog: packaged apps don't see per-user COM class overrides |

Paths go to the host without a `\\?\` prefix (`picker::host_paths`):
Shell parsing rejects verbatim paths, so a pick made after typed-path
navigation used to reach Chrome as an error. A Save target that doesn't
exist yet is parsed with `STGM_CREATE` in the bind context.

Diagnosing a host: set `FILEX_DIALOG_SHIM_TRACE=<file>` in its
environment and the shim appends every dialog call (and its own
decisions) to that file.

Manual check without registering anything:
`cargo build -p filex-dialog-shim --bin filex` then
`cargo run -p filex-dialog-shim --example show_dialog -- open <folder>`.
Set `FILEX_DIALOG_SHIM_DISABLE=1` to force the system dialog in one
process. The DLL is in use by any app that has shown a dialog, so MSI
upgrades may need those apps closed (Restart Manager prompts).

### Linux

`xdg-desktop-portal` FileChooser backend
(`org.freedesktop.impl.portal.FileChooser`) — the sanctioned way to
replace the picker for GTK/Qt/Flatpak apps. Same D-Bus decision as
above. Not started.

### macOS

`NSOpenPanel`/`NSSavePanel` cannot be replaced. Out of reach.

## Picker mode (`filex --pick`)

A Filex window in picker mode: the normal browser and instant search,
plus a bottom bar with the file-name field (Save), the file-type filter,
and the confirm/cancel buttons. Files hidden by the active filter are
not listed (folders always are). Enter/double-click on a file confirms
instead of opening it. The result goes to stdout as one JSON line and
the process exits. Logic that does not need GPUI (filter matching,
save-path resolution, default extension) lives in `filex::picker` with
plain unit tests.

Follow-up: picker startup is a fresh process (~GPUI window creation
time). If that is noticeably slower than the system dialog, route pick
requests to the running instance instead.

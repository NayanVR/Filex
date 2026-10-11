# First-run onboarding

Written 2026-10-11. Design record for the welcome flow a brand-new install
opens with. Motion prototype: [`onboarding-motion.html`](onboarding-motion.html)
(open in a browser; the intro and Magic demo beats are ported 1:1 to
gpui-ce — its aurora bands and gradient chrome were later dropped from the
app).

## Goal

Show what Filex is for — instant filename search — on the user's own disk
within a minute, the way Arc and Raycast sell themselves in their first
screens. Few choices, all reversible, every step skippable.

## Flow

| Step | Content | Notes |
|---|---|---|
| Welcome | Cinematic intro (below): warp of file names, the real logo assembling, "Find anything. Instantly." | Indexing is already running in the background. |
| Make it yours | Theme / accent / density on a live sample list | Reuses the Settings controls; changes apply at once. |
| Bring your favorites | Folders pinned in the OS file manager, ticked by default | Hidden when nothing new is found. |
| Try it | Live file counter, then a real search box (placeholder "resume pdf") with top 6 hits and measured latency | Uses the real search pipeline and `search_latency`. |
| Just say it → Your turn | A scripted Magic run played once ("move pdfs older than 30 days to Archive": command types out, chips pop, plan cascades, Apply glows), held 5 s, then the real thing: the search input in forced Magic mode over the app's own plan pane | The demo touches no files; a test runs its command through `magic::parse_with_gate`. "Try it yourself" skips ahead. Applying a real plan finishes onboarding first, so the job, conflict dialog and undo run in the app. |
| You're all set | Search / Quick Look / shortcuts keycaps (user's actual bindings) | |

Keys: Enter continues, Esc goes back (in the search box, Esc clears text
first). "Skip" finishes without importing. Finishing writes
`onboarding_pending = false` and appends ticked folders to `favorites`.

## Decisions

- **Full-window takeover, not a modal.** The flow renders *instead of* the
  workspace, so no file shortcut (Delete, rename…) can act on a list the
  user can't see.
- **New installs only.** `onboarding_pending` is true only in
  `Settings::first_run`; an existing settings file lacks the key and
  defaults to false. Settings → Appearance → "Welcome tour" replays it.
- **No permissions step** (decided with the user). The sidebar's Full Disk
  Access banner stays the macOS prompt.
- **"Indexed in N s" only for a build we watched.** If the index was
  already built (reinstall, replay), the counter just says "files indexed".
- **No animation on the search path.** Demo results paint immediately; the
  motion budget is spent on the intro, step changes and the final cascade.

## Favorites import (`filex::favorites_import`)

| OS | Source | Caveat |
|---|---|---|
| macOS | `~/Library/Application Support/com.apple.sharedfilelist/com.apple.LSSharedFileList.FavoriteItems.sfl3` (`.sfl2` before macOS 14): `NSKeyedArchiver` plist of bookmark blobs | TCC-protected: without Full Disk Access the read fails and the step hides. |
| Windows | Quick Access shell folder (`shell:::{679f85cb-…}`), enumerated over COM on its own MTA thread | Includes frequent folders, not only pins. |
| Linux | `$XDG_CONFIG_HOME/gtk-3.0/bookmarks` | KDE Dolphin's `user-places.xbel` not read yet. |

Parsers are pure and tested on every runner with synthetic fixtures; the
bookmark-blob layout assumptions are documented on `bookmark_path`.

## Motion (gpui-ce)

Every scene is a pure function of `t`, the seconds since its step appeared
(`ui::onboarding`), and the workspace requests another frame while a step
moves — the same model as the prototype, so the port is line-for-line.

- **Welcome (3.4 s, then idle loops):** 40 file names warp outward while a
  counter races to 2.4 M; 24 more implode into the centre; a gradient burst
  and three shockwave rings fire; the real logo assembles from its layers
  (`assets/branding/layers/`: back folder drops, front panel rises, "fx"
  pops with overshoot); a shine sweeps across; the halo breathes and the
  logo floats.
- **Every step:** a soft brand-blue wash from the top. Buttons, borders
  and chips are solid colours: the only gradients are the wash, the
  intro's burst and the logo's shine (decided after review: more read as
  noise).
- **Magic:** the scripted demo plays once and holds its finished plan for
  5 s before the step goes live (`MAGIC_HANDOVER`).
- **Search and live Magic:** nothing animates once the step has settled,
  so no frame work runs under a live search.
- **Hand-off to the app (0.8 s):** the last screen lifts away (0–0.3 s),
  then the onboarding sheet dissolves (0.2–0.8 s) while the app's main
  area settles 16 px up into place. Same for Skip and a Magic Apply, minus
  the lifted screen. The overlay doesn't block clicks.
- **Reduced motion** (`App::reduce_motion`): each scene renders its settled
  frame and no frames are requested.

Only opacity, position, size and two-stop linear gradients are animated.
gpui-ce has no text gradients, so "Instantly." is solid brand blue.

## Open items

- **Magic grammar bug (pre-existing, not onboarding):** `phrases.rs` maps
  "screenshot(s)" to `kind:image` only, although its comment says a
  screenshot is an image *named* like one. So "delete screenshots older
  than 1000 days" selects every old image, and on a big disk trips the
  1,000-op cap. The demo uses "pdfs" to stay honest. Fix and add a
  regression test in its own change.

- macOS: offer Finder import once Full Disk Access is granted later
  (today it is only discoverable via "Welcome tour").
- Linux: read KDE `user-places.xbel`.
- No `#[gpui::test]` harness exists yet: `Workspace::new` starts the real
  daemon client and reads the real settings file. The finish logic is a
  pure, tested function instead (`finish_settings`).

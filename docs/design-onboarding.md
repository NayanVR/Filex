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
| Loading | Live first index (below): the user's own file names spiral into a glowing core, a rolling file count, "Reading your disk… → Building your index… → Ready.", rotating tips | Shown only while the first index builds; not a counted step. Hands over to Welcome by itself. |
| Welcome | Cinematic intro (below): warp of file names, the real logo assembling, "Find anything. Instantly." | The counter races to the real index size. |
| Make it yours | Theme / accent / density on a live sample list | Reuses the Settings controls; changes apply at once. |
| Bring your favorites | Folders pinned in the OS file manager, ticked by default | Hidden when nothing new is found. |
| Try it | Live file counter, then a real search box (placeholder "resume pdf") with top 6 hits and measured latency | Uses the real search pipeline and `search_latency`. |
| Just say it → Your turn | A scripted Magic run played once ("move pdfs older than 30 days to Archive": command types out, chips pop, plan cascades, Apply glows), held 5 s, then the real thing: the search input in forced Magic mode over the app's own plan pane | The demo touches no files; a test runs its command through `magic::parse_with_gate`. "Try it yourself" skips ahead. Applying a real plan finishes onboarding first, so the job, conflict dialog and undo run in the app. |
| Open your folders here? | One checkbox, ticked by default: "Make Filex my default file explorer" | Shown only where Filex can be the default (Windows today) and isn't already; a background registry check decides. Applied on finish, never on Skip. See docs/design-default-explorer.md. |
| You're all set | Search / Quick Look / shortcuts keycaps (user's actual bindings) | |

Keys: Enter continues, Esc goes back (in the search box, Esc clears text
first). "Skip" finishes without importing or changing the default file
explorer. Finishing writes
`onboarding_pending = false` and appends ticked folders to `favorites`.

## Decisions

- **Full-window takeover, not a modal.** The flow renders *instead of* the
  workspace, so no file shortcut (Delete, rename…) can act on a list the
  user can't see.
- **New installs only.** `onboarding_pending` is true only in
  `Settings::first_run`; an existing settings file lacks the key and
  defaults to false. Settings → Appearance → "Welcome tour" replays it.
- **Default explorer is asked, ticked by default (2026-10-28).** The step
  sits after Magic, once the user has seen what Filex does. Its box
  starts ticked (owner's call, like the favorites' ticks), so finishing
  the tour makes Filex the default; unticking it or Skip keeps File
  Explorer. It is the one onboarding choice that reaches outside Filex,
  so it is stated plainly with where to undo it.
- **No permissions step** (decided with the user). The sidebar's Full Disk
  Access banner stays the macOS prompt.
- **"Indexed in N s" only for a build we watched.** If the index was
  already built (reinstall, replay), the counter just says "files indexed".
- **No animation on the search path.** Demo results paint immediately; the
  motion budget is spent on the intro, step changes and the final cascade.

- **Wait for the index before the welcome (2026-10-13).** Every later step
  (the live search demo above all) needs a built index, so the flow opens
  on a loading scene until the daemon reports it ready. It shows at least
  `LOADING_MIN` (2.4 s), then plays a 1.15 s completion beat. An index that
  was already built (reinstall, replay) skips it. After 20 s it offers
  "Continue while it finishes" so a huge disk never traps the user. The
  daemon reports the walk through `Status.progress` (`daemon::progress`):
  entries reached, walk vs. writing, and every 61st name, skipping hidden
  and system entries.

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
- **Loading (until the index is ready):** 22 lanes of real file names
  spiral in from beyond the window edge and are absorbed into a pulsing
  brand core, each landing with a ripple. Satellites circle three orbits
  and speed up once the walk ends and the index is being written; names
  stop arriving then. The count eases toward each status poll so it rolls
  rather than jumps. On completion the core flares, three shockwaves roll
  out and the scene clears for the welcome.
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

# Design system

The rules every UI component follows. The code is the source of truth;
this page says where to look and what not to do.

## Colour — `src/ui/theme.rs`

- **Semantic slots** on `Theme` (`bg`, `panel`, `elevated`, `hover`,
  `field`, `shadow`, `selected`, `stripe`, `border`, `text`, `text_dim`,
  `accent`, `on_accent`, `accent_selection`, `warn`, `success`, and the
  material slots `glass`, `glass_edge`, `sheen`, `float_shadow`, `glow`,
  `blur`). `hover` and `field` are translucent, so one value reads on both
  surfaces below.
  Read via `cx.theme()`. Dark, light and OLED palettes; accent presets
  and custom hex are applied by `Theme::resolve`.
- **Fixed palettes** in `theme::fixed`: identity colours that don't change
  with the theme (file-kind tints, Finder tag colours, paper artwork,
  modal scrim).
- **Rule:** `theme.rs` is the only UI file that spells a hex value. A new
  colour becomes a `Theme` slot if it differs per palette, otherwise a
  `fixed` fn.

## Surfaces (2026-10 refresh)

- **Shell** (`ui::shell_backdrop`): tab bar, nav bar, sidebar and status
  bar are one continuous surface, a diagonal wash from the accent-tinted
  `glow` (top left) into `panel`. No borders between them — don't add
  `border_b` to a bar. OLED keeps a flat black shell (`glow == panel`).
- **Canvas** (`bg`): the content heading, the file pane and the details
  panel sit on one raised card (`rounded_xl`, hairline `border`,
  `ui::soft_shadow`) inset from the window edge. Children of the canvas
  don't paint their own `bg` — gpui clips to the box, not the radius, so
  a filled child squares off the canvas corners.
- **No window translucency** (Mica, vibrancy): gpui-ce turns off
  ClearType text for the *whole window* whenever its background isn't
  `Opaque` (`Window::should_use_subpixel_rendering`), which visibly
  softens every label. Tried with Mica Alt in 2026-10 and rejected;
  revisit only if gpui-ce keeps ClearType on opaque regions. Glass is done
  *inside* the window instead (below). Dividers *inside* the canvas
  (column header, details panel edge) are fine.

## Glass material (2026-10-30)

Windows 11 and macOS have moved to translucent, lit surfaces; a flat
fill now reads as dated. Filex gets there without a translucent window:

- **Glass** (`ui::glass`, `ui::card`): transient popovers — context and
  dropdown menus, tooltips. A `glass` tint over gpui-ce's in-window
  `backdrop_blur` (Metal, DirectX and wgpu all implement it; it clips to
  the corner radius), a `glass_edge` hairline, a `sheen` top edge and a
  soft `float_shadow`. The window stays opaque, so ClearType stays on.
- **Solid glass** (`ui::solid_card`): dialogs, settings, onboarding
  cards. Same edge, sheen and shadow on opaque `elevated`, no blur. Over
  the modal scrim, real glass reads as murky grey and outline-only fields
  vanish into it; cards that paint their own fills would hide the blur
  anyway.
- **Lit edges** (`ui::sheen`, `ui::raised`): a 1px inset highlight on
  raised things — the canvas, active tab and segment, focused field, the
  active sidebar row, outlined buttons. Filled buttons get a top-lit
  gradient plus `fixed::gloss`.
- **Cost:** a blur pass only while a menu or tooltip is on screen, never
  on the canvas, the shell or anything inside a virtualized list. The
  shell gradient is one quad. Search and navigation paths are unchanged.
- **Settings → Appearance → Transparency effects** (`transparency_effects`)
  turns glass opaque and drops the blur (`Theme::with_transparency`), for
  readability or a weak GPU.
- **Don't** blur the full-window scrim or any large always-on surface
  (the sidebar, the canvas): with nothing but a smooth gradient behind
  them, the blur adds nothing and costs a full-size pass every frame.
- **Fields** (`top_bar::field`): filled `field` recess at rest, no
  outline; focus lifts to `bg` with an accent ring, errors ring in `warn`.
- **Elevated** (`elevated`): dialogs (`ui::solid_card`), and menus and
  tooltips with transparency off, sit one step above the canvas — white on light, lighter graphite on
  dark. Never use `panel` for something that floats.
- **Raised selection** (active tab, active segment): `bg` fill +
  `ui::raised` (contact shadow + lit edge) on a `field`/`panel` track.

## Window chrome

- Windows: the system titlebar is hidden; the tab bar is the titlebar.
  `tabs::drag_region` fills the gap after the tabs and
  `window_controls::caption_buttons` draws min/max/close tagged with
  `WindowControlArea`, so the OS still handles drag, double-click and
  Snap Layouts. Gate on `platform::draws_own_caption()`.
- macOS: transparent titlebar, traffic lights inset into the tab bar.
- Linux: native decorations for now (client-side decorations need their
  own resize edges).

## Motion — `src/ui/motion.rs`

- **Animate what doesn't make the user wait:** panels sliding, dialogs
  settling, menus/tooltips fading, the drag preview lifting, a wheel
  notch gliding. **Never** animate what they're waiting *for*: search
  results, a folder's contents, list rows (and never animate inside a
  virtualized list).
- Durations: `motion::PANEL` (180ms) for panels, `motion::FADE` (140ms)
  for fades; ease-out curves. Everything is interactive from the first
  frame — motion is decoration, not a gate.
- Panels: `motion::Toggle` gives a per-frame openness; clip the panel to
  `width × openness` so its content keeps its full-width layout.
- Dialogs: wrap the finished backdrop in `modal::present`.
- Scroll containers that feel jumpy on a mouse wheel: `overflow_hidden`
  + `track_scroll` + `motion::SmoothScroll` (touchpad deltas pass 1:1).

## Tooltips

- Every icon-only control has a tooltip. If the action has a shortcut,
  use `Workspace::action_tip(label, shortcut_id)` so the tooltip shows
  the live (user-remappable) binding, formatted per platform by
  `shortcuts::label` ("Ctrl+Shift+C" / "⇧⌘C").

## Drag and drop

- Rows and cards attach `external_drag_payload` alongside `on_drag`, so
  a drag leaving the window becomes an OS file drag: natively on macOS
  (gpui-ce), via the Shell's drag source on Windows
  (`platform::start_os_drag`). The Windows drag loop is modal: it runs
  on the UI thread but from a top-level thread timer, never inside a GPUI
  handler or task, or GPUI drops every window message during the drag
  ("RefCell already borrowed") and the window stops painting. Code on a
  GPUI handler's stack must not send messages to our own window either
  (e.g. `WindowFromPoint` → `WM_NCHITTEST`), for the same reason. Linux drags stay in-app for now.

## Metrics

- Row height and list icon size live on `Theme` (density-driven).
- Spacing: use GPUI's `gap_N` / `p_N` / `m_N` scale. Use `px(…)` only for a
  component's own fixed dimensions (bar heights, panel widths), and keep
  those next to the component.
- Radius: `rounded_md` for controls, `rounded_lg` for regular-size
  buttons and fields, `rounded_xl` for the canvas and floating
  cards, `rounded_full` for dots and pills.
- Type: Inter (`ui::fonts`), sizes from `text_xs` / `text_sm`. Per-size
  tokens are deferred until a third size is actually needed.

## Components — `src/ui/`

| Need | Use |
|---|---|
| Text button | `button::button(theme, id, label, Variant, Size)` |
| Button that can't be pressed yet | `button::disabled_button` |
| Icon-only button | `button::icon_button(theme, id, icon, color, IconSize)` |
| Popover (menu, dropdown) | `ui::card` (glass) |
| Dialog or card holding its own fills | `ui::solid_card` |
| Glass on a custom shape (tooltip) | `ui::glass(element, theme)` |
| Raised selection / lit edge | `ui::raised`, `ui::sheen` |
| Dialog | `modal::{backdrop, panel, title, message, buttons, button}` |
| Context menu | `menu::{overlay, panel, scroll_panel, model_row, icon_bar, icon_button, separator, heading}`; contents from `filex::context_menu` |
| Tooltip | `tooltip::text_tooltip` |
| Key hint | `kbd::keycap` |
| Icon glyph | `icon::ui_icon` (always pass an explicit colour) |
| Text field frame | `top_bar::field(theme, focused, error)` |
| Window caption buttons | `window_controls::caption_buttons` |

**Button variants:** `Primary` (accent fill, the Enter-key choice),
`Secondary` (outlined), `Danger` (warn fill, for irreversible actions).
Filled buttons dim on hover rather than losing their fill. **Sizes:**
`Regular` (32px, dialogs and panes), `Small` (22px, slim bars and inline
editors). **Icon sizes:** `Large` 30/18 (toolbar), `Medium` 24/16 (tab
strip), `Small` 20/12 (inline ✕).

Feature-specific controls (settings `switch`, magic-mode `checkbox`,
`filter_chip`, `tag_chip`, `magic_toggle`, `shortcut_button`) stay in
their feature module until a second feature needs them. Then they move
here.

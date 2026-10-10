# Design system

The rules every UI component follows. The code is the source of truth;
this page says where to look and what not to do.

## Colour — `src/ui/theme.rs`

- **Semantic slots** on `Theme` (`bg`, `panel`, `hover`, `selected`,
  `stripe`, `border`, `text`, `text_dim`, `accent`, `on_accent`,
  `accent_selection`, `warn`, `success`). Read via `cx.theme()`. Dark,
  light and OLED palettes; accent presets and custom hex are applied by
  `Theme::resolve`.
- **Fixed palettes** in `theme::fixed`: identity colours that don't change
  with the theme (file-kind tints, Finder tag colours, paper artwork,
  modal scrim).
- **Rule:** `theme.rs` is the only UI file that spells a hex value. A new
  colour becomes a `Theme` slot if it differs per palette, otherwise a
  `fixed` fn.

## Metrics

- Row height and list icon size live on `Theme` (density-driven).
- Spacing: use GPUI's `gap_N` / `p_N` / `m_N` scale. Use `px(…)` only for a
  component's own fixed dimensions (bar heights, panel widths), and keep
  those next to the component.
- Radius: `rounded_md` for controls, `rounded_lg` for cards and
  regular-size buttons, `rounded_full` for dots and pills.
- Type: Inter (`ui::fonts`), sizes from `text_xs` / `text_sm`. Per-size
  tokens are deferred until a third size is actually needed.

## Components — `src/ui/`

| Need | Use |
|---|---|
| Text button | `button::button(theme, id, label, Variant, Size)` |
| Button that can't be pressed yet | `button::disabled_button` |
| Icon-only button | `button::icon_button(theme, id, icon, color, IconSize)` |
| Floating surface (menu, dialog, card) | `ui::card` |
| Dialog | `modal::{backdrop, panel, title, message, buttons, button}` |
| Context menu | `menu::{overlay, panel, item, separator, heading}` |
| Tooltip | `tooltip::text_tooltip` |
| Key hint | `kbd::keycap` |
| Icon glyph | `icon::ui_icon` (always pass an explicit colour) |

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

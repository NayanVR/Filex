# Icon system

Decided 2026-10-30; symbols moved to Phosphor 2026-11-01. Design system with
live previews: https://claude.ai/artifact/YKt3QDB7f4tGGBCocqCDDR (the earlier
spec, https://claude.ai/artifact/7Hb7PbxxNN616KsdtTmPzP, predates Phosphor).
The code is the source of truth; this page records the rules and where they
live.

## Idea

One icon family, **Phosphor** (MIT, `@phosphor-icons/core` 2.1.1), drawn
three ways:

- **On a page** — a file. The page is a clean rounded sheet (no folded
  corner, since 2026-11-01) in the family's pale colour with a header band
  in the family's tint; the **Duotone** symbol carries the hue, and at card
  size the extension is bold type in the family colour straight on the page.
- **Debossed into a folder** — a special folder (Pictures, Downloads…), using
  the same icon's **Regular** outline at 60% in the panel's own darker hue.
  Folders are one soft front panel over a darker back: no paper sheets,
  gloss or highlight line; the front's top edge meets the back through a
  very soft shadow.
- **Bare** — sidebar places and drives (Duotone), tinted like any glyph.

Toolbar, menu and control glyphs are Phosphor **Regular** too, so the app has
one icon language. The custom 12-unit set and Lucide were replaced on
2026-11-01: the custom set was inconsistent (some symbols boxed, some bare)
and turned to blobs at 16px, and Lucide made a second style.

Files used to be one pale-blue page with a 75%-opacity mark, so a PDF, a photo
and a zip looked alike. Now each family has its own hue *and* symbol, so colour
never works alone (grayscale and colour-blind users still tell them apart).

## Pieces

| What | Where |
|---|---|
| Extension → family (12 families) | `filex::file_types` (pure, tested) |
| Symbols (Phosphor Duotone on its 256-unit grid: `#secondary` interior at 20% under `#primary`) | `assets/symbols/*.svg`, `ui::icon_art::Symbol` |
| Folder marks (Phosphor Regular) | `assets/symbols/marks/*.svg`, `Symbol::mark_source` |
| UI glyphs (Phosphor Regular, under the app's old file names) | `assets/icons/*.svg`, `ui::assets` |
| Colour ramps (OKLCH) and contrast tests | `ui::icon_palette` |
| Paper and folder SVG per size class | `ui::icon_art::{file_svg, folder_svg}` |
| Raster cache, label overlay, public API | `ui::icon::{file_icon, folder_icon}` |

`FileKind` stays: search chips and thumbnail decisions still use it. Families
refine it; `families_agree_with_file_kinds` keeps the two from contradicting
each other (disk images moved from Archive to App on purpose).

## Families

Text, PDF, Spreadsheet, Presentation, Image, Video, Audio, Archive, Code,
Font, App & installer, Other. Coloured families sit at least 20° apart in hue;
Font, App and Other keep only part of the chroma.

| Role (file in `assets/symbols`) | Phosphor icon |
|---|---|
| text, pdf, sheet, slides | text-align-left, book-open-text, table, presentation-chart |
| image, video, audio, archive | image, play, music-note, archive |
| code, font, app | code, text-aa, app-window |
| home, desktop, download, drive | house, desktop, download-simple, hard-drive |
| clock, star, trash | clock, star, trash |

PDF uses an open book, not `file-pdf`: that one is a page on a page and
repeats the card's "PDF" label. Other has no symbol (plain lines). To add a
symbol, take the Phosphor Duotone file, move the `opacity="0.2"` path into
`<g id="secondary" opacity=".2">` and the rest into `<g id="primary">`, and
add the role to `symbols!`; never draw a custom one.

## Colour

Every colour is a hue angle on fixed OKLCH lightness/chroma steps, so no family
is louder than another and every accent gives an equally calm folder.

- Files: paper top/bottom, fold (the header band's tint), edge, symbol
  ink, label (the extension's type colour). Dark pages are dimmed a step so
  a folder of documents doesn't glare.
- Folders: back top/bottom, front top/bottom and the debossed mark, on the
  accent's hue. Folders are the only accent-coloured icons.
- A grey accent (OKLCH chroma under 0.03) has no real hue, so its folders
  are a cool graphite at 15% of the usual chroma. Before this, `#808080` gave
  salmon folders and `#7f8080` cyan ones.
- Tests hold the targets: symbol vs paper ≥ 3:1 and the extension's type
  colour vs the page ≥ 4.5:1 (the label step was darkened to 0.50 / 0.46
  lightness for this), in both themes; a folder's mark colour ≥ 3:1 on its
  front for both default
  accents, every preset and extreme custom accents (near-black, near-white,
  grey, pure yellow, navy).
- Colours are computed, never spelled, so `theme.rs` stays the only UI file
  with hex literals.

## Size classes

Drawn separately, not scaled (`SizeClass::of`):

- **Micro, 16–19px** (Compact rows): the page filled with the fold tone and
  the symbol's outline alone (16 units); a flat folder with no soft edge or
  mark.
- **List, 20–31px** (Comfortable rows, drag preview): gradient page, header
  band, hairline edge, two-tone symbol (15 units); folder soft edge and mark
  (13 units).
- **Card, 32px+** (grid, details, Quick Look): adds the extension as type
  and a contact shadow under the page; symbol 13 units, folder mark 11.5.

Phosphor builds padding into its grid, so these sizes are larger than the old
set's.

The card's extension is GPUI text (bold, in the family's `label` colour)
laid over `LABEL_BOX`, so one raster serves every extension in a family.

## Cost

Each (family or folder symbol, size class, theme, raster bucket) is rasterized
once on first use (64/128/384px buckets by display size) and shared through
one GPUI image id. Folder entries for an old accent are dropped when the accent
changes. A row costs a mutex lock and a hash lookup, the same as the folder
cache before it. Search and navigation paths are unchanged.

## Thumbnails

A preview fills the cell; icon artwork in a grid is drawn at 80% of it, for
images too while their preview is missing, so cards stay balanced.

## Not built yet

- Badges (shortcut, online-only, syncing, no access, broken link) and the
  single bottom-right slot: needs the listing to expose links, cloud state and
  permissions.
- Thumbnail rules: square crop with a hairline in list rows, play mark and
  duration on video cards, document previews clipped to the paper.
- Visual check on macOS (verified on Linux/wgpu and Windows/DirectX).

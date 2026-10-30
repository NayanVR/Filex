# Icon system

Decided 2026-10-30. Interactive spec with previews:
https://claude.ai/artifact/7Hb7PbxxNN616KsdtTmPzP. The code is the source of
truth; this page records the rules and where they live.

## Idea

One set of filled, two-tone symbols, drawn three ways:

- **On paper** — a file. The paper is the family's pale colour; the symbol
  and the folded corner carry the hue.
- **Pressed into a folder** — a special folder (Pictures, Downloads…).
- **Bare** — sidebar places and drives, tinted like any glyph.

Files used to be one pale-blue page with a 75%-opacity mark, so a PDF, a photo
and a zip looked alike. Now each family has its own hue *and* symbol, so colour
never works alone (grayscale and colour-blind users still tell them apart).

## Pieces

| What | Where |
|---|---|
| Extension → family (12 families) | `filex::file_types` (pure, tested) |
| Symbols (12-unit grid, `#secondary` at 35% under `#primary`) | `assets/symbols/*.svg`, `ui::icon_art::Symbol` |
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

## Colour

Every colour is a hue angle on fixed OKLCH lightness/chroma steps, so no family
is louder than another and every accent gives an equally calm folder.

- Files: paper top/bottom, fold, edge, symbol ink, label. Dark pages are
  dimmed a step so a folder of documents doesn't glare.
- Folders: back, front, lip highlight, pressed mark, sheet, edge — on the
  accent's hue. Folders are the only accent-coloured icons.
- Tests hold the targets: symbol vs paper ≥ 3:1, white label text ≥ 4.5:1,
  in both themes; a folder's mark ≥ 3:1 on its front for every accent.
- Colours are computed, never spelled, so `theme.rs` stays the only UI file
  with hex literals.

## Size classes

Drawn separately, not scaled (`SizeClass::of`):

- **Micro, 16–19px** (Compact rows): paper filled with the fold tone, solid
  symbol, no gradient, highlight or folder mark.
- **List, 20–31px** (Comfortable rows, drag preview): fold, two-tone symbol,
  hairline edge, folder lip and mark.
- **Card, 32px+** (grid, details, Quick Look): extension label, top
  highlight, contact shadow, sheets of paper inside folders.

The card label's text is GPUI text laid over `LABEL_BOX`, so one raster serves
every extension in a family.

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
- Context-menu and toolbar glyphs are still Lucide outlines; move them to
  symbols where one exists.
- Visual check on Windows and macOS (verified on Linux/wgpu only).

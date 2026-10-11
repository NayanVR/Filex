# Filex app icon

`filex.svg` is the source artwork. The other files are generated from it and
checked in so release builds do not need graphics tools:

- `filex.icns`: macOS app bundle and Dock icon.
- `filex.ico`: Windows executable, taskbar, and installer icon.
- `filex.png`: Linux desktop launcher icon (256 x 256).

`folder.svg` is the blank folder shape derived from `filex.svg` with the `fx`
removed. Filex recolors its gradient stops from the selected accent at runtime,
then draws a separate folder-type symbol over it.

Known OS folders and common names get a symbol automatically. A folder's
context menu has **Folder Icon…** to choose a symbol, force a plain folder,
or return to automatic selection. Compact 16px list rows omit the center
symbol because it is not legible at that size.

`layers/` holds `filex.svg` split into its three parts (`back`, `front`,
`mark`), each on the full canvas so they stack back into the mark exactly.
The onboarding intro assembles the logo from them. Regenerate them by hand
if the artwork changes.

On macOS, install `librsvg` and Pillow, then run
`python3 packaging/generate-icons.py` after changing the SVG. The script uses
macOS `iconutil` for the `.icns` file. Commit all generated files with the SVG.

#!/usr/bin/env python3
"""Generate platform icons from assets/branding/filex.svg on macOS.

Requires librsvg (`rsvg-convert`), Pillow, and macOS `iconutil`.
The generated files are committed so release builds need none of these tools.
"""

from io import BytesIO
from pathlib import Path
import subprocess
import tempfile

from PIL import Image


ROOT = Path(__file__).resolve().parents[1]
BRANDING = ROOT / "assets" / "branding"


def main() -> None:
    # The source artwork is 700 x 601. Leave consistent clear space around it
    # on a square canvas so Dock and Start icons do not clip the folder tabs.
    raster = subprocess.check_output(
        ["rsvg-convert", "--width", "896", str(BRANDING / "filex.svg")]
    )
    artwork = Image.open(BytesIO(raster)).convert("RGBA")
    master = Image.new("RGBA", (1024, 1024), (0, 0, 0, 0))
    master.alpha_composite(
        artwork, ((1024 - artwork.width) // 2, (1024 - artwork.height) // 2)
    )

    master.resize((256, 256), Image.Resampling.LANCZOS).save(
        BRANDING / "filex.png"
    )
    master.save(
        BRANDING / "filex.ico",
        format="ICO",
        sizes=[(n, n) for n in (16, 24, 32, 48, 64, 128, 256)],
    )

    with tempfile.TemporaryDirectory() as tmp:
        iconset = Path(tmp) / "filex.iconset"
        iconset.mkdir()
        for size in (16, 32, 128, 256, 512):
            master.resize((size, size), Image.Resampling.LANCZOS).save(
                iconset / f"icon_{size}x{size}.png"
            )
        for size in (16, 32, 128, 256, 512):
            pixels = size * 2
            master.resize((pixels, pixels), Image.Resampling.LANCZOS).save(
                iconset / f"icon_{size}x{size}@2x.png"
            )
        subprocess.run(
            ["iconutil", "-c", "icns", str(iconset), "-o", str(BRANDING / "filex.icns")],
            check=True,
        )


if __name__ == "__main__":
    main()

"""Trims a screenshot to the size a floating window would have.

A tiling compositor stretches the window over the whole output, so a capture of the window
rectangle also holds Omarchy's bar and whatever the monitor overlay drew. Cropping from the
top-left keeps exactly the application.

    python3 tools/crop.py OUT.png       (OMAD_SHOT_SIZE=1280x820)
"""

import os
import sys

from PIL import Image


def main(path: str) -> None:
    width, height = (int(part) for part in os.environ.get("OMAD_SHOT_SIZE", "1280x820").split("x"))
    image = Image.open(path)
    if image.width > width or image.height > height:
        image.crop((0, 0, min(width, image.width), min(height, image.height))).save(path)


if __name__ == "__main__":
    main(sys.argv[1])

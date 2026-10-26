"""Runs CUA's computer server against this machine, with two patches it needs here.

1. **FastAPI version skew.** `cua-computer-server` 0.3.46 passes `websocket_max_size=` to
   `FastAPI(...)`, which no released FastAPI accepts (0.139 and 0.162 both raise
   `TypeError`), so the server cannot start as shipped. The keyword is dropped.

2. **Capture on Xwayland.** The Linux backend takes screenshots with `PIL.ImageGrab`, which
   reads the X root window. Under Xwayland a GL client's pixels live in a Wayland surface,
   not in the X root, so every capture comes back black while input works perfectly. When
   `OMAD_CUA_OUTPUT` names a Hyprland output, `screenshot` is taken with `grim` on that
   output instead.

Input (clicks, keys) still goes through CUA's own handler and XTEST, so what this file
changes is the picture, not the driving.

    DISPLAY=:95 OMAD_CUA_OUTPUT=HEADLESS-1 python tools/cua/server.py --port 7799
"""

from __future__ import annotations

import asyncio
import base64
import io
import os
import subprocess
import sys


def patch_fastapi() -> None:
    import fastapi

    original = fastapi.FastAPI.__init__

    def __init__(self, *args, **kwargs):  # noqa: N807 - it is a constructor
        kwargs.pop("websocket_max_size", None)
        original(self, *args, **kwargs)

    fastapi.FastAPI.__init__ = __init__


def patch_screenshot(output: str) -> None:
    """Makes the Linux handler take its picture with grim, on the named Hyprland output."""
    from computer_server.handlers import linux

    async def screenshot(self, format: str = "png", quality: int = 95):  # noqa: A002
        png = subprocess.run(
            ["grim", "-o", output, "-t", "png", "-"],
            capture_output=True,
            check=False,
        )
        if png.returncode != 0 or not png.stdout:
            return {"success": False, "error": png.stderr.decode() or "grim failed"}
        return {
            "success": True,
            "image_data": base64.b64encode(png.stdout).decode("ascii"),
            "format": "png",
        }

    linux.LinuxAutomationHandler.screenshot = screenshot


def main(argv: list[str]) -> int:
    patch_fastapi()
    output = os.environ.get("OMAD_CUA_OUTPUT")
    if output:
        patch_screenshot(output)
    from computer_server.cli import main as server_main

    sys.argv = ["computer-server", *argv]
    return server_main() or 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))

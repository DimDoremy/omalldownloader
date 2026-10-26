"""Is a bar widget id placed in the user's shell.json?

`omarchy plugin disable` removes a widget from the bar and `enable` puts it back, but a bar
without the widget looks the same as a bar whose widget failed to load. The installer asks
this instead of hoping.

    python3 tools/bar-placed.py ~/.config/omarchy/shell.json dimdoremy.omalldownloader
"""

import json
import sys


def main(path: str, plugin: str) -> int:
    try:
        layout = json.load(open(path))["bar"]["layout"]
    except Exception:
        return 1
    placed = any(plugin in str(item) for section in layout.values() for item in section)
    return 0 if placed else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1], sys.argv[2]))

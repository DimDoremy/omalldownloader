#!/bin/sh
# The browser starts this program; it must not print anything but native-messaging frames
# on stdout, so everything the app might log goes to a file instead.
exec "$HOME/.local/bin/omalld" native-host --port "${OMAD_BRIDGE_PORT:-14370}" 2>>"${XDG_CACHE_HOME:-$HOME/.cache}/omalldownloader/native-host.log"

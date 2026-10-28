#!/usr/bin/env bash
# Builds the x86_64 AppImage from a release build of this workspace.
#
#   packaging/make-appimage.sh [--out DIR] [--tool PATH]
#
# It bundles only our own binaries: the GUI needs the system's Wayland/X11/Vulkan libraries,
# so bundling them (which is what a fully portable AppImage does) is a separate job. This is
# enough to run the released build on the machine that made it, and in CI.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
out=$root/dist-appimage
tool=${APPIMAGETOOL:-appimagetool}

while [ $# -gt 0 ]; do
  case "$1" in
    --out) out=$2; shift 2 ;;
    --tool) tool=$2; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

echo "==> building the release binaries"
(cd "$root" && cargo build --release -p omad-app -p omad-cli)

appdir=$(mktemp -d)
trap 'rm -rf "$appdir"' EXIT
mkdir -p "$appdir/usr/bin" \
         "$appdir/usr/share/applications" \
         "$appdir/usr/share/icons/hicolor/256x256/apps"

install -m 0755 "$root/target/release/omalldownloader" "$appdir/usr/bin/"
install -m 0755 "$root/target/release/omalld" "$appdir/usr/bin/"
install -m 0644 "$root/packaging/omad.desktop" "$appdir/usr/share/applications/omalldownloader.desktop"

# The extension ships inside the image, so a user who has only the AppImage can still load
# it: `--appimage-extract` and point "Load unpacked" at usr/share/omalldownloader/extension.
(cd "$root" && node extension/scripts/build.mjs >/dev/null)
mkdir -p "$appdir/usr/share/omalldownloader/extension"
cp -r "$root/extension/dist/." "$appdir/usr/share/omalldownloader/extension/"
install -m 0644 "$root/packaging/icons/omalldownloader-256.png" \
  "$appdir/usr/share/icons/hicolor/256x256/apps/omalldownloader.png"

# AppImage wants the desktop file and an icon at the root of the AppDir as well.
install -m 0644 "$root/packaging/omad.desktop" "$appdir/omalldownloader.desktop"
install -m 0644 "$root/packaging/icons/omalldownloader-256.png" "$appdir/omalldownloader.png"

cat > "$appdir/AppRun" <<'RUN'
#!/bin/sh
HERE="$(dirname "$(readlink -f "$0")")"
export PATH="$HERE/usr/bin:$PATH"
exec "$HERE/usr/bin/omalldownloader" "$@"
RUN
chmod 0755 "$appdir/AppRun"

mkdir -p "$out"
name="OmaLLDownloader-x86_64.AppImage"
echo "==> packaging $name"
# `--appimage-extract-and-run` keeps this working where FUSE is not mounted (containers, CI).
ARCH=x86_64 "$tool" --appimage-extract-and-run "$appdir" "$out/$name"
echo "==> $out/$name"

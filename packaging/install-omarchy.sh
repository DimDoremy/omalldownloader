#!/usr/bin/env bash
# Installs OmaLLDownloader into the user's Omarchy session: the binaries, the desktop
# entry the launcher reads, the icon, and the bar widget.
#
#   packaging/install-omarchy.sh            # build release, install, enable the widget
#   packaging/install-omarchy.sh --debug    # build debug (faster) and install that
#   packaging/install-omarchy.sh --no-build # use what the target directory already holds
#   packaging/install-omarchy.sh --remove   # take it all back out
#
# Nothing here needs root: every file goes under $HOME.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/.." && pwd)

plugin_id="dimdoremy.omalldownloader"
bin_dir="${XDG_BIN_HOME:-$HOME/.local/bin}"
data_dir="${XDG_DATA_HOME:-$HOME/.local/share}"
app_dir="$data_dir/omalldownloader"
applications="$data_dir/applications"
icons="$data_dir/icons/hicolor"
plugins="$HOME/.config/omarchy/plugins/$plugin_id"

build=1
remove=0
profile=release
for arg in "$@"; do
  case "$arg" in
    --debug) profile=debug ;;
    --release) profile=release ;;
    --no-build) build=0 ;;
    --remove) remove=1 ;;
    -h|--help) sed -n '2,9p' "$0"; exit 0 ;;
    *) echo "unknown option $arg" >&2; exit 2 ;;
  esac
done

if [[ $remove == 0 && $build == 0 ]]; then
  # --no-build: use whichever profile is there rather than failing on a missing one.
  if [[ ! -x "$root/target/$profile/omalldownloader" ]]; then
    if [[ -x "$root/target/debug/omalldownloader" ]]; then
      profile=debug
      echo "==> no release build; using the debug one"
    else
      echo "nothing to install: build first, or drop --no-build" >&2
      exit 1
    fi
  fi
fi

if [[ $remove == 0 && $build == 1 ]]; then
  echo "==> building ($profile)"
  if [[ $profile == release ]]; then
    (cd "$root" && cargo build --release -p omad-app -p omad-cli)
  else
    (cd "$root" && cargo build -p omad-app -p omad-cli)
  fi
fi

if [[ $remove == 1 ]]; then
  echo "==> removing"
  rm -f "$bin_dir/omalldownloader" "$bin_dir/omalld"
  rm -rf "$app_dir"
  rm -f "$applications/omalldownloader.desktop"
  for size in 16 24 32 48 64 128 256 512; do
    rm -f "$icons/${size}x${size}/apps/omalldownloader.png"
  done
  rm -rf "$plugins"
  if command -v omarchy >/dev/null 2>&1; then
    omarchy plugin disable "$plugin_id" >/dev/null 2>&1 || true
    omarchy-shell shell rescanPlugins >/dev/null 2>&1 || true
  fi
  command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "$applications" >/dev/null 2>&1 || true
  command -v gtk-update-icon-cache >/dev/null 2>&1 && gtk-update-icon-cache -q -t -f "$data_dir/icons/hicolor" >/dev/null 2>&1 || true
  echo "removed. The task list and downloads were left alone."
  exit 0
fi

echo "==> installing the binaries into $bin_dir"
install -d "$bin_dir"
install -m 0755 "$root/target/$profile/omalldownloader" "$bin_dir/omalldownloader"
install -m 0755 "$root/target/$profile/omad" "$bin_dir/omalld"

echo "==> desktop entry and icon"
install -d "$applications"
sed "s|^Exec=omalldownloader|Exec=$bin_dir/omalldownloader|" "$here/omad.desktop" > "$applications/omalldownloader.desktop"
chmod 0644 "$applications/omalldownloader.desktop"
for size in 16 24 32 48 64 128 256 512; do
  install -d "$icons/${size}x${size}/apps"
  install -m 0644 "$here/icons/omalldownloader-$size.png" "$icons/${size}x${size}/apps/omalldownloader.png"
done
command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "$applications" || true
command -v gtk-update-icon-cache >/dev/null 2>&1 && gtk-update-icon-cache -q -t -f "$data_dir/icons/hicolor" >/dev/null 2>&1 || true

# Native messaging: the transport a browser can always reach, whatever it thinks of
# loopback HTTP. The manifest names the extension ids allowed to use it.
echo "==> native messaging host"
install -m 0755 "$here/native-host.sh" "$bin_dir/omalld-native-host"
for browser_dir in \
  "$HOME/.config/microsoft-edge/NativeMessagingHosts" \
  "$HOME/.config/microsoft-edge-dev/NativeMessagingHosts" \
  "$HOME/.config/google-chrome/NativeMessagingHosts" \
  "$HOME/.config/chromium/NativeMessagingHosts"; do
  parent=$(dirname "$browser_dir")
  [ -d "$parent" ] || continue
  install -d "$browser_dir"
  printf '%s\n' "{
  \"name\": \"com.dimdoremy.omalldownloader\",
  \"description\": \"OmaLLDownloader bridge\",
  \"path\": \"$bin_dir/omalld-native-host\",
  \"type\": \"stdio\",
  \"allowed_origins\": [\"chrome-extension://iphlaibgoabcbaepfakbjefffeipgoni/\"]
}" > "$browser_dir/com.dimdoremy.omalldownloader.json"
  echo "    $browser_dir/com.dimdoremy.omalldownloader.json"
done

echo "==> bar widget into $plugins"
install -d "$plugins"
install -m 0644 "$here/omarchy/bar-plugin/manifest.json" "$plugins/manifest.json"
install -m 0644 "$here/omarchy/bar-plugin/OmaLLDownloaderWidget.qml" "$plugins/OmaLLDownloaderWidget.qml"

if command -v omarchy >/dev/null 2>&1; then
  echo "==> validating the plugin against the shell schema"
  omarchy plugin validate "$plugins" || {
    echo "the plugin did not validate; leaving it disabled" >&2
    exit 1
  }
  echo "==> enabling and placing it in the bar"
  omarchy-shell shell rescanPlugins >/dev/null 2>&1 || true
  omarchy plugin enable "$plugin_id" right >/dev/null 2>&1 || true
  omarchy-shell shell rescanPlugins >/dev/null 2>&1 || true

  # `plugin disable` takes the widget out of the bar and `enable` puts it back. A bar
  # without it is silent, so check rather than assume.
  if ! python3 "$here/../tools/bar-placed.py" "$HOME/.config/omarchy/shell.json" "$plugin_id"; then
    echo "warning: the bar widget is enabled but not placed; run:" >&2
    echo "  omarchy plugin enable $plugin_id right" >&2
  fi
fi

cat <<EOF

Done.

  Launcher   OmaLLDownloader   (the icon is in your applications menu)
  Command    $bin_dir/omalld --help
  Bar        $plugin_id — drag it in the bar, or:
             omarchy bar move $plugin_id right

If the bar widget shows nothing at first, give the shell a moment and click it:
it reads \`omalld tasks --json\` every couple of seconds.
EOF

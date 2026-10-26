#!/usr/bin/env bash
# Screenshots OmaLLDownloader without disturbing the user's screen.
#
# Hyprland can create a headless output (`hyprctl output create headless`). This focuses that
# output, opens the app there at a fixed size, captures it with grim, and removes the output
# again. The user's own monitors are never shown and never captured; their focus is put back
# where it was.
#
#   tools/shot-omarchy.sh OUT.png [--seed] [--keep]
#
# --seed   download two small files over loopback first, so the list is not empty
# --keep   leave the app and the headless output in place (for poking at it)
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/.." && pwd)
out=${1:-/tmp/omalldownloader.png}
shift || true
seed=0
keep=0
for arg in "$@"; do
  case "$arg" in
    --seed) seed=1 ;;
    --keep) keep=1 ;;
    *) echo "unknown option $arg" >&2; exit 2 ;;
  esac
done

unset XDG_STATE_HOME || true
export PATH="$HOME/.local/bin:$PATH"
bin=${OMAD_BIN:-$root/target/debug/omalldownloader}
cli=${OMAD_CLI:-$root/target/debug/omad}

# One private output per run, named by the compositor. Anything left over from an
# interrupted run is removed first, so a run never stacks outputs.
cleanup_outputs() {
  for name in $(hyprctl monitors -j 2>/dev/null | python3 -c '
import json, sys
for m in json.load(sys.stdin):
    if m["name"].startswith("HEADLESS-"):
        print(m["name"])
'); do
    hyprctl output remove "$name" >/dev/null 2>&1 || true
  done
}
cleanup_outputs
hyprctl output create headless >/dev/null
sleep 0.5
output=$(hyprctl monitors -j 2>/dev/null | python3 -c '
import json, sys
for m in json.load(sys.stdin):
    if m["name"].startswith("HEADLESS-"):
        print(m["name"]); break
')
if [[ -z ${output:-} ]]; then
  echo "could not create a headless output" >&2
  exit 1
fi

previous=$(hyprctl monitors -j 2>/dev/null | python3 -c '
import json, sys
for m in json.load(sys.stdin):
    if m["focused"]:
        print(m["name"]); break
')

restore() {
  if [[ $keep == 0 ]]; then
    pkill -f omalldownloader 2>/dev/null || true
    if [[ -n ${previous:-} ]]; then
      hyprctl eval "hl.dispatch(hl.dsp.focus({monitor=\"$previous\"}))" >/dev/null 2>&1 || true
    fi
    hyprctl output remove "$output" >/dev/null 2>&1 || true
    cleanup_outputs
  fi
}
trap restore EXIT

hyprctl eval "hl.dispatch(hl.dsp.focus({monitor=\"$output\"}))" >/dev/null
sleep 0.4

if [[ $seed == 1 ]]; then
  echo "==> seeding: two files over loopback"
  work=$(mktemp -d)
  head -c 320000 /dev/urandom >"$work/ubuntu-24.04-desktop.iso"
  head -c 180000 /dev/urandom >"$work/dataset-sample.zip"
  port=18990
  (cd "$work" && exec python3 -m http.server "$port" --bind 127.0.0.1 >/dev/null 2>&1) &
  server=$!
  sleep 1
  "$cli" --allow-private-network add "http://127.0.0.1:$port/ubuntu-24.04-desktop.iso" >/dev/null
  "$cli" --allow-private-network add "http://127.0.0.1:$port/dataset-sample.zip" >/dev/null
  kill "$server" 2>/dev/null || true
  rm -rf "$work"
fi

pkill -f omalldownloader 2>/dev/null || true
sleep 0.3
echo "==> opening $bin on $output"
OMAD_WINDOW=${OMAD_WINDOW:-1280x820} setsid "$bin" >/tmp/omalldownloader-gui.log 2>&1 &

for _ in $(seq 40); do
  sleep 0.25
  hyprctl clients -j 2>/dev/null | grep -q '"class": "omalldownloader"' && break
done
sleep 1.2
# Crop to the app's own rectangle: the output also holds Omarchy's bar and its
# monitor-label overlay, which are not part of the application.
geometry=$(hyprctl clients -j 2>/dev/null | python3 -c "
import json, sys
want = '$output'
for c in json.load(sys.stdin):
    if c.get('class') == 'omalldownloader' and c.get('monitor') == want:
        x, y = c['at']
        w, h = c['size']
        print('%d,%d %dx%d' % (x, y, w, h))
        break
")
if [[ -n $geometry ]]; then
  grim -g "$geometry" "$out"
else
  grim -o "$output" "$out"
fi

# A tiling compositor fills the output, so the window rectangle is the whole monitor and
# the capture includes Omarchy's bar. Trim to the size a floating window would have.
OMAD_SHOT_SIZE=${OMAD_SHOT_SIZE:-1280x820} python3 "$here/crop.py" "$out"
echo "$out"

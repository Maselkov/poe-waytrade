#!/bin/sh
# Renders the panel for an item and saves it as an image, in a nested,
# invisible KWin (kwin_wayland --virtual) with its own D-Bus session and its
# own daemon, so nothing in your desktop session is touched. For checking a
# UI change without the game.
#
# Needs vkcube, a built layer (build/) and daemon (daemon/target/debug/).
# Usage: tests/ui-snapshot.sh <item.txt> <out.png|out.ppm> [ui dir]
#   item.txt  item text as copied from the game (maps open the map check)
#   ui dir    a directory holding ui/*, default assets/ (to compare against
#             another version: git archive HEAD assets/ui | tar -x -C dir
#             --strip-components=1)
# Other POE_WAYTRADE_DEMO_* variables (docs/DEVELOPMENT.md) pass through, and
# so does POE_WAYTRADE_SNAPSHOT_FRAMES, to wait for a trade search.
# A .png needs ImageMagick (magick).
set -eu
if [ -z "${WAYTRADE_IN_DBUS:-}" ]; then
    WAYTRADE_IN_DBUS=1 exec dbus-run-session -- "$0" "$@"
fi
[ $# -ge 2 ] || { sed -n 's/^# \{0,1\}//; 2,15p' "$0"; exit 2; }
repo=$(cd "$(dirname "$0")/.." && pwd)
item=$(realpath "$1")
out=$(realpath -m "$2")
ui=$(realpath "${3:-$repo/assets}")
tmp=$(mktemp -d)
sock=waytrade-snapshot-$$
daemon_sock=poe-waytrade-snapshot-$$

kwin_wayland --virtual --no-lockscreen --socket "$sock" --width 1280 --height 900 >"$tmp/kwin.log" 2>&1 &
kwin=$!
POE_WAYTRADE_SOCKET=$daemon_sock "$repo/daemon/target/debug/poe-waytrade-daemon" >"$tmp/daemon.log" 2>&1 &
daemon=$!
# KWin's D-Bus-activated services (knighttimed) outlive the private bus and
# hold this script's stdout, which hangs a pipe reading it: stop everything
# started on that bus.
bus_children() {
    for env in /proc/[0-9]*/environ; do
        [ -r "$env" ] || continue  # only our own processes'
        tr '\0' '\n' 2>/dev/null <"$env" | grep -qxF "DBUS_SESSION_BUS_ADDRESS=$DBUS_SESSION_BUS_ADDRESS" || continue
        pid=${env#/proc/}; pid=${pid%/environ}
        [ "$pid" = $$ ] || echo "$pid"
    done
}
cleanup() {
    kill "$daemon" "$kwin" 2>/dev/null || true
    for _ in 1 2 3 4 5 6 7 8 9 10; do kill -0 "$kwin" 2>/dev/null || break; sleep 0.2; done
    kill -KILL "$kwin" 2>/dev/null || true
    # shellcheck disable=SC2046
    kill $(bus_children) 2>/dev/null || true
    rm -rf "$tmp"
}
trap cleanup EXIT
sleep 3
# The panel asks the daemon once: wait until its prices are in (a minute at
# most), or it shows "Loading prices". A currency's price needs them.
printf 'Item Class: Stackable Currency\nRarity: Currency\nDivine Orb\n' >"$tmp/probe.txt"
for _ in $(seq 60); do
    POE_WAYTRADE_SOCKET=$daemon_sock "$repo/daemon/target/debug/poe-waytrade-daemon" query "$tmp/probe.txt" 2>/dev/null |
        grep -q "^status.loading" || break
    sleep 1
done

# The layer saves a frame after 300 frames, then the app keeps running.
WAYLAND_DISPLAY=$sock POE_WAYTRADE_SOCKET=$daemon_sock POE_WAYTRADE_UI_DIR="$ui" POE_WAYTRADE_DEMO_ITEM="$item" \
    POE_WAYTRADE_SNAPSHOT="$tmp/shot.ppm" POE_WAYTRADE_LOG="$tmp/layer.log" \
    VK_ADD_IMPLICIT_LAYER_PATH="$repo/build/layer" ENABLE_POE_WAYTRADE=1 \
    vkcube --wsi wayland >/dev/null 2>&1 &
cube=$!
# 20 s, or longer with POE_WAYTRADE_SNAPSHOT_FRAMES (60 fps or more).
for _ in $(seq $((100 + ${POE_WAYTRADE_SNAPSHOT_FRAMES:-0} / 12))); do
    grep -q "snapshot saved" "$tmp/layer.log" 2>/dev/null && break
    kill -0 "$cube" 2>/dev/null || break
    sleep 0.2
done
kill "$cube" 2>/dev/null || true
wait "$cube" 2>/dev/null || true

if [ ! -s "$tmp/shot.ppm" ]; then
    echo "FAIL: no snapshot. Layer log:"
    cat "$tmp/layer.log" 2>/dev/null || true
    exit 1
fi
case $out in
*.ppm) cp "$tmp/shot.ppm" "$out" ;;
*) magick "$tmp/shot.ppm" "$out" ;;
esac
echo "saved $out"

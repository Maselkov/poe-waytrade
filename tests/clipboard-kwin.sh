#!/bin/sh
# Checks that Ctrl+D keeps the user's clipboard, in a nested, invisible
# KWin (kwin_wayland --virtual) with its own D-Bus session, so nothing in
# your desktop session is touched. Needs wl-clipboard, vkcube and a built
# layer (build/). Usage: tests/clipboard-kwin.sh [text|image]
#
# POE_WAYTRADE_TEST_COPY=1 makes the layer do a Ctrl+D two seconds in
# without sending keys; this script plays the game's copy with wl-copy.
set -eu
if [ -z "${WAYTRADE_IN_DBUS:-}" ]; then
    WAYTRADE_IN_DBUS=1 exec dbus-run-session -- "$0" "$@"
fi
mode=${1:-text}
repo=$(cd "$(dirname "$0")/.." && pwd)
tmp=$(mktemp -d)
sock=waytrade-cliptest-$$
kwin_wayland --virtual --no-lockscreen --socket "$sock" --width 1280 --height 900 >"$tmp/kwin.log" 2>&1 &
kwin=$!
cleanup() {
    # wl-copy's background servers exit with the compositor.
    kill "$kwin" 2>/dev/null || true
    for _ in 1 2 3 4 5 6 7 8 9 10; do kill -0 "$kwin" 2>/dev/null || break; sleep 0.2; done
    kill -KILL "$kwin" 2>/dev/null || true
    rm -rf "$tmp"
}
trap cleanup EXIT
sleep 3
export WAYLAND_DISPLAY=$sock

if [ "$mode" = image ]; then
    head -c 300000 /dev/urandom >"$tmp/original"
    wl-copy -t image/png <"$tmp/original" >/dev/null 2>&1
else
    printf 'original clipboard text, ünïcode' >"$tmp/original"
    wl-copy <"$tmp/original" >/dev/null 2>&1
fi

POE_WAYTRADE_TEST_COPY=1 POE_WAYTRADE_SOCKET=poe-waytrade-cliptest POE_WAYTRADE_LOG="$tmp/layer.log" \
    VK_ADD_IMPLICIT_LAYER_PATH="$repo/build/layer" ENABLE_POE_WAYTRADE=1 \
    timeout 12 vkcube --wsi wayland >/dev/null 2>&1 &
cube=$!
sleep 3.5
printf 'Item Class: Rings\nRarity: Rare\nWoe Loop\nIron Ring\n' | wl-copy >/dev/null 2>&1  # "the game's copy"
sleep 1.5

type=text/plain
[ "$mode" = image ] && type=image/png
if timeout 3 wl-paste -n -t "$type" | cmp -s - "$tmp/original"; then
    result=0
    echo "OK: the clipboard was restored ($mode)"
else
    result=1
    echo "FAIL: the clipboard wasn't restored ($mode)"
    cat "$tmp/layer.log"
fi
kill $cube 2>/dev/null || true
wait $cube 2>/dev/null || true
exit $result

#!/bin/sh
# The waytrade launcher: it passes the command's exit status through, turns
# the layer on, starts a daemon for the command's lifetime, and leaves an
# already-running daemon alone. Uses its own socket name.
set -eu
daemon=$(realpath "${1:-daemon/target/debug/poe-waytrade-daemon}")
launcher=$(realpath packaging/waytrade)
export POE_WAYTRADE_SOCKET=poe-waytrade-ci POE_WAYTRADE_DAEMON="$daemon"
export XDG_STATE_HOME="$(mktemp -d)" XDG_CONFIG_HOME="$(mktemp -d)" XDG_CACHE_HOME="$(mktemp -d)"
log="$XDG_STATE_HOME/poe-waytrade/daemon.log"
fail() { echo "FAIL: $*" >&2; exit 1; }

sh -n "$launcher"

# Exit status and environment reach the command.
set +e; "$launcher" sh -c 'exit 7'; [ $? -eq 7 ] || fail "exit status not passed through"; set -e
"$launcher" sh -c '[ "$ENABLE_POE_WAYTRADE" = 1 ] && [ "$PROTON_ENABLE_WAYLAND" = 1 ]' || fail "env not set"
PROTON_ENABLE_WAYLAND=0 "$launcher" sh -c '[ "$PROTON_ENABLE_WAYLAND" = 0 ]' || fail "PROTON_ENABLE_WAYLAND overridden"

# A daemon runs while the command does, and is gone after.
"$launcher" sh -c 'sleep 2; pgrep -f -- "^$POE_WAYTRADE_DAEMON --exit-with-parent" >/dev/null' || fail "no daemon while running"
grep -q "listening on @poe-waytrade-ci" "$log" || fail "daemon didn't listen: $(cat "$log")"
sleep 1
! pgrep -f -- "^$daemon --exit-with-parent" >/dev/null || fail "daemon left running"

# Killing the launcher outright doesn't leave the daemon behind.
"$launcher" sleep 30 & launched=$!
sleep 2
pgrep -f -- "^$daemon --exit-with-parent" >/dev/null || fail "no daemon to orphan"
kill -9 "$launched"
sleep 1
! pgrep -f -- "^$daemon --exit-with-parent" >/dev/null || fail "daemon outlived a killed launcher"
pkill -f "^sleep 30$" || true

# With a daemon already serving, the launcher's own exits quietly.
"$daemon" & other=$!
sleep 1
"$launcher" sh -c 'sleep 1'
grep -q "already served" "$log" || fail "second daemon didn't step aside: $(cat "$log")"
kill -0 "$other" || fail "the running daemon was stopped"
kill "$other"
# The game comes from Steam's app id; PoE2's daemon logs to its own file.
SteamAppId=2694490 "$launcher" sh -c '[ "$POE_WAYTRADE_GAME" = poe2 ]' || fail "PoE2 not picked by its app id"
SteamAppId=238960 "$launcher" sh -c '[ "$POE_WAYTRADE_GAME" = poe1 ]' || fail "PoE1 not picked by its app id"
SteamAppId=2694490 POE_WAYTRADE_GAME=poe1 "$launcher" sh -c '[ "$POE_WAYTRADE_GAME" = poe1 ]' \
    || fail "POE_WAYTRADE_GAME didn't override the app id"
SteamAppId=2694490 "$launcher" sh -c 'sleep 2'
grep -q "for poe2" "$XDG_STATE_HOME/poe-waytrade/daemon-poe2.log" \
    || fail "PoE2's daemon didn't log to daemon-poe2.log: $(cat "$XDG_STATE_HOME/poe-waytrade/daemon-poe2.log")"

echo "launcher OK"

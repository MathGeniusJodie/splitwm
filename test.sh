#!/bin/bash
# Launch splitwm in Xephyr for testing.
# Usage: ./test.sh [display_num] [WxH]
set -u
DPY="${1:-1}"
SIZE="${2:-1280x800}"
DIR="$(cd "$(dirname "$0")" && pwd)"

# Kill only processes this script started, by PID — a pattern pkill could
# take down another session's Xephyr. Vars are initialised before the trap
# so an early exit under `set -u` can't expand them unset.
WM_PID=""
XEPHYR_PID=""
cleanup() {
    [ -n "$WM_PID" ] && kill "$WM_PID" 2>/dev/null
    [ -n "$XEPHYR_PID" ] && kill "$XEPHYR_PID" 2>/dev/null
}
trap cleanup EXIT INT TERM

Xephyr ":${DPY}" -ac -screen "${SIZE}" -resizeable >/tmp/xephyr.log 2>&1 &
XEPHYR_PID=$!
for i in $(seq 1 40); do
    kill -0 "$XEPHYR_PID" 2>/dev/null || break
    DISPLAY=":${DPY}" xdotool getdisplaygeometry >/dev/null 2>&1 && break
    sleep 0.25
done
if ! kill -0 "$XEPHYR_PID" 2>/dev/null; then echo "Xephyr failed"; cat /tmp/xephyr.log; exit 1; fi
DISPLAY=":${DPY}" xdotool getdisplaygeometry >/dev/null 2>&1 || { echo "Xephyr failed"; cat /tmp/xephyr.log; exit 1; }

cargo build --manifest-path "$DIR/Cargo.toml" 2>/tmp/build.log || { cat /tmp/build.log; exit 1; }

echo "Xephyr :${DPY} (pid $XEPHYR_PID). Running splitwm..."
DISPLAY=":${DPY}" TERMINAL="${TERMINAL:-xterm}" "$DIR/target/debug/splitwm" >/tmp/splitwm.log 2>&1 &
WM_PID=$!
echo "splitwm pid $WM_PID"
wait $WM_PID
STATUS=$?
WM_PID=""
exit "$STATUS"

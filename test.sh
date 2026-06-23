#!/bin/bash
# Launch splitwm in Xephyr for testing.
# Usage: ./test.sh [display_num] [WxH]
set -u
DPY="${1:-1}"
SIZE="${2:-1280x800}"
DIR="$(cd "$(dirname "$0")" && pwd)"

pkill -f "Xephyr :${DPY}" 2>/dev/null
sleep 0.3
Xephyr ":${DPY}" -ac -screen "${SIZE}" -resizeable >/tmp/xephyr.log 2>&1 &
XEPHYR_PID=$!
sleep 1
if ! kill -0 "$XEPHYR_PID" 2>/dev/null; then echo "Xephyr failed"; cat /tmp/xephyr.log; exit 1; fi

cargo build --manifest-path "$DIR/Cargo.toml" 2>/tmp/build.log || { cat /tmp/build.log; kill $XEPHYR_PID; exit 1; }

echo "Xephyr :${DPY} (pid $XEPHYR_PID). Running splitwm..."
DISPLAY=":${DPY}" TERMINAL="${TERMINAL:-xterm}" "$DIR/target/debug/splitwm" >/tmp/splitwm.log 2>&1 &
WM_PID=$!
echo "splitwm pid $WM_PID"
trap "kill $WM_PID $XEPHYR_PID 2>/dev/null" INT TERM
wait $WM_PID
kill $XEPHYR_PID 2>/dev/null

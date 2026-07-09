#!/bin/bash
# Self-contained visual test drive on the headless backend: boots the
# compositor offscreen (nothing pops up on the host desktop), drives it
# over the debug channel, and screenshots each step into /tmp/splitshots.
# Requires: alacritty, ImageMagick. The channel acks every command on
# stdout, so each step is synchronized, not timed.
set -u
DIR="$(cd "$(dirname "$0")" && pwd)"
SHOTS=/tmp/splitshots
LOG=/tmp/drive-splitwm.log
FIFO=$(mktemp -u /tmp/drive-splitwm.XXXXXX.fifo)
mkdir -p "$SHOTS"; rm -f "$SHOTS"/*.png
mkfifo "$FIFO"

WM_PID=""
FAILED=0
cleanup() {
    if [ -n "$WM_PID" ]; then
        # $WM_PID may be the dbus-run-session wrapper: end its children
        # (splitwm, dbus-daemon) too — parent-scoped, never by name.
        pkill -TERM -P "$WM_PID" 2>/dev/null
        kill "$WM_PID" 2>/dev/null
    fi
    rm -f "$FIFO"
}
trap cleanup EXIT

# cargo test/check don't produce the binary this script runs; build it here
# so the drive never exercises a stale target/debug/splitwm.
cargo build --manifest-path "$DIR/Cargo.toml" 2>/tmp/drive-build.log || { cat /tmp/drive-build.log; exit 1; }

# A private session bus lets the compositor's notification daemon own
# org.freedesktop.Notifications instead of losing it to the live session's
# (whose refusal bubble would squat in every screenshot).
BUS=""
command -v dbus-run-session >/dev/null && BUS="dbus-run-session --"
SPLITWM_HEADLESS=1 SPLITWM_DEBUG_CHANNEL=1 TERMINAL=alacritty \
    SPLITWM_WALLPAPER="${SPLITWM_WALLPAPER:-/tmp/wall.png}" \
    $BUS "$DIR/target/debug/splitwm" <"$FIFO" >"$LOG" 2>&1 &
WM_PID=$!
exec 3>"$FIFO" # hold the channel's write end open for the whole drive

# wait_line <pattern>: wait for a line in the log (an ack, or startup).
wait_line() {
    for _ in $(seq 1 100); do
        grep -qF "$1" "$LOG" && return 0
        kill -0 "$WM_PID" 2>/dev/null || { echo "WM DIED waiting for: $1"; tail -20 "$LOG"; exit 1; }
        sleep 0.1
    done
    echo "TIMEOUT waiting for: $1"; FAILED=1; return 1
}

wait_line "WAYLAND_DISPLAY=" || exit 1
echo "WM up ($(grep -m1 WAYLAND_DISPLAY= "$LOG"))"

key() { echo "key $1" >&3; wait_line "ok key $1"; sleep 0.4; }
shot() { echo "shot $SHOTS/$1.png" >&3; wait_line "ok shot $SHOTS/$1.png" && echo "shot $1"; }
term() { key super+Return; sleep 1.2; }
# A solid-colour terminal so window-content sampling has something to read.
cterm() { echo "spawn alacritty -o colors.primary.background='\"$1\"'" >&3; wait_line "ok spawn"; sleep 1.2; }

cmd() { echo "$1" >&3; wait_line "ok $1"; sleep 0.3; }

# 1: each new terminal opens in its own split, right of the focused one;
# the taskbar mirrors them left-to-right
term; term
shot 01_two_splits
cterm "#006400"
shot 02_third_split_right

# 2: split vertically (Mod4+h) -> empty placeholder below; the next
# terminal fills it
key super+h
term
shot 03_split_v_filled

# 3: focus prev / next (brackets cycle focus too)
key super+Left
shot 04_focus_left
key super+bracketright
shot 05_focus_right

# 4: move the focused split left (Mod4+Shift+[), then reorder by drag:
# grab the leftmost split's titlebar, drop it on the left half of the
# first taskbar tile
key super+shift+bracketleft
shot 06_moved_left
cmd "press 90 33"
cmd "motion 400 400"
cmd "release 30 752"
shot 07_dragged_first

# 5: grow / shrink
key super+l
key super+l
shot 08_grow
key super+shift+l
shot 09_shrink

# 6: scroll the canvas (many splits)
term; term; term
shot 10_many_splits
echo "scroll 2" >&3; wait_line "ok scroll 2"
sleep 0.6 # let the glide settle
shot 11_scrolled

# 7: close the focused split (Mod4+q): the window and its split go
# together; neighbours reclaim the space once the client exits
key super+q
sleep 1.0
shot 12_closed

echo "=== splitwm.log tail ==="
tail -5 "$LOG"
if [ "$FAILED" -eq 0 ]; then
    echo "DONE"
else
    echo "DONE (with failures)"
    exit 1
fi

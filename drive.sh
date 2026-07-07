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

# 1: two terminals stacked as tabs in one split
term; term
shot 01_two_tabs

# 2: split horizontally (Mod4+v) -> new empty split to the right
key super+v
shot 02_split_h
# put a coloured terminal in the new split -> content-sampled accent
cterm "#006400"
shot 03_term_in_split2

# 3: split vertically (Mod4+h)
key super+h
term
shot 04_split_v

# 4: focus prev / next
key super+Left
shot 05_focus_left
key super+Right
shot 06_focus_right

# 5: cycle tabs in first split
key super+Left
key super+bracketright
shot 07_next_tab

# 6: grow / shrink
key super+l
key super+l
shot 08_grow
key super+shift+l
shot 09_shrink

# 7: scroll the canvas (many splits)
key super+v; term
key super+v; term
key super+v; term
shot 10_many_splits
echo "scroll 2" >&3; wait_line "ok scroll 2"
sleep 0.6 # let the glide settle
shot 11_scrolled

# 8: close a split (Mod4+q)
key super+q
shot 12_closed

echo "=== splitwm.log tail ==="
tail -5 "$LOG"
if [ "$FAILED" -eq 0 ]; then
    echo "DONE"
else
    echo "DONE (with failures)"
    exit 1
fi

#!/bin/bash
# Full self-contained Xephyr test drive. Runs everything in one process group
# so the nested X session lives for the duration of this script.
set -u
DIR="$(cd "$(dirname "$0")" && pwd)"
SHOTS=/tmp/splitshots
mkdir -p "$SHOTS"; rm -f "$SHOTS"/*.png
export DISPLAY_HOST="${DISPLAY:-:0}"

# Initialised before the trap: under `set -u` an early exit would otherwise
# expand unset variables inside cleanup.
WM_PID=""
XEPHYR_PID=""
FAILED=0
cleanup() {
    [ -n "$WM_PID" ] && kill "$WM_PID" 2>/dev/null
    [ -n "$XEPHYR_PID" ] && kill "$XEPHYR_PID" 2>/dev/null
}
trap cleanup EXIT

# Overridable so a server already on :1 (someone's live session) can't get
# hijacked: default to a display an interactive session won't be using.
DPY=":${SPLITWM_DRIVE_DPY:-9}"

# cargo test/check don't produce the binary this script runs; build it here
# so the drive never exercises a stale target/debug/splitwm.
cargo build --manifest-path "$DIR/Cargo.toml" 2>/tmp/drive-build.log || { cat /tmp/drive-build.log; exit 1; }

DISPLAY="$DISPLAY_HOST" Xephyr "$DPY" -ac -screen 1280x800 >/tmp/drive-xephyr.log 2>&1 &
XEPHYR_PID=$!
for i in $(seq 1 40); do
    kill -0 "$XEPHYR_PID" 2>/dev/null || { echo "Xephyr DOWN"; cat /tmp/drive-xephyr.log; exit 1; }
    DISPLAY="$DPY" xdotool getdisplaygeometry >/dev/null 2>&1 && break
    sleep 0.25
done
kill -0 "$XEPHYR_PID" 2>/dev/null || { echo "Xephyr DOWN"; cat /tmp/drive-xephyr.log; exit 1; }
DISPLAY="$DPY" xdotool getdisplaygeometry >/dev/null 2>&1 || { echo "Xephyr DOWN"; cat /tmp/drive-xephyr.log; exit 1; }
echo "Xephyr up"

DISPLAY="$DPY" TERMINAL=xterm SPLITWM_WALLPAPER=/tmp/wall.png "$DIR/target/debug/splitwm" >/tmp/drive-splitwm.log 2>&1 &
WM_PID=$!
sleep 0.7
kill -0 $WM_PID 2>/dev/null || { echo "WM DOWN"; cat /tmp/drive-splitwm.log; exit 1; }
echo "WM up"

shot() { DISPLAY="$DPY" import -window root "$SHOTS/$1.png" 2>/dev/null && echo "shot $1" || { echo "shot $1 FAILED"; FAILED=1; }; }
key() { DISPLAY="$DPY" xdotool key --clearmodifiers "$1"; sleep 0.4; }
term() { DISPLAY="$DPY" xterm -e "sleep 3000" & sleep 1.2; }
# A solid-colour terminal so window-content sampling has something to read.
cterm() { DISPLAY="$DPY" xterm -bg "$1" -e "sleep 3000" & sleep 1.2; }

# 1: two terminals stacked as tabs in one split
term; term
shot 01_two_tabs

# 2: split horizontally (Mod4+v) -> new empty split to the right
key "super+v"
shot 02_split_h
# put a coloured terminal in the new split -> content-sampled accent
cterm "DarkGreen"
sleep 0.6
shot 03_term_in_split2

# 3: split vertically (Mod4+h)
key "super+h"
term
shot 04_split_v

# 4: focus prev / next
key "super+Left"
shot 05_focus_left
key "super+Right"
shot 06_focus_right

# 5: cycle tabs in first split
key "super+Left"
key "super+bracketright"
shot 07_next_tab

# 6: grow / shrink
key "super+l"
key "super+l"
shot 08_grow
key "super+shift+l"
shot 09_shrink

# 7: scroll the canvas (many splits)
key "super+v"; term
key "super+v"; term
key "super+v"; term
shot 10_many_splits
DISPLAY="$DPY" xdotool keydown super; DISPLAY="$DPY" xdotool click 4; DISPLAY="$DPY" xdotool click 4; DISPLAY="$DPY" xdotool keyup super
sleep 0.4
shot 11_scrolled

# 8: close a split (Mod4+q)
key "super+q"
shot 12_closed

echo "=== splitwm.log tail ==="
tail -5 /tmp/drive-splitwm.log
if [ "$FAILED" -eq 0 ]; then
    echo "DONE"
else
    echo "DONE (with failures)"
    exit 1
fi

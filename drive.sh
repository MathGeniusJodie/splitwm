#!/bin/bash
# Full self-contained Xephyr test drive. Runs everything in one process group
# so the nested X session lives for the duration of this script.
set -u
DIR="$(cd "$(dirname "$0")" && pwd)"
SHOTS=/tmp/splitshots
mkdir -p "$SHOTS"; rm -f "$SHOTS"/*.png
export DISPLAY_HOST="${DISPLAY:-:0}"

cleanup() { kill $WM_PID 2>/dev/null; kill $XEPHYR_PID 2>/dev/null; }
trap cleanup EXIT

DISPLAY="$DISPLAY_HOST" Xephyr :1 -ac -screen 1280x800 >/tmp/xephyr.log 2>&1 &
XEPHYR_PID=$!
for i in $(seq 1 40); do
    DISPLAY=:1 xdotool getdisplaygeometry >/dev/null 2>&1 && break
    sleep 0.25
done
DISPLAY=:1 xdotool getdisplaygeometry >/dev/null 2>&1 || { echo "Xephyr DOWN"; cat /tmp/xephyr.log; exit 1; }
echo "Xephyr up"

DISPLAY=:1 TERMINAL=xterm SPLITWM_WALLPAPER=/tmp/wall.png "$DIR/target/debug/splitwm" >/tmp/splitwm.log 2>&1 &
WM_PID=$!
sleep 0.7
kill -0 $WM_PID 2>/dev/null || { echo "WM DOWN"; cat /tmp/splitwm.log; exit 1; }
echo "WM up"

shot() { DISPLAY=:1 import -window root "$SHOTS/$1.png" 2>/dev/null && echo "shot $1"; }
key() { DISPLAY=:1 xdotool key --clearmodifiers "$1"; sleep 0.4; }
term() { DISPLAY=:1 xterm -e "sleep 3000" & sleep 1.2; }
# A solid-colour terminal so window-content sampling has something to read.
cterm() { DISPLAY=:1 xterm -bg "$1" -e "sleep 3000" & sleep 1.2; }

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
DISPLAY=:1 xdotool keydown super; DISPLAY=:1 xdotool click 4; DISPLAY=:1 xdotool click 4; DISPLAY=:1 xdotool keyup super
sleep 0.4
shot 11_scrolled

# 8: close a split (Mod4+q)
key "super+q"
shot 12_closed

echo "=== splitwm.log tail ==="
tail -5 /tmp/splitwm.log
echo "DONE"

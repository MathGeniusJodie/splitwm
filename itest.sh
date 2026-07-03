#!/bin/bash
# Asserting integration test: boots splitwm in Xephyr and checks the ICCCM/
# EWMH surface the unit tests can't reach — WM_STATE transitions, client-list
# maintenance, focus, withdrawal, and restore-on-exit. Exits nonzero on any
# failed assertion. Requires: Xephyr, xdotool, xprop, xterm.
set -u
DIR="$(cd "$(dirname "$0")" && pwd)"
# Overridable so parallel runs (or another session's Xephyr) don't collide.
DPY=":${SPLITWM_ITEST_DPY:-7}"
FAILS=0
WM_PID=""
XEPHYR_PID=""

cleanup() {
    [ -n "$WM_PID" ] && kill "$WM_PID" 2>/dev/null
    [ -n "$XEPHYR_PID" ] && kill "$XEPHYR_PID" 2>/dev/null
}
trap cleanup EXIT

fail() { echo "FAIL: $*"; FAILS=$((FAILS + 1)); }
pass() { echo "ok:   $*"; }

# assert_eq <label> <got> <want>
assert_eq() {
    if [ "$2" = "$3" ]; then pass "$1"; else fail "$1: got '$2', want '$3'"; fi
}

# wait_for <label> <cmd...>: poll a condition up to ~5s, then assert it.
wait_for() {
    local label="$1"; shift
    for _ in $(seq 1 50); do
        if "$@" >/dev/null 2>&1; then pass "$label"; return 0; fi
        sleep 0.1
    done
    fail "$label (timed out: $*)"
    return 1
}

hexid() { printf '0x%x' "$1"; }
wm_state() { xprop -display "$DPY" -id "$1" WM_STATE 2>/dev/null | sed -n 's/.*window state: //p'; }
client_list() { xprop -display "$DPY" -root _NET_CLIENT_LIST 2>/dev/null; }
# Anchored so 0x40000 cannot match 0x400001: an id ends at a comma or EOL.
in_client_list() { client_list | grep -qiE "$(hexid "$1")(,|$)"; }
not_in_client_list() { ! in_client_list "$1"; }
active_win() { xprop -display "$DPY" -root _NET_ACTIVE_WINDOW | grep -oi '0x[0-9a-f]*'; }
key() { DISPLAY="$DPY" xdotool key --clearmodifiers "$1"; sleep 0.3; }

# spawn_xterm <varname>: start an xterm and store its window id.
spawn_xterm() {
    DISPLAY="$DPY" xterm -e 'sleep 3000' &
    local pid=$!
    local win=""
    for _ in $(seq 1 50); do
        win=$(DISPLAY="$DPY" xdotool search --pid "$pid" 2>/dev/null | head -1)
        [ -n "$win" ] && break
        sleep 0.1
    done
    [ -n "$win" ] || { fail "xterm (pid $pid) window never appeared"; return 1; }
    eval "$1=$win"
    sleep 0.4 # let manage() finish (grabs, list update, focus)
}

# --- boot ---
cargo build --manifest-path "$DIR/Cargo.toml" 2>/tmp/itest-build.log \
    || { cat /tmp/itest-build.log; exit 1; }
DISPLAY="${DISPLAY:-:0}" Xephyr "$DPY" -ac -screen 1280x800 >/tmp/itest-xephyr.log 2>&1 &
XEPHYR_PID=$!
wait_for "Xephyr up" env DISPLAY="$DPY" xdotool getdisplaygeometry || exit 1
DISPLAY="$DPY" TERMINAL=xterm "$DIR/target/debug/splitwm" >/tmp/itest-wm.log 2>&1 &
WM_PID=$!
wait_for "EWMH WM check published" \
    sh -c "xprop -display $DPY -root _NET_SUPPORTING_WM_CHECK | grep -q 0x"

# --- manage: first client is listed, Normal, and active ---
spawn_xterm W1 || exit 1
wait_for "w1 in _NET_CLIENT_LIST" in_client_list "$W1"
assert_eq "w1 WM_STATE Normal" "$(wm_state "$W1")" "Normal"
assert_eq "w1 is _NET_ACTIVE_WINDOW" "$(active_win)" "$(hexid "$W1")"

# --- displacement: second client takes the slot, first goes Iconic ---
spawn_xterm W2 || exit 1
wait_for "w2 in _NET_CLIENT_LIST" in_client_list "$W2"
assert_eq "displaced w1 goes Iconic" "$(wm_state "$W1")" "Iconic"
assert_eq "w2 WM_STATE Normal" "$(wm_state "$W2")" "Normal"
assert_eq "w2 is active" "$(active_win)" "$(hexid "$W2")"

# --- split + taskbar cycle: both clients end up visible ---
# The split keeps focus on w2's leaf; move to the new empty split first so
# the taskbar cycle fills it instead of displacing w2.
key super+v
key super+Right
key super+bracketright
wait_for "w1 Normal again after split+cycle" \
    sh -c "[ \"\$(xprop -display $DPY -id $W1 WM_STATE | sed -n 's/.*window state: //p')\" = Normal ]"
assert_eq "w2 stays Normal across split" "$(wm_state "$W2")" "Normal"

# --- polite close (WM_DELETE_WINDOW): active client leaves the list ---
ACTIVE=$(active_win)
key super+shift+c
wait_for "closed client leaves _NET_CLIENT_LIST" not_in_client_list "$ACTIVE"

# --- withdrawal: an unmapped client is unmanaged, not remapped ---
spawn_xterm W3 || exit 1
wait_for "w3 in _NET_CLIENT_LIST" in_client_list "$W3"
DISPLAY="$DPY" xdotool windowunmap --sync "$W3"
wait_for "withdrawn w3 leaves _NET_CLIENT_LIST" not_in_client_list "$W3"
# A forcible remap would flip WM_STATE back to Normal; Withdrawn proves the
# WM honoured the withdrawal and left the window unmapped.
wait_for "withdrawn w3 is not forcibly remapped" \
    sh -c "[ \"\$(xprop -display $DPY -id $W3 WM_STATE | sed -n 's/.*window state: //p')\" = Withdrawn ]"

# --- restore on exit: a taskbar'd (Iconic) client is remapped ---
spawn_xterm W4 || exit 1 # fills the split the earlier close emptied
spawn_xterm W5 || exit 1 # displaces w4 to the taskbar
assert_eq "displaced w4 goes Iconic" "$(wm_state "$W4")" "Iconic"
ICONIC=$W4
if [ "$(wm_state "$ICONIC")" = "Iconic" ]; then
    key super+shift+e # Quit
    wait_for "WM exits on Quit" sh -c "! kill -0 $WM_PID 2>/dev/null" && WM_PID=""
    wait_for "iconic client restored to Normal on WM exit" \
        sh -c "[ \"\$(xprop -display $DPY -id $ICONIC WM_STATE | sed -n 's/.*window state: //p')\" = Normal ]"
else
    fail "expected w4 to be Iconic before quit"
fi

echo
if [ "$FAILS" -eq 0 ]; then
    echo "ALL PASS"
else
    echo "$FAILS FAILURE(S)"
    echo "=== wm log tail ==="; tail -20 /tmp/itest-wm.log
    exit 1
fi

#!/bin/bash
# Real-VT test drive for the tty backend. Run it from a bare VT login
# (Ctrl+Alt+F3); it refuses to start anywhere else. From the X session,
# `./vttest.sh kill` ends the test compositor and `./vttest.sh log`
# shows its log. Never pkill: the live X session runs a binary with the
# same name.
set -u
DIR="$(cd "$(dirname "$0")" && pwd)"
LOG=/tmp/splitwm-tty.log
PIDFILE=/tmp/splitwm-tty.pid
BIN="$DIR/target/debug/splitwm"

# The recorded pid, but only while it is really our test binary — a
# stale pidfile must never aim `kill` at a recycled pid.
test_pid() {
    [ -f "$PIDFILE" ] || return 1
    local pid
    pid=$(cat "$PIDFILE")
    [ -n "$pid" ] || return 1
    grep -qa "target/debug/splitwm" "/proc/$pid/cmdline" 2>/dev/null || return 1
    echo "$pid"
}

case "${1:-run}" in
kill)
    if pid=$(test_pid); then
        kill "$pid" && echo "sent SIGTERM to test compositor (pid $pid)"
    else
        echo "no test compositor running (nothing matching $PIDFILE)"
    fi
    exit 0
    ;;
log)
    exec tail -n 40 "$LOG"
    ;;
run) ;;
*)
    echo "usage: $0 [run|kill|log]"
    exit 1
    ;;
esac

# --- run: only on a real VT ------------------------------------------
case "$(tty)" in
/dev/tty[0-9]*) ;;
*)
    echo "This starts a compositor that takes over the seat."
    echo "Run it from a VT login: Ctrl+Alt+F3, log in, then ./vttest.sh"
    echo "(from here you can only './vttest.sh kill' or './vttest.sh log')"
    exit 1
    ;;
esac

if pid=$(test_pid); then
    echo "test compositor already running (pid $pid); './vttest.sh kill' first"
    exit 1
fi

echo "building (cargo build --features tty)..."
cargo build --features tty --manifest-path "$DIR/Cargo.toml" 2>/tmp/vttest-build.log ||
    { cat /tmp/vttest-build.log; exit 1; }

THIS_VT="$(tty | grep -o '[0-9]*$')"
cat <<EOF

  About to take over this VT with the Wayland splitwm.

  Inside:   Mod4+Return opens alacritty; splits/bindings as on master.
  Leave:    Ctrl+Alt+F2 (or wherever X is) switches back to X.
  Kill it:  from X, run  ./vttest.sh kill   (log: ./vttest.sh log)
  Return:   Ctrl+Alt+F$THIS_VT comes back here.

  If the screen wedges and VT switching is dead, ssh in and
  kill \$(cat $PIDFILE).

EOF
read -rp "Enter to start (Ctrl+C to bail) " _

# A DISPLAY leaking from a shell profile would send main.rs down the
# nested-winit path; scrub both so backend selection sees a bare VT.
env -u DISPLAY -u WAYLAND_DISPLAY RUST_LOG=info "$BIN" 2>"$LOG" &
WM_PID=$!
echo "$WM_PID" >"$PIDFILE"

# Stay in the foreground so the exit status and log tail land back on
# this VT when the compositor ends (don't log out meanwhile: HUP).
wait "$WM_PID"
STATUS=$?
rm -f "$PIDFILE"
echo "compositor exited with status $STATUS; last log lines:"
tail -n 15 "$LOG"

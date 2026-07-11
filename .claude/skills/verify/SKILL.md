---
name: verify
description: Drive splitwm on its headless backend and screenshot it, for verifying WM changes end-to-end.
---

# Verifying splitwm

`./drive.sh` is the canonical harness: builds the debug binary (`cargo
test`/`cargo check` do NOT produce it), boots the compositor on the
**headless** backend (`SPLITWM_HEADLESS=1`, nothing appears on the host
desktop — no Xephyr, no X server), drives it by writing line commands to a
FIFO wired to its stdin (`SPLITWM_DEBUG_CHANNEL=1`, see `src/comp/debug.rs`
for the full command set: `key`, `spawn`, `motion`/`click`/`press`/`release`,
`scroll`, `shot`, `focus`, `layout`, `cursor`), and screenshots to
`/tmp/splitshots/`. Every command is acked on stdout so the drive is
synchronized, not timed.

For a custom drive, copy `drive.sh`'s skeleton (FIFO setup, `wait_line`
sync-on-ack helper, `key`/`shot`/`cmd` wrappers, cleanup trap). Never touch
the live session: Jodie's real WM is this repo's release binary — kill only
PIDs you spawned, never `pkill splitwm`.

For a visible/interactive check (e.g. verifying pointer/cursor behavior by
eye), run the **winit** backend instead: `cargo run`. It opens a plain
window in whatever host session you run it from (nested Wayland compositor,
or a live X11 desktop via XWayland-less winit) and takes that session's
input. To keep it off Jodie's real desktop, point it at a scratch session
first (e.g. a `Xephyr :9 &` then `DISPLAY=:9 cargo run`, or a nested
Wayland compositor). This is for manual spot-checks only — `drive.sh` is
what to reach for by default.

Gotchas:
- Taskbar strip: crop `1280x90+0+710` on the 1280x800 headless screen
  (`theme::TASKBAR_H` = 82).
- The drive spawns `alacritty` (the default `$TERMINAL`); it needs ~1.2s
  after spawn before it maps.
- A private session bus (`dbus-run-session`) keeps the compositor's
  notification daemon from losing `org.freedesktop.Notifications` to the
  live session's — without it the refusal bubble squats in every
  screenshot.

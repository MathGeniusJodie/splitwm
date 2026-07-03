---
name: verify
description: Drive splitwm in a nested Xephyr session and screenshot it, for verifying WM changes end-to-end.
---

# Verifying splitwm

`./drive.sh` is the canonical harness: builds the binary (cargo test/check
do NOT produce it), boots Xephyr on `:9` (`SPLITWM_DRIVE_DPY` overrides),
runs the WM, drives it with xdotool, screenshots to `/tmp/splitshots/`.

For a custom drive, copy its skeleton (Xephyr boot loop, `shot`/`key`/`term`
helpers, cleanup trap). Never touch the live session: Jodie's real WM is
this repo's debug binary — kill only PIDs you spawned, never `pkill splitwm`.

Gotchas:
- Wait ~3s after starting the WM before the first screenshot; the first
  underlay composite lands late and earlier shots are all black.
- Taskbar strip: crop `1280x90+0+710` on the 1280x800 screen
  (`TASKBAR_H` = 82).
- Fake an app being "running" with `xterm -class <WM_CLASS>` (class match
  is case-insensitive).
- `xterm` needs ~1.2s after spawn before it maps.

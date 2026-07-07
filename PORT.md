# Wayland port (Smithay) — plan & status

Working doc for the from-scratch Wayland rewrite on this branch. Delete when
the port ships. The **behavioral spec is master's `src/` + README**: consult
`git show master:src/<file>` — the X11 implementation defines every behavior
this port must reproduce unless a deviation is listed below.

## Locked decisions (Jodie, 2026-07-06)

- Same repo, this branch (`wayland`), replacing `src/`; master keeps X11.
- **Smithay 0.7** (crates.io), GLES renderer. Chrome stays palette-indexed
  pixel art drawn by pixel-graphics into CPU buffers uploaded as textures.
- **Full XWayland** from the start; X11 and Wayland windows share one
  window abstraction and lifecycle.
- Nested development via the **winit backend**; real sessions via a
  DRM/libinput/libseat backend behind the `tty` cargo feature (off by
  default until `seatd` is installed — libseat is a link-time dep).
- Priorities: thorough testing from the start, invalid states
  unrepresentable, never block the event loop, no per-frame malloc/clone.

## Approved deviations from X11 behavior

- Default terminal: **alacritty** (was xterm) — `$TERMINAL` still wins.
- Launcher stays `rofi -show combi`, running as an X11 client under
  XWayland (override-redirect float). No layer-shell in v1.

## Open questions for Jodie (batch, don't block)

- **Quit path**: X11 splitwm exits only via `--replace`/SIGTERM. A Wayland
  compositor on a TTY has no `--replace` analog; exiting means the session
  ends. Keep "no quit binding" (kill from another VT), or add a chord?
- `theme::QUICK` terminal entry default also xterm → alacritty?
- Env var names: keep `SPLITWM_WALLPAPER` / `SPLITWM_DOCK_TITLE` /
  `SPLITWM_DEBUG_SCROLL` as-is (assumed yes).
- Decoration policy: implement xdg-decoration and force server-side
  (clients told not to draw their own titlebars) — assumed yes, matches
  the X11 look; some GTK apps will keep CSD regardless (they don't
  implement the protocol) and will get chrome drawn around their shadows
  unless we special-case. Nitpick pending real testing.
- App icons: Wayland has no `_NET_WM_ICON`. Plan: `xdg_toplevel_icon_v1`
  protocol + `.desktop`/icon-theme lookup fallback (keyed on app_id), and
  real `_NET_WM_ICON` for XWayland clients. Hue-rotation disambiguation
  unchanged. OK?
- Notifications: reimplement the daemon on **zbus** (pure Rust, async,
  driven by calloop — no libdbus, no dedicated thread, nothing blocks).
  Same org.freedesktop.Notifications surface incl. close reasons,
  replaces_id rules, foreign-daemon fallback.

## Milestones

- [ ] **M0** scaffold: winit window, GLES clear, calloop loop, clean exit
- [ ] **M1** protocol core: compositor/xdg-shell/shm/dmabuf/seat/output/
      data-device; alacritty runs with keyboard focus
- [ ] **M2** pure core ported with tests: theme, tree, layout state, oklch
- [ ] **M3** chrome rendering: pixel-graphics → GLES textures (borders,
      titlebars, buttons, taskbar, wallpaper); reused buffers
- [ ] **M4** tiling behavior: splits ↔ windows, focus, all keybindings,
      taskbar stash/cycle, titlebar buttons, close protocol
- [ ] **M5** pointer: handle/edge drags, '+' insertion, canvas scroll +
      glide, Mod4+swipe over windows, ease-out-back animations
- [ ] **M6** floats, fullscreen, dock (DOCK_TITLE/DOCK_OVERLAP)
- [ ] **M7** XWayland: full X11 lifecycle, rofi works
- [ ] **M8** notifications (zbus), quick-launch, .desktop/icon lookup,
      icon hue rotation
- [ ] **M9** TTY backend: udev/DRM/GBM/libinput/libseat, output resize
- [ ] **Harness** (grows from M1): headless socket tests, screenshot drive

## Architecture (new src/)

```
main.rs        CLI, logging, backend selection (winit now, tty later)
assets.rs      baked chrome art (unchanged from master; build.rs unchanged)
theme.rs       palette/metrics/bindings/QUICK (keysyms via xkbcommon)
oklch.rs       icon hue rotation (ported verbatim + tests)
tree.rs        pure split-tree math (ported verbatim + tests)
layout.rs      master's state.rs: per-tag layout + taskbar mutations
comp/          compositor State, calloop wiring, delegate handlers, focus
shell/         window abstraction (Wayland | X11), xdg handlers, floats,
               fullscreen, dock
input/         keybinding dispatch, pointer drags/hit regions, scroll
render/        output rendering: wallpaper/chrome/windows; texture cache
notify/        zbus notification daemon + popup surfaces (M8)
launch.rs      spawn via systemd-run scope w/ bounded probe (ported)
```

Mapping notes vs X11:
- No underlay window and no unmap/remap dance: the compositor composites
  chrome directly and simply doesn't draw taskbar'd windows. WM_STATE /
  restore-on-exit machinery has no analog (clients just get configured).
- "Politely close then kill" → xdg_toplevel.close(), kill only for
  XWayland (WM_DELETE_WINDOW / XKillClient as on master).
- Canvas scroll offsets become render-time offsets, not window moves.
- Frame pacing: winit/DRM vblank-driven redraws replace the hand-paced
  60fps animation timer; animations keyed off `Instant` as before.

## Environment

- Present: wayland 1.25, libinput 1.31, xkbcommon 1.13, EGL, GBM, udev,
  libdisplay-info, pixman, alacritty, rofi.
- Missing (Jodie to install): `xorg-xwayland` (M7), `seatd` (M9/`tty`).

## Testing

- Unit: ported tree/layout/theme/oklch tests + new ones per module.
- Integration (itest.sh analog): compositor on a headless/winit backend
  with a private `WAYLAND_DISPLAY`; test clients via smithay-client-toolkit
  assert configure sizes, states, focus order, close semantics over the
  real socket. Rust `#[test]`s, no display needed for the headless set.
- Visual (drive.sh analog): nested winit run inside the X session, spawn
  alacritty/test clients, drive bindings by injecting via the compositor's
  own debug channel or wtype through a virtual-keyboard protocol (TBD),
  screenshot via wlr-screencopy into /tmp/splitshots.
- X11 test scripts (test.sh/itest.sh/drive.sh) stay in-tree as the spec
  for their replacements until the harness lands, then get deleted.

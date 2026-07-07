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

## Approved deviations & settled design (Jodie, 2026-07-06/07)

- Default terminal: **alacritty** (was xterm) — `$TERMINAL` still wins;
  `theme::QUICK` terminal default likewise.
- Launcher stays `rofi -show combi`, running as an X11 client under
  XWayland (override-redirect float). No layer-shell in v1.
- **No quit binding**, faithful to master: SIGTERM (another VT / remote
  shell) is the only way out, on every backend.
- **Icons**: xdg-toplevel-icon protocol when offered → freedesktop
  icon-theme lookup keyed on app_id → XWayland `_NET_WM_ICON`.
  Hue-rotation disambiguation unchanged.
- **Notifications on zbus** driven by calloop (no libdbus, no thread).
- **Natural scrolling forced on** for the tty seat's libinput devices
  (Jodie, 2026-07-07) — master inherited the X server's scroll config,
  which this replaces; nested sessions still inherit the host's.
- **Volume keys are single-shot per press** (deviation: X11 auto-repeat
  used to make holding the key keep adjusting; no compositor-side repeat
  timer).
- xdg-decoration forces ServerSide on every toplevel (implemented in M4);
  clients that ignore the protocol keep their CSD and that's accepted.
- Env var names unchanged: `SPLITWM_WALLPAPER`/`SPLITWM_DOCK_TITLE`/
  `SPLITWM_DEBUG_SCROLL`.
- Key repeats: chords the compositor intercepts swallow their repeats and
  release; volume single-shot covers the only hold-relevant case.

## Later (post-v1)

- Wire up **layer-shell** (wlr-layer-shell): would let rofi and friends run
  as native Wayland surfaces instead of the XWayland override-redirect
  pin, and opens the door to panels/OSDs. Revisit the LAUNCHER_CMD
  WAYLAND_DISPLAY scrub when it lands.

## Milestones

- [x] **M0** scaffold: winit window, GLES clear, calloop loop, clean exit
- [x] **M1** protocol core: compositor/xdg-shell/shm/dmabuf/seat/output/
      data-device; alacritty runs with keyboard focus
- [x] **M2** pure core ported with tests: theme, tree, layout state, oklch
- [x] **M3** chrome rendering (notify_popup bubble drawing waits on M8)
- [x] **M4** tiling behavior incl. taskbar + xdg-decoration ServerSide
- [x] **M5** pointer: chrome clicks (buttons/tiles/badges/quick/"+"),
      gap+edge drags, canvas panning w/ Mod4 gate + glide, ease-out-back
      animations. Scroll glide is drive-verified end-to-end; stash
      restore is socket-tested via the keyboard cycle. Still unverified
      (needs pointer injection, see Harness gaps): taskbar-tile *clicks*
      on a stashed window, edge drags, hover cursor shapes (still
      unimplemented after M9: the tty backend renders a cursor, but
      shape switching over chrome is absent).
- [x] **M6** floats, fullscreen, dock (DOCK_TITLE/DOCK_OVERLAP);
      verified: zenity float + frame drag, cozyui dock layering,
      startup-fullscreen. Note: clicking the dock hands it the keyboard
      via the same override slot as floats (kept until the next
      deliberate focus move — arrange runs oftener than on X11, where
      dock focus survived until any WM focus action).
- [x] **M7** XWayland: X11 clients share the classify/tile/float/dock
      path; o-r windows (rofi) topmost + keyboard while mapped; tiled
      X11 ConfigureRequests denied by re-assert. Verified: xterm tiles,
      rofi drun overlays. No XKillClient fallback yet for unresponsive
      X11 clients (close is polite-only both backends).
- [x] **M8** notifications (zbus), quick-launch, .desktop/icon lookup,
      icon hue rotation. Verified: theme icons + green hue-rotated
      same-app icon, notify-send bubbles w/ urgency styling + corner
      transparency, GetServerInformation. Gaps vs master: NameLost
      mid-run not watched (a replacing daemon isn't detected until
      restart); CloseNotification's signal emits on the 250ms tick
      instead of immediately; foreign-daemon X11 notification windows
      N/A on Wayland. xdg-toplevel-icon protocol not in smithay 0.7 —
      icons come from theme lookup by app_id/class only (X11
      _NET_WM_ICON also unexposed by smithay's X11Surface).
- [x] **M9** TTY backend: udev/DRM/GBM/libinput/libseat behind the `tty`
      feature (now links; seatd installed). Single GPU, single output:
      the first connected connector at its preferred mode, vblank-paced
      via DrmOutputManager, connector hotplug replaces the output (mode
      republish + full relayout). VT switching (Ctrl+Alt+Fn) handled by
      the compositor — the X server's job on master. Composited cursor:
      client cursor surfaces, else the xcursor-theme arrow, Kind::Cursor
      for hardware-plane offload. Verified: builds/tests both feature
      sets, nested winit regression drive (tiling+split+chrome intact),
      and a real-VT session (Jodie, 2026-07-07): seat/input/scanout,
      tiling, chrome, VT switching all work. Findings fixed after:
      debug builds unusably slow at native resolution (vttest.sh now
      builds release into `target/vttest/`, never touching the live
      session's `target/release` binary); a wayland-capable rofi
      picking its layer-shell backend (LAUNCHER_CMD scrubs
      WAYLAND_DISPLAY so rofi stays on XWayland per the v1 decision);
      o-r keyboard focus granted at map time, before XWayland
      associates the wl_surface, so rofi typed into nothing (now
      granted in surface_associated); clicks on o-r surfaces falling
      through to the chrome hit-test. Rofi input+centering re-verified
      nested; VT re-test pending. Gaps: named cursor shapes
      beyond the arrow (hover feedback still absent, see M5); output
      name fixed at startup even if the connector swaps; mode changes on
      an unchanged connector ignored; libinput devices run defaults
      except natural scrolling (no tap-to-click config); multi-GPU and
      multi-output out of scope (master had one X screen).
- [x] **Harness**: headless socket tests + screenshot drive. A headless
      backend (offscreen GLES on surfaceless EGL, fixed 1280x800, no
      feature gate, `SPLITWM_HEADLESS=1`) and a stdin **debug channel**
      (`SPLITWM_DEBUG_CHANNEL=1`; `key`/`spawn`/`scroll`/`shot`, acked on
      stdout) carry both: `tests/socket.rs` boots the real binary and
      asserts manage/displacement/golden-ratio splits/taskbar cycle/polite
      close/focus order/SIGTERM as a real Wayland client over the socket;
      `drive.sh` (reborn, invisible — no Xephyr) walks the old X11 drive
      sequence and screenshots 12 steps into /tmp/splitshots. Verified:
      both feature sets green, drive shots eyeballed (content-sampled
      accent, icon hue rotation, canvas pan). Gaps: no pointer injection
      (chrome clicks/drags/hover ride manual nested runs), no XWayland
      client in the socket tests, output size fixed.

## Architecture (new src/)

```
main.rs        logging, backend selection (nested → winit, bare VT → tty,
               SPLITWM_HEADLESS → headless)
backend/       Backend enum + winit, tty (feature-gated), and headless
               (offscreen, for the harness) sessions; Comp reaches
               presentation only through the enum
assets.rs      baked chrome art (unchanged from master; build.rs unchanged)
theme.rs       palette/metrics/bindings/QUICK (keysyms via xkbcommon)
oklch.rs       icon hue rotation (ported verbatim + tests)
tree.rs        pure split-tree math (ported verbatim + tests)
layout.rs      master's state.rs: per-tag layout + taskbar mutations
comp/          compositor State, calloop wiring, delegate handlers, focus,
               the harness's stdin debug channel
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
  libdisplay-info, pixman, alacritty, rofi, xorg-xwayland, seatd 0.9.
- `tty` stays an opt-in cargo feature even with seatd installed; folding
  it into the default build is Jodie's call.

## Testing

- Unit: ported tree/layout/theme/oklch tests + new ones per module.
- Integration (`tests/socket.rs`, the itest.sh analog): `cargo test`
  boots the real binary on the headless backend (cargo builds the bin for
  integration tests; `CARGO_BIN_EXE_splitwm` points at it) and connects
  as a plain wayland-client — raw protocol rather than the planned
  smithay-client-toolkit; fewer moving parts and the same crate family.
  Asserts configure sizes, activated state, keyboard focus order,
  displacement, split shares, taskbar cycle, polite close, and SIGTERM
  shutdown, all client-observably over the real socket. Nothing appears
  on the host desktop.
- Visual (`drive.sh`): the same headless run driven from bash over the
  debug channel — spawns alacritty, walks the old X11 drive's
  split/tab/focus/grow/scroll/close sequence, screenshots each step into
  /tmp/splitshots (framebuffer readback, ImageMagick encode). Runs under
  `dbus-run-session` so the notification daemon owns its bus name
  instead of squatting a refusal bubble in every shot.
- Debug channel protocol (stdin, `SPLITWM_DEBUG_CHANNEL=1`, any
  backend): `key <chord>` resolves through the same `binding_action`
  table as the keyboard; `spawn <cmd>`; `scroll <clicks>`; `shot <path>`
  (headless only). Every command acks on stdout — drivers synchronize on
  acks, not sleeps.
- Real VT (tty backend): `./vttest.sh` from a VT login builds with the
  `tty` feature, scrubs leaked `DISPLAY` vars, and takes the seat;
  `./vttest.sh kill` from X ends it by recorded pid (never pkill — the
  live session's WM shares the binary name).
- The X11 scripts are gone from this branch (master keeps them as the
  X11 spec): itest.sh → tests/socket.rs, drive.sh → rewritten headless,
  test.sh → obsolete (a nested run is just `cargo run` in a session).

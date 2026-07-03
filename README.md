# splitwm (Rust)

A from-scratch X11 window manager in Rust, a standalone port of
[MathGeniusJodie/awesome](https://github.com/MathGeniusJodie/awesome)'s
**splitwm** layout (originally Lua on AwesomeWM) — a
terminal-multiplexer-style tiling layout where:

- Splits are persistent containers arranged in an n-ary tree (split
  horizontally or vertically).
- Each split shows **one window**; everything else lives in a **bottom
  taskbar** and can be cycled into a split (`Mod4+[` / `]`) or clicked in.
- The whole layout lives on a **horizontally-scrollable canvas** that can be
  wider than the screen (trackpad two-finger swipe, or Mod4+swipe over a
  window).
- All chrome — bitmap window borders, titlebars, buttons, drag handles,
  taskbar — is **palette-swapped pixel art** (the na16 palette), composited
  in software onto a single full-screen underlay window below every client.

## Stack

- **[x11rb](https://crates.io/crates/x11rb)** — pure-Rust X11/XCB binding
  (`xinput` feature for smooth trackpad scrolling).
- **pixel-graphics** (vendored) — palette-indexed software rasteriser,
  sprite drawing, and the palette-swap machinery behind the per-split
  accent colours; the underlay is composited into its framebuffers and
  blitted via `PutImage`.
- **pixel-fonts** (vendored) — baked bitmap pixel font for labels (text
  degrades to nothing if the font atlas can't be loaded).
- pixel-graphics PNG decoding — user wallpapers (non-PNG formats are
  converted via ImageMagick's `magick`/`convert` when installed).
- **dbus** — the WM doubles as the session's
  `org.freedesktop.Notifications` daemon on its own thread.

## Architecture

| file | role |
|------|------|
| `src/theme.rs`      | palette indices, colours, layout metrics |
| `src/tree.rs`       | pure split-tree math + geometry |
| `src/state.rs`      | per-tag layout state + all tree/taskbar mutations |
| `src/render.rs`     | software rendering: 9-slice borders, buttons, icons |
| `src/oklch.rs`      | OKLCH hue rotation for same-app icon disambiguation |
| `src/launch.rs`     | quick-launch + `.desktop` command/icon resolution |
| `src/wm/mod.rs`     | become WM, EWMH announcement, event loop |
| `src/wm/events.rs`  | event dispatch: keys, buttons, drags, scroll coalescing |
| `src/wm/clients.rs` | client lifecycle, icons, focus, WM_DELETE/WM_STATE |
| `src/wm/arrange.rs` | layout → placements, underlay compositing, animation |
| `src/wm/widgets.rs` | hit-region computation (handles, "+", tabs, taskbar) |

There is **no reparenting**: clients stay children of the root, positioned
below their split's titlebar, and all decoration is drawn on the underlay.
Windows hidden from the layout are unmapped (with ICCCM `WM_STATE` kept in
sync, and self-inflicted unmaps distinguished from client withdrawal); on
quit or WM handover everything is remapped so nothing is stranded.

ICCCM/EWMH surface: `WM_S<n>` manager selection (`--replace` supported both
ways), `WM_STATE`, `WM_DELETE_WINDOW` for polite closing,
`_NET_SUPPORTING_WM_CHECK`, `_NET_CLIENT_LIST`, `_NET_ACTIVE_WINDOW`,
`_NET_WM_ICON`. Single monitor, single tag; RandR screen resizes and
keyboard-mapping changes are handled.

## Keybindings (Mod4 = Super)

| key | action |
|-----|--------|
| `Mod4+Return`        | open terminal (`$TERMINAL`, default `xterm`) |
| `Mod4+Space`         | app launcher (`rofi -show drun`) |
| `Mod4+v`             | split into columns |
| `Mod4+h`             | split into rows |
| `Mod4+q`             | close current split (window goes to sibling/taskbar) |
| `Mod4+Tab` / `Right` | focus next split |
| `Mod4+Shift+Tab` / `Left` | focus previous split |
| `Mod4+]` / `[`       | cycle taskbar window into the focused split (fwd/back) |
| `Mod4+Shift+]` / `[` | move window to next / previous split |
| `Mod4+l` / `=`       | grow split |
| `Mod4+Shift+l` / `Mod4+-` | shrink split |
| `Mod4+Shift+c`       | close focused window (`WM_DELETE_WINDOW`, falls back to kill) |
| trackpad h-swipe     | scroll the canvas (over gaps; hold Mod4 over a window) |
| drag a gap handle    | resize the two adjacent columns |
| drag a canvas edge   | resize the outer column into its margin |
| click a gap/edge `+` | insert an empty column there |
| taskbar tile         | focus that window / bring it into the focused split |
| taskbar tile corner `x` | close that window politely |
| taskbar quick-launch icons | spawn that app (right of the pill separator) |
| titlebar buttons     | minimize / split (right-click: other direction) / close split |

There is no quit binding: splitwm ends its session by being replaced
(another WM's `--replace`) or via `SIGTERM`/`SIGINT` — remapping all hidden
windows on the way out either way.

## Environment

- `SPLITWM_WALLPAPER=/path/to/image` — scaled wallpaper behind the gaps
  (PNG natively; other formats need ImageMagick installed).
- `SPLITWM_DOCK_TITLE` (default `cozyui`) — a window with this `WM_NAME` is
  docked off-screen past the canvas's right end, revealed by scrolling all
  the way right.
- `SPLITWM_DEBUG_SCROLL=1` — log scroll-device discovery and batch timings.
- `TERMINAL`, `BROWSER`, `FILEMANAGER`, `OBSIDIAN`, `CLAUDE_DESKTOP` —
  commands behind the taskbar's quick-launch icons.

## Build & test

```sh
cargo build --release
cargo test          # layout-state unit tests

# Asserting ICCCM/EWMH integration test in a nested X server (Xephyr):
# WM_STATE transitions, _NET_CLIENT_LIST, focus, withdrawal, restore-on-exit.
# Exits nonzero on any failed assertion.
./itest.sh

# Launch in a nested X server (Xephyr) and drive an automated UI test that
# splits, tabs, scrolls, resizes and closes, dropping screenshots in
# /tmp/splitshots:
./drive.sh

# Or just open a Xephyr and run it interactively:
Xephyr :1 -ac -screen 1280x800 &
DISPLAY=:1 ./target/release/splitwm
```

## Status

Implemented: n-ary tree splits with flattening, scrollable canvas with
edge/gap resize and column insertion, bottom taskbar with cycle/close,
per-split persistent accent colours (palette-swapped bitmap chrome), app
icons from `_NET_WM_ICON` quantized to the chrome palette (hue-rotated per
window when one app has several), taskbar quick-launch icons, docked
sidebar, layout animations (ease-out-back, ~60 fps paced), smooth trackpad
canvas panning, `--replace` in both directions.

Intentionally not implemented: multi-monitor, multiple tags, a status
bar/clock, and the Lua original's slanted tab bars, per-leaf tab stacks,
window-content colour sampling, and XTEST "smush" — this port shows one
window per split and keeps the rest in the taskbar.

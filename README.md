# splitwm (Rust)

A from-scratch X11 window manager in Rust, cloning the behaviour and look of
[MathGeniusJodie/awesome](https://github.com/MathGeniusJodie/awesome)'s
**splitwm** — a terminal-multiplexer-style tiling layout where:

- Splits are persistent containers arranged in an n-ary tree (split
  horizontally or vertically).
- Each split is a **leaf** holding a *tab stack* of windows; the active tab is
  shown, the rest are hidden but kept.
- The whole layout lives on a **horizontally-scrollable canvas** that can be
  wider than the screen.
- Each leaf draws a custom **slanted tab bar** and a rounded focus border.

The original is ~5500 lines of Lua running on top of AwesomeWM's engine; this
is a standalone WM with no Lua/awesome dependency.

## Stack

- **[x11rb](https://crates.io/crates/x11rb)** — pure-Rust X11/XCB binding.
- **[tiny-skia](https://crates.io/crates/tiny-skia)** — pure-Rust 2D rasteriser
  for the tab bars / borders (rendered to a buffer, blitted via `PutImage`).
- **[fontdue](https://crates.io/crates/fontdue)** — glyph rasterising for tab
  labels (a system monospace TTF is loaded at runtime, as a fallback when a
  client has no `_NET_WM_ICON`).

## Architecture

| file | role |
|------|------|
| `src/theme.rs`  | colours + layout metrics (ported from `theme.lua` / `rc.lua`) |
| `src/tree.rs`   | pure split-tree math + geometry (ported from `tree.lua`) |
| `src/state.rs`  | per-tag layout state + all tree/tab mutations (`core.lua`+`ops.lua`) |
| `src/render.rs` | tiny-skia drawing of leaf decorations → BGRX buffer |
| `src/wm.rs`     | X11 event loop: become WM, per-leaf frame windows, reparenting, keybindings |

Each leaf gets a **frame window** (child of root) covering its area; the active
client is reparented into the frame and positioned below the tab bar. Scrolling
moves the frames; off-screen frames are unmapped. Frames paint their own slice
of the wallpaper as the background, so they blend into the surrounding gaps.

Gap **drag handles** and **"+" insert buttons** are pooled child-of-root
windows positioned in the gaps each arrange; layout-changing actions play an
**ease-out-back** animation by interpolating frame geometry over ~0.28 s.

## Keybindings (Mod4 = Super)

| key | action |
|-----|--------|
| `Mod4+Return`        | open terminal (`$TERMINAL`, default `xterm`) |
| `Mod4+v`             | split horizontally |
| `Mod4+h`             | split vertically |
| `Mod4+q`             | close current split |
| `Mod4+Tab` / `Right` | focus next split |
| `Mod4+Shift+Tab` / `Left` | focus previous split |
| `Mod4+]` / `[`       | next / previous tab in split |
| `Mod4+Shift+]` / `[` | move tab to next / previous split |
| `Mod4+l` / `=`       | grow split |
| `Mod4+Shift+l` / `Mod4+-` | shrink split |
| `Mod4+Shift+c`       | kill focused window |
| `Mod4+Shift+q`       | quit splitwm |
| `Mod4 + scroll wheel`| scroll the canvas horizontally |
| drag a gap handle    | resize the two adjacent columns |
| click a gap / edge `+`| insert an empty column there |

Set `SPLITWM_WALLPAPER=/path/to.png` to render a scaled wallpaper behind the
gaps.

## Build & test

```sh
cargo build --release

# Launch in a nested X server (Xephyr) and drive an automated UI test that
# splits, tabs, scrolls, resizes and closes, dropping screenshots in
# /tmp/splitshots:
./drive.sh

# Or just open a Xephyr and run it interactively:
Xephyr :1 -ac -screen 1280x800 &
DISPLAY=:1 ./target/release/splitwm
```

## Status

Implemented: tree splits (H/V, n-ary, flattening), tabbed leaves, custom slanted
tab bars, rounded focus border, scrollable canvas, focus engine, resize, tab
cycling/moving, split close with tab merge, keybindings, click-to-focus,
Mod4+wheel scroll.

Parity features ported from the Lua original:

- **App icons** in tabs from `_NET_WM_ICON` (letter-glyph fallback).
- **Window-content colour sampling** — the focused client's top strip is
  sampled to tint its active tab and focus border.
- **Wallpaper underlay** (`SPLITWM_WALLPAPER`), with frames painting their own
  wallpaper slice so gaps blend seamlessly.
- **Split / close / resize animations** (ease-out-back).
- Gap **drag-to-resize handles** and **"+" column-insert** buttons (gaps + edges).
- **Smush** — auto font-shrink (Ctrl+0 / Ctrl+-) into narrow focused splits via
  XTEST.

Not ported: the **status bar** / clock (intentionally excluded), and
multi-tag/multi-monitor support (runs a single tag on the primary screen).

> Note: pointer-driven drag/insert is exercised by unit tests on the underlying
> layout ops; it can't be synthesised in the headless Xephyr used for the
> screenshot drive (`XWarpPointer` is ignored there).

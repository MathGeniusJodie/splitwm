# splitwm

A from-scratch Wayland compositor in Rust ([smithay](https://smithay.github.io/))
with a terminal-multiplexer-style tiling layout where:

- Splits are persistent containers in a **flat strip of columns**; a
  column is one split or one vertical stack of splits — nothing nests
  deeper. Columns own their width in pixels; the strip is exactly the
  columns laid end to end.
- Each split shows **one window**, and every window has a split: a new
  window fills the focused split if it's an empty placeholder, else it
  opens as a fresh column right of the focused one — never resizing any
  other column — and a dying window takes its split with it (a whole
  column just leaves the strip; a stacked row's neighbours reclaim its
  height). The **bottom taskbar** mirrors the splits one icon per split,
  in the same left-to-right order.
- Splits reorder by **drag and drop**: grab a titlebar or a taskbar icon
  and drop it. On either half of a split or icon it lands as a column —
  left half before, right half after; on a vertical gap it becomes a
  column right there; on a horizontal gap it slots into that stack.
- The whole layout lives on a **horizontally-scrollable canvas** that can be
  wider than the screen (trackpad two-finger swipe, or Mod4+swipe over a
  window).
- All chrome — bitmap window borders, titlebars, buttons, drag handles,
  taskbar, even the pointer — is **palette-swapped pixel art** (the na16
  palette), drawn as palette indices in software, resolved to colour by a
  GPU palette shader, and composited under/around the client surfaces.

## Stack

- **smithay 0.7** — compositor framework: protocol machinery, GLES
  renderer, XWayland. Three backends: **winit** (nested development runs
  inside an existing session), **tty** (real seat via
  DRM/GBM/libinput/libseat, the default-on `tty` cargo feature — linking
  needs the system `libseat`), and **headless** (offscreen, for the test
  harness, `SPLITWM_HEADLESS=1`).
- **pixel-graphics / pixel-fonts** (vendored) — palette-indexed software
  rasteriser and bitmap font behind all chrome; the raw index bytes upload
  as `R8` textures and a fragment shader does the palette lookup on the
  GPU (`render::indexed`). Wallpapers and theme icons decode via
  ImageMagick shell-out.
- **zbus** — the compositor doubles as the session's
  `org.freedesktop.Notifications` daemon, bridged to calloop (no thread
  per notification, no libdbus).
- **xkbcommon** (via smithay) + **xkeysym** — keyboard handling; xkb
  config comes from the environment (`XKB_DEFAULT_LAYOUT` …).
- **freedesktop-icons / freedesktop-desktop-entry** — taskbar icon lookup
  and `.desktop` resolution.

## Protocols

wl_compositor, xdg-shell, wl_shm, linux-dmabuf (v4 feedback with a render
node), wl_seat, wl_output/xdg-output, wl_data_device, primary-selection,
xdg-decoration (ServerSide forced on every toplevel; clients that ignore it
keep their CSD), **wlr-layer-shell**, **cursor-shape-v1**, and full
**XWayland** — X11 and Wayland windows share one window abstraction and
lifecycle.

## Architecture

| path | role |
|------|------|
| `src/main.rs`    | logging, backend selection (nested → winit, bare VT → tty, `SPLITWM_HEADLESS` → headless) |
| `src/backend/`   | backend enum + winit/tty/headless sessions; `Comp` reaches presentation only through the enum |
| `src/theme.rs`   | palette/metrics/bindings/quick-launch table |
| `src/layout.rs`  | pure column-strip layout math (flat columns/stacks + tests) |
| `src/state.rs`   | layout state + all strip/taskbar mutations |
| `src/render/`    | indexed chrome rendering (software-drawn indices, GPU palette shader): wallpaper, 9-slice borders, buttons, icons, taskbar |
| `src/widgets.rs` | hit-region computation (handles, "+", titles, taskbar) |
| `src/oklch.rs`   | OKLCH hue rotation for same-app icon disambiguation |
| `src/comp/`      | compositor state, calloop wiring, delegate handlers, input, pointer/cursor, layer-shell, XWayland, debug channel |
| `src/shell.rs`   | window abstraction (Wayland \| X11), floats, fullscreen, dock |
| `src/notify.rs`  | zbus notification daemon (popups render as chrome) |
| `src/launch.rs`  | spawn via systemd-run scope with a bounded probe |

Behavior notes:

- Minimized windows are simply not drawn (their split shows a restore
  strip); nothing is unmapped or moved.
- Closing is always the polite request (`xdg_toplevel.close()` /
  `WM_DELETE_WINDOW`); there is no force-kill fallback for either kind
  of client.
- Canvas scroll offsets are render-time offsets, not window moves.
- Redraws are demand-driven: damage queues one, the tty backend presents
  on vblank, winit/headless present as queued, and an idle compositor
  sleeps; animations key off `Instant`.
- The pointer is composited from hand-drawn cursor sprites
  (arrow, hand, disabled, text) on every backend, including nested (winit)
  sessions: the host cursor is hidden and every named shape a client
  requests via cursor-shape-v1 — or that chrome hover feedback picks — maps
  onto one of the four sprites by intent (`comp::cursor::sprite_buf`), not
  looked up in an xcursor theme.

## Keybindings (Mod4 = Super)

| key | action |
|-----|--------|
| `Mod4+Return`        | open terminal (`$TERMINAL`, default `alacritty`) |
| `Mod4+Space`         | app launcher (`rofi -show combi`, native layer-shell) |
| `Mod4+h`             | stack an empty split below the focused one (it takes focus, so the next window fills it) |
| `Mod4+q`             | close current split *and* its window (a placeholder is just removed) |
| `Mod4+Tab` / `Right` | focus next split |
| `Mod4+Shift+Tab` / `Left` | focus previous split |
| `Mod4+]` / `[`       | focus next / previous split |
| `Mod4+Shift+]` / `[` | move the focused split right / left |
| `Mod4+l` / `=`       | grow split |
| `Mod4+Shift+l` / `Mod4+-` | shrink split |
| `Mod4+Shift+c` / `Alt+F4` | close focused window politely |
| `XF86Audio{Raise,Lower}Volume` / `Mute` | volume via `wpctl` (single-shot per press) |
| `XF86MonBrightness{Up,Down}` | step the backlight by 5% |
| `Ctrl+Alt+F1..F12`   | VT switch (tty backend) |
| trackpad h-swipe     | scroll the canvas (over gaps; hold Mod4 over a window) |
| drag a column gap handle | resize the column left of it (the rest of the strip slides) |
| drag a stack gap handle  | re-split the two rows' heights |
| drag a canvas edge   | resize the outer column into its margin |
| click a gap/edge `+` | insert an empty column there (in a stack gap: an empty row) |
| taskbar tile         | focus that split and scroll it into view; drag to reorder |
| taskbar tile corner `x` | close that window politely (its split collapses when it dies) |
| taskbar quick-launch icons | spawn that app (right of the pill separator) |
| titlebar buttons     | minimize / ⊞ split (wide lone window: new column right; else stack below; right-click flips) / close window+split |
| drag a titlebar      | move that split; drop on a frame/icon half or into a gap |

There is deliberately no quit binding: `SIGTERM` (from another VT or a
remote shell) is the only way out, on every backend.

## Environment

- `SPLITWM_WALLPAPER=/path/to/image` — scaled wallpaper behind the gaps
  (any format ImageMagick can decode).
- `SPLITWM_DOCK_TITLE` — override the app_id/title matched to dock a
  window off-screen past the canvas's right end (default in
  `theme::DOCK_TITLE`).
- `SPLITWM_HEADLESS=1` — offscreen backend for the harness;
  `SPLITWM_DEBUG_CHANNEL=1` — line protocol on stdin driving keys,
  pointer, spawns, screenshots, and cursor queries (see `comp/debug.rs`).
- Taskbar quick-launch commands are configured per entry in
  `theme::QUICK` (each with its own env-var override).

## Build & test

```sh
cargo build --release                        # all backends incl. real-seat DRM
cargo build --release --no-default-features  # without tty (no libseat needed)

cargo test          # unit tests + tests/socket.rs: boots the real binary
                    # headless and asserts manage/placement/splits/drag/
                    # focus/layer-shell/cursor/close/SIGTERM as a real
                    # Wayland client over the socket. Nothing appears on
                    # the host desktop.

# Headless screenshot drive: walks the split/tab/focus/grow/scroll/close
# sequence over the debug channel, dropping screenshots in /tmp/splitshots:
./drive.sh

# Nested interactive run inside an existing Wayland/X session:
cargo run

# Real VT session (build with the tty feature, from a VT login):
./vttest.sh          # takes the seat; ./vttest.sh kill ends it by pid
```

## Design notes

- **Windows and splits are 1:1**: there are no splitless windows and no
  stash — a window never leaves its split, and window death collapses
  the split. The taskbar mirrors the splits in strip order; its icons
  and the titlebars drag-and-drop to reorder splits. There is one close
  action: the titlebar close and the taskbar badge both close the
  window, and the split collapses when it dies.
- **The layout is a flat column strip**: a flat list of pixel-width
  columns, each one split or one vertical stack — deeper nesting is
  unrepresentable. Opening, closing, and resizing a column never
  resizes any other column; the strip absorbs the difference (only
  stacked rows still trade height). A new window fills the focused
  split only when it's an empty placeholder, else it opens a fresh
  column right of the focused one at a third of the viewport — an
  unfocused placeholder attracts nothing. `Mod4+h` stacks below and
  focuses the new placeholder. The ⊞ titlebar button opens a column
  right of a wide lone window and stacks below otherwise (right-click
  flips); drops into gaps insert by the gap's orientation.
- The launcher runs as a **native layer-shell surface** (a
  wayland-capable rofi picks its wayland backend; an X11-only rofi still
  works through XWayland as an override-redirect float).
- **Icons**: freedesktop icon-theme lookup keyed on app_id/class only —
  smithay 0.7 exposes neither xdg-toplevel-icon nor `_NET_WM_ICON`.
  Hue rotation disambiguates same-app icons.
- **Volume keys are single-shot per press**: there is no compositor-side
  repeat timer for them.
- **Natural scrolling is forced on** for the tty seat's libinput
  devices; nested sessions inherit the host's.
- **Keystrokes follow the mouse**: keyboard delivery is hover-based —
  keys go to the window under the pointer (anywhere in the focused
  split's column, the focused split wins) — while the focus border stays
  click/keybind-driven; a keybind focus move glides the focused column
  under the pointer so the two agree.
- Clicking the dock hands it the keyboard via the same override slot as
  floats, kept until the next deliberate focus move.
- Layer-shell specifics: **Background** surfaces render behind the
  compositor's own opaque full-output wallpaper, so they're occluded and
  get no pointer input; **Bottom** surfaces render just above the chrome
  pieces and take clicks; the taskbar strip is not exclusive-zone-aware,
  so a bottom-anchored exclusive panel overlaps it; OnDemand keyboard
  interactivity is click-to-focus and yields on the next layout focus
  move.
- The dock panel's native layer surface (Bottom, namespace = dock
  identity, anchored full-height right) rides the scrolling canvas like
  a docked window: its exclusive zone becomes scroll room past the
  canvas end instead of statically shrinking the layout, and its
  position shifts with the scroll, so scrolling left tucks it offscreen
  and scrolling fully right reveals all but its declared overlap strip.

## Known gaps

- No `XKillClient` fallback for unresponsive X11 clients (close is
  polite-only on both backends).
- Notifications: a daemon replacing us mid-run isn't detected until
  restart (`NameLost` unwatched); `CloseNotification`'s signal emits on
  the 250 ms popup tick instead of immediately.
- TTY backend: output name fixed at startup even if the connector swaps;
  mode changes on an unchanged connector are ignored; libinput devices
  run defaults except natural scrolling (no tap-to-click config);
  single GPU, single output by design.
- Harness: headless output size fixed at 1280x800.

Intentionally not implemented: multi-monitor, multiple tags, and a
status bar/clock — this compositor shows one window per split, one
split per window.

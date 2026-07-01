//! Shared types, helpers and constants for the X11 window-manager core.

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap
)]

use std::collections::HashMap;
use std::rc::Rc;

use x11rb::protocol::xproto::{Atom, ConnectionExt, Gcontext, Window};
use x11rb::rust_connection::RustConnection;

use crate::icon::Icon;
use crate::render::Renderer;
use crate::state::State;
use crate::tree::{Boundary, Dir, NodeId, Rect, Win};

pub type R<T> = Result<T, Box<dyn std::error::Error>>;

// --- X11 keysyms we bind ---
pub mod ks {
    pub const RETURN: u32 = 0xff0d;
    pub const TAB: u32 = 0xff09;
    pub const LEFT: u32 = 0xff51;
    pub const RIGHT: u32 = 0xff53;
    pub const BRACKETLEFT: u32 = 0x5b;
    pub const BRACKETRIGHT: u32 = 0x5d;
    pub const MINUS: u32 = 0x2d;
    pub const EQUAL: u32 = 0x3d;
    pub const V: u32 = 0x76;
    pub const H: u32 = 0x68;
    pub const Q: u32 = 0x71;
    pub const L: u32 = 0x6c;
    pub const C: u32 = 0x63;
}

#[derive(Clone, Copy, Debug)]
pub enum Action {
    SplitH,
    SplitV,
    Close,
    FocusNext,
    FocusPrev,
    NextTab,
    PrevTab,
    MoveTabNext,
    MoveTabPrev,
    Grow,
    Shrink,
    SpawnTerminal,
    Quit,
    /// Ask the focused window to close via `WM_DELETE_WINDOW`, falling back
    /// to disconnecting its client if it doesn't speak the protocol.
    CloseWindow,
}

/// Interned atoms used for ICCCM/EWMH interop, fetched once at startup.
pub struct Atoms {
    pub wm_protocols: Atom,
    pub wm_delete_window: Atom,
    pub wm_state: Atom,
    pub net_wm_icon: Atom,
    pub net_supported: Atom,
    pub net_client_list: Atom,
    pub net_active_window: Atom,
    pub net_supporting_wm_check: Atom,
    pub net_wm_name: Atom,
    pub utf8_string: Atom,
}

impl Atoms {
    pub fn intern(conn: &RustConnection) -> R<Self> {
        // Send every InternAtom before reading any reply, so interning costs
        // one round trip instead of ten.
        let names: [&[u8]; 10] = [
            b"WM_PROTOCOLS",
            b"WM_DELETE_WINDOW",
            b"WM_STATE",
            b"_NET_WM_ICON",
            b"_NET_SUPPORTED",
            b"_NET_CLIENT_LIST",
            b"_NET_ACTIVE_WINDOW",
            b"_NET_SUPPORTING_WM_CHECK",
            b"_NET_WM_NAME",
            b"UTF8_STRING",
        ];
        let cookies = names.map(|n| conn.intern_atom(false, n));
        let mut atoms = [0 as Atom; 10];
        for (slot, cookie) in atoms.iter_mut().zip(cookies) {
            *slot = cookie?.reply()?.atom;
        }
        let [wm_protocols, wm_delete_window, wm_state, net_wm_icon, net_supported, net_client_list, net_active_window, net_supporting_wm_check, net_wm_name, utf8_string] =
            atoms;
        Ok(Self {
            wm_protocols,
            wm_delete_window,
            wm_state,
            net_wm_icon,
            net_supported,
            net_client_list,
            net_active_window,
            net_supporting_wm_check,
            net_wm_name,
            utf8_string,
        })
    }
}

pub struct Client {
    pub label: char,
    pub icon: Option<Rc<Icon>>,
    /// Hue-rotated variant of `icon` for same-app disambiguation, rendered
    /// once when a second window of the same class appears (see
    /// `Wm::refresh_icon_rotations`) — the per-pixel OKLCH rotation is far
    /// too heavy to run per frame. `None` for hue slot 0 (a 0° rotation).
    pub icon_rotated: Option<Rc<Icon>>,
    /// WM_CLASS class string, used to group windows of the same app for
    /// icon color-rotation.
    pub class: Rc<str>,
    /// Persistent icon hue-rotation slot (see `theme::icon_hue_rotation`),
    /// assigned once when the window is managed and kept for its lifetime.
    /// Only applied while another window of the same `class` is also open —
    /// separate from split accent colours (`Leaf::color`).
    pub icon_slot: Option<usize>,
    /// Whether *we* currently have the window mapped. Drives the
    /// self-inflicted-unmap bookkeeping (`Wm::ignore_unmaps`) that lets a
    /// client's own withdraw be told apart from our layout hiding it.
    pub mapped: bool,
}

pub struct Wm {
    pub conn: RustConnection,
    pub root: Window,
    pub depth: u8,
    pub state: State,
    pub clients: HashMap<Win, Client>,
    /// Stable insertion order of managed windows, for the bottom bar.
    pub bar_order: Vec<Win>,
    /// The window pinned past the right end of the scrolling canvas, only
    /// revealed by scrolling all the way right (see `Wm::dock_title`),
    /// if one is currently mapped. It lives outside `clients`/the split
    /// tree/`bar_order` entirely: no chrome, no taskbar entry, not part of
    /// focus cycling, and normal tiled columns never lay out under it.
    pub docked: Option<Win>,
    /// Width of `docked`, captured from its own requested geometry when it
    /// was first managed.
    pub docked_w: i32,
    /// `WM_NAME` that marks the dock window (`SPLITWM_DOCK_TITLE`, default
    /// `theme::DOCK_TITLE`).
    pub dock_title: String,
    pub underlay: Window,
    /// Never-mapped window owning the ICCCM `WM_S<n>` manager selection for
    /// the whole process lifetime; a `SelectionClear` naming it means
    /// another WM has taken over (e.g. via its own `--replace`), so we quit
    /// gracefully and let it grab `SUBSTRUCTURE_REDIRECT`.
    pub sel_owner: Window,
    pub renderer: Renderer,
    pub gc: Gcontext,
    pub keymap: HashMap<u32, u8>,
    pub bindings: Vec<(u16, u8, Action)>,
    pub running: bool,
    pub max_req_bytes: usize,
    pub atoms: Atoms,
    /// Root geometry, cached at startup and refreshed by the root's own
    /// `ConfigureNotify` (RandR resize) — `arrange` consults it several
    /// times per frame, so it must not cost a server round trip.
    pub workarea: Rect,
    /// `SPLITWM_WALLPAPER`, kept so a root resize can rescale the wallpaper.
    pub wallpaper_path: Option<String>,
    /// `SPLITWM_DEBUG_SCROLL` presence, read once — the checks sit on
    /// per-event hot paths.
    pub debug_scroll: bool,
    pub animate: bool,
    pub prev_frame_rect: HashMap<NodeId, FrameRect>,
    pub handle_regions: Vec<(FrameRect, Boundary)>,
    pub plus_regions: Vec<(FrameRect, usize)>,
    /// The launcher "+" button at the right end of the bottom taskbar.
    pub taskbar_plus: FrameRect,
    pub tab_regions: Vec<(FrameRect, NodeId)>,
    pub taskbar_regions: Vec<TaskTile>,
    pub btn_regions: Vec<(FrameRect, NodeId, BtnKind)>,
    pub menu: MenuUi,
    pub drag: Option<Drag>,
    /// Hit-regions for the outer canvas-edge resize handles (see
    /// `Wm::compute_edge_handle_widgets`); the bool is `true` for the left
    /// edge, `false` for the right.
    pub edge_handle_regions: Vec<(FrameRect, bool)>,
    pub edge_drag: Option<EdgeDrag>,
    /// Startup-created pointer cursors + the one currently on the underlay.
    pub cursors: Cursors,
    /// Reusable BGRX staging buffer for `PutImage`, so the full-screen
    /// conversion doesn't reallocate each frame.
    pub bgrx: Vec<u8>,
    /// Slave devices with a horizontal scroll valuator (trackpads, scroll
    /// wheels with tilt), rebuilt whenever the device hierarchy changes.
    pub hscroll: Vec<HScroll>,
    /// Cached result of the last "is scrolling allowed here" pointer query
    /// and when it was taken, so a fast scroll burst doesn't force a
    /// `QueryPointer` round trip for every single event.
    pub hscroll_gate: (std::time::Instant, bool),
    /// Per-window count of `UnmapNotify` events we caused ourselves (layout
    /// hiding a client) and must swallow; any unmap beyond the count is the
    /// client withdrawing itself (ICCCM) and unmanages it.
    pub ignore_unmaps: HashMap<Win, u32>,
}

/// A device's horizontal scroll axis: which valuator carries it and how many
/// valuator units make up one wheel "click" (for scaling into pixels).
#[derive(Clone, Copy)]
pub struct HScroll {
    pub dev: u16,
    pub valuator: u16,
    pub incr: f64,
}

/// An in-progress gap resize started by dragging a handle.
#[derive(Clone, Copy)]
pub struct Drag {
    pub parent: NodeId,
    pub idx: usize,
    /// True when a horizontal gap (between stacked rows) is being dragged
    /// along y; false for a vertical gap dragged along x.
    pub vertical: bool,
    /// First (left/top) child's start along the drag axis, canvas-space.
    pub start: i32,
    pub combined: i32,
    pub gap: i32,
}

/// The pointer cursors the WM ever shows, created once at startup, plus the
/// one currently set on the underlay (so hover motion only issues a
/// `ChangeWindowAttributes` when it actually changes).
#[derive(Clone, Copy)]
pub struct Cursors {
    pub arrow: u32,
    /// Left/right double arrow, over vertical-gap and canvas-edge handles.
    pub h_resize: u32,
    /// Up/down double arrow, over horizontal-gap handles.
    pub v_resize: u32,
    /// Shown over disabled titlebar buttons (X-shaped `XC_X_cursor`; the
    /// core cursor font has no dedicated "not-allowed" glyph).
    pub disabled: u32,
    pub current: u32,
}

/// An in-progress edge-of-canvas resize, started by dragging the handle at
/// the canvas's outer left/right margin (see `State::resize_edge`).
#[derive(Clone, Copy)]
pub struct EdgeDrag {
    pub left: bool,
    /// Screen-space x of the resized column's *far* edge (the one not
    /// being dragged), fixed for the whole gesture — the mouse's distance
    /// from it is the column's new width directly, no scroll conversion
    /// needed.
    pub anchor_x: i32,
}

/// App launcher popup: a main column plus one optional category submenu, each
/// in its own override-redirect window composited like the underlay.
pub struct MenuUi {
    pub tree: crate::menu::MenuTree,
    pub main_win: Window,
    pub sub_win: Window,
    pub open: bool,
    pub main: FrameRect,
    pub main_cw: i32,
    pub main_hi: Option<usize>,
    pub open_cat: Option<usize>,
    pub sub_cw: i32,
    pub sub_hi: Option<usize>,
    pub target_leaf: NodeId,
}

/// The three split-control buttons on the right of every leaf's tab bar
/// (count mirrored by `theme::N_SPLIT_BTNS`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BtnKind {
    Minimize,
    Split,
    Close,
}

#[derive(Clone, Copy)]
pub struct FrameRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// A bottom-bar tile with its window and accent/visibility resolved once at
/// compute time, so per-frame compositing needs no tree walks.
#[derive(Clone, Copy)]
pub struct TaskTile {
    pub rect: FrameRect,
    /// The close ("x") badge in the tile's bottom-right corner; hit-tested
    /// before `rect` so it wins the click.
    pub close: FrameRect,
    pub win: Win,
    pub accent: crate::Index,
    pub on_screen: bool,
}

/// Per-leaf state driving the split-control buttons' icons/enabled state.
#[derive(Clone, Copy)]
pub struct LeafMeta {
    pub parent_dir: Option<Dir>,
    pub wider: bool,
    pub can_split: bool,
    pub minimized: bool,
}

/// A leaf placed during an arrange, retained so the animator can move it.
#[derive(Clone, Copy)]
pub struct Placement {
    pub leaf: NodeId,
    pub target: FrameRect,
    pub active_client: Option<Win>,
    pub focused: bool,
}

/// ease-out-back (slight overshoot then settle), matching animation.lua.
pub fn ease_out_back(t: f32) -> f32 {
    let c = 1.1_f32;
    let t = t - 1.0;
    let inner = (c + 1.0).mul_add(t, c);
    (t * t).mul_add(inner, 1.0)
}

pub const fn rect_contains(r: FrameRect, x: i32, y: i32) -> bool {
    x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h
}

pub fn lerp_rect(a: FrameRect, b: FrameRect, p: f32) -> FrameRect {
    let l = |s: i32, e: i32| s + ((e - s) as f32 * p) as i32;
    FrameRect {
        x: l(a.x, b.x),
        y: l(a.y, b.y),
        w: l(a.w, b.w).max(1),
        h: l(a.h, b.h).max(1),
    }
}

pub const MOD4: u16 = 0x40; // ModMask::M4

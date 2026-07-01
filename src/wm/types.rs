//! Shared types, helpers and constants for the X11 window-manager core.

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap
)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use x11rb::protocol::xproto::{ConnectionExt, Gcontext, Window};
use x11rb::rust_connection::RustConnection;

use crate::render::{Icon, Renderer};
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
    KillClient,
}

pub struct Client {
    pub label: char,
    pub icon: Option<Rc<Icon>>,
    /// WM_CLASS class string, used to group windows of the same app for
    /// icon color-rotation.
    pub class: Rc<str>,
    /// Persistent icon hue-rotation slot (see `theme::icon_hue_rotation`),
    /// assigned once when the window is managed and kept for its lifetime.
    /// Only applied while another window of the same `class` is also open —
    /// separate from split accent colours (`Leaf::color`).
    pub icon_slot: Option<usize>,
}

pub struct Wm {
    pub conn: RustConnection,
    pub root: Window,
    pub depth: u8,
    pub state: State,
    pub clients: HashMap<Win, Client>,
    /// Stable insertion order of managed windows, for the bottom bar.
    pub bar_order: Vec<Win>,
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
    pub atom_net_wm_icon: u32,
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
    pub bgrx: RefCell<Vec<u8>>,
}

/// An in-progress gap resize started by dragging a handle.
#[derive(Clone, Copy)]
pub struct Drag {
    pub parent: NodeId,
    pub idx: usize,
    pub left_x: i32,
    pub combined: i32,
    pub gap: i32,
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

/// The four split-control buttons on the right of every leaf's tab bar.
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
    pub win: Win,
    pub accent: u32,
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

pub fn workarea(conn: &RustConnection, root: Window) -> R<Rect> {
    let geo = conn.get_geometry(root)?.reply()?;
    Ok(Rect {
        x: 0,
        y: 0,
        w: i32::from(geo.width),
        h: i32::from(geo.height),
    })
}

//! Shared types, helpers and constants for the X11 window-manager core.

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap
)]

use std::collections::HashMap;
use std::rc::Rc;

use x11rb::protocol::xproto::{Atom, ConnectionExt, Gcontext, Pixmap, Window};
use x11rb::rust_connection::RustConnection;

use crate::icon::Icon;
use crate::render::Renderer;
use crate::state::State;
use crate::tree::{Boundary, Dir, NodeId, Rect, Win};

pub type R<T> = Result<T, WmError>;

/// The `wm` error type, split at construction time into "the X connection
/// itself is gone" versus everything else. Classifying at the `?` site —
/// via the `From` impls below, before any wrapping can happen — is what
/// lets the event loop decide fatality with a plain `match` instead of
/// walking `source()` chains and hoping no intermediate layer flattened the
/// original error into a string.
#[derive(Debug)]
pub enum WmError {
    /// The X connection is dead (socket closed, server gone): the event
    /// loop must exit — retrying would spin forever on a socket that can
    /// never deliver again.
    Fatal(x11rb::errors::ConnectionError),
    /// An ordinary per-request failure (e.g. a reply from a window that
    /// raced us and died): contained and logged, the session continues.
    Other(Box<dyn std::error::Error>),
}

impl std::fmt::Display for WmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Fatal(e) => write!(f, "X connection lost: {e}"),
            Self::Other(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for WmError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Fatal(e) => Some(e),
            Self::Other(e) => Some(e.as_ref()),
        }
    }
}

impl WmError {
    pub const fn is_fatal(&self) -> bool {
        matches!(self, Self::Fatal(_))
    }
}

impl From<x11rb::errors::ConnectionError> for WmError {
    fn from(e: x11rb::errors::ConnectionError) -> Self {
        Self::Fatal(e)
    }
}

impl From<x11rb::errors::ReplyError> for WmError {
    fn from(e: x11rb::errors::ReplyError) -> Self {
        match e {
            x11rb::errors::ReplyError::ConnectionError(c) => Self::Fatal(c),
            other => Self::Other(other.into()),
        }
    }
}

impl From<x11rb::errors::ReplyOrIdError> for WmError {
    fn from(e: x11rb::errors::ReplyOrIdError) -> Self {
        match e {
            x11rb::errors::ReplyOrIdError::ConnectionError(c) => Self::Fatal(c),
            other => Self::Other(other.into()),
        }
    }
}

impl From<x11rb::errors::ConnectError> for WmError {
    fn from(e: x11rb::errors::ConnectError) -> Self {
        Self::Other(e.into())
    }
}

impl From<std::io::Error> for WmError {
    fn from(e: std::io::Error) -> Self {
        Self::Other(e.into())
    }
}

impl From<String> for WmError {
    fn from(e: String) -> Self {
        Self::Other(e.into())
    }
}

impl From<&str> for WmError {
    fn from(e: &str) -> Self {
        Self::Other(e.into())
    }
}

// Keyboard configuration (keysyms, actions, the binding table) lives in
// `theme` with the rest of the user-tunable config; re-exported here so
// `wm` code imports it alongside the other shared types.
pub use crate::theme::{Action, MOD4};

/// Declare the `Atoms` struct and its `intern`: each field is written next
/// to the atom name it holds, so the pairing is checked by construction —
/// a positional array + destructuring pattern would compile fine even if
/// the two lists drifted out of order, silently binding fields to wrong
/// atoms.
macro_rules! atoms {
    ($($(#[$doc:meta])* $field:ident => $name:expr,)+) => {
        /// Interned atoms used for ICCCM/EWMH interop, fetched once at startup.
        pub struct Atoms {
            $($(#[$doc])* pub $field: Atom,)+
        }

        impl Atoms {
            pub fn intern(conn: &RustConnection) -> R<Self> {
                // Send every InternAtom before reading any reply, so
                // interning costs one round trip instead of one per atom.
                $(let $field = conn.intern_atom(false, $name)?;)+
                Ok(Self {
                    $($field: $field.reply()?.atom,)+
                })
            }
        }
    };
}

atoms! {
    wm_protocols => b"WM_PROTOCOLS",
    wm_delete_window => b"WM_DELETE_WINDOW",
    wm_state => b"WM_STATE",
    net_wm_icon => b"_NET_WM_ICON",
    net_supported => b"_NET_SUPPORTED",
    net_client_list => b"_NET_CLIENT_LIST",
    net_active_window => b"_NET_ACTIVE_WINDOW",
    net_supporting_wm_check => b"_NET_SUPPORTING_WM_CHECK",
    net_wm_name => b"_NET_WM_NAME",
    net_wm_window_type => b"_NET_WM_WINDOW_TYPE",
    net_wm_window_type_notification => b"_NET_WM_WINDOW_TYPE_NOTIFICATION",
    net_wm_window_type_dialog => b"_NET_WM_WINDOW_TYPE_DIALOG",
    net_wm_state => b"_NET_WM_STATE",
    net_wm_state_fullscreen => b"_NET_WM_STATE_FULLSCREEN",
    net_workarea => b"_NET_WORKAREA",
    net_number_of_desktops => b"_NET_NUMBER_OF_DESKTOPS",
    net_current_desktop => b"_NET_CURRENT_DESKTOP",
    wm_take_focus => b"WM_TAKE_FOCUS",
    utf8_string => b"UTF8_STRING",
    /// Wakeup ClientMessage type from the notification-daemon thread (see
    /// `crate::notify`): "drain the note channel and update popups".
    splitwm_note => crate::notify::PING_ATOM.as_bytes(),
    /// Wakeup ClientMessage type from a background theme-icon fetch thread
    /// (see `Wm::spawn_theme_icon_fetch`): "drain the icon-result channel".
    splitwm_icon => b"SPLITWM_ICON",
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
    /// `WM_NORMAL_HINTS` minimum size, read at manage time. Tiling never
    /// configures the window below it (the split clips instead), so apps
    /// with a hard minimum aren't handed geometry they can't honour.
    pub min_size: (i32, i32),
    /// ICCCM focus model, read from `WM_HINTS.input` / `WM_TAKE_FOCUS` in
    /// `WM_PROTOCOLS` at manage time (see `Wm::give_focus`).
    pub focus: FocusModel,
    /// When `_NET_WM_ICON` was last fetched for this window. A fetch can
    /// transfer up to 16 MiB and forces a full recomposite, and the property
    /// is client-controlled: `on_icon_change` rate-limits fetches against
    /// this (see `ICON_FETCH_COOLDOWN`).
    pub icon_fetched: std::time::Instant,
    /// An icon PropertyNotify arrived inside the cooldown window; the fetch
    /// is deferred to `Wm::flush_stale_icons` so a burst's final icon still
    /// lands.
    pub icon_stale: bool,
}

/// How a window wants keyboard focus delivered (ICCCM 4.1.7): whether
/// `SetInputFocus` applies (`WM_HINTS.input`, default true) and whether the
/// client wants a `WM_TAKE_FOCUS` handshake.
#[derive(Clone, Copy)]
pub struct FocusModel {
    pub input: bool,
    pub take_focus: bool,
}

/// A floating window: a dialog/transient (`WM_TRANSIENT_FOR` or
/// `_NET_WM_WINDOW_TYPE_DIALOG`) or a fixed-size client (min == max in
/// `WM_NORMAL_HINTS`). Never in `clients`/the split tree/taskbar: shown at
/// its requested size, centered over its parent's split (or the workarea),
/// stacked above every tiled client, focused on map and click but not part
/// of Mod4+Tab cycling.
pub struct FloatWin {
    pub win: Win,
    /// Our own chrome window stacked just below `win`: the split border art
    /// (border + titlebar, no control buttons), draggable to move the float.
    pub frame: Window,
    /// `WM_TRANSIENT_FOR` target, used for centering and for handing focus
    /// back when the float goes away.
    pub parent: Option<Win>,
    pub focus: FocusModel,
    /// Client-window screen geometry (the frame extends `BORDER_LEFT` /
    /// `tb_h` around it), tracked so drags and repaints don't need a
    /// `GetGeometry` round trip.
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    /// Accent palette index for the chrome — the transient parent's split
    /// colour when it has one, so the dialog visibly belongs to it.
    pub accent: crate::Index,
    /// Titlebar app icon/label, resolved once at manage time.
    pub icon: Option<Rc<Icon>>,
    pub label: char,
}

pub struct Wm {
    pub conn: RustConnection,
    pub root: Window,
    pub depth: u8,
    pub state: State,
    pub clients: HashMap<Win, Client>,
    /// Floating dialogs/transients/fixed-size windows, in mapping order
    /// (see `FloatWin`). Kept above tiled clients by every arrange.
    pub floats: Vec<FloatWin>,
    /// The float that last took focus, if it still has it — keyboard
    /// actions that target "the focused window" (close) act on this before
    /// falling back to the focused split's client. Cleared whenever focus
    /// moves back into the tree.
    pub focused_float: Option<Win>,
    /// Timestamp of the last user input event, used instead of
    /// `CURRENT_TIME` for `SetInputFocus`/`WM_TAKE_FOCUS` (ICCCM wants real
    /// timestamps so a slow client can't steal focus back across a race).
    pub last_event_time: u32,
    /// Wall-clock moment `last_event_time` was harvested. Server timestamps
    /// can't be compared to local clocks, but the harvest *age* can: past
    /// `STALE_TIMESTAMP`, `give_focus` fetches a fresh server time (see
    /// `Wm::fresh_timestamp`) instead of passing a stale one, which the
    /// server would silently ignore if focus moved more recently.
    pub last_event_instant: std::time::Instant,
    /// Keycode of a layout-mutating key (split/close) currently held down,
    /// if any — cleared on its `KeyRelease` and set on its `KeyPress`. A
    /// `KeyPress` for the keycode already recorded here is autorepeat (no
    /// release arrived in between) and is swallowed: holding Mod4+V must
    /// not create ~20 splits a second. A `KeyPress` for a different keycode,
    /// or for this one after its release cleared the record, is a genuine
    /// new press and goes through. This only distinguishes repeat from
    /// fresh presses structurally if XKB detectable autorepeat is on (see
    /// `enable_detectable_autorepeat`) or the classic same-timestamp
    /// release/press pairing is in effect — both deliver the release before
    /// the repeat, so a wall-clock heuristic is never needed.
    pub held_layout_key: Option<u8>,
    /// Parent lookup for every node, rebuilt from one arena walk per
    /// `arrange` — per-event callers (`hover_cursor`, `click_split_button`)
    /// read this instead of paying `Tree::find_parent`'s full arena scan.
    pub parents: HashMap<NodeId, (NodeId, usize)>,
    /// Events drained while waiting for something specific (currently only
    /// `Wm::fresh_timestamp`'s PropertyNotify); the main loop consumes these
    /// before blocking for new ones, preserving their order.
    pub pending_events: Vec<x11rb::protocol::Event>,
    /// Stable insertion order of managed windows, for the bottom bar.
    pub bar_order: Vec<Win>,
    /// The dock window and its tracked geometry/title (see `DockState`).
    pub dock: DockState,
    /// Foreign notification windows and our own served-notification popups
    /// (see `NoteState`).
    pub notes: NoteState,
    pub underlay: Window,
    /// Server-side pixmap holding the underlay's composited image, set as the
    /// underlay's `background_pixmap` so the server repaints exposed regions
    /// itself (no black flash while a shaped client moves over it).
    pub underlay_pix: Pixmap,
    /// Current size of `underlay_pix`; recreated by `compose` on mismatch
    /// (RandR resize).
    pub underlay_pix_size: (u16, u16),
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
    /// The in-flight layout animation, if any (see `LayoutAnim`); stepped by
    /// the main event loop, replaced or cancelled by the next `arrange`.
    pub anim: Option<LayoutAnim>,
    /// Monotonic counter stamping each `LayoutAnim` (see `LayoutAnim::seq`).
    pub anim_seq: u64,
    /// MIT-SHM frame-blit segment, created on first blit and recreated when
    /// a frame outgrows it (see `ShmSeg`). MIT-SHM 1.2 is required: there
    /// is no core-protocol fallback, so a server without it can't run
    /// splitwm at all.
    pub shm: Option<ShmSeg>,
    pub prev_frame_rect: HashMap<NodeId, FrameRect>,
    /// Every hit-testable widget rect for the current layout, rebuilt as one
    /// unit by `compute_widgets` each arrange (see `Widgets`).
    pub widgets: Widgets,
    /// The quick-launch entries shown as taskbar icons after the window
    /// tiles (see `QuickSlot`), resolved once at startup.
    pub quick: Vec<QuickSlot>,
    /// In-progress gap/float/edge drags (see `DragState`).
    pub drags: DragState,
    /// Startup-created pointer cursors + the one currently on the underlay.
    pub cursors: Cursors,
    /// Slave devices with a horizontal scroll valuator (trackpads, scroll
    /// wheels with tilt), rebuilt whenever the device hierarchy changes.
    pub hscroll: Vec<HScroll>,
    /// Sub-pixel scroll remainder carried between batches, so slow swipes
    /// whose per-batch delta rounds to zero pixels still accumulate.
    pub hscroll_frac: f64,
    /// Cached result of the last "is scrolling allowed here" pointer query
    /// and when it was taken, so a fast scroll burst doesn't force a
    /// `QueryPointer` round trip for every single event. `None` until the
    /// first query.
    pub hscroll_gate: Option<(std::time::Instant, bool)>,
    /// Per-window (u16-truncated) sequence numbers of `UnmapWindow` requests
    /// we issued ourselves (layout hiding a client), so `on_unmap` can match
    /// the resulting `UnmapNotify` by sequence and swallow it; any unmap
    /// with no matching record is the client withdrawing itself (ICCCM) and
    /// unmanages it.
    pub ignore_unmaps: HashMap<Win, Vec<u16>>,
    /// The managed client currently in EWMH fullscreen
    /// (`_NET_WM_STATE_FULLSCREEN`), if any: covers the whole workarea above
    /// every tiled client and float. Its split slot is kept, so leaving
    /// fullscreen drops it straight back into the layout.
    pub fullscreen: Option<Win>,
    /// Whether any client has `icon_stale` set, so the per-batch
    /// `Wm::flush_stale_icons` can skip its clients scan in the (usual)
    /// steady state where no icon refresh was throttled.
    pub icons_stale: bool,
    /// Results from background theme-icon fetches (see
    /// `Wm::spawn_theme_icon_fetch`), drained by `Wm::on_icon_ping`.
    pub icon_rx: std::sync::mpsc::Receiver<IconResult>,
    /// Cloned into each background theme-icon fetch thread so it can report
    /// its result back; kept here so `manage` doesn't need its own thread
    /// plumbing at every call site.
    pub icon_tx: std::sync::mpsc::Sender<IconResult>,
}

/// The docked-sidebar identity config and the currently docked window.
pub struct DockState {
    /// The window pinned past the right end of the scrolling canvas, only
    /// revealed by scrolling all the way right (see `DockState::title`),
    /// if one is currently mapped. It lives outside `clients`/the split
    /// tree/`bar_order` entirely: no chrome, no taskbar entry, not part of
    /// focus cycling, and normal tiled columns never lay out under it.
    pub docked: Option<Dock>,
    /// Identity that marks the dock window — matched against either half of
    /// its `WM_CLASS` (`SPLITWM_DOCK_TITLE`, default `theme::DOCK_TITLE`);
    /// also the desktop id used to autostart it.
    pub title: String,
}

/// A docked window and the width captured from its own requested geometry
/// when it was first managed — the width only exists while something is
/// docked, so the pair travels as one value.
#[derive(Clone, Copy)]
pub struct Dock {
    pub win: Win,
    pub w: i32,
}

impl Dock {
    /// `theme::DOCK_OVERLAP` clamped to the dock's own width — an overlap
    /// wider than the dock would otherwise shove its right edge permanently
    /// away from the screen edge (fully tucked is the useful maximum).
    pub fn overlap(self) -> i32 {
        crate::theme::DOCK_OVERLAP.min(self.w)
    }
}

/// Result of a background theme-icon fetch (see
/// `Wm::spawn_theme_icon_fetch`), tagged with the window it was resolved
/// for. By the time this arrives `win` may already be unmanaged — the
/// receiver must check before applying it, same as `Wm::on_icon_change`
/// already does for its own late-arriving fetch.
pub struct IconResult {
    pub win: Win,
    /// `None` when the theme lookup/decode failed — nothing to apply, but
    /// still worth draining so the channel doesn't grow unbounded.
    pub icon: Option<Icon>,
}

/// Foreign notification windows and our own served-notification popups.
pub struct NoteState {
    /// Notification windows (`_NET_WM_WINDOW_TYPE_NOTIFICATION`), in mapping
    /// order. Like the dock window, they live outside `clients`/the split
    /// tree/`bar_order`: no chrome, no taskbar entry, no focus cycling.
    /// They stack above everything at the bottom-right of the screen
    /// (see `Wm::place_notifications`), at whatever size they requested —
    /// tracked here (updated on ConfigureRequest) so restacking the pile
    /// doesn't cost a `GetGeometry` round trip per window.
    pub foreign: Vec<ForeignNote>,
    /// Speech-bubble popups for notifications *we* serve as the session's
    /// `org.freedesktop.Notifications` daemon (see `crate::notify` and
    /// `Wm::on_note_ping`). Own override-redirect windows, drawn by the
    /// renderer, stacked bottom-right above the `foreign` pile.
    pub popups: Vec<NotePopup>,
    /// Incoming notification events from the daemon thread.
    pub rx: std::sync::mpsc::Receiver<crate::notify::NoteMsg>,
    /// `(id, close reason)` of popups the WM closed itself — user click
    /// (`CloseReason::Dismissed`) or popup-cap eviction
    /// (`CloseReason::Undefined`) — reported back to the daemon thread so
    /// it emits the matching `NotificationClosed` signal.
    pub dismiss: std::sync::mpsc::Sender<(u32, crate::notify::CloseReason)>,
}

/// In-progress gap/float/edge drags.
pub struct DragState {
    pub split: Option<Drag>,
    /// An in-progress float move, started by pressing button 1 on a float's
    /// frame window.
    pub float: Option<FloatDrag>,
    pub edge: Option<EdgeDrag>,
}

/// Every hit-testable widget rect computed for the current layout: gap drag
/// handles, "+" insert buttons, tab titles, split-control buttons, taskbar
/// tiles, the quick-launch icons, and the canvas-edge resize handles.
/// Grouped so the whole set is rebuilt (and cleared) as one unit by
/// `Wm::compute_widgets` — the caches must always describe the same arrange.
#[derive(Default)]
pub struct Widgets {
    pub handle_regions: Vec<(FrameRect, Boundary)>,
    pub plus_regions: Vec<(FrameRect, usize)>,
    /// Quick-launch icons in the bottom taskbar (after the window tiles),
    /// paired with their `Wm::quick` index; entries hidden by their
    /// `ShowWhen` rule get no region.
    pub quick_regions: Vec<(FrameRect, usize)>,
    /// The pill separating window tiles from the quick-launch icons; only
    /// present when both groups are (an unpaired separator is just clutter).
    pub taskbar_sep: Option<FrameRect>,
    pub tab_regions: Vec<(FrameRect, NodeId)>,
    pub taskbar_regions: Vec<TaskTile>,
    pub btn_regions: Vec<(FrameRect, NodeId, BtnKind)>,
    /// Hit-regions for the outer canvas-edge resize handles (see
    /// `Wm::compute_edge_handle_widgets`); the bool is `true` for the left
    /// edge, `false` for the right.
    pub edge_handle_regions: Vec<(FrameRect, bool)>,
}

impl Widgets {
    /// Drop every region (and stale rect) from the previous layout.
    pub fn clear(&mut self) {
        self.handle_regions.clear();
        self.plus_regions.clear();
        self.quick_regions.clear();
        self.taskbar_sep = None;
        self.tab_regions.clear();
        self.btn_regions.clear();
        self.taskbar_regions.clear();
        self.edge_handle_regions.clear();
    }
}

/// A foreign notification window (`_NET_WM_WINDOW_TYPE_NOTIFICATION`) and
/// its last-known size, so the bottom-right pile can be restacked without
/// re-querying geometry.
#[derive(Clone, Copy)]
pub struct ForeignNote {
    pub win: Win,
    pub w: i32,
    pub h: i32,
}

/// One on-screen speech-bubble notification popup and the note it shows.
pub struct NotePopup {
    pub win: Window,
    pub note: crate::notify::Note,
    pub w: i32,
    pub h: i32,
}

/// A device's horizontal scroll axis: which valuator carries it and how many
/// valuator units make up one wheel "click" (for scaling into pixels).
#[derive(Clone, Copy)]
pub struct HScroll {
    pub dev: u16,
    pub valuator: u16,
    pub incr: f64,
}

/// An in-progress float move: dragging a float's frame repositions the
/// frame + client pair. `dx`/`dy` are the pointer's offset from the client
/// window's origin at press time, so the window tracks the grab point.
#[derive(Clone, Copy)]
pub struct FloatDrag {
    pub win: Win,
    pub dx: i32,
    pub dy: i32,
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
    /// Shown over disabled titlebar buttons (the hand-drawn circled-X
    /// sprite; `XC_X_cursor` when the server lacks RENDER cursors).
    pub disabled: u32,
    /// Pointing hand over clickable things: live titlebar buttons, boundary
    /// "+" buttons, and the taskbar (`XC_hand2` fallback).
    pub hand: u32,
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

/// One taskbar quick-launch entry: the command it spawns and its icon,
/// resolved once at startup (see `crate::launch::quick_launches`).
pub struct QuickSlot {
    pub cmd: String,
    /// Decoded, palette-quantized icon; `None` falls back to the label glyph.
    pub icon: Option<Rc<crate::icon::Icon>>,
    /// First letter of the entry's label, the no-icon fallback glyph.
    pub label: char,
    /// Visibility rule, re-evaluated against the managed clients each
    /// arrange (see `compute_taskbar`).
    pub show: crate::theme::ShowWhen,
}

/// The three split-control buttons on the right of every leaf's tab bar
/// (count mirrored by `theme::N_SPLIT_BTNS`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BtnKind {
    Minimize,
    Split,
    Close,
}

/// Screen-space rect; the same shape as canvas-space `tree::Rect`, aliased
/// so signatures can still say which space they mean.
pub type FrameRect = Rect;

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
    /// Whether the window occupies a split (drives the accent highlight).
    /// Deliberately not "on screen": a split scrolled out of the viewport
    /// still counts.
    pub in_split: bool,
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

/// ease-out-back: slight overshoot past the target, then settle.
pub fn ease_out_back(t: f32) -> f32 {
    let c = 1.1_f32;
    let t = t - 1.0;
    let inner = (c + 1.0).mul_add(t, c);
    (t * t).mul_add(inner, 1.0)
}

/// Clamp a signed window dimension (width/height) to the `u32` X11 wire
/// type, floored at 1px (X rejects zero-size windows). A negative input
/// indicates a layout bug upstream; in debug builds that fails loudly via
/// the assertion instead of silently producing a 1px sliver.
pub fn clamp_dim(v: i32) -> u32 {
    debug_assert!(v > 0, "clamp_dim: non-positive dimension {v}");
    v.max(1) as u32
}

/// Screen geometry `(x, y, w, h)` of the client window inside a leaf frame
/// rect: inset by the border and titlebar, never below the client's
/// `WM_NORMAL_HINTS` minimum (the split clips instead of handing the app
/// geometry it can't honour). The single inset formula behind both
/// `Wm::place_clients` (configuring) and `Wm::tracked_geometry` (answering
/// denied ConfigureRequests), so the two can't drift apart.
pub fn client_rect_in_frame(r: FrameRect, (min_w, min_h): (i32, i32)) -> (i32, i32, i32, i32) {
    let (bw, tb) = (crate::theme::BORDER_LEFT, crate::theme::tb_h());
    (
        r.x + bw,
        r.y + tb,
        (r.w - 2 * bw).max(min_w).max(1),
        (r.h - tb - bw).max(min_h).max(1),
    )
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

/// An in-flight layout animation, stepped by the main event loop (one frame
/// per loop iteration, ~60 Hz) rather than a blocking render loop inside
/// `arrange` — so events keep flowing while chrome slides. Client windows
/// are already at their final rects (placed by the arrange that started
/// this); only the composited chrome interpolates.
pub struct LayoutAnim {
    /// Distinguishes this animation from one started while handling the
    /// same event batch, so a batch's cut-short signal can't kill the very
    /// animation it triggered.
    pub seq: u64,
    pub start: std::time::Instant,
    /// Each animated leaf's start rect paired with its target placement —
    /// one entry per leaf, so start and target can't desync.
    pub placed: Vec<(FrameRect, Placement)>,
}

/// A memfd-backed shared memory segment attached to the server via
/// `ShmAttachFd`. The fd is handed to the server at attach time; the local
/// mapping outlives it.
///
/// The mapping is split into two halves used as alternating frame buffers:
/// the blit path writes frame N+1 into one half while the server may still
/// be reading frame N from the other, so no per-frame round trip is needed
/// to serialise reuse (see `Wm::blit_fb`).
pub struct ShmSeg {
    /// Server-side segment id (XID).
    pub seg: u32,
    ptr: *mut u8,
    pub len: usize,
    /// Which half the next frame is written into.
    pub half: usize,
    /// Per-half: whether an unconfirmed `ShmPutImage` reading that half is
    /// (potentially) still in flight. Set on put, cleared by the round trip
    /// `Wm::blit_fb` performs before overwriting a pending half.
    pub pending: [bool; 2],
}

impl ShmSeg {
    /// # Safety
    /// `ptr` must be the start of a live `MAP_SHARED` mapping of at least
    /// `len` bytes that this `ShmSeg` uniquely owns: `slice()` will hand out
    /// `&mut [u8]` views of it and `Drop` will `munmap(ptr, len)`.
    pub unsafe fn new(seg: u32, ptr: *mut u8, len: usize) -> Self {
        Self {
            seg,
            ptr,
            len,
            half: 0,
            pending: [false; 2],
        }
    }

    /// Byte capacity of one half (a frame must fit in this).
    pub fn half_len(&self) -> usize {
        self.len / 2
    }

    /// Byte offset of the current half within the segment.
    pub fn offset(&self) -> usize {
        self.half * self.half_len()
    }

    /// The first `len` bytes of the *current half* of the mapping (callers
    /// size frames to fit; see `half_len`).
    pub fn slice(&mut self, len: usize) -> &mut [u8] {
        assert!(len <= self.half_len());
        // SAFETY: ptr/len describe a live MAP_SHARED mapping owned by self,
        // and offset + len stays within it.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.add(self.offset()), len) }
    }
}

impl Drop for ShmSeg {
    fn drop(&mut self) {
        // SAFETY: mapping was created by mmap with this exact ptr/len.
        unsafe {
            libc::munmap(self.ptr.cast(), self.len);
        }
    }
}

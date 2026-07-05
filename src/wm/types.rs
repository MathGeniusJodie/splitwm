//! The `Wm` struct itself (assembly, its self-healing-record accessors),
//! the error type, interned atoms, and the small set of geometry helpers
//! genuinely shared across every subsystem. Each subsystem's own types live
//! next to the code that defines them (see `dock`, `floats`, `icons`,
//! `input`, `notifications`, `present`, `widgets`) and are re-exported here
//! only where `Wm`'s own fields need to name them.

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
use crate::tree::{NodeId, Rect, Win};

use super::arrange::LayoutAnim;
use super::dock::DockState;
use super::floats::FloatWin;
use super::icons::IconResult;
use super::input::{Cursors, DragState, HScroll, KeyRepeatState};
use super::notifications::NoteState;
use super::present::ShmSeg;
use super::widgets::{QuickSlot, Widgets};

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
    /// `_NET_WM_NAME`/`WM_NAME`, kept live by `Wm::on_title_change`; drawn in
    /// the titlebar next to the icon.
    pub title: Rc<str>,
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

/// Which of the four ways a managed window is handled — the single fact
/// `Wm::kind_of` answers instead of every event-dispatch site probing
/// `clients`/`floats`/`dock.docked`/`notes.foreign` in turn (previously
/// duplicated across `on_map_request`, `on_unmap`, `on_destroy`,
/// `on_configure_request`, `on_activate_request` and `tracked_geometry`).
/// Payload stays in each kind's own container; this only says which one, so
/// a window can't be registered under two kinds at once, and an exhaustive
/// match over it won't compile once a fifth kind exists until every site is
/// updated to handle it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WindowKind {
    Tiled,
    Float,
    Dock,
    Notification,
}

pub struct Wm {
    pub conn: RustConnection,
    pub root: Window,
    pub depth: u8,
    pub state: State,
    pub clients: HashMap<Win, Client>,
    /// Single source of truth for which container (this one, `floats`,
    /// `dock.docked`, `notes.foreign`) a managed window belongs to. Private:
    /// the only way to add or remove an entry is `register_kind`/
    /// `unregister_kind`, called at the same point the window is inserted
    /// into or removed from its payload container, so the two can't drift
    /// apart.
    window_kind: HashMap<Win, WindowKind>,
    /// Floating dialogs/transients/fixed-size windows, in mapping order
    /// (see `FloatWin`). Kept above tiled clients by every arrange.
    pub floats: Vec<FloatWin>,
    /// The float that last took focus, if it still has it — keyboard
    /// actions that target "the focused window" (close) act on this before
    /// falling back to the focused split's client. Cleared whenever focus
    /// moves back into the tree. Private: can dangle if the float it names
    /// is removed from `floats` without this being cleared too, so all
    /// reads/writes go through `Wm::focused_float`/`Wm::set_focused_float`/
    /// `Wm::clear_focused_float`, which keep the two in sync.
    focused_float: Option<Win>,
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
    /// `KeyRepeatState` of each layout-mutating key (split/close/mute-toggle)
    /// touched since startup. A `KeyPress` for a keycode already `Held` is
    /// autorepeat under XKB detectable autorepeat (consecutive `KeyPress`es
    /// with no intervening `KeyRelease`) and is swallowed: holding Mod4+V
    /// must not create ~20 splits a second, and holding mute must not
    /// re-toggle it 20 times a second. Under classic (non-detectable)
    /// autorepeat the `KeyRelease` *does* arrive before each repeat
    /// `KeyPress`, moving the keycode to `ReleasedAt(time)` — see
    /// `Wm::key_is_repeating` (which is what callers actually use to check
    /// "is this a repeat", not a raw lookup) for how that pairing is still
    /// recognised. A `Vec` rather than a `HashMap` because at most a
    /// couple of these keys are ever tracked at once (e.g. a split key and
    /// mute, held together), each entry independent so one key's release
    /// can't be confused for another's.
    pub layout_key_state: Vec<(u8, KeyRepeatState)>,
    /// Wall-clock moment `VolumeUp`/`VolumeDown` (index 0/1) last actually
    /// spawned a command, tracked separately per direction so a tap of one
    /// doesn't throttle an unrelated tap of the other. Unlike
    /// `held_layout_keys`, volume repeats are meant to keep landing while the
    /// key is held — it's a "resize by feel" action, not a discrete mutation
    /// — so a repeat isn't swallowed outright, just rate-limited: a
    /// `KeyPress` less than `VOLUME_SPAWN_INTERVAL` after this is skipped.
    /// That structural distinction doesn't apply here (there's nothing to
    /// "release back to fresh"), so this is the one place a wall-clock
    /// heuristic is the right tool rather than the KeyRelease bookkeeping
    /// above.
    pub last_volume_spawn: [Option<std::time::Instant>; 2],
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
    /// Private: the monotonic property only holds if every advance goes
    /// through `Wm::bump_anim_seq`, so nothing else in the module can
    /// reset or decrement it.
    anim_seq: u64,
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
    /// unmanages it. Private: the sequence-matching protocol only works if
    /// entries are recorded and pruned exactly as `on_unmap` expects, so all
    /// access goes through `Wm::record_ignored_unmap`/`Wm::take_ignored_unmap`/
    /// `Wm::forget_ignored_unmaps`.
    ignore_unmaps: HashMap<Win, Vec<u16>>,
    /// The managed client currently in EWMH fullscreen
    /// (`_NET_WM_STATE_FULLSCREEN`), if any: covers the whole workarea above
    /// every tiled client and float. Its split slot is kept, so leaving
    /// fullscreen drops it straight back into the layout. Private: can
    /// dangle if the window it names is unmanaged without this being
    /// cleared too, so all reads/writes go through `Wm::fullscreen`/
    /// `Wm::set_fullscreen_win`/`Wm::clear_fullscreen`.
    fullscreen: Option<Win>,
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

/// Values `wm::run` has no constant default for — resolved from the X
/// connection, config, or spawned helper threads — bundled so `Wm::assemble`
/// can build the rest of `Wm` (including its module-private fields) as one
/// literal without every other field of the struct needing to become
/// nameable from outside `types.rs`.
pub(super) struct WmInit {
    pub conn: RustConnection,
    pub root: Window,
    pub depth: u8,
    pub gc: Gcontext,
    pub renderer: Renderer,
    pub underlay: Window,
    pub sel_owner: Window,
    pub atoms: Atoms,
    pub workarea: Rect,
    pub debug_scroll: bool,
    pub quick: Vec<QuickSlot>,
    pub cursors: Cursors,
    pub icon_rx: std::sync::mpsc::Receiver<IconResult>,
    pub icon_tx: std::sync::mpsc::Sender<IconResult>,
    pub note_rx: std::sync::mpsc::Receiver<crate::notify::NoteMsg>,
    pub note_dismiss: std::sync::mpsc::Sender<(u32, crate::notify::CloseReason)>,
}

impl Wm {
    /// Build the initial `Wm`: everything not supplied by `init` starts at
    /// the "nothing managed/in-flight yet" value, since this only runs once
    /// at startup before the event loop's first iteration.
    pub(super) fn assemble(init: WmInit) -> Self {
        Self {
            conn: init.conn,
            root: init.root,
            depth: init.depth,
            gc: init.gc,
            keymap: HashMap::new(),
            bindings: Vec::new(),
            renderer: init.renderer,
            state: State::new(),
            clients: HashMap::new(),
            window_kind: HashMap::new(),
            floats: Vec::new(),
            focused_float: None,
            last_event_time: 0,
            last_event_instant: std::time::Instant::now(),
            layout_key_state: Vec::new(),
            last_volume_spawn: [None, None],
            parents: HashMap::new(),
            pending_events: Vec::new(),
            bar_order: Vec::new(),
            dock: DockState {
                docked: None,
                title: std::env::var("SPLITWM_DOCK_TITLE")
                    .unwrap_or_else(|_| crate::theme::DOCK_TITLE.to_string()),
            },
            notes: NoteState {
                foreign: Vec::new(),
                popups: Vec::new(),
                rx: init.note_rx,
                dismiss: init.note_dismiss,
            },
            underlay: init.underlay,
            underlay_pix: 0,
            underlay_pix_size: (0, 0),
            sel_owner: init.sel_owner,
            running: true,
            atoms: init.atoms,
            workarea: init.workarea,
            wallpaper_path: std::env::var("SPLITWM_WALLPAPER").ok(),
            debug_scroll: init.debug_scroll,
            animate: false,
            anim: None,
            anim_seq: 0,
            shm: None,
            prev_frame_rect: HashMap::new(),
            widgets: Widgets::default(),
            quick: init.quick,
            drags: DragState { active: None },
            cursors: init.cursors,
            hscroll: Vec::new(),
            hscroll_frac: 0.0,
            hscroll_gate: None,
            ignore_unmaps: HashMap::new(),
            fullscreen: None,
            icons_stale: false,
            icon_rx: init.icon_rx,
            icon_tx: init.icon_tx,
        }
    }

    /// The float holding keyboard focus, if it still exists — self-healing:
    /// a `focused_float` naming a float that's since been removed from
    /// `floats` is stale bookkeeping, not a live target, so it's cleared
    /// here and treated as `None` rather than trusted by callers.
    pub(crate) fn focused_float(&mut self) -> Option<Win> {
        if self
            .focused_float
            .is_some_and(|w| !self.floats.iter().any(|f| f.win == w))
        {
            self.focused_float = None;
        }
        self.focused_float
    }

    /// Record `win` as the float holding keyboard focus.
    pub(crate) fn set_focused_float(&mut self, win: Win) {
        self.focused_float = Some(win);
    }

    /// Drop the focused-float record unconditionally (focus is moving back
    /// into the tree, or elsewhere by deliberate action).
    pub(crate) fn clear_focused_float(&mut self) {
        self.focused_float = None;
    }

    /// Drop the focused-float record if it currently names `win` — used
    /// when `win` itself is going away, so a stale record for anything
    /// else is left untouched. Reports whether it did, since callers use
    /// this both to clean up and to decide whether `win` held focus.
    pub(crate) fn clear_focused_float_if(&mut self, win: Win) -> bool {
        self.focused_float.take_if(|&mut w| w == win).is_some()
    }

    /// Which kind of managed window `win` is, if any — the single lookup
    /// behind every event-dispatch cascade that used to test
    /// `clients`/`floats`/`dock.docked`/`notes.foreign` in sequence.
    pub(crate) fn kind_of(&self, win: Win) -> Option<WindowKind> {
        self.window_kind.get(&win).copied()
    }

    /// Record `win` as `kind`, called at the same point it's inserted into
    /// that kind's own container. Panics on silently re-registering a window
    /// under a *different* kind without unregistering it first — two
    /// containers claiming the same window is exactly the drift this map
    /// exists to rule out, so this is checked in release builds too rather
    /// than trusting every call site to pair register/unregister correctly.
    pub(crate) fn register_kind(&mut self, win: Win, kind: WindowKind) {
        if let Some(prev) = self.window_kind.insert(win, kind) {
            assert_eq!(prev, kind, "{win:#x} re-registered as a different WindowKind");
        }
    }

    /// Drop `win`'s kind record, called at the same point it's removed from
    /// that kind's own container. A no-op if it was never registered.
    pub(crate) fn unregister_kind(&mut self, win: Win) {
        self.window_kind.remove(&win);
    }

    /// `win`'s current geometry, or `None` on any request/reply failure —
    /// the shared cookie-to-option idiom behind every manage-time size probe
    /// (`Wm::manage_dock`, `Wm::manage_float`, `Wm::manage_notification`),
    /// each of which picks its own fallback for the `None` case.
    pub(crate) fn geometry(&self, win: Win) -> Option<x11rb::protocol::xproto::GetGeometryReply> {
        self.conn.get_geometry(win).ok()?.reply().ok()
    }

    /// Advance the animation-sequence counter and return the new value —
    /// the only way it moves, so it can only ever increase.
    pub(crate) fn bump_anim_seq(&mut self) -> u64 {
        self.anim_seq += 1;
        self.anim_seq
    }

    /// The window currently in EWMH fullscreen, if it's still managed —
    /// self-healing: a `fullscreen` naming a window that's since been
    /// unmanaged (neither a client nor a float) is stale, so it's cleared
    /// here and treated as `None` rather than trusted by callers.
    pub(crate) fn fullscreen(&mut self) -> Option<Win> {
        if self.fullscreen.is_some_and(|w| {
            !matches!(self.kind_of(w), Some(WindowKind::Tiled | WindowKind::Float))
        }) {
            self.fullscreen = None;
        }
        self.fullscreen
    }

    /// Set the fullscreen window, returning whatever window it previously
    /// named (even if stale) so callers can clean up the window it's
    /// replacing.
    pub(crate) fn set_fullscreen_win(&mut self, win: Win) -> Option<Win> {
        self.fullscreen.replace(win)
    }

    /// Drop the fullscreen record if it currently names `win`; reports
    /// whether it did, since callers use this both to clean up and to
    /// decide whether `win` was the fullscreen window at all.
    pub(crate) fn clear_fullscreen_if(&mut self, win: Win) -> bool {
        self.fullscreen.take_if(|&mut w| w == win).is_some()
    }

    /// Read-only peek at the fullscreen record, for `&self` contexts that
    /// immediately re-validate it themselves (so the self-healing of
    /// `fullscreen` isn't needed) and can't take `&mut self` to run it.
    pub(crate) fn raw_fullscreen(&self) -> Option<Win> {
        self.fullscreen
    }

    /// Record that an `UnmapWindow` request we issued ourselves for `win`
    /// used sequence number `seq`, so `take_ignored_unmap` can recognise
    /// the resulting `UnmapNotify` as self-inflicted.
    pub(crate) fn record_ignored_unmap(&mut self, win: Win, seq: u16) {
        self.ignore_unmaps.entry(win).or_default().push(seq);
    }

    /// Consume `win`'s ignored-unmap records against an `UnmapNotify` whose
    /// sequence is `seq`: prunes every record at or behind `seq` (modular
    /// u16 comparison — they've either just matched or can never arrive),
    /// drops the entry entirely once empty, and reports whether `seq`
    /// itself was one of the pruned records (a self-inflicted unmap to
    /// swallow, as opposed to the client's own withdraw).
    pub(crate) fn take_ignored_unmap(&mut self, win: Win, seq: u16) -> bool {
        let Some(seqs) = self.ignore_unmaps.get_mut(&win) else {
            return false;
        };
        let matched = prune_and_match_unmap_seq(seqs, seq);
        if seqs.is_empty() {
            self.ignore_unmaps.remove(&win);
        }
        matched
    }

    /// Drop every ignored-unmap record for `win` — it's being unmanaged, so
    /// any outstanding self-inflicted-unmap bookkeeping for it is moot.
    pub(crate) fn forget_ignored_unmaps(&mut self, win: Win) {
        self.ignore_unmaps.remove(&win);
    }

    /// Whether `ev`'s sequence is recorded as a self-inflicted unmap of its
    /// window, without consuming the record — used to exempt these from
    /// cutting an in-flight animation short (see `cuts_animation`), which
    /// must not prune entries that `on_unmap` still needs to see.
    pub(crate) fn is_ignored_unmap(&self, win: Win, seq: u16) -> bool {
        self.ignore_unmaps
            .get(&win)
            .is_some_and(|seqs| seqs.contains(&seq))
    }
}

/// Prune every recorded sequence at or behind `seq` (modular u16 comparison
/// — they've either just matched or can never arrive, since sequences only
/// increase), and report whether `seq` itself was one of the pruned records.
/// Pulled out of `Wm::take_ignored_unmap` since the matching/pruning is pure
/// `Vec` bookkeeping with no need for `self`.
fn prune_and_match_unmap_seq(seqs: &mut Vec<u16>, seq: u16) -> bool {
    let matched = seqs.contains(&seq);
    seqs.retain(|&s| s.wrapping_sub(seq) < 0x8000 && s != seq);
    matched
}

/// Screen-space rect; the same shape as canvas-space `tree::Rect`, aliased
/// so signatures can still say which space they mean.
pub type FrameRect = Rect;

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

#[cfg(test)]
mod tests {
    use super::prune_and_match_unmap_seq;

    /// A sequence that was never recorded matches nothing and leaves every
    /// outstanding record in place.
    #[test]
    fn unmatched_sequence_retains_all_and_reports_no_match() {
        let mut seqs = vec![10, 20, 30];
        assert!(!prune_and_match_unmap_seq(&mut seqs, 5));
        assert_eq!(seqs, vec![10, 20, 30]);
    }

    /// The exact recorded sequence matches and is itself removed.
    #[test]
    fn exact_match_is_consumed() {
        let mut seqs = vec![42];
        assert!(prune_and_match_unmap_seq(&mut seqs, 42));
        assert!(seqs.is_empty());
    }

    /// Sequences only ever increase, so a record numerically behind `seq`
    /// (within half the u16 space, per the wraparound comparison) can never
    /// arrive as its own UnmapNotify and is pruned as stale even though it
    /// isn't an exact match.
    #[test]
    fn stale_record_behind_current_seq_is_pruned_without_matching() {
        let mut seqs = vec![5];
        assert!(!prune_and_match_unmap_seq(&mut seqs, 10));
        assert!(seqs.is_empty());
    }

    /// A record numerically *ahead* of `seq` (an unmap we issued but whose
    /// notify hasn't arrived yet) is still outstanding and must survive.
    #[test]
    fn record_ahead_of_current_seq_survives() {
        let mut seqs = vec![20];
        assert!(!prune_and_match_unmap_seq(&mut seqs, 10));
        assert_eq!(seqs, vec![20]);
    }

    /// u16 sequence numbers wrap; a record just behind `seq` across the
    /// wraparound point must still be recognised as stale, not mistaken for
    /// one far in the future.
    #[test]
    fn wraparound_stale_record_is_pruned() {
        let mut seqs = vec![u16::MAX - 2];
        assert!(!prune_and_match_unmap_seq(&mut seqs, 3));
        assert!(seqs.is_empty());
    }

    /// With multiple outstanding sequences, only the one matching `seq` is
    /// reported as matched, and only stale ones (including the match itself)
    /// are pruned — an unrelated still-outstanding sequence survives.
    #[test]
    fn multiple_outstanding_only_one_matches() {
        let mut seqs = vec![10, 15, 25];
        assert!(prune_and_match_unmap_seq(&mut seqs, 15));
        assert_eq!(seqs, vec![25]);
    }
}

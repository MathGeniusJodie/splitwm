//! The managed-window store: the bridge between the pure layout core
//! (which tracks opaque `Win` ids) and smithay's `Window` objects, for
//! every kind of managed window — tiled, floating, docked.
//!
//! `Win`s are allocated here and only here, on manage; an id in the layout
//! always resolves while the window is managed, and tiled insertion order
//! is the taskbar order (matching master's `managed` store).

use smithay::desktop::Window;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;

use crate::theme;
use crate::tree::{Rect, Win};

/// A floating window's payload: a dialog/transient (xdg parent set) or a
/// fixed-size client (min == max). Never in the split tree/taskbar: shown
/// at its requested size, centered over its parent's split (or the
/// workarea), stacked above every tiled client, focused on map and click
/// but not part of Mod4+Tab cycling.
pub struct FloatData {
    /// The xdg parent's `Win`, used for centering/accent and for handing
    /// focus back when the float goes away.
    pub parent: Option<Win>,
    /// Client-window screen geometry (the chrome frame extends
    /// `BORDER_LEFT`/`tb_h`/`BORDER_BOTTOM` around it).
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    /// Accent palette index for the chrome — the transient parent's split
    /// colour when it has one, so the dialog visibly belongs to it.
    pub accent: crate::Index,
    /// The frame chrome's indexed GPU texture, rendered as its own element
    /// just below the client surface; re-uploaded only when its content
    /// (size/title/accent) changes — a drag just moves the element.
    pub frame: FrameTex,
}

/// A float frame's chrome texture and its freshness in one state: `Fresh`
/// can only be built with a texture, so "no texture yet not due for a
/// repaint" — a frame that would render as nothing forever — is
/// unrepresentable.
pub enum FrameTex {
    /// The frame's content (size/title/accent) changed since the held
    /// texture (if any — a float starts with none) was uploaded; repainted
    /// before the next composite, the old texture shown until then. The
    /// texture rides along so the repaint can reuse its GPU allocation.
    Stale(Option<crate::comp::indexed::IndexedTexture>),
    /// The texture matches the frame's current content.
    Fresh(crate::comp::indexed::IndexedTexture),
}

impl FrameTex {
    /// Whatever texture there is to draw, fresh or stale.
    pub fn texture(&self) -> Option<&crate::comp::indexed::IndexedTexture> {
        match self {
            FrameTex::Stale(tex) => tex.as_ref(),
            FrameTex::Fresh(tex) => Some(tex),
        }
    }

    pub fn is_stale(&self) -> bool {
        matches!(self, FrameTex::Stale(_))
    }

    /// Flag the frame for a repaint, keeping the texture to draw (and to
    /// reuse for the re-upload) meanwhile.
    pub fn mark_stale(&mut self) {
        *self = FrameTex::Stale(self.take());
    }

    /// Pull the texture out (leaving `Stale(None)`), for handing its
    /// allocation to an upload.
    pub fn take(&mut self) -> Option<crate::comp::indexed::IndexedTexture> {
        match std::mem::replace(self, FrameTex::Stale(None)) {
            FrameTex::Fresh(tex) | FrameTex::Stale(Some(tex)) => Some(tex),
            FrameTex::Stale(None) => None,
        }
    }
}

impl FloatData {
    /// Frame insets around a float's client window: the same border art
    /// the splits use.
    pub const fn insets() -> (i32, i32, i32) {
        (theme::BORDER_LEFT, theme::BORDER_TOP, theme::BORDER_BOTTOM)
    }

    /// The chrome frame's screen rect around the tracked client geometry.
    pub const fn frame_rect(&self) -> Rect {
        let (bw, tb, bb) = Self::insets();
        Rect {
            x: self.x - bw,
            y: self.y - tb,
            w: self.w + 2 * bw,
            h: self.h + tb + bb,
        }
    }
}

/// The docked sidebar's payload: the width captured from its own first
/// commit — the only fact about it besides which window it is.
#[derive(Clone, Copy)]
pub struct DockData {
    pub w: i32,
}

impl DockData {
    /// `theme::DOCK_OVERLAP` clamped to the dock's own width — an overlap
    /// wider than the dock would shove its right edge permanently away
    /// from the screen edge (fully tucked is the useful maximum).
    pub fn overlap(self) -> i32 {
        theme::DOCK_OVERLAP.min(self.w)
    }
}

/// What role a managed window plays; the payload carries the role's state.
pub enum Kind {
    Tiled,
    Float(FloatData),
    Dock(DockData),
}

pub struct ManagedWindow {
    pub win: Win,
    pub window: Window,
    pub kind: Kind,
    /// Palette-quantized app icon, resolved off-thread after manage (see
    /// `comp::icons`); `icon_rotated` is the pre-rendered hue variant for
    /// same-app disambiguation, `icon_slot` the persistent hue slot.
    pub icon: Option<std::rc::Rc<crate::icon::Icon>>,
    pub icon_rotated: Option<std::rc::Rc<crate::icon::Icon>>,
    pub icon_slot: Option<usize>,
}

#[derive(Default)]
pub struct Managed {
    /// Monotonic id source; `Win`s are never reused within a session, so a
    /// stale id from a closed window can never alias a live one.
    next: Win,
    /// Insertion-ordered; tiled entries' relative order is the taskbar
    /// order.
    entries: Vec<ManagedWindow>,
}

impl Managed {
    pub fn insert(&mut self, window: Window, kind: Kind) -> Win {
        self.next += 1;
        self.entries.push(ManagedWindow {
            win: self.next,
            window,
            kind,
            icon: None,
            icon_rotated: None,
            icon_slot: None,
        });
        self.next
    }

    pub fn remove(&mut self, win: Win) -> Option<ManagedWindow> {
        let idx = self.entries.iter().position(|m| m.win == win)?;
        Some(self.entries.remove(idx))
    }

    pub fn get(&self, win: Win) -> Option<&Window> {
        self.entry(win).map(|m| &m.window)
    }

    pub fn entry(&self, win: Win) -> Option<&ManagedWindow> {
        self.entries.iter().find(|m| m.win == win)
    }

    pub fn entry_mut(&mut self, win: Win) -> Option<&mut ManagedWindow> {
        self.entries.iter_mut().find(|m| m.win == win)
    }

    pub fn float(&self, win: Win) -> Option<(&Window, &FloatData)> {
        self.entry(win).and_then(|m| match &m.kind {
            Kind::Float(f) => Some((&m.window, f)),
            _ => None,
        })
    }

    pub fn float_mut(&mut self, win: Win) -> Option<(&Window, &mut FloatData)> {
        self.entry_mut(win).and_then(|m| match &mut m.kind {
            Kind::Float(f) => Some((&m.window, f)),
            _ => None,
        })
    }

    /// The docked window, if any (at most one at a time by construction:
    /// `manage_dock` refuses a second).
    pub fn dock(&self) -> Option<(Win, &Window, DockData)> {
        self.entries.iter().find_map(|m| match m.kind {
            Kind::Dock(d) => Some((m.win, &m.window, d)),
            _ => None,
        })
    }

    /// The `Win` whose window's root surface is `surface`, any kind and
    /// either backend (X11 surfaces resolve via their associated wl
    /// surface).
    pub fn win_for_surface(&self, surface: &WlSurface) -> Option<Win> {
        use smithay::desktop::WindowSurface;
        use smithay::wayland::seat::WaylandFocus as _;
        let _ = WindowSurface::Wayland; // backend-agnostic via WaylandFocus
        self.entries.iter().find_map(|m| {
            m.window
                .wl_surface()
                .is_some_and(|s| *s == *surface)
                .then_some(m.win)
        })
    }

    /// Every managed entry as `(Win, &Window)`, any kind.
    pub fn entries_windows(&self) -> impl Iterator<Item = (Win, &Window)> {
        self.entries.iter().map(|m| (m.win, &m.window))
    }

    pub fn win_for_window(&self, window: &Window) -> Option<Win> {
        self.entries
            .iter()
            .find_map(|m| (m.window == *window).then_some(m.win))
    }

    pub fn kind_of(&self, win: Win) -> Option<&Kind> {
        self.entry(win).map(|m| &m.kind)
    }

    /// Tiled windows in taskbar order.
    pub fn tiled_iter(&self) -> impl DoubleEndedIterator<Item = (Win, &Window)> {
        self.entries.iter().filter_map(|m| match m.kind {
            Kind::Tiled => Some((m.win, &m.window)),
            _ => None,
        })
    }

    /// Floats in insertion order (stacking is `Comp::float_stack`).
    pub fn float_iter(&self) -> impl Iterator<Item = (Win, &Window, &FloatData)> {
        self.entries.iter().filter_map(|m| match &m.kind {
            Kind::Float(f) => Some((m.win, &m.window, f)),
            _ => None,
        })
    }

    /// Every managed window, for frame-callback delivery.
    pub fn windows(&self) -> impl Iterator<Item = &Window> {
        self.entries.iter().map(|m| &m.window)
    }
}

/// The window's current title (xdg toplevel title / X11 `_NET_WM_NAME`),
/// or empty when unset.
pub fn toplevel_title(window: &Window) -> std::rc::Rc<str> {
    if let Some(x11) = window.x11_surface() {
        return x11.title().into();
    }
    read_toplevel_data(window, |d| d.title.as_deref().map(std::rc::Rc::from))
        .unwrap_or_else(|| "".into())
}

/// The window's class identity (xdg app_id / X11 `WM_CLASS` class half),
/// grouping windows of one app for labels/icons/quick-launch rules.
pub fn toplevel_app_id(window: &Window) -> String {
    if let Some(x11) = window.x11_surface() {
        return x11.class();
    }
    read_toplevel_data(window, |d| d.app_id.clone()).unwrap_or_default()
}

/// Politely ask a window to close, whichever backend it speaks.
pub fn close_window(window: &Window) {
    if let Some(toplevel) = window.toplevel() {
        toplevel.send_close();
    } else if let Some(x11) = window.x11_surface() {
        if let Err(err) = x11.close() {
            tracing::warn!("x11 close: {err}");
        }
    }
}

/// The xdg toplevel's parent surface (`xdg_toplevel.set_parent`), the
/// Wayland analogue of `WM_TRANSIENT_FOR`.
pub fn toplevel_parent(
    window: &Window,
) -> Option<smithay::reexports::wayland_server::protocol::wl_surface::WlSurface> {
    window.toplevel().and_then(|t| {
        smithay::wayland::compositor::with_states(t.wl_surface(), |states| {
            states
                .data_map
                .get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()
                .and_then(|d| d.lock().ok().and_then(|d| d.parent.clone()))
        })
    })
}

/// Whether the toplevel declares itself fixed-size (min == max, nonzero) —
/// it can't be resized, so stretching it into a split only produces gravel.
pub fn toplevel_fixed_size(window: &Window) -> bool {
    window
        .toplevel()
        .map(|t| {
            smithay::wayland::compositor::with_states(t.wl_surface(), |states| {
                let mut guard = states
                    .cached_state
                    .get::<smithay::wayland::shell::xdg::SurfaceCachedState>();
                let s = guard.current();
                s.min_size.w > 0 && s.min_size.h > 0 && s.min_size == s.max_size
            })
        })
        .unwrap_or(false)
}

fn read_toplevel_data<R>(
    window: &Window,
    f: impl Fn(&smithay::wayland::shell::xdg::XdgToplevelSurfaceRoleAttributes) -> Option<R>,
) -> Option<R> {
    window.toplevel().and_then(|t| {
        smithay::wayland::compositor::with_states(t.wl_surface(), |states| {
            states
                .data_map
                .get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()
                .and_then(|d| d.lock().ok().and_then(|d| f(&d)))
        })
    })
}

/// Client-area rect inside a leaf's chrome frame: below the titlebar,
/// inside the side/bottom borders. `min` lets a client's size floor
/// overhang the frame rather than be clipped (matching master).
pub fn client_rect_in_frame(r: Rect, (min_w, min_h): (i32, i32)) -> (i32, i32, i32, i32) {
    let (bw, tb) = (theme::BORDER_LEFT, theme::tb_h());
    (
        r.x + bw,
        r.y + tb,
        (r.w - 2 * bw).max(min_w).max(1),
        (r.h - tb - theme::BORDER_BOTTOM).max(min_h).max(1),
    )
}

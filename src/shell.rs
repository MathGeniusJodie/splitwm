//! The managed-window store: the bridge between the pure layout core
//! (which tracks opaque `Win` ids) and smithay's `Window` objects, for
//! every kind of managed window — tiled, floating, docked.
//!
//! `Win`s are allocated here and only here, on manage; an id in the layout
//! always resolves while the window is managed, and tiled insertion order
//! is the taskbar order (matching master's `managed` store).

use smithay::desktop::Window;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;

use crate::layout::{Rect, Win};
use crate::theme;

/// A floating window's payload: a dialog/transient (xdg parent set) or a
/// fixed-size client (min == max). Never in the layout/taskbar: shown
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
    /// colour when it has one, so the dialog visibly belongs to it. Fixed
    /// for the float's life (set at manage, never reassigned), which is
    /// what lets the border element keep a constant commit.
    pub accent: crate::Index,
    /// The titlebar contents' indexed GPU texture (icon + title on a
    /// transparent `w`x`tb_h` strip), rendered as its own element just
    /// below the client surface; re-uploaded only when its content
    /// (size/title) changes — a drag just moves the element. The border
    /// around it is not a texture at all: the shared border art sliced
    /// over the frame rect by the GPU (see `render::indexed`).
    pub frame: FrameTex,
    /// The border element's persistent identity for the damage tracker.
    /// Its commit never bumps: the accent is fixed and everything else
    /// about the border is geometry, which the tracker sees itself.
    pub frame_id: smithay::backend::renderer::element::Id,
}

/// A float titlebar strip's texture and its freshness in one state:
/// `Fresh` can only be built with a texture, so "no texture yet not due
/// for a repaint" — a strip that would render as nothing forever — is
/// unrepresentable.
pub enum FrameTex {
    /// The strip's content (size/title) changed since the held texture (if
    /// any — a float starts with none) was uploaded; repainted before the
    /// next composite, the old texture shown until then. The texture rides
    /// along so the repaint can reuse its GPU allocation.
    Stale(Option<crate::render::indexed::IndexedTexture>),
    /// The texture matches the strip's current content.
    Fresh(crate::render::indexed::IndexedTexture),
}

impl FrameTex {
    /// Whatever texture there is to draw, fresh or stale.
    pub fn texture(&self) -> Option<&crate::render::indexed::IndexedTexture> {
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
    pub fn take(&mut self) -> Option<crate::render::indexed::IndexedTexture> {
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
    /// either backend (`WaylandFocus::wl_surface` resolves an X11 window
    /// through its associated wl surface).
    pub fn win_for_surface(&self, surface: &WlSurface) -> Option<Win> {
        use smithay::wayland::seat::WaylandFocus as _;
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

/// Memoized `Rc` of a toplevel's title, stored in the surface's user data:
/// the chrome fingerprints re-read the title every frame, and without the
/// cache each read would allocate a fresh `Rc<str>` just to compare equal.
struct TitleCache(std::cell::RefCell<std::rc::Rc<str>>);

/// The window's current title (xdg toplevel title / X11 `_NET_WM_NAME`),
/// or empty when unset. Repeated reads of an unchanged title return clones
/// of one shared `Rc` (see `TitleCache`).
pub fn toplevel_title(window: &Window) -> std::rc::Rc<str> {
    if let Some(x11) = window.x11_surface() {
        return x11.title().into();
    }
    let Some(t) = window.toplevel() else {
        return "".into();
    };
    smithay::wayland::compositor::with_states(t.wl_surface(), |states| {
        let data = states
            .data_map
            .get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()
            .and_then(|d| d.lock().ok());
        let title = data.as_ref().and_then(|d| d.title.as_deref()).unwrap_or("");
        states
            .data_map
            .insert_if_missing(|| TitleCache(std::cell::RefCell::new("".into())));
        let mut cached = states
            .data_map
            .get::<TitleCache>()
            .expect("inserted above")
            .0
            .borrow_mut();
        if &**cached != title {
            *cached = title.into();
        }
        std::rc::Rc::clone(&cached)
    })
}

/// The window's class identity (xdg app_id / X11 `WM_CLASS` class half),
/// grouping windows of one app for labels/icons/quick-launch rules.
pub fn toplevel_app_id(window: &Window) -> String {
    if let Some(x11) = window.x11_surface() {
        return x11.class();
    }
    read_toplevel_data(window, |d| d.app_id.clone()).unwrap_or_default()
}

/// The window's fallback glyph — its class identity's first character,
/// uppercased, `?` when unset — without cloning the whole app_id the way
/// `toplevel_app_id` + `label_from_class` would (the chrome fingerprints
/// re-derive it every frame).
pub fn toplevel_label(window: &Window) -> char {
    if let Some(x11) = window.x11_surface() {
        return crate::widgets::label_from_class(&x11.class());
    }
    read_toplevel_data(window, |d| {
        d.app_id.as_deref().map(crate::widgets::label_from_class)
    })
    .unwrap_or('?')
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

/// Send a window its rect, whichever backend it speaks: an xdg toplevel is
/// configured with the size only (its position is the compositor's `Space`
/// mapping), an X11 window with the full geometry.
pub fn configure_rect(window: &Window, x: i32, y: i32, w: i32, h: i32) {
    if let Some(toplevel) = window.toplevel() {
        toplevel.with_pending_state(|s| s.size = Some((w, h).into()));
        toplevel.send_pending_configure();
    } else if let Some(x11) = window.x11_surface() {
        let _ = x11.configure(
            smithay::utils::Rectangle::<i32, smithay::utils::Logical>::new(
                (x, y).into(),
                (w, h).into(),
            ),
        );
    }
}

/// Flip an X11 window's map state (its `WM_STATE` bookkeeping lives inside
/// smithay, so hiding one must really unmap it). Wayland toplevels carry no
/// such state — they show and hide purely by their `Space` mapping — so
/// this is a no-op for them.
pub fn set_x11_mapped(window: &Window, mapped: bool) {
    if let Some(x11) = window.x11_surface() {
        let _ = x11.set_mapped(mapped);
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

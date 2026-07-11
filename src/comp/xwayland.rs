//! XWayland integration: spawning the X server, the XWM handlers, and
//! override-redirect windows (rofi). Managed X11 windows flow through the
//! same classify/tile/float/dock lifecycle as Wayland toplevels — the
//! `Window` abstraction hides which backend a client speaks.

use std::os::unix::io::OwnedFd;
use std::process::Stdio;

use smithay::desktop::Window;
use smithay::utils::{Logical, Rectangle};
use smithay::wayland::selection::data_device::{
    clear_data_device_selection, current_data_device_selection_userdata,
    request_data_device_client_selection, set_data_device_selection,
};
use smithay::wayland::selection::primary_selection::{
    clear_primary_selection, current_primary_selection_userdata, request_primary_client_selection,
    set_primary_selection,
};
use smithay::wayland::selection::SelectionTarget;
use smithay::wayland::xwayland_shell::{XWaylandShellHandler, XWaylandShellState};
use smithay::xwayland::xwm::{Reorder, ResizeEdge, XwmId};
use smithay::xwayland::{X11Surface, X11Wm, XWayland, XWaylandEvent};
use smithay::{
    delegate_xwayland_shell, reexports::wayland_server::protocol::wl_surface::WlSurface,
};

use super::Comp;
use crate::shell::Kind;

/// A mapped override-redirect X11 window (rofi, menus) with its true
/// root-relative geometry. `X11Surface::geometry()` cannot be trusted for
/// these: a client that gains the override-redirect flag after creation
/// (rofi does) has its pre-map ConfigureNotify dropped by smithay's XWM,
/// which leaves the cached geometry at the creation rect. `rect` is
/// fetched from the X server at map time and tracked through
/// `configure_notify` afterwards.
pub struct OrWindow {
    pub surface: X11Surface,
    pub rect: Rectangle<i32, Logical>,
}

impl Comp {
    /// Spawn the XWayland server; the WM connection arrives via the Ready
    /// event once it is up. Its `DISPLAY` is recorded then, so children
    /// spawned by the compositor (rofi, quick-launch X11 apps) get it
    /// injected (`launch::spawn`).
    pub fn start_xwayland(&mut self) {
        let (xwayland, client) = match XWayland::spawn(
            &self.dh,
            None,
            std::iter::empty::<(String, String)>(),
            true,
            Stdio::inherit(),
            Stdio::inherit(),
            |_| {},
        ) {
            Ok(pair) => pair,
            Err(err) => {
                tracing::warn!("XWayland spawn failed (X11 clients unavailable): {err}");
                return;
            }
        };
        let res = self
            .handle
            .insert_source(xwayland, move |event, (), comp| match event {
                XWaylandEvent::Ready {
                    x11_socket,
                    display_number,
                } => {
                    match X11Wm::start_wm(comp.handle.clone(), x11_socket, client.clone()) {
                        Ok(wm) => comp.xwm = Some(wm),
                        Err(err) => {
                            tracing::error!("X11Wm start failed: {err}");
                            return;
                        }
                    }
                    crate::launch::set_x11_display(format!(":{display_number}"));
                    // Announced like WAYLAND_DISPLAY at startup: harness
                    // drivers synchronize on this before launching X11
                    // clients.
                    println!("DISPLAY=:{display_number}");
                    // A plain client connection for queries the WM
                    // connection doesn't expose (o-r geometry at map time,
                    // see OrWindow). Local Xwayland: roundtrips are cheap
                    // and only happen per o-r map.
                    match smithay::reexports::x11rb::rust_connection::RustConnection::connect(Some(
                        &format!(":{display_number}"),
                    )) {
                        Ok((conn, _)) => comp.x11_query = Some(conn),
                        Err(err) => tracing::warn!("x11 query connection failed: {err}"),
                    }
                    tracing::info!("XWayland ready on DISPLAY=:{display_number}");
                }
                XWaylandEvent::Error => {
                    tracing::warn!("XWayland exited during startup; X11 clients unavailable");
                }
            });
        if let Err(err) = res {
            tracing::error!("insert xwayland source: {err}");
        }
    }

    /// Whether the keyboard focus is an X11 window's surface (managed or
    /// override-redirect).
    fn x11_holds_focus(&self) -> bool {
        let Some(focus) = self.keyboard.current_focus() else {
            return false;
        };
        self.or_windows
            .iter()
            .map(|o| &o.surface)
            .chain(
                self.managed
                    .entries_windows()
                    .filter_map(|(_, w)| w.x11_surface()),
            )
            .any(|s| s.wl_surface().is_some_and(|ws| ws == focus))
    }

    /// An X11 window went away (unmap or destroy): drop it from whichever
    /// store holds it, exactly like a Wayland toplevel's destruction.
    fn forget_x11(&mut self, surface: &X11Surface) {
        if let Some(idx) = self
            .or_windows
            .iter()
            .position(|o| o.surface.window_id() == surface.window_id())
        {
            self.or_windows.remove(idx);
            self.refocus();
            return;
        }
        let win = self.managed.entries_windows().find_map(|(w, window)| {
            window
                .x11_surface()
                .is_some_and(|s| s.window_id() == surface.window_id())
                .then_some(w)
        });
        let Some(win) = win else {
            return;
        };
        match self.managed.kind_of(win) {
            Some(Kind::Tiled) => self.unmanage_tiled(win),
            Some(Kind::Float(_)) => self.forget_float(win),
            Some(Kind::Dock(_)) => self.unmanage_dock(win),
            None => {}
        }
    }
}

impl XWaylandShellHandler for Comp {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.xwayland_shell_state
    }

    /// The wl_surface for an X11 window arrives only with a later commit,
    /// usually after the map. An override-redirect window that mapped
    /// surfaceless gets its promised keyboard focus here (rofi's X-side
    /// keyboard grab is dead until XWayland holds our focus). A managed
    /// window was arranged at map time when it had no surface to focus, so
    /// `refocus` re-resolves the keyboard target now that one exists.
    fn surface_associated(&mut self, _xwm: XwmId, wl_surface: WlSurface, window: X11Surface) {
        if self
            .or_windows
            .iter()
            .any(|o| o.surface.window_id() == window.window_id())
        {
            let keyboard = self.keyboard.clone();
            let serial = smithay::utils::SERIAL_COUNTER.next_serial();
            keyboard.set_focus(self, Some(wl_surface), serial);
        } else {
            self.refocus();
        }
    }
}
delegate_xwayland_shell!(Comp);

impl smithay::xwayland::XwmHandler for Comp {
    fn xwm_state(&mut self, _xwm: XwmId) -> &mut X11Wm {
        self.xwm.as_mut().expect("xwm events imply a live X11Wm")
    }

    fn new_window(&mut self, _xwm: XwmId, _window: X11Surface) {}
    fn new_override_redirect_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn map_window_request(&mut self, _xwm: XwmId, window: X11Surface) {
        if let Err(err) = window.set_mapped(true) {
            tracing::warn!("x11 set_mapped: {err}");
            return;
        }
        // X11 pre-map fullscreen arrives via `fullscreen_request` on the
        // WM connection instead of a pending record.
        self.classify_and_manage(Window::new_x11_window(window), false);
    }

    /// Override-redirect windows (rofi, menus) position themselves and are
    /// never managed: shown topmost as-is; keyboard goes to them while
    /// mapped (rofi grabs the keyboard X11-side, but that grab only works
    /// while XWayland holds our keyboard focus).
    fn mapped_override_redirect_window(&mut self, _xwm: XwmId, window: X11Surface) {
        // The one place the cached geometry may be stale (see OrWindow):
        // ask the server where the window really is.
        let rect = self
            .x11_query
            .as_ref()
            .and_then(|conn| {
                use smithay::reexports::x11rb::protocol::xproto::ConnectionExt as _;
                let geo = conn.get_geometry(window.window_id()).ok()?.reply().ok()?;
                Some(Rectangle::new(
                    (i32::from(geo.x), i32::from(geo.y)).into(),
                    (i32::from(geo.width), i32::from(geo.height)).into(),
                ))
            })
            .unwrap_or_else(|| window.geometry());
        self.or_windows.push(OrWindow {
            surface: window.clone(),
            rect,
        });
        if let Some(surface) = window.wl_surface() {
            let keyboard = self.keyboard.clone();
            let serial = smithay::utils::SERIAL_COUNTER.next_serial();
            keyboard.set_focus(self, Some(surface), serial);
        }
    }

    fn unmapped_window(&mut self, _xwm: XwmId, window: X11Surface) {
        self.forget_x11(&window);
    }

    fn destroyed_window(&mut self, _xwm: XwmId, window: X11Surface) {
        self.forget_x11(&window);
    }

    #[allow(clippy::too_many_arguments)]
    fn configure_request(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        x: Option<i32>,
        y: Option<i32>,
        w: Option<u32>,
        h: Option<u32>,
        _reorder: Option<Reorder>,
    ) {
        let win = self.managed.entries_windows().find_map(|(ww, wd)| {
            wd.x11_surface()
                .is_some_and(|s| s.window_id() == window.window_id())
                .then_some(ww)
        });
        match win.and_then(|w| self.managed.kind_of(w).map(|k| (w, k))) {
            // Tiled geometry is the layout's to decide: deny by
            // re-asserting the current state (a synthetic ConfigureNotify,
            // master's answer to denied ConfigureRequests).
            Some((_, Kind::Tiled)) => {
                let _ = window.configure(None);
            }
            // A float resizing itself is honored; the frame repaints
            // around the tracked geometry.
            Some((fw, Kind::Float(_))) => {
                let mut rect = window.geometry();
                if let Some(w) = w {
                    rect.size.w = w as i32;
                }
                if let Some(h) = h {
                    rect.size.h = h as i32;
                }
                if let Some((_, f)) = self.managed.float_mut(fw) {
                    rect.loc = (f.x, f.y).into();
                    if (f.w, f.h) != (rect.size.w, rect.size.h) {
                        f.w = rect.size.w.max(1);
                        f.h = rect.size.h.max(1);
                        f.frame.mark_stale();
                    }
                }
                let _ = window.configure(rect);
            }
            Some((_, Kind::Dock(_))) => {
                let _ = window.configure(None);
            }
            // Unmanaged (pre-map or override-redirect): grant as asked.
            None => {
                let mut rect = window.geometry();
                if let Some(x) = x {
                    rect.loc.x = x;
                }
                if let Some(y) = y {
                    rect.loc.y = y;
                }
                if let Some(w) = w {
                    rect.size.w = w as i32;
                }
                if let Some(h) = h {
                    rect.size.h = h as i32;
                }
                let _ = window.configure(rect);
            }
        }
    }

    /// Post-map moves of override-redirect windows (a menu tracking its
    /// parent) land here; managed windows' geometry is the layout's.
    fn configure_notify(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        geometry: Rectangle<i32, Logical>,
        _above: Option<smithay::reexports::x11rb::protocol::xproto::Window>,
    ) {
        if let Some(o) = self
            .or_windows
            .iter_mut()
            .find(|o| o.surface.window_id() == window.window_id())
        {
            o.rect = geometry;
        }
    }

    fn resize_request(
        &mut self,
        _xwm: XwmId,
        _window: X11Surface,
        _button: u32,
        _edges: ResizeEdge,
    ) {
        // Interactive resize is the layout's job; the chrome handles it.
    }

    fn move_request(&mut self, _xwm: XwmId, _window: X11Surface, _button: u32) {
        // Floats move by their chrome frame; tiled windows don't move.
    }

    // The X11 half of the X11↔Wayland clipboard bridge (the Wayland half
    // lives in `SelectionHandler`): X-owned selections are mirrored onto
    // the seat as compositor selections, and X clients may read the
    // Wayland selections while an X window holds keyboard focus — an X
    // selection request carries no identity to authorize more finely.

    fn allow_selection_access(&mut self, _xwm: XwmId, _selection: SelectionTarget) -> bool {
        self.x11_holds_focus()
    }

    fn send_selection(
        &mut self,
        _xwm: XwmId,
        selection: SelectionTarget,
        mime_type: String,
        fd: OwnedFd,
    ) {
        let res = match selection {
            SelectionTarget::Clipboard => {
                request_data_device_client_selection(&self.seat, mime_type, fd)
                    .map_err(|err| err.to_string())
            }
            SelectionTarget::Primary => request_primary_client_selection(&self.seat, mime_type, fd)
                .map_err(|err| err.to_string()),
        };
        if let Err(err) = res {
            tracing::warn!("sending selection to X11 failed: {err}");
        }
    }

    fn new_selection(&mut self, _xwm: XwmId, selection: SelectionTarget, mime_types: Vec<String>) {
        match selection {
            SelectionTarget::Clipboard => {
                set_data_device_selection(&self.dh, &self.seat, mime_types, ());
            }
            SelectionTarget::Primary => {
                set_primary_selection(&self.dh, &self.seat, mime_types, ());
            }
        }
    }

    fn cleared_selection(&mut self, _xwm: XwmId, selection: SelectionTarget) {
        // Only clear a selection the XWM itself set (compositor-provided,
        // `()` user data): a Wayland client may have taken the selection
        // since, and the X clear arriving late must not wipe it.
        match selection {
            SelectionTarget::Clipboard => {
                if current_data_device_selection_userdata(&self.seat).is_some() {
                    clear_data_device_selection(&self.dh, &self.seat);
                }
            }
            SelectionTarget::Primary => {
                if current_primary_selection_userdata(&self.seat).is_some() {
                    clear_primary_selection(&self.dh, &self.seat);
                }
            }
        }
    }
}

/// The topmost override-redirect window's surface under `pos`, if any.
pub fn or_surface_under(
    or_windows: &[OrWindow],
    pos: smithay::utils::Point<f64, Logical>,
) -> Option<(WlSurface, smithay::utils::Point<f64, Logical>)> {
    for o in or_windows.iter().rev() {
        let geo = o.rect;
        if pos.x >= f64::from(geo.loc.x)
            && pos.x < f64::from(geo.loc.x + geo.size.w)
            && pos.y >= f64::from(geo.loc.y)
            && pos.y < f64::from(geo.loc.y + geo.size.h)
        {
            if let Some(surface) = o.surface.wl_surface() {
                return Some((surface, geo.loc.to_f64()));
            }
        }
    }
    None
}

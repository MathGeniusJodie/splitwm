//! XWayland integration: spawning the X server, the XWM handlers, and
//! override-redirect windows (rofi). Managed X11 windows flow through the
//! same classify/tile/float/dock lifecycle as Wayland toplevels — the
//! `Window` abstraction hides which backend a client speaks.

use std::process::Stdio;

use smithay::desktop::Window;
use smithay::utils::{Logical, Rectangle};
use smithay::wayland::xwayland_shell::{XWaylandShellHandler, XWaylandShellState};
use smithay::xwayland::xwm::{Reorder, ResizeEdge, XwmId};
use smithay::xwayland::{X11Surface, X11Wm, XWayland, XWaylandEvent};
use smithay::{
    delegate_xwayland_shell, reexports::wayland_server::protocol::wl_surface::WlSurface,
};

use super::Comp;
use crate::shell::Kind;

impl Comp {
    /// Spawn the XWayland server; the WM connection arrives via the Ready
    /// event once it is up. `DISPLAY` is set then, so children spawned by
    /// the compositor (rofi, quick-launch X11 apps) inherit it.
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
                    std::env::set_var("DISPLAY", format!(":{display_number}"));
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

    /// An X11 window went away (unmap or destroy): drop it from whichever
    /// store holds it, exactly like a Wayland toplevel's destruction.
    fn forget_x11(&mut self, surface: &X11Surface) {
        if let Some(idx) = self
            .or_windows
            .iter()
            .position(|s| s.window_id() == surface.window_id())
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
        if self.fullscreen == Some(win) {
            self.fullscreen = None;
        }
        match self.managed.kind_of(win) {
            Some(Kind::Tiled) => {
                if let Some(m) = self.managed.remove(win) {
                    self.space.unmap_elem(&m.window);
                }
                self.state.unpin_client(win);
                self.arrange();
            }
            Some(Kind::Float(_)) => self.forget_float(win),
            Some(Kind::Dock(_)) => {
                self.managed.remove(win);
                let wa = self.layout_area();
                self.state.clamp_scroll(wa, 0);
                self.arrange();
            }
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
    /// keyboard grab is dead until XWayland holds our focus).
    fn surface_associated(&mut self, _xwm: XwmId, wl_surface: WlSurface, window: X11Surface) {
        if self
            .or_windows
            .iter()
            .any(|s| s.window_id() == window.window_id())
        {
            let keyboard = self.seat.get_keyboard().expect("seat has a keyboard");
            let serial = smithay::utils::SERIAL_COUNTER.next_serial();
            keyboard.set_focus(self, Some(wl_surface), serial);
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
        self.classify_and_manage(Window::new_x11_window(window));
    }

    /// Override-redirect windows (rofi, menus) position themselves and are
    /// never managed: shown topmost as-is; keyboard goes to them while
    /// mapped (rofi grabs the keyboard X11-side, but that grab only works
    /// while XWayland holds our keyboard focus).
    fn mapped_override_redirect_window(&mut self, _xwm: XwmId, window: X11Surface) {
        self.or_windows.push(window.clone());
        if let Some(surface) = window.wl_surface() {
            let keyboard = self.seat.get_keyboard().expect("seat has a keyboard");
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
                        f.frame_dirty = true;
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

    fn configure_notify(
        &mut self,
        _xwm: XwmId,
        _window: X11Surface,
        _geometry: Rectangle<i32, Logical>,
        _above: Option<smithay::reexports::x11rb::protocol::xproto::Window>,
    ) {
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
}

/// The topmost override-redirect window's surface under `pos`, if any.
pub fn or_surface_under(
    or_windows: &[X11Surface],
    pos: smithay::utils::Point<f64, Logical>,
) -> Option<(WlSurface, smithay::utils::Point<f64, Logical>)> {
    for window in or_windows.iter().rev() {
        let geo = window.geometry();
        if pos.x >= f64::from(geo.loc.x)
            && pos.x < f64::from(geo.loc.x + geo.size.w)
            && pos.y >= f64::from(geo.loc.y)
            && pos.y < f64::from(geo.loc.y + geo.size.h)
        {
            if let Some(surface) = window.wl_surface() {
                return Some((surface, geo.loc.to_f64()));
            }
        }
    }
    None
}

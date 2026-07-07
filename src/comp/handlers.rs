//! Delegate implementations wiring smithay's protocol machinery to `Comp`.

use smithay::backend::renderer::ImportDma as _;
use smithay::desktop::{PopupKind, Window};
use smithay::input::pointer::CursorImageStatus;
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::protocol::wl_seat::WlSeat;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::Client;
use smithay::utils::Serial;
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    get_parent, is_sync_subsurface, with_states, CompositorClientState, CompositorHandler,
    CompositorState,
};
use smithay::wayland::dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier};
use smithay::wayland::output::OutputHandler;
use smithay::wayland::selection::data_device::{
    ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
};
use smithay::wayland::selection::SelectionHandler;
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
    XdgToplevelSurfaceData,
};
use smithay::wayland::shm::{ShmHandler, ShmState};
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::wayland::shell::xdg::decoration::XdgDecorationHandler;
use smithay::{
    delegate_compositor, delegate_cursor_shape, delegate_data_device, delegate_dmabuf,
    delegate_output, delegate_seat, delegate_shm, delegate_xdg_decoration, delegate_xdg_shell,
};

use super::{ClientState, Comp};

impl CompositorHandler for Comp {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        // The XWayland client is inserted by smithay's spawn with its own
        // data type; every client we insert ourselves carries ClientState.
        if let Some(data) = client.get_data::<smithay::xwayland::XWaylandClientData>() {
            return &data.compositor_state;
        }
        &client
            .get_data::<ClientState>()
            .expect("every client carries ClientState or XWaylandClientData")
            .compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        smithay::backend::renderer::utils::on_commit_buffer_handler::<Self>(surface);
        if !is_sync_subsurface(surface) {
            let mut root = surface.clone();
            while let Some(parent) = get_parent(&root) {
                root = parent;
            }
            let window = self
                .managed
                .windows()
                .chain(self.pending.iter())
                .find(|w| w.toplevel().is_some_and(|t| *t.wl_surface() == root))
                .cloned();
            if let Some(window) = window {
                window.on_commit();
            }
        }
        self.ensure_initial_configure(surface);

        // A pending toplevel's first buffer commit maps it: classify and
        // manage (tiled/float/dock).
        let has_buffer =
            smithay::backend::renderer::utils::with_renderer_surface_state(surface, |state| {
                state.buffer().is_some()
            })
            .unwrap_or(false);
        if has_buffer {
            if let Some(idx) = self
                .pending
                .iter()
                .position(|w| w.toplevel().is_some_and(|t| t.wl_surface() == surface))
            {
                let window = self.pending.remove(idx);
                self.classify_and_manage(window);
            } else if let Some(win) = self.managed.win_for_surface(surface) {
                // A float resizing itself: track the new size and repaint
                // its frame around it.
                let geo = self.managed.get(win).map(|w| w.geometry().size);
                if let Some((_, f)) = self.managed.float_mut(win) {
                    if let Some(size) = geo {
                        if (f.w, f.h) != (size.w, size.h) {
                            f.w = size.w.max(1);
                            f.h = size.h.max(1);
                            f.frame_dirty = true;
                        }
                    }
                }
            }
        }

        self.layer_commit(surface);
        self.popups.commit(surface);
    }
}
delegate_compositor!(Comp);

impl Comp {
    /// xdg surfaces may not be mapped before their first configure; send it
    /// on the surface's first commit.
    fn ensure_initial_configure(&mut self, surface: &WlSurface) {
        if let Some(window) = self
            .pending
            .iter()
            .chain(self.managed.windows())
            .find(|w| w.toplevel().is_some_and(|t| t.wl_surface() == surface))
        {
            let toplevel = window.toplevel().expect("matched on toplevel above");
            let initial_configure_sent = with_states(surface, |states| {
                states
                    .data_map
                    .get::<XdgToplevelSurfaceData>()
                    .expect("xdg toplevel data on xdg surface")
                    .lock()
                    .expect("no poisoned toplevel data")
                    .initial_configure_sent
            });
            if !initial_configure_sent {
                toplevel.send_configure();
            }
            return;
        }

        if let Some(popup) = self.popups.find_popup(surface) {
            if let PopupKind::Xdg(ref xdg) = popup {
                if !xdg.is_initial_configure_sent() {
                    // A popup positioner is valid by construction, so the
                    // only send_configure error (invalid positioner) can't
                    // happen.
                    xdg.send_configure().expect("initial popup configure");
                }
            }
        }
    }
}

impl XdgShellHandler for Comp {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        // Wayland clients set app_id/parent/size hints after creating the
        // role; classification (tiled/float/dock) waits for the first
        // buffer commit (see `Comp::classify_and_manage`).
        self.pending.push(Window::new_wayland_window(surface));
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        self.pending
            .retain(|w| w.toplevel().is_none_or(|t| *t != surface));
        let Some(win) = self.managed.win_for_surface(surface.wl_surface()) else {
            return;
        };
        if self.fullscreen == Some(win) {
            self.fullscreen = None;
        }
        match self.managed.kind_of(win) {
            Some(crate::shell::Kind::Tiled) => {
                if let Some(m) = self.managed.remove(win) {
                    self.space.unmap_elem(&m.window);
                }
                self.state.unpin_client(win);
                // arrange refocuses, so focus never rests on a dead client.
                self.arrange();
            }
            Some(crate::shell::Kind::Float(_)) => self.forget_float(win),
            Some(crate::shell::Kind::Dock(_)) => {
                self.managed.remove(win);
                // Re-clamp now that the scroll headroom it needed is gone.
                let wa = self.layout_area();
                self.state.clamp_scroll(wa, 0);
                self.arrange();
            }
            None => {}
        }
    }

    fn fullscreen_request(
        &mut self,
        surface: ToplevelSurface,
        _output: Option<smithay::reexports::wayland_server::protocol::wl_output::WlOutput>,
    ) {
        surface.with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Fullscreen);
        });
        if let Some(win) = self.managed.win_for_surface(surface.wl_surface()) {
            self.fullscreen = Some(win);
            self.arrange();
        } else {
            // Requested before the first commit (a startup-fullscreen
            // client); honored once the window is classified.
            self.pending_fullscreen.push(surface.wl_surface().clone());
        }
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        surface.with_pending_state(|state| {
            state.states.unset(xdg_toplevel::State::Fullscreen);
        });
        if self.fullscreen == self.managed.win_for_surface(surface.wl_surface()) {
            self.fullscreen = None;
        }
        self.arrange();
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        if let Err(err) = self.popups.track_popup(PopupKind::Xdg(surface)) {
            tracing::warn!("failed to track popup: {err}");
        }
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        surface.with_pending_state(|state| {
            state.geometry = positioner.get_geometry();
            state.positioner = positioner;
        });
        surface.send_repositioned(token);
    }

    fn grab(&mut self, _surface: PopupSurface, _seat: WlSeat, _serial: Serial) {
        // Popup grabs (keyboard redirect into menus) arrive with the real
        // input model in M4.
    }
}
delegate_xdg_shell!(Comp);

/// All decoration is compositor chrome: every toplevel is told to draw
/// nothing of its own, whatever it asks for.
impl XdgDecorationHandler for Comp {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(DecorationMode::ServerSide);
        });
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, _requested: DecorationMode) {
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(DecorationMode::ServerSide);
        });
        if toplevel.is_initial_configure_sent() {
            toplevel.send_pending_configure();
        }
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(DecorationMode::ServerSide);
        });
        if toplevel.is_initial_configure_sent() {
            toplevel.send_pending_configure();
        }
    }
}
delegate_xdg_decoration!(Comp);

impl BufferHandler for Comp {
    fn buffer_destroyed(&mut self, _buffer: &WlBuffer) {}
}

impl ShmHandler for Comp {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}
delegate_shm!(Comp);

impl DmabufHandler for Comp {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        dmabuf: smithay::backend::allocator::dmabuf::Dmabuf,
        notifier: ImportNotifier,
    ) {
        if self.backend.renderer().import_dmabuf(&dmabuf, None).is_ok() {
            let _ = notifier.successful::<Comp>();
        } else {
            notifier.failed();
        }
    }
}
delegate_dmabuf!(Comp);

impl SeatHandler for Comp {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Comp> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, _seat: &Seat<Self>, _focused: Option<&WlSurface>) {}
    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        // Set by clients (a committed cursor surface or a cursor-shape-v1
        // request) and by the chrome's hover feedback; consumed at redraw
        // by whichever backend presents the cursor.
        self.cursor_status = image;
    }
}
delegate_seat!(Comp);

// cursor-shape-v1 serves tablet tools too; splitwm has no tablet support,
// so the trait's default no-op image callback is the whole implementation.
impl smithay::wayland::tablet_manager::TabletSeatHandler for Comp {}
delegate_cursor_shape!(Comp);

impl SelectionHandler for Comp {
    type SelectionUserData = ();
}

impl DataDeviceHandler for Comp {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}
impl ClientDndGrabHandler for Comp {}
impl ServerDndGrabHandler for Comp {}
delegate_data_device!(Comp);

impl OutputHandler for Comp {}
delegate_output!(Comp);

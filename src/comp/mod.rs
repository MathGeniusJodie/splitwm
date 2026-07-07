//! Compositor state: the Wayland display and its protocol globals, the
//! window space, the seat, and the winit backend that presents it all.
//!
//! M1 shape: clients connect over a private socket, xdg toplevels map
//! full-output into a `Space`, and input is forwarded to whatever holds
//! focus. The split-tree layout replaces the naive Space placement in M4.

pub mod handlers;
pub mod input;

use std::sync::Arc;
use std::time::Duration;

use smithay::backend::egl::EGLDevice;
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::{Color32F, ImportDma as _};
use smithay::backend::winit::WinitGraphicsBackend;
use smithay::desktop::{PopupManager, Space, Window};
use smithay::input::{Seat, SeatState};
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop;
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{EventLoop, Interest, LoopHandle, LoopSignal, PostAction};
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::{Display, DisplayHandle};
use smithay::utils::{Logical, Physical, Point, Rectangle, Size, Transform};
use smithay::wayland::compositor::{CompositorClientState, CompositorState};
use smithay::wayland::dmabuf::{DmabufFeedbackBuilder, DmabufGlobal, DmabufState};
use smithay::wayland::output::OutputManagerState;
use smithay::wayland::selection::data_device::DataDeviceState;
use smithay::wayland::shell::xdg::XdgShellState;
use smithay::wayland::shm::ShmState;
use smithay::wayland::socket::ListeningSocketSource;

/// Per-client protocol state, stored as the client's `ClientData`.
#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

pub struct Comp {
    // Presentation.
    pub backend: WinitGraphicsBackend<GlesRenderer>,
    pub output: Output,
    pub damage_tracker: OutputDamageTracker,
    /// Gap background (na16 gunmetal), resolved once from the baked palette.
    pub clear: Color32F,

    // Wayland plumbing.
    pub dh: DisplayHandle,
    #[allow(dead_code)] // timers and deferred work hang off this from M4 on
    pub handle: LoopHandle<'static, Comp>,
    pub signal: LoopSignal,
    pub socket_name: std::ffi::OsString,

    // Windows and input.
    pub space: Space<Window>,
    pub popups: PopupManager,
    pub seat: Seat<Comp>,
    pub start: std::time::Instant,

    // Protocol globals.
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub shm_state: ShmState,
    /// Never read, but dropping it would unpublish the xdg-output global.
    #[allow(dead_code)]
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<Comp>,
    pub data_device_state: DataDeviceState,
    pub dmabuf_state: DmabufState,
    /// Never read, but it identifies the live dmabuf global for teardown.
    #[allow(dead_code)]
    pub dmabuf_global: DmabufGlobal,
}

impl Comp {
    pub fn new(
        event_loop: &mut EventLoop<'static, Comp>,
        display: Display<Comp>,
        mut backend: WinitGraphicsBackend<GlesRenderer>,
        clear: Color32F,
    ) -> Comp {
        let dh = display.handle();
        let handle = event_loop.handle();

        let compositor_state = CompositorState::new::<Comp>(&dh);
        let xdg_shell_state = XdgShellState::new::<Comp>(&dh);
        let shm_state = ShmState::new::<Comp>(&dh, vec![]);
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Comp>(&dh);
        let mut seat_state = SeatState::new();
        let data_device_state = DataDeviceState::new::<Comp>(&dh);

        let mut seat: Seat<Comp> = seat_state.new_wl_seat(&dh, "seat-0");
        // xkb defaults come from the environment (XKB_DEFAULT_LAYOUT etc.),
        // matching how the X11 version inherited the server keymap.
        seat.add_keyboard(Default::default(), 600, 25)
            .expect("keyboard with default xkb config");
        seat.add_pointer();

        // Advertise dmabuf so GL clients (alacritty) can hand us GPU
        // buffers: with a render node, v4 with default feedback; without
        // one (software GL in CI), a plain v3 global.
        let mut dmabuf_state = DmabufState::new();
        let render_node = EGLDevice::device_for_display(backend.renderer().egl_context().display())
            .ok()
            .and_then(|device| device.try_get_render_node().ok().flatten());
        let formats = backend.renderer().dmabuf_formats();
        let dmabuf_global = match render_node {
            Some(node) => {
                let feedback = DmabufFeedbackBuilder::new(node.dev_id(), formats)
                    .build()
                    .expect("default dmabuf feedback");
                dmabuf_state.create_global_with_default_feedback::<Comp>(&dh, &feedback)
            }
            None => dmabuf_state.create_global::<Comp>(&dh, formats),
        };

        let output = Output::new(
            "winit".to_string(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "splitwm".into(),
                model: "winit".into(),
            },
        );
        let _global = output.create_global::<Comp>(&dh);
        let mode = Mode {
            size: backend.window_size(),
            refresh: 60_000,
        };
        output.change_current_state(Some(mode), Some(Transform::Flipped180), None, Some((0, 0).into()));
        output.set_preferred(mode);
        let damage_tracker = OutputDamageTracker::from_output(&output);

        let mut space = Space::default();
        space.map_output(&output, (0, 0));

        let socket_name = Self::init_wayland_listener(display, event_loop);

        Comp {
            backend,
            output,
            damage_tracker,
            clear,
            dh,
            handle,
            signal: event_loop.get_signal(),
            socket_name,
            space,
            popups: PopupManager::default(),
            seat,
            start: std::time::Instant::now(),
            compositor_state,
            xdg_shell_state,
            shm_state,
            output_manager_state,
            seat_state,
            data_device_state,
            dmabuf_state,
            dmabuf_global,
        }
    }

    /// Listen on a fresh `wayland-N` socket and drive client requests from
    /// calloop — both sources are edge/level fd-driven, nothing polls.
    fn init_wayland_listener(
        display: Display<Comp>,
        event_loop: &mut EventLoop<'static, Comp>,
    ) -> std::ffi::OsString {
        let socket = ListeningSocketSource::new_auto().expect("open wayland socket");
        let socket_name = socket.socket_name().to_os_string();
        let handle = event_loop.handle();

        handle
            .insert_source(socket, move |client_stream, (), comp| {
                comp.dh
                    .insert_client(client_stream, Arc::new(ClientState::default()))
                    .expect("insert client");
            })
            .expect("insert socket source");

        handle
            .insert_source(
                Generic::new(display, Interest::READ, calloop::Mode::Level),
                |_, display, comp| {
                    // SAFETY: the display is never dropped or replaced while
                    // this source lives; calloop owns it for the loop's life.
                    unsafe {
                        display.get_mut().dispatch_clients(comp).expect("dispatch clients");
                    }
                    Ok(PostAction::Continue)
                },
            )
            .expect("insert display source");

        socket_name
    }

    /// The topmost surface under `pos`, with its surface-local coordinates,
    /// for pointer focus.
    pub fn surface_under(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(
        smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
        Point<f64, Logical>,
    )> {
        self.space.element_under(pos).and_then(|(window, location)| {
            window
                .surface_under(pos - location.to_f64(), smithay::desktop::WindowSurfaceType::ALL)
                .map(|(s, p)| (s, (p.to_f64() + location.to_f64())))
        })
    }

    /// The output was resized (nested window resize stands in for RandR).
    pub fn resize_output(&mut self, size: Size<i32, Physical>) {
        let mode = Mode {
            size,
            refresh: 60_000,
        };
        self.output.change_current_state(Some(mode), None, None, None);
        self.output.set_preferred(mode);
    }

    /// Composite one frame and pace clients' frame callbacks.
    pub fn redraw(&mut self) {
        let size = self.backend.window_size();
        let full: Rectangle<i32, Physical> = Rectangle::from_size(size);
        let rendered = {
            let Ok((renderer, mut fb)) = self
                .backend
                .bind()
                .inspect_err(|err| tracing::error!("bind: {err}"))
            else {
                return;
            };
            smithay::desktop::space::render_output::<_, WaylandSurfaceRenderElement<GlesRenderer>, _, _>(
                &self.output,
                renderer,
                &mut fb,
                1.0,
                0,
                [&self.space],
                &[],
                &mut self.damage_tracker,
                self.clear,
            )
            .inspect_err(|err| tracing::error!("render: {err}"))
            .is_ok()
        };
        if rendered {
            if let Err(err) = self.backend.submit(Some(&[full])) {
                tracing::error!("submit: {err}");
            }
        }

        // Frame callbacks let clients produce their next buffer; throttle to
        // once per redraw cycle.
        let output = self.output.clone();
        let elapsed = self.start.elapsed();
        for window in self.space.elements() {
            window.send_frame(&output, elapsed, Some(Duration::ZERO), |_, _| {
                Some(output.clone())
            });
        }
    }
}

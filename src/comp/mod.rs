//! Compositor state: the Wayland display and its protocol globals, the
//! window space, the seat, and the winit backend that presents it all.
//!
//! M1 shape: clients connect over a private socket, xdg toplevels map
//! full-output into a `Space`, and input is forwarded to whatever holds
//! focus. The split-tree layout replaces the naive Space placement in M4.

pub mod actions;
pub mod chrome;
pub mod handlers;
pub mod input;
pub mod pointer;

use std::sync::Arc;
use std::time::Duration;

use smithay::backend::egl::EGLDevice;
use smithay::backend::renderer::damage::OutputDamageTracker;
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
use smithay::wayland::shell::xdg::decoration::XdgDecorationState;
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

    // Software-rendered chrome (the ported pixel-art renderer) and its
    // GPU-side buffer.
    pub chrome: crate::render::Renderer,
    pub chrome_buf: smithay::backend::renderer::element::memory::MemoryRenderBuffer,
    pub chrome_size: (i32, i32),
    pub chrome_dirty: bool,
    /// On-screen leaves as of the last arrange (chrome + hit regions).
    pub placed: Vec<crate::widgets::Placement>,
    /// Every hit-testable widget rect for the current layout, rebuilt as
    /// one unit each arrange.
    pub widgets: crate::widgets::Widgets,
    /// Taskbar quick-launch entries, resolved once at startup (icons and
    /// .desktop resolution arrive with M8).
    pub quick: Vec<crate::widgets::QuickSlot>,
    /// Parent lookup for every node, rebuilt from one arena walk per
    /// arrange, so per-event consumers skip `Tree::find_parent`'s scan.
    pub parents: std::collections::HashMap<crate::tree::NodeId, (crate::tree::NodeId, usize)>,
    /// `SPLITWM_WALLPAPER`, kept so an output resize can rescale it.
    pub wallpaper_path: Option<String>,

    // Pointer interaction state (see comp::pointer).
    pub drag: Option<pointer::ActiveDrag>,
    /// Sub-pixel scroll remainder carried between axis events.
    pub hscroll_frac: f64,
    /// Set by an action that wants its layout change animated; consumed by
    /// the next `arrange`.
    pub animate: bool,
    /// In-flight layout transition (chrome-only interpolation).
    pub anim: Option<chrome::LayoutAnim>,
    /// Every leaf's frame rect from the last arrange, on-screen or not —
    /// animation start rects and the empty-leaf-body hit region.
    pub prev_frame_rect: std::collections::HashMap<crate::tree::NodeId, crate::widgets::FrameRect>,

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

    // The layout core (pure, ported from master) and the Win <-> Window
    // bridge it drives.
    pub state: crate::state::State,
    pub managed: crate::shell::Managed,
    /// Keycodes whose press we intercepted for a binding: their repeats are
    /// swallowed (a nested winit session auto-repeats; libinput doesn't)
    /// and their release must not leak to the client that never saw the
    /// press.
    pub held_bound_keys: Vec<u32>,

    // Protocol globals.
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    /// Never read, but dropping it would unpublish the xdg-decoration
    /// global (all chrome is ours; clients are told not to decorate).
    #[allow(dead_code)]
    pub xdg_decoration_state: XdgDecorationState,
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
        let xdg_decoration_state = XdgDecorationState::new::<Comp>(&dh);
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
        output.change_current_state(
            Some(mode),
            Some(Transform::Flipped180),
            None,
            Some((0, 0).into()),
        );
        output.set_preferred(mode);
        let damage_tracker = OutputDamageTracker::from_output(&output);

        let mut space = Space::default();
        space.map_output(&output, (0, 0));

        let socket_name = Self::init_wayland_listener(display, event_loop);

        let mut chrome = crate::render::Renderer::new();
        let wallpaper_path = std::env::var("SPLITWM_WALLPAPER").ok();
        if let Some(path) = &wallpaper_path {
            let size = backend.window_size();
            if !chrome.set_wallpaper(path, size.w, size.h) {
                tracing::warn!("could not load wallpaper {path}");
            }
        }

        Comp {
            backend,
            output,
            damage_tracker,
            clear,
            chrome,
            chrome_buf: smithay::backend::renderer::element::memory::MemoryRenderBuffer::new(
                smithay::backend::allocator::Fourcc::Argb8888,
                (1, 1),
                1,
                Transform::Normal,
                None,
            ),
            chrome_size: (0, 0),
            chrome_dirty: true,
            placed: Vec::new(),
            widgets: crate::widgets::Widgets::default(),
            quick: crate::theme::QUICK
                .iter()
                .map(|q| crate::widgets::QuickSlot {
                    cmd: std::env::var(q.env).unwrap_or_else(|_| q.default.to_string()),
                    icon: None,
                    label: q
                        .label
                        .chars()
                        .next()
                        .map_or('?', |c| c.to_ascii_uppercase()),
                    show: q.show,
                })
                .collect(),
            parents: std::collections::HashMap::new(),
            wallpaper_path,
            drag: None,
            hscroll_frac: 0.0,
            animate: false,
            anim: None,
            prev_frame_rect: std::collections::HashMap::new(),
            dh,
            handle,
            signal: event_loop.get_signal(),
            socket_name,
            space,
            popups: PopupManager::default(),
            seat,
            start: std::time::Instant::now(),
            state: crate::state::State::new(),
            managed: crate::shell::Managed::default(),
            held_bound_keys: Vec::new(),
            compositor_state,
            xdg_shell_state,
            xdg_decoration_state,
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
                        display
                            .get_mut()
                            .dispatch_clients(comp)
                            .expect("dispatch clients");
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
        self.space
            .element_under(pos)
            .and_then(|(window, location)| {
                window
                    .surface_under(
                        pos - location.to_f64(),
                        smithay::desktop::WindowSurfaceType::ALL,
                    )
                    .map(|(s, p)| (s, (p.to_f64() + location.to_f64())))
            })
    }

    /// The split-layout area: the output minus the bottom taskbar strip
    /// (master's `la()`). Scale is 1 — this compositor lives in the same
    /// pixel world as its chrome art.
    pub fn layout_area(&self) -> crate::tree::Rect {
        let size = self
            .output
            .current_mode()
            .map(|m| m.size)
            .unwrap_or_else(|| self.backend.window_size());
        crate::tree::Rect {
            x: 0,
            y: 0,
            w: size.w,
            h: (size.h - crate::theme::TASKBAR_H).max(1),
        }
    }

    /// Re-place every window from the layout state: configure sizes, map
    /// what a shown leaf displays, unmap the stash / minimized / scrolled-
    /// out-of-view, then re-derive keyboard focus. The equivalent of
    /// master's `arrange` minus chrome and animation (M3/M5).
    pub fn arrange(&mut self) {
        let wa = self.layout_area();
        // Canvas width / dock scroll room are State's own invariants; the
        // dock contributes from M6.
        self.state.update_canvas(wa, 0);
        let geos = self.state.compute(wa);
        let scroll_x = self.state.scroll_x();
        let focused = self.state.focused_leaf_valid();

        // Every on-screen leaf gets a placement (chrome draws empty and
        // minimized frames too); only occupied, unminimized ones map a
        // window. frame_rects keeps every leaf's rect, on-screen or not,
        // so a leaf scrolled out of view keeps a sane animation start /
        // hit rect when it returns.
        self.placed.clear();
        let mut frame_rects: std::collections::HashMap<
            crate::tree::NodeId,
            crate::widgets::FrameRect,
        > = std::collections::HashMap::new();
        let mut shown: Vec<crate::tree::Win> = Vec::new();
        for leaf in self.state.tree.collect_leaves() {
            let Some(geo) = geos.get(&leaf).copied() else {
                continue;
            };
            let frame = crate::tree::Rect {
                x: geo.x - scroll_x,
                y: geo.y,
                w: geo.w.max(1),
                h: geo.h.max(1),
            };
            frame_rects.insert(leaf, frame);
            if frame.x + frame.w <= wa.x || frame.x >= wa.x + wa.w {
                continue;
            }
            let leaf_data = self.state.tree.leaf(leaf);
            let minimized = leaf_data.is_some_and(|l| l.minimized);
            let client = leaf_data.and_then(|l| l.client);
            self.placed.push(crate::widgets::Placement {
                leaf,
                target: frame,
                active_client: client,
                focused: focused == leaf,
            });
            let Some(c) = client else {
                continue;
            };
            if minimized {
                continue;
            }
            let Some(window) = self.managed.get(c).cloned() else {
                continue;
            };
            let (cx, cy, cw, ch) = crate::shell::client_rect_in_frame(frame, (1, 1));
            if let Some(toplevel) = window.toplevel() {
                toplevel.with_pending_state(|s| s.size = Some((cw, ch).into()));
                toplevel.send_pending_configure();
            }
            self.space.map_element(window, (cx, cy), false);
            shown.push(c);
        }
        let to_hide: Vec<Window> = self
            .managed
            .iter()
            .filter(|(w, _)| !shown.contains(w))
            .map(|(_, window)| window.clone())
            .collect();
        for window in &to_hide {
            self.space.unmap_elem(window);
        }

        // Hit regions and taskbar tiles for this layout, as one unit.
        self.parents = self.state.tree.parent_map();
        self.widgets.clear();
        crate::widgets::compute_leaf_widgets(&mut self.widgets, &self.state.tree, &self.placed);
        crate::widgets::compute_boundary_widgets(&mut self.widgets, &self.state, wa);
        let full = crate::tree::Rect {
            x: 0,
            y: 0,
            w: wa.w,
            h: wa.h + crate::theme::TASKBAR_H,
        };
        let app_ids: Vec<(crate::tree::Win, String)> = self
            .managed
            .iter()
            .map(|(w, window)| (w, crate::shell::toplevel_app_id(window)))
            .collect();
        let classes: Vec<(crate::tree::Win, &str)> =
            app_ids.iter().map(|(w, s)| (*w, s.as_str())).collect();
        let bar_order: Vec<crate::tree::Win> = self.managed.iter().map(|(w, _)| w).collect();
        let leaves = self.state.tree.collect_leaves();
        crate::widgets::compute_taskbar(
            &mut self.widgets,
            &self.state.tree,
            &classes,
            &self.quick,
            &bar_order,
            full,
            &leaves,
        );

        // Layout-changing actions animate: capture start rects and let the
        // redraw tick interpolate the chrome. Client windows are already
        // configured at their final rects above, so focus delivered right
        // after this arrange targets a mapped window; only the composited
        // chrome slides. A non-animated arrange cancels any transition in
        // flight (it describes a newer layout).
        if std::mem::take(&mut self.animate) {
            let placed_from = self
                .placed
                .iter()
                .map(|p| {
                    let from = self.prev_frame_rect.get(&p.leaf).copied().unwrap_or(
                        crate::widgets::FrameRect {
                            x: p.target.x,
                            y: p.target.y,
                            w: 1,
                            h: p.target.h,
                        },
                    );
                    (from, *p)
                })
                .collect();
            self.anim = Some(chrome::LayoutAnim {
                start: std::time::Instant::now(),
                placed: placed_from,
            });
        } else {
            self.anim = None;
            self.chrome_dirty = true;
        }
        self.prev_frame_rect = frame_rects;
        self.refocus();
    }

    /// Point keyboard focus (and xdg activated state) at the layout's
    /// focused client, or nothing when the focused leaf is empty.
    pub fn refocus(&mut self) {
        let focused = self.state.focused_client();
        for (win, window) in self.managed.iter() {
            if window.set_activated(focused == Some(win)) {
                if let Some(toplevel) = window.toplevel() {
                    toplevel.send_pending_configure();
                }
            }
        }
        let target = focused
            .and_then(|c| self.managed.get(c))
            .and_then(|w| w.toplevel().map(|t| t.wl_surface().clone()));
        let keyboard = self.seat.get_keyboard().expect("seat has a keyboard");
        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
        keyboard.set_focus(self, target, serial);
    }

    /// The output was resized (nested window resize stands in for RandR).
    pub fn resize_output(&mut self, size: Size<i32, Physical>) {
        let mode = Mode {
            size,
            refresh: 60_000,
        };
        self.output
            .change_current_state(Some(mode), None, None, None);
        self.output.set_preferred(mode);
        if let Some(path) = self.wallpaper_path.clone() {
            self.chrome.set_wallpaper(&path, size.w, size.h);
        }
    }

    /// Composite one frame and pace clients' frame callbacks.
    pub fn redraw(&mut self) {
        // Scroll glide: step toward the target and re-place windows.
        if self.state.scroll_animating() {
            self.state.step_scroll();
            self.arrange();
        }
        if self.anim.is_some() {
            self.step_animation();
            self.chrome_dirty = false;
        } else if self.chrome_dirty {
            self.compose_chrome();
            self.chrome_dirty = false;
        }
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
            // Front-to-back: client windows (and popups), then the chrome
            // underlay behind everything.
            let mut elements: Vec<chrome::OutputElement> =
                smithay::desktop::space::space_render_elements::<_, Window, _>(
                    renderer,
                    [&self.space],
                    &self.output,
                    1.0,
                )
                .map_or_else(
                    |err| {
                        tracing::error!("space elements: {err:?}");
                        Vec::new()
                    },
                    |els| els.into_iter().map(chrome::OutputElement::Window).collect(),
                );
            match smithay::backend::renderer::element::memory::MemoryRenderBufferRenderElement::from_buffer(
                renderer,
                (0.0, 0.0),
                &self.chrome_buf,
                None,
                None,
                None,
                smithay::backend::renderer::element::Kind::Unspecified,
            ) {
                Ok(el) => elements.push(chrome::OutputElement::Chrome(el)),
                Err(err) => tracing::error!("chrome element: {err}"),
            }
            self.damage_tracker
                .render_output(renderer, &mut fb, 0, &elements, self.clear)
                .inspect_err(|err| tracing::error!("render: {err:?}"))
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

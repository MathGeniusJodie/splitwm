//! Compositor state: the Wayland display and its protocol globals, the
//! window space, the seat, and the winit backend that presents it all.
//!
//! M1 shape: clients connect over a private socket, xdg toplevels map
//! full-output into a `Space`, and input is forwarded to whatever holds
//! focus. The split-tree layout replaces the naive Space placement in M4.

pub mod actions;
pub mod chrome;
pub mod cursor;
pub mod debug;
pub mod handlers;
pub mod icons;
pub mod input;
pub mod layers;
pub mod manage;
pub mod notifications;
pub mod pointer;
pub mod xwayland;

use std::sync::Arc;
use std::time::Duration;

use smithay::backend::egl::EGLDevice;
use smithay::backend::renderer::{Color32F, ImportDma as _};
use smithay::desktop::{PopupManager, Space, Window};
use smithay::input::pointer::CursorImageStatus;
use smithay::input::{Seat, SeatState};
use smithay::output::{Mode, Output};
use smithay::reexports::calloop;
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{EventLoop, Interest, LoopHandle, LoopSignal, PostAction};
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::{Display, DisplayHandle};
use smithay::utils::{Logical, Physical, Point, Rectangle, Size, Transform};
use smithay::wayland::compositor::{CompositorClientState, CompositorState};
use smithay::wayland::cursor_shape::CursorShapeManagerState;
use smithay::wayland::dmabuf::{DmabufFeedbackBuilder, DmabufGlobal, DmabufState};
use smithay::wayland::output::OutputManagerState;
use smithay::wayland::selection::data_device::DataDeviceState;
use smithay::wayland::shell::wlr_layer::WlrLayerShellState;
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
    // Presentation (winit window or DRM output; see crate::backend).
    pub backend: crate::backend::Backend,
    pub output: Output,
    /// Gap background (na16 gunmetal), resolved once from the baked palette.
    pub clear: Color32F,
    /// What the seat's pointer wants shown: a client-committed cursor
    /// surface, a named shape (from chrome hover feedback or a client's
    /// cursor-shape-v1 request), or hidden. The tty backend composites it;
    /// nested sessions map named shapes onto the host's hardware cursor
    /// and composite only cursor surfaces.
    pub cursor_status: CursorImageStatus,
    /// Lazily-uploaded named cursor images: master's hand-drawn sprites
    /// plus xcursor-theme lookups (see `comp::cursor`).
    pub cursors: cursor::CursorCache,

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
    /// A button press the chrome consumed: its release must be swallowed
    /// too, so no client sees half a click.
    pub chrome_press: bool,
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
    /// Toplevels between role creation and their first buffer commit;
    /// classified (tiled/float/dock) only once mapped, since Wayland
    /// clients set app_id/parent/size hints after creating the role.
    pub pending: Vec<Window>,
    /// Float stacking, topmost first.
    pub float_stack: Vec<crate::tree::Win>,
    /// Private invariant: reads go through `Comp::focused_float()`, which
    /// re-validates against the store, so a dangling record is never
    /// handed out.
    focused_float: Option<crate::tree::Win>,
    /// The fullscreen tiled client, covering the whole output above every
    /// tiled window (floats still render above it).
    pub fullscreen: Option<crate::tree::Win>,
    /// Surfaces that requested fullscreen while still pending (clients ask
    /// before their first commit); honored at classify time, master's
    /// pre-map `_NET_WM_STATE` behavior.
    pub pending_fullscreen:
        Vec<smithay::reexports::wayland_server::protocol::wl_surface::WlSurface>,
    /// Keycodes whose press we intercepted for a binding: their repeats are
    /// swallowed (a nested winit session auto-repeats; libinput doesn't)
    /// and their release must not leak to the client that never saw the
    /// press.
    pub held_bound_keys: Vec<u32>,

    /// Off-thread icon fetches report back over this channel (see
    /// `comp::icons`).
    pub icon_tx: calloop::channel::Sender<icons::IconResult>,

    // Served-notification popups and the dismissal path back to the
    // daemon thread (which emits NotificationClosed on the bus).
    pub note_popups: Vec<notifications::NotePopup>,
    pub note_dismiss_tx: std::sync::mpsc::Sender<(u32, crate::notify::CloseReason)>,

    // XWayland: the WM connection (once Ready) and unmanaged
    // override-redirect windows (rofi, menus), topmost last.
    pub xwm: Option<smithay::xwayland::X11Wm>,
    pub or_windows: Vec<xwayland::OrWindow>,
    /// Plain X11 client connection for queries the WM connection doesn't
    /// expose (o-r geometry at map, see `xwayland::OrWindow`).
    pub x11_query: Option<smithay::reexports::x11rb::rust_connection::RustConnection>,
    pub xwayland_shell_state: smithay::wayland::xwayland_shell::XWaylandShellState,

    // Layer-shell surfaces live in the output's LayerMap (smithay); this
    // caches the map's non-exclusive zone so `layout_area` — called from
    // every input path — never takes the map's mutex.
    pub layer_zone: Rectangle<i32, Logical>,

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
    pub layer_shell_state: WlrLayerShellState,
    /// Never read, but dropping it would unpublish the cursor-shape-v1
    /// global (how clients name pointer shapes for us to draw).
    #[allow(dead_code)]
    pub cursor_shape_state: CursorShapeManagerState,
    pub dmabuf_state: DmabufState,
    /// Never read, but it identifies the live dmabuf global for teardown.
    #[allow(dead_code)]
    pub dmabuf_global: DmabufGlobal,
}

impl Comp {
    /// `output` arrives configured by the backend (mode, transform,
    /// global): everything after this reads sizes from the output alone,
    /// never from the backend.
    pub fn new(
        event_loop: &mut EventLoop<'static, Comp>,
        display: Display<Comp>,
        output: Output,
        mut backend: crate::backend::Backend,
    ) -> Comp {
        let dh = display.handle();
        let handle = event_loop.handle();

        let g = crate::assets::palette().color(crate::theme::palette_color::GUNMETAL);
        let clear = Color32F::new(
            f32::from(g.r) / 255.0,
            f32::from(g.g) / 255.0,
            f32::from(g.b) / 255.0,
            1.0,
        );

        let compositor_state = CompositorState::new::<Comp>(&dh);
        let xdg_shell_state = XdgShellState::new::<Comp>(&dh);
        let xdg_decoration_state = XdgDecorationState::new::<Comp>(&dh);
        let shm_state = ShmState::new::<Comp>(&dh, vec![]);
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Comp>(&dh);
        let xwayland_shell_state =
            smithay::wayland::xwayland_shell::XWaylandShellState::new::<Comp>(&dh);
        let mut seat_state = SeatState::new();
        let data_device_state = DataDeviceState::new::<Comp>(&dh);
        let layer_shell_state = WlrLayerShellState::new::<Comp>(&dh);
        let cursor_shape_state = CursorShapeManagerState::new::<Comp>(&dh);

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
        let renderer = backend.renderer();
        let render_node = EGLDevice::device_for_display(renderer.egl_context().display())
            .ok()
            .and_then(|device| device.try_get_render_node().ok().flatten());
        let formats = renderer.dmabuf_formats();
        let dmabuf_global = match render_node {
            Some(node) => {
                let feedback = DmabufFeedbackBuilder::new(node.dev_id(), formats)
                    .build()
                    .expect("default dmabuf feedback");
                dmabuf_state.create_global_with_default_feedback::<Comp>(&dh, &feedback)
            }
            None => dmabuf_state.create_global::<Comp>(&dh, formats),
        };

        let mut space = Space::default();
        space.map_output(&output, (0, 0));

        let socket_name = Self::init_wayland_listener(display, event_loop);

        // Off-thread icon fetches land back on the loop over this channel.
        let (icon_tx, icon_rx) = calloop::channel::channel();
        handle
            .insert_source(icon_rx, |event, (), comp: &mut Comp| {
                if let calloop::channel::Event::Msg(result) = event {
                    comp.on_icon_result(result);
                }
            })
            .expect("insert icon channel source");

        // The notification daemon feeds Show/Close over its own channel.
        let (note_tx, note_rx) = calloop::channel::channel();
        handle
            .insert_source(note_rx, |event, (), comp: &mut Comp| {
                if let calloop::channel::Event::Msg(msg) = event {
                    comp.on_note_msg(msg);
                }
            })
            .expect("insert notification channel source");
        let note_dismiss_tx = crate::notify::spawn(note_tx);

        let mut chrome = crate::render::Renderer::new();
        let wallpaper_path = std::env::var("SPLITWM_WALLPAPER").ok();
        if let Some(path) = &wallpaper_path {
            let size = output.current_mode().expect("output has a mode").size;
            if !chrome.set_wallpaper(path, size.w, size.h) {
                tracing::warn!("could not load wallpaper {path}");
            }
        }

        // Quick-launch entries with their theme icons, resolved once at
        // startup (a handful of ImageMagick decodes, before the loop).
        let quick: Vec<crate::widgets::QuickSlot> = crate::launch::quick_launches()
            .into_iter()
            .map(|q| crate::widgets::QuickSlot {
                icon: crate::launch::find_icon_file(q.icon)
                    .and_then(|p| crate::icon::load_image(&p))
                    .map(|i| std::rc::Rc::new(crate::icon::quantize(chrome.palette(), &i))),
                cmd: q.cmd,
                label: q
                    .label
                    .chars()
                    .next()
                    .map_or('?', |c| c.to_ascii_uppercase()),
                show: q.show,
            })
            .collect();

        let layer_zone = Rectangle::from_size(
            output
                .current_mode()
                .expect("output has a mode")
                .size
                .to_logical(1),
        );

        Comp {
            backend,
            output,
            clear,
            cursor_status: CursorImageStatus::default_named(),
            cursors: cursor::CursorCache::new(),
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
            quick,
            parents: std::collections::HashMap::new(),
            wallpaper_path,
            drag: None,
            chrome_press: false,
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
            pending: Vec::new(),
            float_stack: Vec::new(),
            focused_float: None,
            fullscreen: None,
            pending_fullscreen: Vec::new(),
            held_bound_keys: Vec::new(),
            icon_tx,
            note_popups: Vec::new(),
            note_dismiss_tx,
            xwm: None,
            or_windows: Vec::new(),
            x11_query: None,
            xwayland_shell_state,
            layer_zone,
            compositor_state,
            xdg_shell_state,
            xdg_decoration_state,
            shm_state,
            output_manager_state,
            seat_state,
            data_device_state,
            layer_shell_state,
            cursor_shape_state,
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
        // Stacking order: Overlay layer surfaces topmost, then
        // override-redirect X11 windows, the Top layer, floats (top of
        // stack first), the tiled/fullscreen Space, the dock at the
        // bottom. Bottom/Background layer surfaces get no pointer input:
        // they render behind the opaque chrome underlay (see
        // chrome::output_elements), and what can't be seen must not
        // swallow clicks meant for the chrome.
        use smithay::wayland::shell::wlr_layer::Layer;
        if let Some(hit) = self.layer_surface_under(&[Layer::Overlay], pos) {
            return Some(hit);
        }
        if let Some(hit) = xwayland::or_surface_under(&self.or_windows, pos) {
            return Some(hit);
        }
        if let Some(hit) = self.layer_surface_under(&[Layer::Top], pos) {
            return Some(hit);
        }
        for &fw in &self.float_stack {
            let Some((window, f)) = self.managed.float(fw) else {
                continue;
            };
            let loc = Point::<i32, Logical>::from((f.x, f.y)) - window.geometry().loc;
            if let Some(hit) = window
                .surface_under(pos - loc.to_f64(), smithay::desktop::WindowSurfaceType::ALL)
                .map(|(s, p)| (s, p.to_f64() + loc.to_f64()))
            {
                return Some(hit);
            }
        }
        if let Some(hit) = self
            .space
            .element_under(pos)
            .and_then(|(window, location)| {
                window
                    .surface_under(
                        pos - location.to_f64(),
                        smithay::desktop::WindowSurfaceType::ALL,
                    )
                    .map(|(s, p)| (s, (p.to_f64() + location.to_f64())))
            })
        {
            return Some(hit);
        }
        let (_, window, d) = self.managed.dock()?;
        let rect = self.dock_geometry(d);
        let loc = Point::<i32, Logical>::from((rect.x, rect.y)) - window.geometry().loc;
        window
            .surface_under(pos - loc.to_f64(), smithay::desktop::WindowSurfaceType::ALL)
            .map(|(s, p)| (s, p.to_f64() + loc.to_f64()))
    }

    /// Current output size in pixels. The backend configures a mode before
    /// `Comp` exists and keeps it current on every resize, so a modeless
    /// output is a backend bug.
    pub fn output_size(&self) -> Size<i32, Physical> {
        self.output.current_mode().expect("output has a mode").size
    }

    /// The split-layout area: the output minus the bottom taskbar strip
    /// (master's `la()`), further shrunk by layer-shell exclusive zones
    /// (panels, OSDs). Scale is 1 — this compositor lives in the same
    /// pixel world as its chrome art.
    pub fn layout_area(&self) -> crate::tree::Rect {
        let size = self.output_size();
        let z = self.layer_zone;
        let bottom = (z.loc.y + z.size.h).min(size.h - crate::theme::TASKBAR_H);
        crate::tree::Rect {
            x: z.loc.x,
            y: z.loc.y,
            w: z.size.w.max(1),
            h: (bottom - z.loc.y).max(1),
        }
    }

    /// Re-place every window from the layout state: configure sizes, map
    /// what a shown leaf displays, unmap the stash / minimized / scrolled-
    /// out-of-view, then re-derive keyboard focus. The equivalent of
    /// master's `arrange` minus chrome and animation (M3/M5).
    pub fn arrange(&mut self) {
        let wa = self.layout_area();
        // Canvas width / dock scroll room are State's own invariants; the
        // compositor only supplies the inputs it alone knows.
        self.state.update_canvas(wa, self.dock_extra());
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
            // The fullscreen client is configured below, over the whole
            // output; don't fight it with split geometry.
            if minimized || Some(c) == self.fullscreen {
                continue;
            }
            let Some(window) = self.managed.get(c).cloned() else {
                continue;
            };
            let (cx, cy, cw, ch) = crate::shell::client_rect_in_frame(frame, (1, 1));
            if let Some(toplevel) = window.toplevel() {
                toplevel.with_pending_state(|s| s.size = Some((cw, ch).into()));
                toplevel.send_pending_configure();
            } else if let Some(x11) = window.x11_surface() {
                let _ = x11.configure(Rectangle::<i32, Logical>::new(
                    (cx, cy).into(),
                    (cw, ch).into(),
                ));
                let _ = x11.set_mapped(true);
            }
            self.space.map_element(window, (cx, cy), false);
            shown.push(c);
        }

        // The fullscreen client covers the whole output above every tiled
        // client, regardless of where (or whether) its split is on screen.
        if let Some(fs) = self.fullscreen {
            if let Some(window) = self.managed.get(fs).cloned() {
                let size = self.output_size();
                if let Some(toplevel) = window.toplevel() {
                    toplevel.with_pending_state(|s| s.size = Some((size.w, size.h).into()));
                    toplevel.send_pending_configure();
                } else if let Some(x11) = window.x11_surface() {
                    let _ = x11.configure(Rectangle::<i32, Logical>::new(
                        (0, 0).into(),
                        (size.w, size.h).into(),
                    ));
                    let _ = x11.set_mapped(true);
                }
                self.space.map_element(window, (0, 0), true);
                shown.push(fs);
            }
        }
        let to_hide: Vec<Window> = self
            .managed
            .tiled_iter()
            .filter(|(w, _)| !shown.contains(w))
            .map(|(_, window)| window.clone())
            .collect();
        for window in &to_hide {
            self.space.unmap_elem(window);
            // A stashed X11 window is really unmapped (its WM_STATE
            // bookkeeping lives inside smithay).
            if let Some(x11) = window.x11_surface() {
                let _ = x11.set_mapped(false);
            }
        }

        // Hit regions and taskbar tiles for this layout, as one unit.
        self.parents = self.state.tree.parent_map();
        self.widgets.clear();
        crate::widgets::compute_leaf_widgets(&mut self.widgets, &self.state.tree, &self.placed);
        crate::widgets::compute_boundary_widgets(&mut self.widgets, &self.state, wa);
        // The taskbar strip spans the full output, not the (possibly
        // zone-shrunk) layout area.
        let size = self.output_size();
        let full = crate::tree::Rect {
            x: 0,
            y: 0,
            w: size.w,
            h: size.h,
        };
        let app_ids: Vec<(crate::tree::Win, String)> = self
            .managed
            .tiled_iter()
            .map(|(w, window)| (w, crate::shell::toplevel_app_id(window)))
            .collect();
        let classes: Vec<(crate::tree::Win, &str)> =
            app_ids.iter().map(|(w, s)| (*w, s.as_str())).collect();
        let bar_order: Vec<crate::tree::Win> = self.managed.tiled_iter().map(|(w, _)| w).collect();
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

    /// Point keyboard focus (and xdg activated state) at the focused
    /// float when one holds the keyboard, else the layout's focused
    /// client, else nothing.
    pub fn refocus(&mut self) {
        let focused = self
            .keyboard_override()
            .or_else(|| self.state.focused_client());
        let updates: Vec<Window> = self
            .managed
            .windows()
            .filter(|window| {
                let win = self.managed.win_for_window(window);
                window.set_activated(win.is_some() && win == focused)
            })
            .cloned()
            .collect();
        for window in updates {
            if let Some(toplevel) = window.toplevel() {
                toplevel.send_pending_configure();
            }
        }
        // An exclusive-keyboard layer surface (rofi) outranks every window
        // while mapped.
        let target = self.exclusive_layer_surface().or_else(|| {
            focused.and_then(|c| self.managed.get(c)).and_then(|w| {
                smithay::wayland::seat::WaylandFocus::wl_surface(w).map(|s| s.into_owned())
            })
        });
        let keyboard = self.seat.get_keyboard().expect("seat has a keyboard");
        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
        keyboard.set_focus(self, target, serial);
    }

    /// The output changed mode (nested window resize, or a tty connector
    /// swap standing in for RandR): republish, rescale the wallpaper, and
    /// relayout everything into the new area.
    pub fn resize_output(&mut self, mode: Mode) {
        self.output
            .change_current_state(Some(mode), None, None, None);
        self.output.set_preferred(mode);
        // Layer surfaces re-anchor to the new size; the zone follows.
        self.sync_layer_zone();
        if let Some(path) = self.wallpaper_path.clone() {
            self.chrome.set_wallpaper(&path, mode.size.w, mode.size.h);
        }
        self.arrange();
        self.chrome_dirty = true;
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
        // Float frames whose content changed since last frame.
        let dirty_frames: Vec<crate::tree::Win> = self
            .managed
            .float_iter()
            .filter(|(_, _, f)| f.frame_dirty)
            .map(|(w, _, _)| w)
            .collect();
        for win in dirty_frames {
            self.paint_float_frame(win);
        }
        // Dock/note element inputs, resolved before the renderer borrow
        // below (both need &self, which the bound renderer excludes).
        let dock_place = self
            .managed
            .dock()
            .map(|(_, window, d)| (window.clone(), self.dock_geometry(d)));
        let note_rects = self.note_rects();

        // The seat pointer's spot, for wherever the cursor is composited
        // (tty always; winit for client cursor surfaces).
        let pointer_loc = self
            .seat
            .get_pointer()
            .expect("seat has a pointer")
            .current_location();

        // Scene borrows and the backend borrow are disjoint `Comp` fields,
        // so the backend can consume the scene while borrowed mutably.
        let scene = chrome::Scene {
            or_windows: &self.or_windows,
            note_popups: &self.note_popups,
            note_rects: &note_rects,
            float_stack: &self.float_stack,
            managed: &self.managed,
            space: &self.space,
            output: &self.output,
            dock_place: &dock_place,
            chrome_buf: &self.chrome_buf,
        };
        match &mut self.backend {
            crate::backend::Backend::Winit(w) => {
                // Named shapes ride the host's hardware cursor (zero
                // latency); a client-committed cursor surface has no host
                // analog, so it composites like on tty.
                let composite_cursor = w.apply_cursor(&self.cursor_status);
                let size = w.backend.window_size();
                let full: Rectangle<i32, Physical> = Rectangle::from_size(size);
                let rendered = {
                    let Ok((renderer, mut fb)) = w
                        .backend
                        .bind()
                        .inspect_err(|err| tracing::error!("bind: {err}"))
                    else {
                        return;
                    };
                    let mut elements = if composite_cursor {
                        cursor::cursor_elements(
                            renderer,
                            pointer_loc,
                            &self.cursor_status,
                            &mut self.cursors,
                        )
                    } else {
                        Vec::new()
                    };
                    elements.extend(chrome::output_elements(renderer, &scene));
                    w.damage_tracker
                        .render_output(renderer, &mut fb, 0, &elements, self.clear)
                        .inspect_err(|err| tracing::error!("render: {err:?}"))
                        .is_ok()
                };
                if rendered {
                    if let Err(err) = w.backend.submit(Some(&[full])) {
                        tracing::error!("submit: {err}");
                    }
                }
            }
            crate::backend::Backend::Headless(h) => h.render(&scene, self.clear),
            #[cfg(feature = "tty")]
            crate::backend::Backend::Tty(t) => {
                t.render(
                    &scene,
                    pointer_loc,
                    &self.cursor_status,
                    &mut self.cursors,
                    self.clear,
                );
            }
        }

        // Frame callbacks let clients produce their next buffer; throttle to
        // once per redraw cycle. Every managed kind gets one (floats and
        // the dock live outside the Space).
        let output = self.output.clone();
        let elapsed = self.start.elapsed();
        for window in self.managed.windows() {
            window.send_frame(&output, elapsed, Some(Duration::ZERO), |_, _| {
                Some(output.clone())
            });
        }
        for or in &self.or_windows {
            if let Some(surface) = or.surface.wl_surface() {
                smithay::desktop::utils::send_frames_surface_tree(
                    &surface,
                    &output,
                    elapsed,
                    Some(Duration::ZERO),
                    |_, _| Some(output.clone()),
                );
            }
        }
        for layer in smithay::desktop::layer_map_for_output(&output).layers() {
            layer.send_frame(&output, elapsed, Some(Duration::ZERO), |_, _| {
                Some(output.clone())
            });
        }
    }
}

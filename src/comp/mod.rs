//! Compositor state: the Wayland display and its protocol globals, the
//! window space, the seat, and the backend that presents it all. Clients
//! connect over a private socket; the split layout (`crate::state`) places
//! every tiled window, and `arrange`/`redraw` below push that placement at
//! the clients and the screen.

pub mod actions;
pub mod anim;
pub mod cursor;
pub mod debug;
pub mod handlers;
pub mod icons;
pub mod input;
pub mod layers;
pub mod manage;
pub mod notifications;
pub mod pieces;
pub mod pointer;
pub mod scene;
pub mod xwayland;

use std::sync::Arc;
use std::time::Duration;

use smithay::backend::egl::EGLDevice;
use smithay::backend::renderer::element::solid::{SolidColorBuffer, SolidColorRenderElement};
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::{Color32F, ImportDma as _};
use smithay::desktop::{PopupKind, PopupManager, Space, Window};
use smithay::input::pointer::CursorImageStatus;
use smithay::input::{Seat, SeatState};
use smithay::output::{Mode, Output};
use smithay::reexports::calloop;
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{EventLoop, Interest, LoopHandle, LoopSignal, PostAction};
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::{Display, DisplayHandle};
use smithay::utils::{Logical, Physical, Point, Rectangle, Size};
use smithay::wayland::compositor::{CompositorClientState, CompositorState};
use smithay::wayland::cursor_shape::CursorShapeManagerState;
use smithay::wayland::dmabuf::{DmabufFeedbackBuilder, DmabufGlobal, DmabufState};
use smithay::wayland::output::OutputManagerState;
use smithay::wayland::selection::data_device::DataDeviceState;
use smithay::wayland::selection::primary_selection::PrimarySelectionState;
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

/// A toplevel between role creation and its first buffer commit, together
/// with what it asked for while pending — one record per window, so a
/// request can't outlive or miss the window it belongs to.
pub struct PendingWindow {
    pub window: Window,
    /// The client requested fullscreen before mapping (a startup-fullscreen
    /// client); honored once the window is classified.
    pub fullscreen: bool,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

/// Everything describing what this arrange put on screen and how it draws:
/// the software chrome renderer, the GPU piece caches, the placements and
/// hit regions, and the layout animation. Owned as one unit so the render
/// and hit-test paths borrow it independently of the rest of `Comp`.
pub struct ChromeView {
    // Software-rendered chrome (the ported pixel-art renderer) and its
    // GPU-side buffer.
    pub chrome: crate::render::Renderer,
    /// The palette shader and its reused upload staging, shared by every
    /// software-drawn chrome buffer on their way to the GPU.
    pub indexed: crate::render::indexed::IndexedProgram,
    /// The independently-textured ex-underlay pieces (wallpaper, per-leaf
    /// frames, plus buttons, taskbar); each re-renders only when its own
    /// content fingerprint changes, so scrolling and animation are pure
    /// element placement (see `comp::pieces`).
    pub pieces: pieces::ChromePieces,
    /// On-screen leaves as of the last arrange (chrome + hit regions).
    pub placed: Vec<crate::widgets::Placement>,
    /// Every hit-testable widget rect for the current layout, rebuilt as
    /// one unit each arrange.
    pub widgets: crate::widgets::Widgets,
    /// Taskbar quick-launch entries, resolved once at startup.
    pub quick: Vec<crate::widgets::QuickSlot>,
    /// `SPLITWM_WALLPAPER`, kept so an output resize can rescale it.
    pub wallpaper_path: Option<String>,
    /// Set by an action that wants its layout change animated; consumed by
    /// the next `arrange`.
    pub animate: bool,
    /// In-flight layout transition (chrome-only interpolation).
    pub anim: Option<anim::LayoutAnim>,
    /// Every leaf's frame rect from the last arrange, on-screen or not —
    /// animation start rects and the empty-leaf-body hit region.
    pub frame_rects:
        std::collections::HashMap<crate::layout::NodeId, crate::widgets::FrameRect>,
    /// The rect the focus outline currently traces: the focused split's
    /// frame, or its interpolated frame mid-animation. `None` when no leaf
    /// holds focus. Tracked outside the underlay so a focus switch moves the
    /// outline without recompositing.
    pub focus_rect: Option<crate::widgets::FrameRect>,
    /// The four persistent solid-colour buffers (top, bottom, left, right)
    /// the focus outline's GPU strips draw from; their stable ids let the
    /// damage tracker follow each strip as the focused rect moves.
    pub focus_outline: [SolidColorBuffer; 4],
}

/// In-flight input interaction state (see `comp::pointer`, `comp::input`).
#[derive(Default)]
pub struct Interaction {
    pub drag: Option<pointer::ActiveDrag>,
    /// A button press the chrome consumed: its release must be swallowed
    /// too, so no client sees half a click.
    pub chrome_press: bool,
    /// Sub-pixel scroll remainder carried between axis events.
    pub hscroll_frac: f64,
    /// A three-finger touchpad swipe is in progress; its updates pan the
    /// canvas.
    pub swipe_pan: bool,
    /// Keycodes whose press we intercepted for a binding: their repeats are
    /// swallowed (a nested winit session auto-repeats; libinput doesn't)
    /// and their release must not leak to the client that never saw the
    /// press.
    pub held_bound_keys: Vec<u32>,
}

/// Window bookkeeping beyond the `Managed` store: toplevels not yet
/// classified, float stacking, and the two records only handed out
/// re-validated.
#[derive(Default)]
pub struct WindowRoles {
    /// Toplevels between role creation and their first buffer commit;
    /// classified (tiled/float/dock) only once mapped, since Wayland
    /// clients set app_id/parent/size hints after creating the role.
    pub pending: Vec<PendingWindow>,
    /// Float stacking, topmost first.
    pub float_stack: Vec<crate::layout::Win>,
    /// Private invariant: reads go through `Comp::focused_float()`, which
    /// re-validates against the store, so a dangling record is never
    /// handed out.
    pub(super) focused_float: Option<crate::layout::Win>,
    /// The fullscreen tiled client, covering the whole output above every
    /// tiled window (floats still render above it). Private invariant:
    /// reads go through `Comp::fullscreen()`, which re-validates against
    /// the store, so a dangling record is never handed out.
    pub(super) fullscreen: Option<crate::layout::Win>,
}

/// The published protocol globals. Most are only ever handed back to their
/// smithay handler traits; the `dead_code` ones exist purely so dropping
/// `Comp` (never in practice) is what unpublishes them.
pub struct Globals {
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
    pub primary_selection_state: PrimarySelectionState,
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

pub struct Comp {
    // Presentation (winit window or DRM output; see crate::backend).
    pub backend: crate::backend::Backend,
    pub output: Output,
    /// Gap background (na16 gunmetal), resolved once from the baked palette.
    pub clear: Color32F,
    /// What the seat's pointer shows: a named shape (from chrome hover
    /// feedback or a cursor-shape-v1 request), a client's own cursor
    /// surface (`wl_pointer.set_cursor` with committed pixels — XWayland
    /// forwards X11 cursors this way too), or hidden after a null-surface
    /// `set_cursor`. The tty and winit backends both composite it over a
    /// hidden host cursor.
    pub cursor_status: CursorImageStatus,
    /// Lazily-uploaded named cursor images: master's hand-drawn sprites
    /// (see `comp::cursor`).
    pub cursors: cursor::CursorCache,

    // What's on screen and how it draws.
    pub view: ChromeView,

    // Pointer/keyboard interaction state.
    pub interaction: Interaction,

    // Wayland plumbing.
    pub dh: DisplayHandle,
    pub handle: LoopHandle<'static, Comp>,
    pub signal: LoopSignal,
    pub socket_name: std::ffi::OsString,

    // Windows and input.
    pub space: Space<Window>,
    pub popups: PopupManager,
    pub seat: Seat<Comp>,
    /// The seat's keyboard/pointer capabilities, added once at construction
    /// and never removed — held here so consumers don't re-prove their
    /// existence through `Seat::get_keyboard`/`get_pointer`'s `Option` on
    /// every use. The handles are cheap `Arc` clones of what the seat holds.
    pub keyboard: smithay::input::keyboard::KeyboardHandle<Comp>,
    pub pointer: smithay::input::pointer::PointerHandle<Comp>,
    pub start: std::time::Instant,

    // The layout core (pure, ported from master), the Win <-> Window
    // bridge it drives, and the window bookkeeping around the bridge.
    pub state: crate::state::State,
    pub managed: crate::shell::Managed,
    pub windows: WindowRoles,

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
    pub globals: Globals,
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
        let primary_selection_state = PrimarySelectionState::new::<Comp>(&dh);
        let layer_shell_state = WlrLayerShellState::new::<Comp>(&dh);
        let cursor_shape_state = CursorShapeManagerState::new::<Comp>(&dh);

        let mut seat: Seat<Comp> = seat_state.new_wl_seat(&dh, "seat-0");
        // xkb defaults come from the environment (XKB_DEFAULT_LAYOUT etc.),
        // matching how the X11 version inherited the server keymap.
        let keyboard = seat
            .add_keyboard(Default::default(), 600, 25)
            .expect("keyboard with default xkb config");
        let pointer = seat.add_pointer();

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

        let indexed = crate::render::indexed::IndexedProgram::new(backend.renderer());

        let mut chrome = crate::render::Renderer::new();
        // The outline colour never changes; bake it into each strip buffer
        // once so a focus move only resizes/relocates them.
        let outline_color = Color32F::from(chrome.focus_color());
        let focus_outline = std::array::from_fn(|_| SolidColorBuffer::new((0, 0), outline_color));
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
            view: ChromeView {
                chrome,
                indexed,
                pieces: pieces::ChromePieces::default(),
                placed: Vec::new(),
                widgets: crate::widgets::Widgets::default(),
                quick,
                wallpaper_path,
                animate: false,
                anim: None,
                frame_rects: std::collections::HashMap::new(),
                focus_rect: None,
                focus_outline,
            },
            interaction: Interaction::default(),
            dh,
            handle,
            signal: event_loop.get_signal(),
            socket_name,
            space,
            popups: PopupManager::default(),
            seat,
            keyboard,
            pointer,
            start: std::time::Instant::now(),
            state: crate::state::State::new(),
            managed: crate::shell::Managed::default(),
            windows: WindowRoles::default(),
            icon_tx,
            note_popups: Vec::new(),
            note_dismiss_tx,
            xwm: None,
            or_windows: Vec::new(),
            x11_query: None,
            xwayland_shell_state,
            layer_zone,
            globals: Globals {
                compositor_state,
                xdg_shell_state,
                xdg_decoration_state,
                shm_state,
                output_manager_state,
                seat_state,
                data_device_state,
                primary_selection_state,
                layer_shell_state,
                cursor_shape_state,
                dmabuf_state,
                dmabuf_global,
            },
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
        // stack first), the tiled/fullscreen Space, the dock, the Bottom
        // layer at the bottom — the same front-to-back order
        // scene::output_elements renders. Background surfaces get no
        // pointer input: they render behind the opaque chrome underlay,
        // and what can't be seen must not swallow clicks meant for the
        // chrome.
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
        for &fw in &self.windows.float_stack {
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
        // Tiled/fullscreen windows, frontmost first. Popups hit-test
        // everywhere: they render uncropped, so a menu overflowing its leaf
        // must take input where it is visible. The toplevel only hit-tests
        // inside its settled client rect — the region it draws in
        // (`Scene::tiled` crops its buffer to it), not its buffer extent: a
        // client whose resize waits for an animation's end still holds an
        // oversized buffer, which must not take input over chrome it
        // visibly no longer covers.
        for t in self.settled_tiled_places() {
            let loc = Point::<i32, Logical>::from((t.rect.x, t.rect.y)) - t.window.geometry().loc;
            let in_rect = pos.x >= f64::from(t.rect.x)
                && pos.x < f64::from(t.rect.x + t.rect.w)
                && pos.y >= f64::from(t.rect.y)
                && pos.y < f64::from(t.rect.y + t.rect.h);
            let surface_type = if in_rect {
                smithay::desktop::WindowSurfaceType::ALL
            } else {
                smithay::desktop::WindowSurfaceType::POPUP
            };
            if let Some(hit) = t
                .window
                .surface_under(pos - loc.to_f64(), surface_type)
                .map(|(s, p)| (s, p.to_f64() + loc.to_f64()))
            {
                return Some(hit);
            }
        }
        if let Some((_, window, d)) = self.managed.dock() {
            let rect = self.dock_geometry(d);
            let loc = Point::<i32, Logical>::from((rect.x, rect.y)) - window.geometry().loc;
            if let Some(hit) = window
                .surface_under(pos - loc.to_f64(), smithay::desktop::WindowSurfaceType::ALL)
                .map(|(s, p)| (s, p.to_f64() + loc.to_f64()))
            {
                return Some(hit);
            }
        }
        self.layer_surface_under(&[Layer::Bottom], pos)
    }

    /// The tiled/fullscreen window whose settled client rect covers `pos` —
    /// the region it draws in (`Scene::tiled` crops its buffer to it), not
    /// its buffer extent: a client whose resize waits for an animation's
    /// end still holds an oversized buffer, which must not take input over
    /// chrome it visibly no longer covers.
    pub fn tiled_under(&self, pos: Point<f64, Logical>) -> Option<scene::TiledPlace> {
        self.settled_tiled_places().into_iter().find(|t| {
            pos.x >= f64::from(t.rect.x)
                && pos.x < f64::from(t.rect.x + t.rect.w)
                && pos.y >= f64::from(t.rect.y)
                && pos.y < f64::from(t.rect.y + t.rect.h)
        })
    }

    /// `positioner`'s geometry, constraint-adjusted (flip/slide/resize per
    /// the client's `constraint_adjustment`) so the popup stays inside the
    /// output. The client only states placement preferences through
    /// `xdg_positioner`; keeping the popup on screen is the compositor's
    /// job, so without this a menu opened near a screen edge runs off it.
    pub fn unconstrained_popup_geometry(
        &self,
        popup: &smithay::wayland::shell::xdg::PopupSurface,
        positioner: smithay::wayland::shell::xdg::PositionerState,
    ) -> Rectangle<i32, Logical> {
        let kind = PopupKind::Xdg(popup.clone());
        let fallback = positioner.get_geometry();
        let Ok(root) = smithay::desktop::find_popup_root_surface(&kind) else {
            return fallback;
        };
        let Some(root_loc) = self.popup_root_loc(&root) else {
            return fallback;
        };
        let size: Size<i32, Logical> = self.output_size().to_logical(1);
        // The positioner works relative to the parent's geometry origin;
        // express the output rect in that space.
        let mut target = Rectangle::new((0, 0).into(), size);
        target.loc -= root_loc + smithay::desktop::get_popup_toplevel_coords(&kind);
        positioner.get_unconstrained_geometry(target)
    }

    /// The global position of `root`'s geometry origin — the space xdg
    /// popup geometries are relative to. Mirrors `surface_under`'s legs:
    /// settled tiled/fullscreen rects, float and dock positions, layer
    /// surfaces at their map (or scrolled-dock) location.
    fn popup_root_loc(
        &self,
        root: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    ) -> Option<Point<i32, Logical>> {
        use smithay::wayland::seat::WaylandFocus as _;
        if let Some(win) = self.managed.win_for_surface(root) {
            return match self.managed.kind_of(win)? {
                crate::shell::Kind::Tiled => self
                    .settled_tiled_places()
                    .into_iter()
                    .find(|t| t.window.wl_surface().is_some_and(|s| *s == *root))
                    .map(|t| Point::from((t.rect.x, t.rect.y))),
                crate::shell::Kind::Float(f) => Some(Point::from((f.x, f.y))),
                crate::shell::Kind::Dock(d) => {
                    let rect = self.dock_geometry(*d);
                    Some(Point::from((rect.x, rect.y)))
                }
            };
        }
        if let Some((surface, loc)) = self.layer_dock_place() {
            if surface == *root {
                return Some(loc);
            }
        }
        let map = smithay::desktop::layer_map_for_output(&self.output);
        let layer = map.layer_for_surface(root, smithay::desktop::WindowSurfaceType::TOPLEVEL)?;
        map.layer_geometry(layer).map(|geo| geo.loc)
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
    pub fn layout_area(&self) -> crate::layout::Rect {
        let size = self.output_size();
        let z = self.layer_zone;
        let bottom = (z.loc.y + z.size.h).min(size.h - crate::theme::TASKBAR_H);
        crate::layout::Rect {
            x: z.loc.x,
            y: z.loc.y,
            w: z.size.w.max(1),
            h: (bottom - z.loc.y).max(1),
        }
    }

    /// Re-place every window from the layout state: compute this arrange's
    /// placements (pure — `widgets::compute_placements`), push them at the
    /// clients, rebuild the hit regions, arm or cancel the layout
    /// animation, then re-derive keyboard focus.
    pub fn arrange(&mut self) {
        let wa = self.layout_area();
        // The strip width is derived from the columns; the dock scroll
        // room is the one input the compositor alone knows.
        self.state.set_dock_extra(self.dock_extra());
        let (placed, frame_rects) = crate::widgets::compute_placements(&self.state, wa);
        self.view.placed = placed;
        let deferred = self.apply_placements();
        self.rebuild_widgets(wa);
        self.arm_animation(deferred, frame_rects);
        self.refocus();
    }

    /// Push the current placements at the clients: configure sizes, map
    /// what a shown leaf displays (plus the fullscreen client over the
    /// whole output), unmap the minimized / scrolled-out-of-view. Returns
    /// the configures withheld from shrinking clients mid-animation.
    fn apply_placements(&mut self) -> Vec<(crate::layout::Win, (i32, i32, i32, i32))> {
        let fullscreen = self.fullscreen();
        let mut deferred: Vec<(crate::layout::Win, (i32, i32, i32, i32))> = Vec::new();
        let mut shown: Vec<crate::layout::Win> = Vec::new();
        let placed = self.view.placed.clone();
        for p in placed {
            let Some(c) = p.active_client else {
                continue;
            };
            let minimized = self.state.layout.leaf(p.leaf).is_some_and(|l| l.minimized);
            // The fullscreen client is configured below, over the whole
            // output; don't fight it with split geometry.
            if minimized || Some(c) == fullscreen {
                continue;
            }
            let Some(window) = self.managed.get(c).cloned() else {
                continue;
            };
            let (cx, cy, cw, ch) = crate::shell::client_rect_in_frame(p.target, (1, 1));
            // An animating client that shrinks keeps its current size until
            // the slide settles (`finish_animation` sends this configure):
            // resizing it now would reflow its content narrow while its
            // frame is still wide. Growers configure immediately — their old
            // buffer is clipped by the still-small frame, and the new size
            // is ready as the frame arrives.
            let cur = window.geometry().size;
            if self.view.animate && (cw < cur.w || ch < cur.h) {
                deferred.push((c, (cx, cy, cw, ch)));
            } else {
                crate::shell::configure_rect(&window, cx, cy, cw, ch);
            }
            // Even when the resize waits, a hidden X11 window coming back
            // must map now.
            crate::shell::set_x11_mapped(&window, true);
            self.space.map_element(window, (cx, cy), false);
            shown.push(c);
        }

        // The fullscreen client covers the whole output above every tiled
        // client, regardless of where (or whether) its split is on screen.
        if let Some(fs) = fullscreen {
            if let Some(window) = self.managed.get(fs).cloned() {
                let size = self.output_size();
                crate::shell::configure_rect(&window, 0, 0, size.w, size.h);
                crate::shell::set_x11_mapped(&window, true);
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
            crate::shell::set_x11_mapped(window, false);
        }
        deferred
    }

    /// Rebuild every hit region and taskbar tile for the current arrange,
    /// as one unit.
    fn rebuild_widgets(&mut self, wa: crate::layout::Rect) {
        self.view.widgets.clear();
        crate::widgets::compute_leaf_widgets(
            &mut self.view.widgets,
            &self.state.layout,
            &self.view.placed,
        );
        crate::widgets::compute_boundary_widgets(&mut self.view.widgets, &self.state, wa);
        // The taskbar strip spans the full output, not the (possibly
        // zone-shrunk) layout area.
        let size = self.output_size();
        let full = crate::layout::Rect {
            x: 0,
            y: 0,
            w: size.w,
            h: size.h,
        };
        let app_ids: Vec<(crate::layout::Win, String)> = self
            .managed
            .tiled_iter()
            .map(|(w, window)| (w, crate::shell::toplevel_app_id(window)))
            .collect();
        // Tiles mirror the splits: the bar reads left-to-right in the same
        // depth-first order the canvas lays the leaves out in. Every tiled
        // window occupies a leaf (`State::place_new_window`), so walking the
        // leaves loses nobody.
        let bar_order: Vec<(crate::layout::Win, crate::layout::NodeId)> = self
            .state
            .layout
            .collect_leaves()
            .iter()
            .filter_map(|&l| {
                self.state
                    .layout
                    .leaf(l)
                    .and_then(|lf| lf.client)
                    .map(|c| (c, l))
            })
            .collect();
        debug_assert_eq!(
            bar_order.len(),
            self.managed.tiled_iter().count(),
            "a tiled window is missing from the split tree"
        );
        crate::widgets::compute_taskbar(
            &mut self.view.widgets,
            &self.state.layout,
            &app_ids,
            &self.view.quick,
            &bar_order,
            full,
        );
    }

    /// Layout-changing actions animate: capture start rects and let the
    /// redraw tick interpolate the chrome and the client windows riding
    /// it (`tiled_places`). Growing clients are already configured at
    /// their final rects by `apply_placements`; shrinking ones get theirs
    /// when the animation settles (`deferred`). A non-animated arrange
    /// cancels any transition in flight (it describes a newer layout, and
    /// configured every window itself). Also stores this arrange's rects
    /// in `view.frame_rects` as the next arrange's animation start rects.
    fn arm_animation(
        &mut self,
        deferred: Vec<(crate::layout::Win, (i32, i32, i32, i32))>,
        frame_rects: std::collections::HashMap<crate::layout::NodeId, crate::widgets::FrameRect>,
    ) {
        if std::mem::take(&mut self.view.animate) {
            let placed_from =
                self.view
                    .placed
                    .iter()
                    .map(|p| {
                        // A leaf with no previous rect just appeared: grow it
                        // along the axis it split off — a row in a stack unfolds
                        // vertically at full width, a new column horizontally at
                        // full height.
                        let from =
                            self.view
                                .frame_rects
                                .get(&p.leaf)
                                .copied()
                                .unwrap_or_else(|| {
                                    let stacked =
                                        self.state.layout.locate(p.leaf).is_some_and(|pos| {
                                            self.state.layout.col_len(pos.col) > 1
                                        });
                                    let (w, h) = if stacked {
                                        (p.target.w, 1)
                                    } else {
                                        (1, p.target.h)
                                    };
                                    crate::widgets::FrameRect {
                                        x: p.target.x,
                                        y: p.target.y,
                                        w,
                                        h,
                                    }
                                });
                        (from, *p)
                    })
                    .collect();
            self.view.anim = Some(anim::LayoutAnim {
                start: std::time::Instant::now(),
                placed: placed_from,
                deferred,
            });
        } else {
            // A non-animated arrange cancels any transition in flight; the
            // per-piece fingerprints (`update_chrome_pieces`) decide what
            // actually re-renders, so a rebuild that only re-aimed focus or
            // only scrolled positions repaints nothing.
            self.view.anim = None;
        }
        self.view.frame_rects = frame_rects;
    }

    /// Point keyboard focus (and xdg activated state) at the focused
    /// float when one holds the keyboard, else the layout's focused
    /// client, else nothing.
    pub fn refocus(&mut self) {
        let focused = self
            .keyboard_override()
            .or_else(|| self.state.focused_client());
        // Only windows whose activated state actually flipped get a
        // configure (set_activated reports the change).
        let mut updates: Vec<Window> = Vec::new();
        for window in self.managed.windows() {
            let win = self.managed.win_for_window(window);
            if window.set_activated(win.is_some() && win == focused) {
                updates.push(window.clone());
            }
        }
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
        let keyboard = self.keyboard.clone();
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
        // A shrink also shrinks the scroll range; don't strand the
        // viewport past the content.
        self.reclamp_scroll();
        if let Some(path) = self.view.wallpaper_path.clone() {
            self.view
                .chrome
                .set_wallpaper(&path, mode.size.w, mode.size.h);
        }
        self.arrange();
        // The wallpaper piece rebuilds on the size change, and every leaf /
        // taskbar fingerprint carries the new size, so `update_chrome_pieces`
        // repaints them on the next redraw with no extra flag.
    }

    /// The focus outline's four solid strips (top, bottom, left, right) for
    /// the current `focus_rect`, or empty when no leaf holds focus. Each
    /// strip resizes its persistent buffer (bumping its damage on a size
    /// change) and draws at scale 1; the buffers' stable ids let the damage
    /// tracker track a strip across frames as the focused rect moves.
    fn focus_outline_elements(&mut self) -> Vec<SolidColorRenderElement> {
        let Some(r) = self.view.focus_rect else {
            return Vec::new();
        };
        const T: i32 = 2;
        // Traced on the frame's edges (top and bottom full width, sides
        // between them), matching the strips the underlay used to paint.
        let strips = [
            (r.x, r.y, r.w, T),
            (r.x, r.y + r.h - T, r.w, T),
            (r.x, r.y + T, T, r.h - 2 * T),
            (r.x + r.w - T, r.y + T, T, r.h - 2 * T),
        ];
        self.view
            .focus_outline
            .iter_mut()
            .zip(strips)
            .map(|(buf, (x, y, w, h))| {
                buf.resize((w.max(0), h.max(0)));
                SolidColorRenderElement::from_buffer(
                    buf,
                    Point::<i32, Logical>::from((x, y)).to_physical(1),
                    1.0,
                    1.0,
                    Kind::Unspecified,
                )
            })
            .collect()
    }

    /// Composite one frame and pace clients' frame callbacks.
    pub fn redraw(&mut self) {
        // A client that dies mid-hover leaves its cursor surface behind;
        // fall back to the arrow.
        use smithay::utils::IsAlive;
        if matches!(&self.cursor_status, CursorImageStatus::Surface(s) if !s.alive()) {
            self.cursor_status = CursorImageStatus::default_named();
        }
        // Scroll glide: step toward the target and re-place windows.
        if self.state.scroll_animating() {
            self.state.step_scroll();
            self.arrange();
        }
        // Advance any layout animation and take this frame's leaf rects
        // (interpolated mid-slide, settled otherwise); `tick_layout` also
        // updates `focus_rect` to ride the focused leaf.
        let leaf_rects = self.tick_layout();
        // Re-render any chrome piece whose content changed, including the
        // animating leaves at their interpolated sizes; unchanged pieces
        // (scroll, idle leaves, wallpaper, taskbar) hit the cache.
        self.update_chrome_pieces(&leaf_rects);
        // Float frames whose content changed since last frame.
        let dirty_frames: Vec<crate::layout::Win> = self
            .managed
            .float_iter()
            .filter(|(_, _, f)| f.frame.is_stale())
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
        let layer_dock = self.layer_dock_place();
        let note_rects = self.note_rects();

        // The seat pointer's spot, for wherever the cursor is composited
        // (tty always; winit for client cursor surfaces).
        let pointer_loc = self.pointer.current_location();

        // Built before the Scene's shared borrows: it mutates the outline
        // buffers, and the owned elements outlive that borrow.
        let focus_outline = self.focus_outline_elements();

        // Tiled/fullscreen windows at this frame's client rects (riding the
        // interpolated leaf rects mid-animation).
        let tiled = self.tiled_places(&leaf_rects);

        // The ex-underlay pieces as positioned elements, built from the
        // caches `update_chrome_pieces` just refreshed. These borrow only
        // `self.view.pieces`, disjoint from the backend the scene renders through.
        let leaf_chrome = self.view.pieces.leaf_elements(&leaf_rects);
        let plus = self
            .view
            .pieces
            .plus_elements(&self.view.widgets.plus_regions, self.view.anim.is_some());
        let taskbar = self.view.pieces.taskbar_element();
        let wallpaper = self.view.pieces.wallpaper_element();

        // Scene borrows and the backend borrow are disjoint `Comp` fields,
        // so the backend can consume the scene while borrowed mutably.
        let scene = scene::Scene {
            or_windows: &self.or_windows,
            note_popups: &self.note_popups,
            note_rects: &note_rects,
            float_stack: &self.windows.float_stack,
            managed: &self.managed,
            tiled: &tiled,
            output: &self.output,
            dock_place: &dock_place,
            layer_dock: &layer_dock,
            indexed: &self.view.indexed,
            wallpaper,
            leaf_chrome: &leaf_chrome,
            frame_art: self.view.pieces.frame_art(),
            plus: &plus,
            taskbar,
            focus_outline: &focus_outline,
        };
        match &mut self.backend {
            crate::backend::Backend::Winit(w) => {
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
                    let mut elements = cursor::cursor_elements(
                        renderer,
                        scene.indexed,
                        pointer_loc,
                        &self.cursor_status,
                        &mut self.cursors,
                    );
                    elements.extend(scene::output_elements(renderer, &scene));
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
        // A client cursor surface animates through frame callbacks too.
        if let CursorImageStatus::Surface(surface) = &self.cursor_status {
            smithay::desktop::utils::send_frames_surface_tree(
                surface,
                &output,
                elapsed,
                Some(Duration::ZERO),
                |_, _| Some(output.clone()),
            );
        }
    }
}

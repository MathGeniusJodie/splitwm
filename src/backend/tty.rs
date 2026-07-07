//! Real-session backend: the compositor owns a seat. libseat grants the
//! devices (and revokes them across VT switches), udev finds the GPU and
//! reports connector changes, libinput feeds the seat, and a
//! `DrmOutputManager` scans out GLES-composited frames with real vblank
//! pacing.
//!
//! Master's world is one X screen, so this drives exactly one output: the
//! first connected connector, replaced (never extended) when connectors
//! come and go. Unlike the nested backend there is no host to draw a
//! cursor, so the frame composites one: the focused client's committed
//! cursor surface, else the xcursor theme's arrow.

use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
use smithay::backend::allocator::Fourcc;
use smithay::backend::drm::compositor::FrameFlags;
use smithay::backend::drm::exporter::gbm::GbmFramebufferExporter;
use smithay::backend::drm::output::{DrmOutput, DrmOutputManager, DrmOutputRenderElements};
use smithay::backend::drm::{DrmDevice, DrmDeviceFd, DrmEvent, DrmNode};
use smithay::backend::egl::{EGLContext, EGLDevice, EGLDisplay};
use smithay::backend::libinput::{LibinputInputBackend, LibinputSessionInterface};
use smithay::backend::renderer::element::memory::{
    MemoryRenderBuffer, MemoryRenderBufferRenderElement,
};
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::Color32F;
use smithay::backend::session::libseat::LibSeatSession;
use smithay::backend::session::{Event as SessionEvent, Session as _};
use smithay::backend::udev::{self, UdevBackend, UdevEvent};
use smithay::input::pointer::{CursorImageStatus, CursorImageSurfaceData};
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::EventLoop;
use smithay::reexports::drm::control::{connector, crtc, Device as _, ModeTypeFlags};
use smithay::reexports::input::Libinput;
use smithay::reexports::rustix::fs::OFlags;
use smithay::reexports::wayland_server::Display;
use smithay::utils::{DeviceFd, IsAlive as _, Logical, Point, Transform};

use crate::comp::chrome::{self, OutputElement};
use crate::comp::Comp;

type Allocator = GbmAllocator<DrmDeviceFd>;
type Exporter = GbmFramebufferExporter<DrmDeviceFd>;
type Outputs = DrmOutputManager<Allocator, Exporter, (), DrmDeviceFd>;
type ScanoutOutput = DrmOutput<Allocator, Exporter, (), DrmDeviceFd>;

/// Scanout formats tried in order when creating the primary-plane
/// swapchain; opaque first, since the bottom element covers the output.
const COLOR_FORMATS: [Fourcc; 2] = [Fourcc::Xrgb8888, Fourcc::Argb8888];

/// Shown until a connector is found (the wl output must always have a
/// mode); the first `reconnect` replaces it with the real one.
fn fallback_mode() -> Mode {
    Mode {
        size: (1920, 1080).into(),
        refresh: 60_000,
    }
}

pub struct Tty {
    pub session: LibSeatSession,
    /// The libinput context, held to suspend/resume it across VT switches.
    libinput: Libinput,
    pub renderer: GlesRenderer,
    drm: Outputs,
    /// The connector currently driven, and its scanout surface. `None`
    /// while no connector is connected (screen unplugged): the compositor
    /// keeps running dark, exactly like master under a sleeping monitor.
    connector: Option<connector::Handle>,
    scanout: Option<ScanoutOutput>,
    /// A frame is queued and its vblank hasn't fired yet; rendering now
    /// would just fail with EBUSY.
    queued: bool,
    /// The session lost the devices (VT switched away).
    paused: bool,
    /// Theme arrow for when no client cursor surface applies: the buffer
    /// and its hotspot.
    cursor: Option<(MemoryRenderBuffer, Point<i32, Logical>)>,
}

impl Tty {
    pub fn change_vt(&mut self, vt: i32) {
        if let Err(err) = self.session.change_vt(vt) {
            tracing::error!("change vt: {err}");
        }
    }

    /// Present one frame if the pipe is idle: composite the scene plus the
    /// cursor, and queue it for the next vblank. An unchanged scene queues
    /// nothing — the redraw timer polls until something is dirty again.
    pub fn render(
        &mut self,
        scene: &chrome::Scene<'_>,
        pointer_loc: Point<f64, Logical>,
        cursor_status: &CursorImageStatus,
        clear: Color32F,
    ) {
        if self.paused || self.queued {
            return;
        }
        let Some(out) = self.scanout.as_mut() else {
            return;
        };
        let mut elements = cursor_elements(
            &mut self.renderer,
            pointer_loc,
            cursor_status,
            self.cursor.as_ref(),
        );
        elements.extend(chrome::output_elements(&mut self.renderer, scene));
        match out.render_frame(&mut self.renderer, &elements, clear, FrameFlags::DEFAULT) {
            Ok(res) => {
                if !res.is_empty {
                    match out.queue_frame(()) {
                        Ok(()) => self.queued = true,
                        Err(err) => tracing::error!("queue frame: {err}"),
                    }
                }
            }
            Err(err) => tracing::error!("render frame: {err}"),
        }
    }
}

/// The tty event sources are only ever inserted by [`run`], whose loop
/// state always carries a tty backend.
fn tty(comp: &mut Comp) -> &mut Tty {
    match &mut comp.backend {
        crate::backend::Backend::Tty(t) => t,
        _ => unreachable!("tty event on a non-tty backend"),
    }
}

pub fn run() {
    let mut event_loop: EventLoop<Comp> = EventLoop::try_new().expect("calloop init");
    let display: Display<Comp> = Display::new().expect("wayland display init");
    let dh = display.handle();

    let (session, session_notifier) =
        LibSeatSession::new().expect("libseat session (is seatd or logind running?)");
    let seat_name = session.seat();

    // Input: libinput on the libseat session, suspended/resumed with it.
    let mut libinput =
        Libinput::new_with_udev::<LibinputSessionInterface<LibSeatSession>>(session.clone().into());
    libinput
        .udev_assign_seat(&seat_name)
        .expect("assign libinput seat");

    // The GPU: the seat's primary boot device. Single-GPU by design — the
    // one renderer serves clients and scanout alike.
    let gpu_path = udev::primary_gpu(&seat_name)
        .ok()
        .flatten()
        .or_else(|| {
            udev::all_gpus(&seat_name)
                .ok()
                .and_then(|g| g.into_iter().next())
        })
        .expect("no GPU on this seat");
    let drm_node = DrmNode::from_path(&gpu_path).expect("gpu path is a drm node");

    let mut session_for_open = session.clone();
    let fd = session_for_open
        .open(
            &gpu_path,
            OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
        )
        .expect("open drm device via session");
    let drm_fd = DrmDeviceFd::new(DeviceFd::from(fd));
    let (drm_device, drm_notifier) = DrmDevice::new(drm_fd.clone(), true).expect("init drm device");
    let gbm = GbmDevice::new(drm_fd).expect("init gbm device");

    // SAFETY: the EGL display/context/renderer chain lives inside `Tty`
    // for the whole session and nothing else touches this EGL context.
    let renderer = unsafe {
        let egl_display = EGLDisplay::new(gbm.clone()).expect("egl display on gbm");
        let context = EGLContext::new(&egl_display).expect("egl context");
        GlesRenderer::new(context).expect("gles renderer")
    };
    let render_node = EGLDevice::device_for_display(renderer.egl_context().display())
        .ok()
        .and_then(|device| device.try_get_render_node().ok().flatten());
    let renderer_formats: Vec<_> = renderer
        .egl_context()
        .dmabuf_render_formats()
        .iter()
        .copied()
        .collect();

    let drm = DrmOutputManager::new(
        drm_device,
        GbmAllocator::new(
            gbm.clone(),
            GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
        ),
        GbmFramebufferExporter::new(gbm.clone(), render_node),
        Some(gbm),
        COLOR_FORMATS,
        renderer_formats,
    );

    // The wl output: named for the connector present at startup. When
    // connectors change later only the mode follows; the name stays (one
    // Output for the session, as master had one X screen).
    let first = pick_connector(drm.device());
    let (name, physical, mode) = match &first {
        Some((info, _, mode)) => (
            format!("{}-{}", info.interface().as_str(), info.interface_id()),
            info.size().map_or((0, 0), |s| (s.0 as i32, s.1 as i32)),
            Mode::from(*mode),
        ),
        None => ("tty".to_string(), (0, 0), fallback_mode()),
    };
    let output = Output::new(
        name,
        PhysicalProperties {
            size: physical.into(),
            subpixel: Subpixel::Unknown,
            make: "splitwm".into(),
            model: "tty".into(),
        },
    );
    let _global = output.create_global::<Comp>(&dh);
    output.change_current_state(
        Some(mode),
        Some(Transform::Normal),
        None,
        Some((0, 0).into()),
    );
    output.set_preferred(mode);

    let mut comp = Comp::new(
        &mut event_loop,
        display,
        output,
        super::Backend::Tty(Tty {
            session,
            libinput: libinput.clone(),
            renderer,
            drm,
            connector: None,
            scanout: None,
            queued: false,
            paused: false,
            cursor: load_cursor(),
        }),
    );
    // Bring up scanout on the connector found above.
    reconnect(&mut comp);

    let handle = event_loop.handle();
    handle
        .insert_source(
            LibinputInputBackend::new(libinput),
            |mut event, (), comp| {
                // Devices run libinput defaults except scroll direction:
                // natural scrolling to match Jodie's X session (the X server
                // owned that knob on master).
                if let smithay::backend::input::InputEvent::DeviceAdded { device } = &mut event {
                    if device.config_scroll_has_natural_scroll() {
                        let _ = device.config_scroll_set_natural_scroll_enabled(true);
                    }
                }
                comp.process_input_event(event);
            },
        )
        .expect("insert libinput source");

    // VT switches: libseat pauses the session (devices revoked) and later
    // hands them back; frames queued across the gap are gone.
    handle
        .insert_source(session_notifier, |event, (), comp| match event {
            SessionEvent::PauseSession => {
                let t = tty(comp);
                t.paused = true;
                t.libinput.suspend();
                t.drm.pause();
            }
            SessionEvent::ActivateSession => {
                {
                    let t = tty(comp);
                    t.paused = false;
                    t.queued = false;
                    if t.libinput.resume().is_err() {
                        tracing::error!("resume libinput");
                    }
                    if let Err(err) = t.drm.activate(false) {
                        tracing::error!("activate drm: {err}");
                    }
                    if let Some(out) = &t.scanout {
                        out.reset_buffers();
                    }
                }
                // Releases of chord keys held across the switch (the VT
                // chord itself, at minimum) were lost with the devices;
                // without this the next press of the same key would be
                // swallowed as a "repeat" of a chord still thought held.
                comp.held_bound_keys.clear();
                comp.chrome_dirty = true;
                comp.redraw();
            }
        })
        .expect("insert session source");

    // Vblank: the queued frame is on glass; present the next one.
    handle
        .insert_source(drm_notifier, |event, _metadata, comp| match event {
            DrmEvent::VBlank(_crtc) => {
                {
                    let t = tty(comp);
                    t.queued = false;
                    if let Some(out) = &mut t.scanout {
                        if let Err(err) = out.frame_submitted() {
                            tracing::error!("frame submitted: {err}");
                        }
                    }
                }
                comp.redraw();
            }
            DrmEvent::Error(err) => tracing::error!("drm: {err}"),
        })
        .expect("insert drm source");

    // Connector hotplug on our GPU. Other GPUs are ignored: single-GPU.
    let udev_backend = UdevBackend::new(&seat_name).expect("udev backend");
    let dev_id = drm_node.dev_id();
    handle
        .insert_source(udev_backend, move |event, (), comp| match event {
            UdevEvent::Changed { device_id } if device_id == dev_id => reconnect(comp),
            UdevEvent::Removed { device_id } if device_id == dev_id => {
                tracing::error!("primary GPU removed; output dark until it returns");
                let t = tty(comp);
                t.scanout = None;
                t.connector = None;
            }
            _ => {}
        })
        .expect("insert udev source");

    // Idle pickup: while frames queue, vblanks drive the redraw; when a
    // frame comes up empty nothing is queued and no vblank will fire, so
    // this timer polls until the scene is dirty again (damage tracking in
    // the compositor keeps those probes cheap).
    handle
        .insert_source(Timer::immediate(), |_, (), comp| {
            comp.redraw();
            TimeoutAction::ToDuration(std::time::Duration::from_millis(16))
        })
        .expect("insert redraw timer");

    super::run(event_loop, comp);
}

/// The first connected connector with a CRTC to drive it, and its
/// preferred (else first) mode.
fn pick_connector(
    device: &DrmDevice,
) -> Option<(
    connector::Info,
    crtc::Handle,
    smithay::reexports::drm::control::Mode,
)> {
    let res = device.resource_handles().ok()?;
    for &conn in res.connectors() {
        let Ok(info) = device.get_connector(conn, true) else {
            continue;
        };
        if info.state() != connector::State::Connected {
            continue;
        }
        let Some(&mode) = info
            .modes()
            .iter()
            .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
            .or_else(|| info.modes().first())
        else {
            continue;
        };
        for &enc in info.encoders() {
            let Ok(enc_info) = device.get_encoder(enc) else {
                continue;
            };
            if let Some(&crtc) = res.filter_crtcs(enc_info.possible_crtcs()).first() {
                return Some((info, crtc, mode));
            }
        }
    }
    None
}

/// Point scanout at the first connected connector (initial bringup and
/// every udev change event). A vanished connector goes dark; a new one
/// replaces the old, relayouting to its mode.
fn reconnect(comp: &mut Comp) {
    let output = comp.output.clone();
    let t = tty(comp);
    let Some((info, crtc, mode)) = pick_connector(t.drm.device()) else {
        if t.connector.is_some() {
            tracing::warn!("no connected connector; output dark until replug");
        }
        t.scanout = None;
        t.connector = None;
        return;
    };
    if t.connector == Some(info.handle()) && t.scanout.is_some() {
        return;
    }
    // Drop the old scanout first: its Drop frees the CRTC in the manager.
    t.scanout = None;
    t.queued = false;

    let wl_mode = Mode::from(mode);
    // The DrmOutput tracks the output's mode, so publish it first.
    output.change_current_state(Some(wl_mode), None, None, None);
    output.set_preferred(wl_mode);
    match t.drm.initialize_output::<_, OutputElement>(
        crtc,
        mode,
        &[info.handle()],
        &output,
        None,
        &mut t.renderer,
        &DrmOutputRenderElements::new(),
    ) {
        Ok(out) => {
            tracing::info!(
                "driving {}-{} at {}x{}",
                info.interface().as_str(),
                info.interface_id(),
                wl_mode.size.w,
                wl_mode.size.h
            );
            t.scanout = Some(out);
            t.connector = Some(info.handle());
        }
        Err(err) => {
            tracing::error!("initialize output: {err}");
            t.connector = None;
            return;
        }
    }
    comp.resize_output(wl_mode);
    comp.redraw();
}

/// The composited cursor: the client's committed cursor surface when one
/// applies, else the xcursor theme arrow. Cursor-kind elements let the
/// DRM compositor place them on the hardware cursor plane.
fn cursor_elements(
    renderer: &mut GlesRenderer,
    loc: Point<f64, Logical>,
    status: &CursorImageStatus,
    fallback: Option<&(MemoryRenderBuffer, Point<i32, Logical>)>,
) -> Vec<OutputElement> {
    match status {
        CursorImageStatus::Hidden => Vec::new(),
        CursorImageStatus::Surface(surface) if surface.alive() => {
            let hotspot = smithay::wayland::compositor::with_states(surface, |states| {
                states
                    .data_map
                    .get::<CursorImageSurfaceData>()
                    .map_or_else(Point::default, |data| data.lock().unwrap().hotspot)
            });
            smithay::backend::renderer::element::surface::render_elements_from_surface_tree(
                renderer,
                surface,
                (loc.to_i32_round() - hotspot).to_physical(1),
                1.0,
                1.0,
                Kind::Cursor,
            )
            .into_iter()
            .map(OutputElement::Float)
            .collect()
        }
        // A named shape (or a dead cursor surface): the theme arrow. Shape
        // lookup beyond the arrow is still an open gap (master showed
        // hand/resize cursors over chrome).
        _ => {
            let Some((buf, hotspot)) = fallback else {
                return Vec::new();
            };
            match MemoryRenderBufferRenderElement::from_buffer(
                renderer,
                (loc.x - f64::from(hotspot.x), loc.y - f64::from(hotspot.y)),
                buf,
                None,
                None,
                None,
                Kind::Cursor,
            ) {
                Ok(el) => vec![OutputElement::Chrome(el)],
                Err(err) => {
                    tracing::error!("cursor element: {err}");
                    Vec::new()
                }
            }
        }
    }
}

/// Load the arrow from the xcursor theme (`XCURSOR_THEME`/`XCURSOR_SIZE`,
/// defaults matching every other Wayland compositor). `None` means no
/// theme is installed; the session runs cursorless like a bare console.
fn load_cursor() -> Option<(MemoryRenderBuffer, Point<i32, Logical>)> {
    let size: i32 = std::env::var("XCURSOR_SIZE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(24);
    let theme = std::env::var("XCURSOR_THEME").unwrap_or_else(|_| "default".into());
    let theme = xcursor::CursorTheme::load(&theme);
    let path = theme.load_icon("left_ptr")?;
    let data = std::fs::read(path).ok()?;
    let images = xcursor::parser::parse_xcursor(&data)?;
    let img = images
        .into_iter()
        .min_by_key(|i| (i.size as i32 - size).abs())?;
    let buffer = MemoryRenderBuffer::from_slice(
        &img.pixels_rgba,
        Fourcc::Abgr8888,
        (img.width as i32, img.height as i32),
        1,
        Transform::Normal,
        None,
    );
    Some((buffer, (img.xhot as i32, img.yhot as i32).into()))
}

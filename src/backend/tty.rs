//! Real-session backend: the compositor owns a seat. libseat grants the
//! devices (and revokes them across VT switches), udev finds the GPU and
//! reports connector changes, libinput feeds the seat, and a
//! `DrmOutputManager` scans out GLES-composited frames with real vblank
//! pacing.
//!
//! Drives up to two connected monitors in mirror mode (showing the same
//! session screen across both connectors), dynamically selecting a shared
//! native mode when available. Unlike the nested backend there is no host
//! to draw a cursor, so the frame composites one: the focused client's
//! committed cursor surface, else the named shape's image (see
//! `comp::cursor`).

use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
use smithay::backend::allocator::Fourcc;
use smithay::backend::drm::compositor::FrameFlags;
use smithay::backend::drm::exporter::gbm::GbmFramebufferExporter;
use smithay::backend::drm::output::{DrmOutput, DrmOutputManager, DrmOutputRenderElements};
use smithay::backend::drm::{DrmDevice, DrmDeviceFd, DrmEvent, DrmNode};
use smithay::backend::egl::{EGLContext, EGLDevice, EGLDisplay};
use smithay::backend::libinput::{LibinputInputBackend, LibinputSessionInterface};
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::session::libseat::LibSeatSession;
use smithay::backend::session::{Event as SessionEvent, Session as _};
use smithay::backend::udev::{self, UdevBackend, UdevEvent};
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::EventLoop;
use smithay::reexports::drm::control::{connector, crtc, Device as _, ModeTypeFlags};
use smithay::reexports::input::{ClickMethod, Libinput};
use smithay::reexports::rustix::fs::OFlags;
use smithay::reexports::wayland_server::Display;
use smithay::utils::{DeviceFd, Transform};

use crate::comp::cursor::cursor_elements;
use crate::comp::scene::{self, OutputElement};
use crate::comp::Comp;

type Allocator = GbmAllocator<DrmDeviceFd>;
type Exporter = GbmFramebufferExporter<DrmDeviceFd>;
type Outputs = DrmOutputManager<Allocator, Exporter, (), DrmDeviceFd>;
type ScanoutOutput = DrmOutput<Allocator, Exporter, (), DrmDeviceFd>;

/// An active physical output scanning out frames.
struct ActiveOutput {
    name: String,
    connector: connector::Handle,
    crtc: crtc::Handle,
    mode: smithay::reexports::drm::control::Mode,
    scanout: ScanoutOutput,
    queued: bool,
}

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
    /// The connectors currently driven, and their scanout surfaces. Empty
    /// while no connector is connected (screen unplugged): the compositor
    /// keeps running dark, exactly like master under a sleeping monitor.
    /// In dual-monitor setups, both outputs mirror the same session screen.
    outputs: Vec<ActiveOutput>,
    /// The session lost the devices (VT switched away).
    paused: bool,
}

impl Tty {
    pub fn change_vt(&mut self, vt: i32) {
        if let Err(err) = self.session.change_vt(vt) {
            tracing::error!("change vt: {err}");
        }
    }

    /// See [`crate::backend::Backend::redraw_paced_externally`].
    pub fn redraw_paced_externally(&self) -> bool {
        self.paused || (!self.outputs.is_empty() && self.outputs.iter().all(|o| o.queued))
    }

    /// Present one frame if any pipe is idle: composite the scene plus the
    /// cursor, and queue it for the next vblank on each unqueued output. An
    /// unchanged scene queues nothing — the next queued redraw picks up when
    /// something is dirty again.
    pub fn render(&mut self, frame: super::Frame<'_>) {
        if self.paused || self.outputs.is_empty() {
            return;
        }
        if self.outputs.iter().all(|o| o.queued) {
            return;
        }
        let mut elements = cursor_elements(
            &mut self.renderer,
            frame.scene.indexed,
            frame.pointer_loc,
            frame.cursor,
            frame.cursors,
        );
        elements.extend(scene::output_elements(&mut self.renderer, frame.scene));
        for out in &mut self.outputs {
            if out.queued {
                continue;
            }
            match out.scanout.render_frame(
                &mut self.renderer,
                &elements,
                frame.clear,
                FrameFlags::DEFAULT,
            ) {
                Ok(res) => {
                    if !res.is_empty {
                        match out.scanout.queue_frame(()) {
                            Ok(()) => out.queued = true,
                            Err(err) => tracing::error!("queue frame on {}: {err}", out.name),
                        }
                    }
                }
                Err(err) => tracing::error!("render frame on {}: {err}", out.name),
            }
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

    // The wl output: named for the primary connector present at startup.
    // When connectors change later only the mode follows; the name stays (one
    // Output for the session, as master had one X screen).
    let targets = pick_connected_targets(drm.device());
    let (name, physical, mode) = match targets.first() {
        Some(target) => (
            format!(
                "{}-{}",
                target.info.interface().as_str(),
                target.info.interface_id()
            ),
            target.info.size().map_or((0, 0), |s| (s.0 as i32, s.1 as i32)),
            Mode::from(target.mode),
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
            outputs: Vec::new(),
            paused: false,
        }),
    );
    // Bring up scanout on the connector(s) found above.
    reconnect(&mut comp);

    let handle = event_loop.handle();
    handle
        .insert_source(
            LibinputInputBackend::new(libinput),
            |mut event, (), comp| {
                // Devices run libinput defaults except: natural scrolling to
                // match Jodie's X session (the X server owned that knob on
                // master), and touchpads get tap-to-click plus clickfinger,
                // so a two-finger tap or press is a right click.
                if let smithay::backend::input::InputEvent::DeviceAdded { device } = &mut event {
                    if device.config_scroll_has_natural_scroll() {
                        let _ = device.config_scroll_set_natural_scroll_enabled(true);
                    }
                    if device.config_tap_finger_count() > 0 {
                        let _ = device.config_tap_set_enabled(true);
                    }
                    if device
                        .config_click_methods()
                        .contains(&ClickMethod::Clickfinger)
                    {
                        let _ = device.config_click_set_method(ClickMethod::Clickfinger);
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
                    for out in &mut t.outputs {
                        out.queued = false;
                        out.scanout.reset_buffers();
                    }
                    if t.libinput.resume().is_err() {
                        tracing::error!("resume libinput");
                    }
                    if let Err(err) = t.drm.activate(false) {
                        tracing::error!("activate drm: {err}");
                    }
                }
                // Releases of chord keys held across the switch (the VT
                // chord itself, at minimum) were lost with the devices;
                // without this the next press of the same key would be
                // swallowed as a "repeat" of a chord still thought held.
                comp.interaction.held_bound_keys.clear();
                // Re-activating the DRM device can lose the GL textures, so
                // drop every cached chrome piece for a full re-upload.
                comp.invalidate_chrome();
                comp.redraw();
            }
        })
        .expect("insert session source");

    // Vblank: a queued frame is on glass; submit and present the next one.
    handle
        .insert_source(drm_notifier, |event, _metadata, comp| match event {
            DrmEvent::VBlank(crtc) => {
                {
                    let t = tty(comp);
                    if let Some(out) = t.outputs.iter_mut().find(|o| o.crtc == crtc) {
                        out.queued = false;
                        if let Err(err) = out.scanout.frame_submitted() {
                            tracing::error!("frame submitted on {}: {err}", out.name);
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
                t.outputs.clear();
            }
            _ => {}
        })
        .expect("insert udev source");

    super::run(event_loop, comp);
}

struct OutputTarget {
    info: connector::Info,
    crtc: crtc::Handle,
    mode: smithay::reexports::drm::control::Mode,
}

/// Negotiate modes for two mirrored displays. Prefers common native resolution with highest
/// refresh rate, falling back to each display's preferred mode if no shared resolution exists.
pub fn pick_mirror_modes(
    modes1: &[smithay::reexports::drm::control::Mode],
    modes2: &[smithay::reexports::drm::control::Mode],
) -> (
    smithay::reexports::drm::control::Mode,
    smithay::reexports::drm::control::Mode,
) {
    assert!(!modes1.is_empty() && !modes2.is_empty());

    let pick_best_for_res = |modes: &[smithay::reexports::drm::control::Mode], res: (u16, u16)| {
        modes
            .iter()
            .filter(|m| m.size() == res)
            .max_by_key(|m| {
                (
                    m.mode_type().contains(ModeTypeFlags::PREFERRED),
                    m.vrefresh(),
                )
            })
            .copied()
            .unwrap()
    };

    let pref1 = modes1
        .iter()
        .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
        .copied()
        .unwrap_or_else(|| {
            *modes1
                .iter()
                .max_by_key(|m| (u32::from(m.size().0) * u32::from(m.size().1), m.vrefresh()))
                .unwrap()
        });
    let pref2 = modes2
        .iter()
        .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
        .copied()
        .unwrap_or_else(|| {
            *modes2
                .iter()
                .max_by_key(|m| (u32::from(m.size().0) * u32::from(m.size().1), m.vrefresh()))
                .unwrap()
        });

    if pref1.size() == pref2.size() {
        return (
            pick_best_for_res(modes1, pref1.size()),
            pick_best_for_res(modes2, pref2.size()),
        );
    }

    let mut common_resolutions: Vec<(u16, u16)> = Vec::new();
    for m1 in modes1 {
        let size1 = m1.size();
        if !common_resolutions.contains(&size1) && modes2.iter().any(|m2| m2.size() == size1) {
            common_resolutions.push(size1);
        }
    }

    if !common_resolutions.is_empty() {
        let chosen_res = if common_resolutions.contains(&pref1.size()) {
            pref1.size()
        } else if common_resolutions.contains(&pref2.size()) {
            pref2.size()
        } else {
            *common_resolutions
                .iter()
                .max_by_key(|s| (u32::from(s.0) * u32::from(s.1), s.0))
                .unwrap()
        };

        return (
            pick_best_for_res(modes1, chosen_res),
            pick_best_for_res(modes2, chosen_res),
        );
    }

    (pref1, pref2)
}

/// Finds connected connectors (up to 2 for mirroring) with distinct CRTCs to drive them,
/// and negotiates their modes (sharing a native mode when possible).
fn pick_connected_targets(device: &DrmDevice) -> Vec<OutputTarget> {
    let Ok(res) = device.resource_handles() else {
        return Vec::new();
    };

    let mut connected: Vec<connector::Info> = Vec::new();
    for &conn in res.connectors() {
        let Ok(info) = device.get_connector(conn, true) else {
            continue;
        };
        if info.state() == connector::State::Connected && !info.modes().is_empty() {
            connected.push(info);
        }
    }

    if connected.is_empty() {
        return Vec::new();
    }

    // Limit to dual monitor (at most 2 connectors) for mirror mode.
    if connected.len() > 2 {
        connected.truncate(2);
    }

    // Assign distinct CRTCs to each connector.
    let mut candidates_per_conn: Vec<Vec<crtc::Handle>> = Vec::new();
    for info in &connected {
        let mut crtcs = Vec::new();
        for &enc in info.encoders() {
            let Ok(enc_info) = device.get_encoder(enc) else {
                continue;
            };
            for crtc in res.filter_crtcs(enc_info.possible_crtcs()) {
                if !crtcs.contains(&crtc) {
                    crtcs.push(crtc);
                }
            }
        }
        candidates_per_conn.push(crtcs);
    }

    fn find_crtc_assignment(
        idx: usize,
        candidates: &[Vec<crtc::Handle>],
        used: &mut Vec<crtc::Handle>,
    ) -> bool {
        if idx == candidates.len() {
            return true;
        }
        for &crtc in &candidates[idx] {
            if !used.contains(&crtc) {
                used.push(crtc);
                if find_crtc_assignment(idx + 1, candidates, used) {
                    return true;
                }
                used.pop();
            }
        }
        false
    }

    let mut assigned_crtcs = Vec::new();
    if !find_crtc_assignment(0, &candidates_per_conn, &mut assigned_crtcs) {
        // Fall back to driving just the first connector if CRTCs cannot be found for both
        assigned_crtcs.clear();
        candidates_per_conn.truncate(1);
        connected.truncate(1);
        if !find_crtc_assignment(0, &candidates_per_conn, &mut assigned_crtcs) {
            return Vec::new();
        }
    }

    if connected.len() == 1 {
        let info = connected.into_iter().next().unwrap();
        let mode = info
            .modes()
            .iter()
            .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
            .copied()
            .unwrap_or(info.modes()[0]);
        vec![OutputTarget {
            info,
            crtc: assigned_crtcs[0],
            mode,
        }]
    } else {
        let (mode1, mode2) = pick_mirror_modes(connected[0].modes(), connected[1].modes());
        let mut it = connected.into_iter();
        let conn1 = it.next().unwrap();
        let conn2 = it.next().unwrap();
        vec![
            OutputTarget {
                info: conn1,
                crtc: assigned_crtcs[0],
                mode: mode1,
            },
            OutputTarget {
                info: conn2,
                crtc: assigned_crtcs[1],
                mode: mode2,
            },
        ]
    }
}

/// Point scanout at the connected connector(s), supporting dual-monitor mirroring.
/// A vanished connector goes dark; a new one joins or replaces, relayouting to the mode.
fn reconnect(comp: &mut Comp) {
    let wl_output = comp.output.handle().clone();
    let targets = pick_connected_targets(tty(comp).drm.device());
    if targets.is_empty() {
        let t = tty(comp);
        if !t.outputs.is_empty() {
            tracing::warn!("no connected connectors; output dark until replug");
        }
        t.outputs.clear();
        return;
    }

    let t = tty(comp);
    let unchanged = t.outputs.len() == targets.len()
        && t.outputs.iter().zip(&targets).all(|(out, target)| {
            out.connector == target.info.handle()
                && out.crtc == target.crtc
                && out.mode == target.mode
        });
    if unchanged {
        return;
    }

    // Drop all old scanouts first: their Drop frees the CRTCs in the manager.
    t.outputs.clear();

    let wl_mode = Mode::from(targets[0].mode);
    comp.output.change_mode(wl_mode);

    let t = tty(comp);
    for target in targets {
        let name = format!(
            "{}-{}",
            target.info.interface().as_str(),
            target.info.interface_id()
        );
        let mode_source: smithay::output::OutputModeSource =
            if target.mode.size() == (wl_mode.size.w as u16, wl_mode.size.h as u16) {
                smithay::output::OutputModeSource::from(&wl_output)
            } else {
                smithay::output::OutputModeSource::Static {
                    size: (target.mode.size().0 as i32, target.mode.size().1 as i32).into(),
                    scale: 1.0.into(),
                    transform: Transform::Normal,
                }
            };
        match t.drm.initialize_output::<_, OutputElement>(
            target.crtc,
            target.mode,
            &[target.info.handle()],
            mode_source,
            None,
            &mut t.renderer,
            &DrmOutputRenderElements::new(),
        ) {
            Ok(out) => {
                tracing::info!(
                    "driving {} at {}x{} (CRTC {:?})",
                    name,
                    target.mode.size().0,
                    target.mode.size().1,
                    target.crtc
                );
                t.outputs.push(ActiveOutput {
                    name,
                    connector: target.info.handle(),
                    crtc: target.crtc,
                    mode: target.mode,
                    scanout: out,
                    queued: false,
                });
            }
            Err(err) => {
                tracing::error!("initialize output for {}: {err}", name);
            }
        }
    }

    if !t.outputs.is_empty() {
        if t.outputs.len() > 1 {
            tracing::info!(
                "dual monitor mirror active: {} outputs synchronized at {}x{}",
                t.outputs.len(),
                wl_mode.size.w,
                wl_mode.size.h
            );
        }
        comp.resize_output(wl_mode);
        comp.redraw();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[repr(C)]
    struct RawModeInfo {
        clock: u32,
        hdisplay: u16,
        hsync_start: u16,
        hsync_end: u16,
        htotal: u16,
        hskew: u16,
        vdisplay: u16,
        vsync_start: u16,
        vsync_end: u16,
        vtotal: u16,
        vscan: u16,
        vrefresh: u32,
        flags: u32,
        type_: u32,
        name: [u8; 32],
    }

    fn make_test_mode(w: u16, h: u16, vrefresh: u32, preferred: bool) -> smithay::reexports::drm::control::Mode {
        let raw = RawModeInfo {
            clock: 0,
            hdisplay: w,
            hsync_start: 0,
            hsync_end: 0,
            htotal: 0,
            hskew: 0,
            vdisplay: h,
            vsync_start: 0,
            vsync_end: 0,
            vtotal: 0,
            vscan: 0,
            vrefresh,
            flags: 0,
            type_: if preferred { ModeTypeFlags::PREFERRED.bits() } else { 0 },
            name: [0; 32],
        };
        assert_eq!(
            std::mem::size_of::<RawModeInfo>(),
            std::mem::size_of::<smithay::reexports::drm::control::Mode>()
        );
        unsafe { std::mem::transmute(raw) }
    }

    #[test]
    fn test_pick_mirror_modes_identical_preferred() {
        let m1 = vec![
            make_test_mode(1920, 1080, 60, true),
            make_test_mode(1280, 720, 60, false),
        ];
        let m2 = vec![
            make_test_mode(1920, 1080, 60, true),
            make_test_mode(1280, 720, 60, false),
        ];
        let (out1, out2) = pick_mirror_modes(&m1, &m2);
        assert_eq!(out1.size(), (1920, 1080));
        assert_eq!(out2.size(), (1920, 1080));
        assert_eq!(out1.vrefresh(), 60);
        assert_eq!(out2.vrefresh(), 60);
    }

    #[test]
    fn test_pick_mirror_modes_common_resolution_found() {
        // Monitor 1 prefers 1080p, Monitor 2 prefers 4K but supports 1080p.
        let m1 = vec![
            make_test_mode(1920, 1080, 60, true),
            make_test_mode(1280, 720, 60, false),
        ];
        let m2 = vec![
            make_test_mode(3840, 2160, 60, true),
            make_test_mode(1920, 1080, 60, false),
            make_test_mode(1280, 720, 60, false),
        ];
        let (out1, out2) = pick_mirror_modes(&m1, &m2);
        assert_eq!(out1.size(), (1920, 1080));
        assert_eq!(out2.size(), (1920, 1080));
    }

    #[test]
    fn test_pick_mirror_modes_highest_refresh_selected() {
        // Monitor 1 supports 1080p@60 and 1080p@144. Monitor 2 supports 1080p@60.
        let m1 = vec![
            make_test_mode(1920, 1080, 60, false),
            make_test_mode(1920, 1080, 144, false),
        ];
        let m2 = vec![
            make_test_mode(1920, 1080, 60, false),
        ];
        let (out1, out2) = pick_mirror_modes(&m1, &m2);
        assert_eq!(out1.size(), (1920, 1080));
        assert_eq!(out2.size(), (1920, 1080));
        assert_eq!(out1.vrefresh(), 144);
        assert_eq!(out2.vrefresh(), 60);
    }

    #[test]
    fn test_pick_mirror_modes_disjoint_fallback() {
        // No common resolutions; each falls back to its preferred mode.
        let m1 = vec![make_test_mode(1920, 1080, 60, true)];
        let m2 = vec![make_test_mode(1366, 768, 60, true)];
        let (out1, out2) = pick_mirror_modes(&m1, &m2);
        assert_eq!(out1.size(), (1920, 1080));
        assert_eq!(out2.size(), (1366, 768));
    }
}

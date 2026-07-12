//! Headless harness backend: the compositor composites into an offscreen
//! GLES renderbuffer on a surfaceless EGL context — no window, no seat, no
//! visible output. Socket integration tests and screenshot drives select it
//! with `SPLITWM_HEADLESS=1`; interaction arrives over the debug channel
//! (`SPLITWM_DEBUG_CHANNEL=1`, see `comp::debug`), which is also where
//! `shot` requests land in `pending_shot`.

use std::io::Write as _;

use smithay::backend::allocator::Fourcc;
use smithay::backend::egl::native::EGLSurfacelessDisplay;
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::gles::{GlesRenderbuffer, GlesRenderer};
use smithay::backend::renderer::{Bind as _, Color32F, ExportMem as _, Offscreen as _};
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::EventLoop;
use smithay::reexports::wayland_server::Display;
use smithay::utils::{Buffer as BufferCoord, Rectangle, Size, Transform};

use crate::comp::scene;
use crate::comp::Comp;

/// Fixed output size: headless runs never resize, and the drive scripts'
/// screenshots share one geometry (the X11 drive.sh used the same).
const SIZE: (i32, i32) = (1280, 800);

pub struct Headless {
    pub renderer: GlesRenderer,
    /// The offscreen frame every redraw composites into; `shot` reads it
    /// back.
    buffer: GlesRenderbuffer,
    damage_tracker: OutputDamageTracker,
    /// A `shot <path>` from the debug channel, fulfilled (and acked on
    /// stdout) right after the next composited frame.
    pub pending_shot: Option<String>,
}

impl Headless {
    /// Composite one frame, then serve any pending screenshot from it.
    pub fn render(&mut self, scene: &scene::Scene<'_>, clear: Color32F) {
        let elements = scene::output_elements(&mut self.renderer, scene);
        let mut fb = match self.renderer.bind(&mut self.buffer) {
            Ok(fb) => fb,
            Err(err) => {
                tracing::error!("bind: {err}");
                return;
            }
        };
        if let Err(err) =
            self.damage_tracker
                .render_output(&mut self.renderer, &mut fb, 0, &elements, clear)
        {
            tracing::error!("render: {err:?}");
            return;
        }
        if let Some(path) = self.pending_shot.take() {
            // Both outcomes ack on stdout: a driver blocked on the ack must
            // never hang on a failed readback or encode.
            match shot(&mut self.renderer, &fb, &path) {
                Ok(()) => println!("ok shot {path}"),
                Err(err) => println!("err shot {path}: {err}"),
            }
        }
    }
}

/// Read the composited frame back and encode it to `path` via ImageMagick
/// (the same shell-out family `pixel_graphics::magick_decode_rgba` uses to
/// decode; the format is inferred from the file extension).
fn shot(
    renderer: &mut GlesRenderer,
    fb: &<GlesRenderer as smithay::backend::renderer::RendererSuper>::Framebuffer<'_>,
    path: &str,
) -> Result<(), String> {
    let size = Size::<i32, BufferCoord>::from(SIZE);
    let mapping = renderer
        .copy_framebuffer(fb, Rectangle::from_size(size), Fourcc::Abgr8888)
        .map_err(|err| format!("copy framebuffer: {err}"))?;
    let bytes = renderer
        .map_texture(&mapping)
        .map_err(|err| format!("map: {err}"))?;

    let mut last_err = String::from("no encoder ran");
    for prog in ["magick", "convert"] {
        // Rows arrive top-down: the renderer draws offscreen targets
        // y-inverted, which exactly cancels glReadPixels' bottom-up order.
        let child = std::process::Command::new(prog)
            .args([
                "-depth",
                "8",
                "-size",
                &format!("{}x{}", SIZE.0, SIZE.1),
                "rgba:-",
                path,
            ])
            .stdin(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn();
        let mut child = match child {
            Ok(child) => child,
            Err(err) => {
                last_err = format!("{prog}: {err}");
                continue;
            }
        };
        child
            .stdin
            .take()
            .expect("piped stdin")
            .write_all(bytes)
            .map_err(|err| format!("{prog} stdin: {err}"))?;
        let out = child
            .wait_with_output()
            .map_err(|err| format!("{prog}: {err}"))?;
        if out.status.success() {
            return Ok(());
        }
        last_err = format!("{prog}: {}", String::from_utf8_lossy(&out.stderr));
    }
    Err(last_err)
}

pub fn run() {
    let mut event_loop: EventLoop<Comp> = EventLoop::try_new().expect("calloop init");
    let display: Display<Comp> = Display::new().expect("wayland display init");

    // SAFETY: the EGL display/context/renderer chain lives inside
    // `Headless` for the whole session and nothing else touches this EGL
    // context (same containment as the tty backend).
    let mut renderer = unsafe {
        let egl_display = EGLDisplay::new(EGLSurfacelessDisplay).expect("surfaceless egl display");
        let context = EGLContext::new(&egl_display).expect("egl context");
        GlesRenderer::new(context).expect("gles renderer")
    };
    let buffer: GlesRenderbuffer = renderer
        .create_buffer(Fourcc::Abgr8888, SIZE.into())
        .expect("offscreen renderbuffer");

    let output = Output::new(
        "headless".to_string(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "splitwm".into(),
            model: "headless".into(),
        },
    );
    let _global = output.create_global::<Comp>(&display.handle());
    let mode = Mode {
        size: SIZE.into(),
        refresh: 60_000,
    };
    output.change_current_state(
        Some(mode),
        Some(Transform::Normal),
        None,
        Some((0, 0).into()),
    );
    output.set_preferred(mode);
    let damage_tracker = OutputDamageTracker::from_output(&output);

    let comp = Comp::new(
        &mut event_loop,
        display,
        output,
        super::Backend::Headless(Headless {
            renderer,
            buffer,
            damage_tracker,
            pending_shot: None,
        }),
    );

    super::run(event_loop, comp);
}

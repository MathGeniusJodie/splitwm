//! Nested development backend: the compositor lives inside a winit window
//! on the host desktop. Redraws are queue-driven (`Comp::queue_redraw`);
//! there is no vblank clock. The compositor draws every pointer itself with
//! its hand-drawn sprites, so the host window's own cursor stays hidden
//! and each frame composites the sprite, exactly as the tty backend does.

use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::winit::{self, WinitEvent, WinitGraphicsBackend};
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::EventLoop;
use smithay::reexports::wayland_server::Display;
use smithay::utils::{Physical, Rectangle, Transform};

use crate::comp::Comp;
use crate::comp::{self, scene};

use super::Frame;

pub struct Winit {
    pub backend: WinitGraphicsBackend<GlesRenderer>,
    pub damage_tracker: OutputDamageTracker,
}

impl Winit {
    /// Composite the scene plus the composited cursor sprite into the
    /// nested window (the host cursor stays hidden over our surface) and
    /// submit. Redraws are queue-driven; there is no vblank clock.
    pub fn present(&mut self, frame: Frame<'_>) {
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
            let mut elements = comp::cursor::cursor_elements(
                renderer,
                frame.scene.indexed,
                frame.pointer_loc,
                frame.cursor,
                frame.cursors,
            );
            elements.extend(scene::output_elements(renderer, frame.scene));
            self.damage_tracker
                .render_output(renderer, &mut fb, 0, &elements, frame.clear)
                .inspect_err(|err| tracing::error!("render: {err:?}"))
                .is_ok()
        };
        if rendered {
            if let Err(err) = self.backend.submit(Some(&[full])) {
                tracing::error!("submit: {err}");
            }
        }
    }
}

pub fn run() {
    let mut event_loop: EventLoop<Comp> = EventLoop::try_new().expect("calloop init");
    let display: Display<Comp> = Display::new().expect("wayland display init");
    let (backend, winit) = winit::init::<GlesRenderer>().expect("winit backend init");
    // The compositor composites its own cursor sprite into every frame, so
    // the host window's pointer stays hidden over our surface.
    backend.window().set_cursor_visible(false);

    let output = Output::new(
        "winit".to_string(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "splitwm".into(),
            model: "winit".into(),
        },
    );
    let _global = output.create_global::<Comp>(&display.handle());
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

    let comp = Comp::new(
        &mut event_loop,
        display,
        output,
        super::Backend::Winit(Winit {
            backend,
            damage_tracker,
        }),
    );

    event_loop
        .handle()
        .insert_source(winit, |event, (), comp| match event {
            WinitEvent::Resized { size, .. } => {
                comp.resize_output(Mode {
                    size,
                    refresh: 60_000,
                });
                comp.redraw();
            }
            WinitEvent::Redraw => comp.redraw(),
            WinitEvent::CloseRequested => comp.signal.stop(),
            WinitEvent::Input(event) => comp.process_input_event(event),
            WinitEvent::Focus(_) => {}
        })
        .expect("insert winit source");

    super::run(event_loop, comp);
}

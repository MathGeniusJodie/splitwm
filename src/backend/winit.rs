//! Nested development backend: the compositor lives inside a winit window
//! on the host desktop. The host session provides the clock (a 60 Hz timer
//! stands in for vblank). The compositor draws every pointer itself with
//! its hand-drawn sprites, so the host window's own cursor stays hidden
//! and each frame composites the sprite, exactly as the tty backend does.

use std::time::Duration;

use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::winit::{self, WinitEvent, WinitGraphicsBackend};
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::EventLoop;
use smithay::reexports::wayland_server::Display;
use smithay::utils::Transform;

use crate::comp::Comp;

pub struct Winit {
    pub backend: WinitGraphicsBackend<GlesRenderer>,
    pub damage_tracker: OutputDamageTracker,
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

    // ~60 Hz repaint pacing. The winit backend has no vblank clock, so a
    // timer stands in; damage tracking inside redraw keeps idle frames
    // cheap.
    event_loop
        .handle()
        .insert_source(Timer::immediate(), |_, (), comp| {
            comp.redraw();
            TimeoutAction::ToDuration(Duration::from_millis(16))
        })
        .expect("insert redraw timer");

    super::run(event_loop, comp);
}

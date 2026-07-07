//! splitwm — a terminal-multiplexer-style tiling Wayland compositor.
//!
//! M1 (see PORT.md): the protocol core. Clients connect over a private
//! socket, xdg toplevels map full-output, keyboard and pointer input is
//! forwarded to focus. Everything runs event-driven on calloop; the winit
//! event loop is itself a calloop source. The X11 splitwm on master is the
//! behavioral spec for everything that comes next.

#[allow(dead_code)]
mod assets;
mod comp;
// oklch is consumed by the icon pipeline in M8; the remaining allows cover
// the layout core's scroll/boundary/taskbar surface until M3/M5 wire the
// chrome renderer and pointer interactions up to it.
#[allow(dead_code)]
mod icon;
#[allow(dead_code)]
mod oklch;
#[allow(dead_code)]
mod render;
mod shell;
#[allow(dead_code)]
mod state;
mod widgets;
#[allow(dead_code)]
mod theme;
#[allow(dead_code)]
mod tree;

/// A `pixel-graphics` palette index, threaded through as the accent-colour
/// representation for splits so border rendering can palette-swap them.
pub type Index = pixel_graphics::Index;

use std::time::Duration;

use comp::Comp;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::Color32F;
use smithay::backend::winit::{self, WinitEvent};
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::EventLoop;
use smithay::reexports::wayland_server::Display;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let mut event_loop: EventLoop<Comp> = EventLoop::try_new().expect("calloop init");
    let display: Display<Comp> = Display::new().expect("wayland display init");
    let (backend, winit) = winit::init::<GlesRenderer>().expect("winit backend init");

    // na16 index 1 = gunmetal, the gap background; named constants arrive
    // with the theme port (M2).
    let g = assets::palette().color(1);
    let clear = Color32F::new(
        f32::from(g.r) / 255.0,
        f32::from(g.g) / 255.0,
        f32::from(g.b) / 255.0,
        1.0,
    );

    let mut comp = Comp::new(&mut event_loop, display, backend, clear);

    event_loop
        .handle()
        .insert_source(winit, |event, (), comp| match event {
            WinitEvent::Resized { size, .. } => {
                comp.resize_output(size);
                comp.redraw();
            }
            WinitEvent::Redraw => comp.redraw(),
            WinitEvent::CloseRequested => comp.signal.stop(),
            WinitEvent::Input(event) => comp.process_input_event(event),
            WinitEvent::Focus(_) => {}
        })
        .expect("insert winit source");

    // ~60 Hz repaint pacing. The winit backend has no vblank clock, so a
    // timer stands in; damage tracking inside render_output keeps idle
    // frames cheap. The DRM backend (M9) replaces this with real vblanks.
    event_loop
        .handle()
        .insert_source(Timer::immediate(), |_, (), comp| {
            comp.redraw();
            TimeoutAction::ToDuration(Duration::from_millis(16))
        })
        .expect("insert redraw timer");

    // Children spawned by the compositor (terminal, launcher, quick-launch)
    // inherit the session; nested test runs read it from stdout.
    std::env::set_var("WAYLAND_DISPLAY", &comp.socket_name);
    println!("WAYLAND_DISPLAY={}", comp.socket_name.to_string_lossy());

    event_loop
        .run(None, &mut comp, |comp| {
            comp.space.refresh();
            comp.popups.cleanup();
            comp.dh.flush_clients().expect("flush clients");
        })
        .expect("event loop run");
}

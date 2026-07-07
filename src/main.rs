//! splitwm — a terminal-multiplexer-style tiling Wayland compositor.
//!
//! M0 scaffold (see PORT.md): the winit backend opens a nested window,
//! frames clear to na16 gunmetal through the GLES renderer, and everything
//! runs event-driven on calloop — the winit event loop is itself a calloop
//! source, so there is no polling and nothing blocks. The X11 splitwm on
//! master is the behavioral spec for everything that comes next.

#[allow(dead_code)]
mod assets;

use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::{Color32F, Frame as _, Renderer as _};
use smithay::backend::winit::{self, WinitEvent, WinitGraphicsBackend};
use smithay::reexports::calloop::{EventLoop, LoopSignal};
use smithay::utils::{Physical, Rectangle, Size, Transform};

struct App {
    backend: WinitGraphicsBackend<GlesRenderer>,
    signal: LoopSignal,
    /// Frame clear colour, resolved once from the baked na16 palette.
    clear: Color32F,
}

impl App {
    /// Clear the whole window to the background colour. Only runs when the
    /// backend reports damage (resize / expose); a static frame needs no
    /// continuous redraw loop.
    fn redraw(&mut self) {
        let size: Size<i32, Physical> = self.backend.window_size();
        let damage = Rectangle::from_size(size);
        let submit = {
            let Ok((renderer, mut fb)) = self
                .backend
                .bind()
                .inspect_err(|err| tracing::error!("bind: {err}"))
            else {
                return;
            };
            renderer
                .render(&mut fb, size, Transform::Flipped180)
                .and_then(|mut frame| {
                    frame.clear(self.clear, &[damage])?;
                    frame.finish().map(drop)
                })
                .inspect_err(|err| tracing::error!("render: {err}"))
                .is_ok()
        };
        if submit {
            if let Err(err) = self.backend.submit(Some(&[damage])) {
                tracing::error!("submit: {err}");
            }
        }
    }
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

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

    let mut event_loop: EventLoop<App> = EventLoop::try_new().expect("calloop init");
    let mut app = App {
        backend,
        signal: event_loop.get_signal(),
        clear,
    };

    event_loop
        .handle()
        .insert_source(winit, |event, (), app| match event {
            WinitEvent::Resized { .. } | WinitEvent::Redraw => app.redraw(),
            WinitEvent::CloseRequested => app.signal.stop(),
            WinitEvent::Input(_) | WinitEvent::Focus(_) => {}
        })
        .expect("insert winit source");

    // First frame: winit only reports damage after something changes, so
    // paint once before entering the loop.
    app.redraw();

    event_loop
        .run(None, &mut app, |_| {})
        .expect("event loop run");
}

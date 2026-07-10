//! splitwm — a terminal-multiplexer-style tiling Wayland compositor.
//!
//! Runs nested (winit backend) when launched from an existing session, or
//! on a bare VT (DRM/libinput/libseat, behind the `tty` cargo feature)
//! when it is the session. The X11 splitwm on master is the behavioral
//! spec; PORT.md tracks the port and its approved deviations.

#[allow(dead_code)]
mod assets;
mod backend;
mod backlight;
mod comp;
mod icon;
mod launch;
#[allow(dead_code)]
mod layout;
mod notify;
mod oklch;
#[allow(dead_code)]
mod render;
mod shell;
#[allow(dead_code)]
mod state;
#[allow(dead_code)]
mod theme;
mod widgets;

/// A `pixel-graphics` palette index, threaded through as the accent-colour
/// representation for splits so border rendering can palette-swap them.
pub type Index = pixel_graphics::Index;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    // The harness runs offscreen regardless of any host display; otherwise,
    // inside a session (Wayland or X11) run nested, and on a bare VT own
    // the seat. Same heuristic X11 tools use for "am I on a display".
    let nested =
        std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some();
    if std::env::var_os("SPLITWM_HEADLESS").is_some_and(|v| v != "0") {
        tracing::info!("SPLITWM_HEADLESS: compositing offscreen for the harness");
        backend::headless::run();
    } else if nested {
        tracing::info!("display detected: running nested on the winit backend");
        backend::winit::run();
    } else {
        #[cfg(feature = "tty")]
        {
            tracing::info!("no display: taking the seat via DRM/libinput");
            backend::tty::run();
        }
        #[cfg(not(feature = "tty"))]
        panic!("no display to nest in, and this build lacks the `tty` feature");
    }
}

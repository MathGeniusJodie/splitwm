//! Presentation backends. `winit` hosts nested development sessions inside
//! an existing desktop; `tty` (feature-gated) owns a real seat via
//! DRM/GBM/libinput/libseat. Everything protocol- and layout-side lives in
//! `Comp`, which only reaches the backend through this enum: to borrow the
//! GLES renderer, to present a frame from `Comp::redraw`, and to switch VTs.

pub mod winit;

#[cfg(feature = "tty")]
pub mod tty;

use smithay::backend::renderer::gles::GlesRenderer;

use crate::comp::Comp;

pub enum Backend {
    Winit(winit::Winit),
    #[cfg(feature = "tty")]
    Tty(tty::Tty),
}

impl Backend {
    /// The one GLES renderer of this session (both backends render on a
    /// single GPU), for dmabuf imports and frame composition.
    pub fn renderer(&mut self) -> &mut GlesRenderer {
        match self {
            Backend::Winit(w) => w.backend.renderer(),
            #[cfg(feature = "tty")]
            Backend::Tty(t) => &mut t.renderer,
        }
    }

    /// Ctrl+Alt+Fn. Session-level rather than a WM binding: on master the
    /// X server owned VT switching, so here the tty session does; a nested
    /// session has no VT to switch (the host's server owns the console).
    pub fn change_vt(&mut self, vt: i32) {
        match self {
            Backend::Winit(_) => {
                let _ = vt;
            }
            #[cfg(feature = "tty")]
            Backend::Tty(t) => t.change_vt(vt),
        }
    }
}

/// The backend-independent tail of a session: XWayland, the launch probe,
/// exporting the socket, and parking in the event loop until SIGTERM.
fn run(mut event_loop: smithay::reexports::calloop::EventLoop<'static, Comp>, mut comp: Comp) {
    // X11 clients (rofi, legacy apps) arrive via XWayland; DISPLAY is set
    // once the server reports Ready.
    comp.start_xwayland();

    // Warm the deadline-bounded systemd-run probe at startup so the first
    // launch never pays for it inside the event loop.
    crate::launch::have_systemd_run();

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

//! Presentation backends. `winit` hosts nested development sessions inside
//! an existing desktop; `tty` (feature-gated) owns a real seat via
//! DRM/GBM/libinput/libseat; `headless` composites offscreen for the test
//! harness. Everything protocol- and layout-side lives in `Comp`, which
//! only reaches the backend through this enum: to borrow the GLES renderer,
//! to present a frame from `Comp::redraw`, and to switch VTs.

pub mod headless;
pub mod winit;

#[cfg(feature = "tty")]
pub mod tty;

use smithay::backend::renderer::gles::GlesRenderer;

use crate::comp::Comp;

pub enum Backend {
    Winit(winit::Winit),
    Headless(headless::Headless),
    #[cfg(feature = "tty")]
    Tty(tty::Tty),
}

impl Backend {
    /// The one GLES renderer of this session (every backend renders on a
    /// single GPU), for dmabuf imports and frame composition.
    pub fn renderer(&mut self) -> &mut GlesRenderer {
        match self {
            Backend::Winit(w) => w.backend.renderer(),
            Backend::Headless(h) => &mut h.renderer,
            #[cfg(feature = "tty")]
            Backend::Tty(t) => &mut t.renderer,
        }
    }

    /// Queue a debug-channel screenshot of the next composited frame.
    /// Only the headless backend can read its frame back; the caller
    /// reports failure on the other backends.
    pub fn request_shot(&mut self, path: &str) -> bool {
        match self {
            Backend::Headless(h) => {
                h.pending_shot = Some(path.to_string());
                true
            }
            _ => false,
        }
    }

    /// Whether a coming redraw is already guaranteed without queueing one:
    /// the tty backend redraws when its in-flight frame's vblank lands, and
    /// a paused session redraws on resume (rendering is impossible until
    /// then anyway). The winit and headless backends have no external
    /// pacer, so their redraws are always queue-driven.
    pub fn redraw_paced_externally(&self) -> bool {
        match self {
            Backend::Winit(_) | Backend::Headless(_) => false,
            #[cfg(feature = "tty")]
            Backend::Tty(t) => t.redraw_paced_externally(),
        }
    }

    /// Ctrl+Alt+Fn. Session-level rather than a WM binding: on master the
    /// X server owned VT switching, so here the tty session does; a nested
    /// session has no VT to switch (the host's server owns the console).
    pub fn change_vt(&mut self, vt: i32) {
        match self {
            Backend::Winit(_) | Backend::Headless(_) => {
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
    // X11 clients (rofi, legacy apps) arrive via XWayland; spawned children
    // get its DISPLAY injected once the server reports Ready (see
    // `launch::spawn` — the process env itself is never mutated, other
    // threads read it concurrently).
    comp.start_xwayland();

    // Harness drivers speak a line protocol over stdin (see comp::debug);
    // opt-in so a stray line on a normal session's stdin can't act.
    if std::env::var_os("SPLITWM_DEBUG_CHANNEL").is_some_and(|v| v != "0") {
        crate::comp::debug::insert_channel(&event_loop.handle());
    }

    // Warm the deadline-bounded systemd-run probe at startup so the first
    // launch never pays for it inside the event loop.
    crate::launch::have_systemd_run();

    // Children spawned by the compositor (terminal, launcher, quick-launch)
    // get the session socket injected; nested test runs read it from stdout.
    crate::launch::set_wayland_display(comp.socket_name.clone());
    println!("WAYLAND_DISPLAY={}", comp.socket_name.to_string_lossy());

    // First frame; after this, redraws happen only when something queues
    // one (commits, input, channel messages) or a tty vblank lands.
    comp.redraw();

    event_loop
        .run(None, &mut comp, |comp| {
            comp.space.refresh();
            comp.popups.cleanup();
            comp.dh.flush_clients().expect("flush clients");
        })
        .expect("event loop run");
}

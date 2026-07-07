//! Nested development backend: the compositor lives inside a winit window
//! on the host desktop. The host session provides the clock (a 60 Hz timer
//! stands in for vblank) and draws the hardware cursor: named shapes map
//! onto the host cursor (approximate glyphs, zero latency); only a
//! client-committed cursor surface is composited into the frame instead.

use std::time::Duration;

use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::winit::{self, WinitEvent, WinitGraphicsBackend};
use smithay::input::pointer::{CursorIcon, CursorImageStatus};
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::EventLoop;
use smithay::reexports::wayland_server::Display;
use smithay::utils::{IsAlive as _, Transform};

use crate::comp::Comp;

/// What the host window's cursor was last set to, so redraw only issues
/// winit cursor calls on change.
#[derive(Clone, Copy, PartialEq, Eq)]
enum HostCursor {
    Icon(CursorIcon),
    Hidden,
}

pub struct Winit {
    pub backend: WinitGraphicsBackend<GlesRenderer>,
    pub damage_tracker: OutputDamageTracker,
    host_cursor: Option<HostCursor>,
}

impl Winit {
    /// Reflect the seat's cursor on the host window. Returns `true` when
    /// the frame must composite the cursor itself (a client-committed
    /// cursor surface, which has no host analog).
    pub fn apply_cursor(&mut self, status: &CursorImageStatus) -> bool {
        let (host, composite) = match status {
            CursorImageStatus::Surface(s) if s.alive() => (HostCursor::Hidden, true),
            CursorImageStatus::Surface(_) => (HostCursor::Icon(CursorIcon::Default), false),
            CursorImageStatus::Named(icon) => (HostCursor::Icon(*icon), false),
            CursorImageStatus::Hidden => (HostCursor::Hidden, false),
        };
        if self.host_cursor != Some(host) {
            let window = self.backend.window();
            match host {
                HostCursor::Icon(icon) => {
                    // smithay and winit share the cursor-icon crate, so the
                    // seat's CursorIcon is winit's own.
                    window.set_cursor(smithay::reexports::winit::window::Cursor::Icon(icon));
                    window.set_cursor_visible(true);
                }
                HostCursor::Hidden => window.set_cursor_visible(false),
            }
            self.host_cursor = Some(host);
        }
        composite
    }
}

pub fn run() {
    let mut event_loop: EventLoop<Comp> = EventLoop::try_new().expect("calloop init");
    let display: Display<Comp> = Display::new().expect("wayland display init");
    let (backend, winit) = winit::init::<GlesRenderer>().expect("winit backend init");

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
            host_cursor: None,
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

//! Forwarding backend input (winit or libinput) to the seat: keyboard
//! chords are intercepted for `theme::BINDINGS` before clients see them,
//! pointer buttons route through the chrome hit-test and click-to-focus,
//! and horizontal scroll/three-finger swipes pan the canvas.

use smithay::backend::input::{
    AbsolutePositionEvent as _, Axis, AxisSource, ButtonState, Event as _, GestureBeginEvent as _,
    GestureSwipeUpdateEvent as _, InputBackend, InputEvent, KeyState, KeyboardKeyEvent as _,
    PointerAxisEvent as _, PointerButtonEvent as _, PointerMotionEvent as _,
};
use smithay::input::keyboard::FilterResult;
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent};
use smithay::utils::{Logical, Point, SERIAL_COUNTER};

use super::Comp;

/// What an intercepted key press asks of the compositor: a WM action from
/// `theme::BINDINGS`, or a VT switch (session-level, tty backend only —
/// the X server owned these chords on master).
enum Intercepted {
    Action(crate::theme::Action),
    SwitchVt(i32),
}

/// Ctrl+Alt+Fn arrives from xkb as one dedicated keysym per VT.
fn vt_switch_target(sym: u32) -> Option<i32> {
    const FIRST: u32 = crate::theme::ks::XF86_Switch_VT_1;
    const LAST: u32 = crate::theme::ks::XF86_Switch_VT_12;
    (FIRST..=LAST)
        .contains(&sym)
        .then(|| (sym - FIRST + 1) as i32)
}

impl Comp {
    pub fn process_input_event<I: InputBackend>(&mut self, event: InputEvent<I>) {
        match event {
            InputEvent::Keyboard { event } => {
                let serial = SERIAL_COUNTER.next_serial();
                let time = event.time_msec();
                let key_state = event.state();
                let code = event.key_code();
                let keyboard = self.keyboard.clone();
                // Bindings are matched on the level-0 keysym plus an exact
                // modifier mask, before the client sees anything. A chord we
                // intercept owns the whole key cycle: its auto-repeats (a
                // nested winit session repeats; libinput doesn't) and its
                // release are swallowed too, so clients never see half a
                // press.
                let action = keyboard
                    .input::<Option<Intercepted>, _>(
                        self,
                        code,
                        key_state,
                        serial,
                        time,
                        |comp, mods, handle| {
                            let raw = code.raw();
                            match key_state {
                                KeyState::Pressed => {
                                    let sym = handle.raw_syms().first().map_or(0, |s| s.raw());
                                    // The VT syms only exist on the
                                    // ctrl+alt-modified level; bindings
                                    // keep matching level-0 syms + mods.
                                    let intercepted = vt_switch_target(handle.modified_sym().raw())
                                        .map(Intercepted::SwitchVt)
                                        .or_else(|| {
                                            crate::comp::actions::binding_action(mods, sym)
                                                .map(Intercepted::Action)
                                        });
                                    let Some(intercepted) = intercepted else {
                                        return FilterResult::Forward;
                                    };
                                    if comp.interaction.held_bound_keys.contains(&raw) {
                                        // Auto-repeat of a held chord.
                                        return FilterResult::Intercept(None);
                                    }
                                    comp.interaction.held_bound_keys.push(raw);
                                    FilterResult::Intercept(Some(intercepted))
                                }
                                KeyState::Released => {
                                    if let Some(idx) = comp
                                        .interaction
                                        .held_bound_keys
                                        .iter()
                                        .position(|&k| k == raw)
                                    {
                                        comp.interaction.held_bound_keys.swap_remove(idx);
                                        FilterResult::Intercept(None)
                                    } else {
                                        FilterResult::Forward
                                    }
                                }
                            }
                        },
                    )
                    .flatten();
                match action {
                    Some(Intercepted::Action(action)) => self.do_action(action),
                    Some(Intercepted::SwitchVt(vt)) => self.backend.change_vt(vt),
                    None => {}
                }
            }
            InputEvent::PointerMotionAbsolute { event } => {
                let output_geo = self
                    .space
                    .output_geometry(&self.output)
                    .expect("output is mapped");
                let pos = event.position_transformed(output_geo.size) + output_geo.loc.to_f64();
                self.pointer_moved(pos, event.time_msec());
            }
            // Relative motion (libinput mice/touchpads on the tty backend;
            // winit only ever reports absolute). The compositor owns the
            // cursor position: integrate and clamp to the output.
            InputEvent::PointerMotion { event } => {
                let output_geo = self
                    .space
                    .output_geometry(&self.output)
                    .expect("output is mapped");
                let mut pos = self.pointer.current_location() + event.delta();
                pos.x = pos.x.clamp(
                    f64::from(output_geo.loc.x),
                    f64::from(output_geo.loc.x + output_geo.size.w) - 1.0,
                );
                pos.y = pos.y.clamp(
                    f64::from(output_geo.loc.y),
                    f64::from(output_geo.loc.y + output_geo.size.h) - 1.0,
                );
                self.pointer_moved(pos, event.time_msec());
            }
            InputEvent::PointerButton { event } => {
                self.pointer_button(event.button_code(), event.state(), event.time_msec());
            }
            InputEvent::PointerAxis { event } => {
                let pointer = self.pointer.clone();
                let horizontal = event.amount(Axis::Horizontal).unwrap_or_else(|| {
                    event.amount_v120(Axis::Horizontal).unwrap_or(0.0) * 15.0 / 120.0
                });
                let vertical = event.amount(Axis::Vertical).unwrap_or_else(|| {
                    event.amount_v120(Axis::Vertical).unwrap_or(0.0) * 15.0 / 120.0
                });

                // Horizontal swipes pan the canvas (always over chrome,
                // Mod4-gated over a client); a panned swipe is consumed,
                // never also delivered to the client underneath.
                let over_client = self.tiled_under(pointer.current_location()).is_some();
                let mut panned = false;
                if horizontal != 0.0 && self.hscroll_allowed(over_client) {
                    // Wheel-click units: discrete steps when the device
                    // reports them, else continuous units at libinput's
                    // ~15/click scale.
                    let clicks = event
                        .amount_v120(Axis::Horizontal)
                        .map_or(horizontal / 15.0, |v| v / 120.0);
                    self.apply_hscroll(clicks);
                    panned = true;
                }

                let mut frame = AxisFrame::new(event.time_msec()).source(event.source());
                let mut any = false;
                if horizontal != 0.0 && !panned {
                    frame = frame.value(Axis::Horizontal, horizontal);
                    any = true;
                }
                if vertical != 0.0 {
                    frame = frame.value(Axis::Vertical, vertical);
                    any = true;
                }
                if event.source() == AxisSource::Finger {
                    if event.amount(Axis::Horizontal) == Some(0.0) {
                        frame = frame.stop(Axis::Horizontal);
                        any = true;
                    }
                    if event.amount(Axis::Vertical) == Some(0.0) {
                        frame = frame.stop(Axis::Vertical);
                        any = true;
                    }
                }
                if any {
                    pointer.axis(self, frame);
                    pointer.frame(self);
                }
            }
            // Touchpad swipe gestures (libinput only; winit never emits
            // them). The pointer-gestures protocol isn't advertised, so no
            // client can claim these: a three-finger horizontal swipe pans
            // the canvas everywhere, no Mod4 needed. Other finger counts
            // stay swallowed.
            InputEvent::GestureSwipeBegin { event } => {
                self.interaction.swipe_pan = event.fingers() == 3;
            }
            InputEvent::GestureSwipeUpdate { event } => {
                if self.interaction.swipe_pan {
                    // Same wheel-click conversion as continuous finger
                    // scroll: ~15 axis units per click.
                    self.apply_hscroll(-event.delta_x() / 15.0);
                }
            }
            InputEvent::GestureSwipeEnd { .. } => self.interaction.swipe_pan = false,
            _ => {}
        }
    }

    /// A pointer button at the current pointer position: chrome hit-tests,
    /// click-to-focus, and forwarding. One path for every source — winit,
    /// libinput, and the harness's debug channel.
    pub fn pointer_button(&mut self, button: u32, state: ButtonState, time_msec: u32) {
        const BTN_LEFT: u32 = 0x110;
        const BTN_RIGHT: u32 = 0x111;
        let pointer = self.pointer.clone();
        let serial = SERIAL_COUNTER.next_serial();
        let pos = pointer.current_location();

        let mut consumed = false;
        if state == ButtonState::Released {
            // A split-move drag drops here (see `Comp::end_drag`).
            self.end_drag(pos);
            // The release of a press we consumed must not leak to a
            // client that never saw the press.
            consumed = std::mem::take(&mut self.interaction.chrome_press);
        } else if !pointer.is_grabbed() {
            // Any click on a notification bubble dismisses it,
            // before everything else (they render topmost).
            if self.dismiss_note_at(pos) {
                self.interaction.chrome_press = true;
                consumed = true;
            } else
            // Float chrome frames overlap client surfaces beneath
            // them; they win the press outright.
            if button == BTN_LEFT && self.float_frame_at(pos).is_some() {
                self.on_chrome_button(pos, false);
                self.interaction.chrome_press = true;
                consumed = true;
            } else {
                let under = self.surface_under(pos).map(|(s, _)| s);
                // A surface that maps to no managed window is an
                // override-redirect X11 window (rofi): the click
                // must forward to it, not hit-test the chrome
                // underneath.
                let over_unmanaged = under.is_some();
                let clicked = under.as_ref().and_then(|s| self.managed.win_for_surface(s));
                match clicked {
                    Some(win) if button == BTN_LEFT => {
                        match self.managed.kind_of(win) {
                            // Click-to-focus through the layout, like
                            // master's activate_client.
                            Some(crate::shell::Kind::Tiled) => {
                                self.clear_focused_float();
                                self.state.activate_client(win);
                                self.arrange();
                            }
                            Some(crate::shell::Kind::Float(_)) => self.focus_float(win),
                            // The dock holds the keyboard until the next
                            // deliberate focus move.
                            Some(crate::shell::Kind::Dock(_)) => self.focus_override(win),
                            None => {}
                        }
                    }
                    Some(_) => {}
                    // Chrome click: hit-test dispatch (buttons, tiles,
                    // handles, "+", titles, float frames...). Only
                    // where no surface is under the pointer at all.
                    None if !over_unmanaged && matches!(button, BTN_LEFT | BTN_RIGHT) => {
                        if self.on_chrome_button(pos, button == BTN_RIGHT) {
                            self.interaction.chrome_press = true;
                            consumed = true;
                        }
                    }
                    // Click on an o-r window: re-grant it the keyboard
                    // before forwarding the button — its X-side grab only
                    // works while XWayland holds our focus, and a click
                    // elsewhere may have moved the focus off it. A layer
                    // surface that accepts keyboard focus (OnDemand
                    // panels; an Exclusive one already holds it) gets the
                    // keyboard by click too, like the dock.
                    None => {
                        let target = under.as_ref().and_then(|s| {
                            if let Some(o) = self
                                .or_windows
                                .iter()
                                .find(|o| o.surface.wl_surface().as_ref() == Some(s))
                            {
                                return Some(crate::comp::focus::FocusTarget::X11(
                                    o.surface.clone(),
                                ));
                            }
                            let focusable = smithay::desktop::layer_map_for_output(&self.output)
                                .layer_for_surface(s, smithay::desktop::WindowSurfaceType::ALL)
                                .is_some_and(|l| l.can_receive_keyboard_focus());
                            focusable.then(|| s.clone().into())
                        });
                        if let Some(target) = target {
                            let keyboard = self.keyboard.clone();
                            keyboard.set_focus(self, Some(target), serial);
                        }
                    }
                }
            }
        }

        if !consumed {
            pointer.button(
                self,
                &ButtonEvent {
                    button,
                    state,
                    serial,
                    time: time_msec,
                },
            );
            pointer.frame(self);
        }
    }

    /// Deliver a pointer position to the seat, shared by the absolute
    /// (winit) and relative (libinput) motion paths — and the harness's
    /// debug channel.
    pub fn pointer_moved(&mut self, pos: Point<f64, Logical>, time_msec: u32) {
        let serial = SERIAL_COUNTER.next_serial();
        // An active gap/edge drag consumes motion: the pointer still moves
        // (for the drag math) but no client gets enter/motion until the
        // button releases.
        let under = if self.on_drag_motion(pos) {
            None
        } else {
            self.surface_under(pos)
        };
        // Off every surface, the cursor is the compositor's: hover
        // feedback over the chrome (master's hover_cursor), and during a
        // drag the gesture's own shape wherever the pointer strays.
        if under.is_none() {
            use smithay::input::pointer::CursorIcon;
            let icon = match self.interaction.drag {
                Some(crate::comp::pointer::ActiveDrag::Gap(d)) => match d.at.dir() {
                    crate::layout::Dir::V => CursorIcon::NsResize,
                    crate::layout::Dir::H => CursorIcon::EwResize,
                },
                Some(crate::comp::pointer::ActiveDrag::Edge(_))
                | Some(crate::comp::pointer::ActiveDrag::Border(_)) => CursorIcon::EwResize,
                Some(crate::comp::pointer::ActiveDrag::Float(_)) => CursorIcon::Pointer,
                // An armed-but-unmoved titlebar/tile press still reads as a
                // click; only real travel shows the grab.
                Some(crate::comp::pointer::ActiveDrag::Move(
                    crate::comp::pointer::MoveDrag::Active { .. },
                )) => CursorIcon::Grabbing,
                Some(crate::comp::pointer::ActiveDrag::Move(
                    crate::comp::pointer::MoveDrag::Armed { .. },
                )) => self.hover_cursor(pos),
                None => self.hover_cursor(pos),
            };
            self.cursor_status = smithay::input::pointer::CursorImageStatus::Named(icon);
        }
        let pointer = self.pointer.clone();
        pointer.motion(
            self,
            under.map(|(s, loc)| (s.into(), loc)),
            &MotionEvent {
                location: pos,
                serial,
                time: time_msec,
            },
        );
        pointer.frame(self);
    }
}

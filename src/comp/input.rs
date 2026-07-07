//! Forwarding winit input to the seat. M1 keeps this deliberately dumb:
//! keys go to the keyboard focus, clicks focus-and-raise the window under
//! the pointer. Bindings, drags, and scroll physics land in M4/M5.

use smithay::backend::input::{
    AbsolutePositionEvent as _, Axis, AxisSource, ButtonState, Event as _, InputBackend,
    InputEvent, KeyState, KeyboardKeyEvent as _, PointerAxisEvent as _, PointerButtonEvent as _,
};
use smithay::input::keyboard::FilterResult;
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent};
use smithay::utils::SERIAL_COUNTER;

use super::Comp;

impl Comp {
    pub fn process_input_event<I: InputBackend>(&mut self, event: InputEvent<I>) {
        match event {
            InputEvent::Keyboard { event } => {
                let serial = SERIAL_COUNTER.next_serial();
                let time = event.time_msec();
                let key_state = event.state();
                let code = event.key_code();
                let keyboard = self.seat.get_keyboard().expect("seat has a keyboard");
                // Bindings are matched on the level-0 keysym plus an exact
                // modifier mask, before the client sees anything. A chord we
                // intercept owns the whole key cycle: its auto-repeats (a
                // nested winit session repeats; libinput doesn't) and its
                // release are swallowed too, so clients never see half a
                // press.
                let action = keyboard
                    .input::<Option<crate::theme::Action>, _>(
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
                                    let Some(action) =
                                        crate::comp::actions::binding_action(mods, sym)
                                    else {
                                        return FilterResult::Forward;
                                    };
                                    if comp.held_bound_keys.contains(&raw) {
                                        // Auto-repeat of a held chord.
                                        return FilterResult::Intercept(None);
                                    }
                                    comp.held_bound_keys.push(raw);
                                    FilterResult::Intercept(Some(action))
                                }
                                KeyState::Released => {
                                    if let Some(idx) =
                                        comp.held_bound_keys.iter().position(|&k| k == raw)
                                    {
                                        comp.held_bound_keys.swap_remove(idx);
                                        FilterResult::Intercept(None)
                                    } else {
                                        FilterResult::Forward
                                    }
                                }
                            }
                        },
                    )
                    .flatten();
                if let Some(action) = action {
                    self.do_action(action);
                }
            }
            InputEvent::PointerMotionAbsolute { event } => {
                let output_geo = self
                    .space
                    .output_geometry(&self.output)
                    .expect("output is mapped");
                let pos = event.position_transformed(output_geo.size) + output_geo.loc.to_f64();
                let serial = SERIAL_COUNTER.next_serial();
                // An active gap/edge drag consumes motion: the pointer
                // still moves (for the drag math) but no client gets
                // enter/motion until the button releases.
                let under = if self.on_drag_motion(pos) {
                    None
                } else {
                    self.surface_under(pos)
                };
                let pointer = self.seat.get_pointer().expect("seat has a pointer");
                pointer.motion(
                    self,
                    under,
                    &MotionEvent {
                        location: pos,
                        serial,
                        time: event.time_msec(),
                    },
                );
                pointer.frame(self);
            }
            InputEvent::PointerButton { event } => {
                const BTN_LEFT: u32 = 0x110;
                const BTN_RIGHT: u32 = 0x111;
                let pointer = self.seat.get_pointer().expect("seat has a pointer");
                let serial = SERIAL_COUNTER.next_serial();
                let pos = pointer.current_location();

                if event.state() == ButtonState::Released {
                    self.end_drag();
                } else if !pointer.is_grabbed() {
                    let clicked = self
                        .space
                        .element_under(pos)
                        .map(|(w, _)| w.clone())
                        .and_then(|w| self.managed.win_for_window(&w));
                    match clicked {
                        // Click-to-focus on a client window (button 1),
                        // through the layout like master's activate_client.
                        Some(win) => {
                            if event.button_code() == BTN_LEFT {
                                self.state.activate_client(win);
                                self.arrange();
                            }
                        }
                        // Chrome click: hit-test dispatch (buttons, tiles,
                        // handles, "+", titles...).
                        None if matches!(event.button_code(), BTN_LEFT | BTN_RIGHT) => {
                            self.on_chrome_button(pos, event.button_code() == BTN_RIGHT);
                        }
                        None => {}
                    }
                }

                pointer.button(
                    self,
                    &ButtonEvent {
                        button: event.button_code(),
                        state: event.state(),
                        serial,
                        time: event.time_msec(),
                    },
                );
                pointer.frame(self);
            }
            InputEvent::PointerAxis { event } => {
                let pointer = self.seat.get_pointer().expect("seat has a pointer");
                let horizontal = event.amount(Axis::Horizontal).unwrap_or_else(|| {
                    event.amount_v120(Axis::Horizontal).unwrap_or(0.0) * 15.0 / 120.0
                });
                let vertical = event.amount(Axis::Vertical).unwrap_or_else(|| {
                    event.amount_v120(Axis::Vertical).unwrap_or(0.0) * 15.0 / 120.0
                });

                // Horizontal swipes pan the canvas (always over chrome,
                // Mod4-gated over a client); a panned swipe is consumed,
                // never also delivered to the client underneath.
                let over_client = self
                    .space
                    .element_under(pointer.current_location())
                    .is_some();
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
            _ => {}
        }
    }
}

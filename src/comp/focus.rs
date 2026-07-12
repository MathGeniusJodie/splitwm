//! The seat's focus target type. An X11 window must be focused *as its
//! `X11Surface`*, not as its bare `wl_surface`: smithay's `KeyboardTarget`
//! impl on `X11Surface` is what performs the X-side focus handover
//! (`SetInputFocus` / `WM_TAKE_FOCUS` per the window's ICCCM input mode) —
//! rootless XWayland leaves that entirely to the window manager. A client
//! whose toplevel never holds X input focus misbehaves in ways that look
//! nothing like a focus bug: Chromium, for one, dismisses its context
//! menus the instant it notices its browser window isn't focused.

use std::borrow::Cow;

use smithay::backend::input::KeyState;
use smithay::desktop::{PopupKind, Window};
use smithay::input::keyboard::{KeyboardTarget, KeysymHandle, ModifiersState};
use smithay::input::pointer::{
    AxisFrame, ButtonEvent, GestureHoldBeginEvent, GestureHoldEndEvent, GesturePinchBeginEvent,
    GesturePinchEndEvent, GesturePinchUpdateEvent, GestureSwipeBeginEvent, GestureSwipeEndEvent,
    GestureSwipeUpdateEvent, MotionEvent, PointerTarget, RelativeMotionEvent,
};
use smithay::input::Seat;
use smithay::reexports::wayland_server::backend::ObjectId;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{IsAlive, Serial};
use smithay::wayland::seat::WaylandFocus;
use smithay::xwayland::X11Surface;

use super::Comp;

/// What the seat's keyboard and pointer aim at: a plain Wayland surface
/// (toplevels, layer surfaces, popups) or an X11 window.
#[derive(Debug, Clone, PartialEq)]
pub enum FocusTarget {
    Surface(WlSurface),
    X11(X11Surface),
}

impl FocusTarget {
    /// A window's focus target, whichever backend it speaks.
    pub fn from_window(window: &Window) -> Option<Self> {
        if let Some(x11) = window.x11_surface() {
            return Some(Self::X11(x11.clone()));
        }
        WaylandFocus::wl_surface(window).map(|s| Self::Surface(s.into_owned()))
    }
}

impl From<WlSurface> for FocusTarget {
    fn from(surface: WlSurface) -> Self {
        Self::Surface(surface)
    }
}

impl From<X11Surface> for FocusTarget {
    fn from(surface: X11Surface) -> Self {
        Self::X11(surface)
    }
}

/// `PopupKeyboardGrab` hands the grabbed popup back through this.
impl From<PopupKind> for FocusTarget {
    fn from(popup: PopupKind) -> Self {
        Self::Surface(popup.wl_surface().clone())
    }
}

impl IsAlive for FocusTarget {
    fn alive(&self) -> bool {
        match self {
            Self::Surface(s) => s.alive(),
            Self::X11(s) => s.alive(),
        }
    }
}

impl WaylandFocus for FocusTarget {
    fn wl_surface(&self) -> Option<Cow<'_, WlSurface>> {
        match self {
            Self::Surface(s) => Some(Cow::Borrowed(s)),
            Self::X11(s) => s.wl_surface().map(Cow::Owned),
        }
    }

    fn same_client_as(&self, object_id: &ObjectId) -> bool {
        match self {
            Self::Surface(s) => s.same_client_as(object_id),
            Self::X11(s) => s.same_client_as(object_id),
        }
    }
}

/// Forward a target-trait method to whichever variant holds the target.
macro_rules! forward {
    ($self:ident, $trait:ident :: $method:ident ( $($arg:expr),* )) => {
        match $self {
            FocusTarget::Surface(s) => $trait::<Comp>::$method(s, $($arg),*),
            FocusTarget::X11(s) => $trait::<Comp>::$method(s, $($arg),*),
        }
    };
}

impl KeyboardTarget<Comp> for FocusTarget {
    fn enter(
        &self,
        seat: &Seat<Comp>,
        data: &mut Comp,
        keys: Vec<KeysymHandle<'_>>,
        serial: Serial,
    ) {
        forward!(self, KeyboardTarget::enter(seat, data, keys, serial))
    }
    fn leave(&self, seat: &Seat<Comp>, data: &mut Comp, serial: Serial) {
        forward!(self, KeyboardTarget::leave(seat, data, serial))
    }
    fn key(
        &self,
        seat: &Seat<Comp>,
        data: &mut Comp,
        key: KeysymHandle<'_>,
        state: KeyState,
        serial: Serial,
        time: u32,
    ) {
        forward!(
            self,
            KeyboardTarget::key(seat, data, key, state, serial, time)
        )
    }
    fn modifiers(
        &self,
        seat: &Seat<Comp>,
        data: &mut Comp,
        modifiers: ModifiersState,
        serial: Serial,
    ) {
        forward!(
            self,
            KeyboardTarget::modifiers(seat, data, modifiers, serial)
        )
    }
}

impl PointerTarget<Comp> for FocusTarget {
    fn enter(&self, seat: &Seat<Comp>, data: &mut Comp, event: &MotionEvent) {
        forward!(self, PointerTarget::enter(seat, data, event))
    }
    fn motion(&self, seat: &Seat<Comp>, data: &mut Comp, event: &MotionEvent) {
        forward!(self, PointerTarget::motion(seat, data, event))
    }
    fn relative_motion(&self, seat: &Seat<Comp>, data: &mut Comp, event: &RelativeMotionEvent) {
        forward!(self, PointerTarget::relative_motion(seat, data, event))
    }
    fn button(&self, seat: &Seat<Comp>, data: &mut Comp, event: &ButtonEvent) {
        forward!(self, PointerTarget::button(seat, data, event))
    }
    fn axis(&self, seat: &Seat<Comp>, data: &mut Comp, frame: AxisFrame) {
        forward!(self, PointerTarget::axis(seat, data, frame))
    }
    fn frame(&self, seat: &Seat<Comp>, data: &mut Comp) {
        forward!(self, PointerTarget::frame(seat, data))
    }
    fn gesture_swipe_begin(
        &self,
        seat: &Seat<Comp>,
        data: &mut Comp,
        event: &GestureSwipeBeginEvent,
    ) {
        forward!(self, PointerTarget::gesture_swipe_begin(seat, data, event))
    }
    fn gesture_swipe_update(
        &self,
        seat: &Seat<Comp>,
        data: &mut Comp,
        event: &GestureSwipeUpdateEvent,
    ) {
        forward!(self, PointerTarget::gesture_swipe_update(seat, data, event))
    }
    fn gesture_swipe_end(&self, seat: &Seat<Comp>, data: &mut Comp, event: &GestureSwipeEndEvent) {
        forward!(self, PointerTarget::gesture_swipe_end(seat, data, event))
    }
    fn gesture_pinch_begin(
        &self,
        seat: &Seat<Comp>,
        data: &mut Comp,
        event: &GesturePinchBeginEvent,
    ) {
        forward!(self, PointerTarget::gesture_pinch_begin(seat, data, event))
    }
    fn gesture_pinch_update(
        &self,
        seat: &Seat<Comp>,
        data: &mut Comp,
        event: &GesturePinchUpdateEvent,
    ) {
        forward!(self, PointerTarget::gesture_pinch_update(seat, data, event))
    }
    fn gesture_pinch_end(&self, seat: &Seat<Comp>, data: &mut Comp, event: &GesturePinchEndEvent) {
        forward!(self, PointerTarget::gesture_pinch_end(seat, data, event))
    }
    fn gesture_hold_begin(
        &self,
        seat: &Seat<Comp>,
        data: &mut Comp,
        event: &GestureHoldBeginEvent,
    ) {
        forward!(self, PointerTarget::gesture_hold_begin(seat, data, event))
    }
    fn gesture_hold_end(&self, seat: &Seat<Comp>, data: &mut Comp, event: &GestureHoldEndEvent) {
        forward!(self, PointerTarget::gesture_hold_end(seat, data, event))
    }
    fn leave(&self, seat: &Seat<Comp>, data: &mut Comp, serial: Serial, time: u32) {
        forward!(self, PointerTarget::leave(seat, data, serial, time))
    }
}

//! The seat's focus target type.
//!
//! # Why this type exists
//!
//! [`PopupManager::grab_popup`](smithay::desktop::PopupManager::grab_popup)
//! requires the seat's keyboard focus type to satisfy
//! `From<PopupKind> + WaylandFocus`. Using `WlSurface` directly cannot work:
//! both `PopupKind` and `WlSurface` are foreign types, so `impl From<PopupKind>
//! for WlSurface` is forbidden by the orphan rule. A local type is the only way
//! to provide that conversion, and without it there is no popup grab — which
//! means menus that only dismiss when the compositor takes a grab stay open.
//!
//! # Why a newtype rather than an enum
//!
//! Compositors that manage several kinds of focusable thing — X11 windows, layer
//! shell surfaces — use an enum here, because those variants behave differently
//! on focus. This compositor has exactly one kind: an `xdg-shell` surface. A
//! popup's keyboard and pointer behaviour is identical to a toplevel's, so a
//! `Popup(PopupKind)` variant would carry data that every single trait method
//! discards on its first line. The newtype keeps the conversion the grab needs
//! without inventing a distinction the compositor does not have.
//!
//! Because the same type serves keyboard, pointer, and touch focus,
//! `PointerFocus: From<KeyboardFocus>` is satisfied by the blanket
//! `impl<T> From<T> for T`.

use std::borrow::Cow;

use smithay::backend::input::KeyState;
use smithay::desktop::PopupKind;
use smithay::input::Seat;
use smithay::input::keyboard::{KeyboardTarget, KeysymHandle, ModifiersState};
use smithay::input::pointer::{
    AxisFrame, ButtonEvent, GestureHoldBeginEvent, GestureHoldEndEvent, GesturePinchBeginEvent,
    GesturePinchEndEvent, GesturePinchUpdateEvent, GestureSwipeBeginEvent, GestureSwipeEndEvent,
    GestureSwipeUpdateEvent, MotionEvent, PointerTarget, RelativeMotionEvent,
};
use smithay::input::touch::{
    DownEvent, MotionEvent as TouchMotionEvent, OrientationEvent, ShapeEvent, TouchTarget, UpEvent,
};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{IsAlive, Serial};
use smithay::wayland::seat::WaylandFocus;

use crate::state::Kiosk;

/// A surface that can hold keyboard, pointer, or touch focus.
#[derive(Debug, Clone, PartialEq)]
pub struct FocusTarget(WlSurface);

impl FocusTarget {
    /// The underlying surface.
    pub fn surface(&self) -> &WlSurface {
        &self.0
    }
}

impl From<WlSurface> for FocusTarget {
    fn from(surface: WlSurface) -> Self {
        Self(surface)
    }
}

/// The conversion that `grab_popup` requires, and the reason this type exists.
impl From<PopupKind> for FocusTarget {
    fn from(popup: PopupKind) -> Self {
        Self(popup.wl_surface().clone())
    }
}

impl IsAlive for FocusTarget {
    fn alive(&self) -> bool {
        self.0.alive()
    }
}

impl WaylandFocus for FocusTarget {
    fn wl_surface(&self) -> Option<Cow<'_, WlSurface>> {
        Some(Cow::Borrowed(&self.0))
    }
}

// Every method below forwards to the inner surface's own implementation. The
// default `replace` methods on both traits are inherited: they are defined in
// terms of `leave`/`enter`, which are forwarded here, so they behave correctly.

impl KeyboardTarget<Kiosk> for FocusTarget {
    fn enter(
        &self,
        seat: &Seat<Kiosk>,
        data: &mut Kiosk,
        keys: Vec<KeysymHandle<'_>>,
        serial: Serial,
    ) {
        KeyboardTarget::enter(&self.0, seat, data, keys, serial);
    }

    fn leave(&self, seat: &Seat<Kiosk>, data: &mut Kiosk, serial: Serial) {
        KeyboardTarget::leave(&self.0, seat, data, serial);
    }

    fn key(
        &self,
        seat: &Seat<Kiosk>,
        data: &mut Kiosk,
        key: KeysymHandle<'_>,
        state: KeyState,
        serial: Serial,
        time: u32,
    ) {
        KeyboardTarget::key(&self.0, seat, data, key, state, serial, time);
    }

    fn modifiers(
        &self,
        seat: &Seat<Kiosk>,
        data: &mut Kiosk,
        modifiers: ModifiersState,
        serial: Serial,
    ) {
        KeyboardTarget::modifiers(&self.0, seat, data, modifiers, serial);
    }
}

impl PointerTarget<Kiosk> for FocusTarget {
    fn enter(&self, seat: &Seat<Kiosk>, data: &mut Kiosk, event: &MotionEvent) {
        PointerTarget::enter(&self.0, seat, data, event);
    }

    fn motion(&self, seat: &Seat<Kiosk>, data: &mut Kiosk, event: &MotionEvent) {
        PointerTarget::motion(&self.0, seat, data, event);
    }

    fn relative_motion(&self, seat: &Seat<Kiosk>, data: &mut Kiosk, event: &RelativeMotionEvent) {
        PointerTarget::relative_motion(&self.0, seat, data, event);
    }

    fn button(&self, seat: &Seat<Kiosk>, data: &mut Kiosk, event: &ButtonEvent) {
        PointerTarget::button(&self.0, seat, data, event);
    }

    fn axis(&self, seat: &Seat<Kiosk>, data: &mut Kiosk, frame: AxisFrame) {
        PointerTarget::axis(&self.0, seat, data, frame);
    }

    fn frame(&self, seat: &Seat<Kiosk>, data: &mut Kiosk) {
        PointerTarget::frame(&self.0, seat, data);
    }

    fn leave(&self, seat: &Seat<Kiosk>, data: &mut Kiosk, serial: Serial, time: u32) {
        PointerTarget::leave(&self.0, seat, data, serial, time);
    }

    fn gesture_swipe_begin(
        &self,
        seat: &Seat<Kiosk>,
        data: &mut Kiosk,
        event: &GestureSwipeBeginEvent,
    ) {
        PointerTarget::gesture_swipe_begin(&self.0, seat, data, event);
    }

    fn gesture_swipe_update(
        &self,
        seat: &Seat<Kiosk>,
        data: &mut Kiosk,
        event: &GestureSwipeUpdateEvent,
    ) {
        PointerTarget::gesture_swipe_update(&self.0, seat, data, event);
    }

    fn gesture_swipe_end(
        &self,
        seat: &Seat<Kiosk>,
        data: &mut Kiosk,
        event: &GestureSwipeEndEvent,
    ) {
        PointerTarget::gesture_swipe_end(&self.0, seat, data, event);
    }

    fn gesture_pinch_begin(
        &self,
        seat: &Seat<Kiosk>,
        data: &mut Kiosk,
        event: &GesturePinchBeginEvent,
    ) {
        PointerTarget::gesture_pinch_begin(&self.0, seat, data, event);
    }

    fn gesture_pinch_update(
        &self,
        seat: &Seat<Kiosk>,
        data: &mut Kiosk,
        event: &GesturePinchUpdateEvent,
    ) {
        PointerTarget::gesture_pinch_update(&self.0, seat, data, event);
    }

    fn gesture_pinch_end(
        &self,
        seat: &Seat<Kiosk>,
        data: &mut Kiosk,
        event: &GesturePinchEndEvent,
    ) {
        PointerTarget::gesture_pinch_end(&self.0, seat, data, event);
    }

    fn gesture_hold_begin(
        &self,
        seat: &Seat<Kiosk>,
        data: &mut Kiosk,
        event: &GestureHoldBeginEvent,
    ) {
        PointerTarget::gesture_hold_begin(&self.0, seat, data, event);
    }

    fn gesture_hold_end(&self, seat: &Seat<Kiosk>, data: &mut Kiosk, event: &GestureHoldEndEvent) {
        PointerTarget::gesture_hold_end(&self.0, seat, data, event);
    }
}

impl TouchTarget<Kiosk> for FocusTarget {
    fn down(&self, seat: &Seat<Kiosk>, data: &mut Kiosk, event: &DownEvent, seq: Serial) {
        TouchTarget::down(&self.0, seat, data, event, seq);
    }

    fn up(&self, seat: &Seat<Kiosk>, data: &mut Kiosk, event: &UpEvent, seq: Serial) {
        TouchTarget::up(&self.0, seat, data, event, seq);
    }

    fn motion(&self, seat: &Seat<Kiosk>, data: &mut Kiosk, event: &TouchMotionEvent, seq: Serial) {
        TouchTarget::motion(&self.0, seat, data, event, seq);
    }

    fn frame(&self, seat: &Seat<Kiosk>, data: &mut Kiosk, seq: Serial) {
        TouchTarget::frame(&self.0, seat, data, seq);
    }

    fn cancel(&self, seat: &Seat<Kiosk>, data: &mut Kiosk, seq: Serial) {
        TouchTarget::cancel(&self.0, seat, data, seq);
    }

    fn shape(&self, seat: &Seat<Kiosk>, data: &mut Kiosk, event: &ShapeEvent, seq: Serial) {
        TouchTarget::shape(&self.0, seat, data, event, seq);
    }

    fn orientation(
        &self,
        seat: &Seat<Kiosk>,
        data: &mut Kiosk,
        event: &OrientationEvent,
        seq: Serial,
    ) {
        TouchTarget::orientation(&self.0, seat, data, event, seq);
    }
}

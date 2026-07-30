//! libinput event forwarding.
//!
//! This is a thin forwarder: keyboard, pointer, and touch events go to the
//! focused surface. The only compositor-level interception is `--exit-key`.

use smithay::backend::input::{
    AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputEvent, KeyState,
    KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent, PointerMotionEvent, TouchDownEvent,
    TouchMotionEvent as TouchMotionEventTrait, TouchUpEvent,
};
use smithay::backend::libinput::LibinputInputBackend;
use smithay::desktop::WindowSurfaceType;
use smithay::input::keyboard::FilterResult;
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent};
use smithay::input::touch::{DownEvent, MotionEvent as TouchMotionEvent, UpEvent};
use smithay::utils::{Logical, Point, SERIAL_COUNTER, Size};
use smithay::wayland::compositor::get_parent;

use crate::focus::FocusTarget;
use crate::state::Kiosk;

/// The exit status used when `--exit-key` fires. Pressing the escape hatch is a
/// clean shutdown, not a failure.
const EXIT_KEY_STATUS: i32 = 0;

/// A pointer or touch focus: the target surface plus **the coordinates of that
/// surface's origin in global compositor space**, which is what Smithay's input
/// APIs expect. Smithay derives the surface-local position as
/// `event.location - focus.1`, so passing a pointer-relative offset here makes
/// every client see the pointer at the surface origin.
type Focus = (FocusTarget, Point<f64, Logical>);

/// Confine a pointer position to the output.
///
/// The upper bound is exclusive so the hotspot always lands on a real pixel: a
/// cursor at exactly `width` is one pixel past the last column and would be
/// clipped away entirely. A zero or negative size clamps everything to the
/// origin rather than producing a negative bound.
fn clamp_to_output(location: Point<f64, Logical>, size: Size<i32, Logical>) -> Point<f64, Logical> {
    let max_x = (size.w as f64 - 1.0).max(0.0);
    let max_y = (size.h as f64 - 1.0).max(0.0);
    (location.x.clamp(0.0, max_x), location.y.clamp(0.0, max_y)).into()
}

impl Kiosk {
    /// Release every key the client currently believes is held.
    ///
    /// Called when the session is paused: the user pressed `Ctrl+Alt+F<n>`, and
    /// the *release* of those modifiers is delivered to the incoming VT, not to
    /// us. Without this the client sees `Ctrl+Alt` held forever, and because
    /// [`crate::cli::Binding::matches`] requires exact modifiers, `--exit-key`
    /// stops matching — the escape hatch dies exactly when it is needed.
    pub fn release_all_keys(&mut self) {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        // Reuse libinput's clock, not `start_time`. Real key events carry
        // milliseconds since boot; `start_time.elapsed()` is milliseconds since the
        // compositor started, so a synthetic release would be timestamped far in
        // the past relative to the press it releases, and a client that filters on
        // monotonic timestamps could discard it — the exact stuck-modifier failure
        // this function exists to prevent.
        let time = self.last_input_time;
        let held: Vec<_> = self.pressed_keys.drain().collect();
        for code in held {
            let serial = SERIAL_COUNTER.next_serial();
            keyboard.input::<(), _>(self, code, KeyState::Released, serial, time, |_, _, _| {
                FilterResult::Forward
            });
        }
    }

    /// Dispatch one libinput event.
    pub fn process_input_event(&mut self, event: InputEvent<LibinputInputBackend>) {
        // While another VT is in front we hold no DRM master and should not be
        // delivering input to the client.
        if !self.backend.is_active() {
            return;
        }

        match event {
            InputEvent::Keyboard { event } => self.on_keyboard(event),
            InputEvent::PointerMotion { event } => {
                let delta: Point<f64, Logical> = (event.delta_x(), event.delta_y()).into();
                let target = self.pointer_location + delta;
                self.move_pointer(target, event.time_msec());
            }
            InputEvent::PointerMotionAbsolute { event } => {
                let size = self.output_size();
                let location = (event.x_transformed(size.w), event.y_transformed(size.h)).into();
                self.move_pointer(location, event.time_msec());
            }
            InputEvent::PointerButton { event } => self.on_pointer_button(event),
            InputEvent::PointerAxis { event } => self.on_pointer_axis(event),
            InputEvent::TouchDown { event } => self.on_touch_down(event),
            InputEvent::TouchMotion { event } => self.on_touch_motion(event),
            InputEvent::TouchUp { event } => self.on_touch_up(event),
            InputEvent::TouchCancel { .. } => {
                if let Some(touch) = self.seat.get_touch() {
                    touch.cancel(self);
                }
            }
            InputEvent::TouchFrame { .. } => {
                if let Some(touch) = self.seat.get_touch() {
                    touch.frame(self);
                }
            }
            // Device add/remove and gesture/tablet/switch events. Capabilities
            // are established up front from the initial device set.
            _ => {}
        }
    }

    fn on_keyboard(&mut self, event: impl KeyboardKeyEvent<LibinputInputBackend>) {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };

        let serial = SERIAL_COUNTER.next_serial();
        let time = event.time_msec();
        // Remember the timebase so `release_all_keys` can synthesise releases that
        // are not in the past relative to the presses they release.
        self.last_input_time = time;
        let code = event.key_code();
        let key_state = event.state();
        let exit_key = self.exit_key;

        // The filter is the only place a keycode's keysym is known, because it
        // runs with the keymap and current modifier state applied.
        let intercepted = keyboard.input::<bool, _>(
            self,
            code,
            key_state,
            serial,
            time,
            |_state, modifiers, handle| {
                let Some(binding) = exit_key else {
                    return FilterResult::Forward;
                };
                if key_state != KeyState::Pressed {
                    return FilterResult::Forward;
                }
                // Check every keysym this keycode produces, so a binding written
                // against one layout level still fires.
                if handle
                    .raw_syms()
                    .iter()
                    .any(|sym| binding.matches(modifiers, *sym))
                {
                    // Intercept: the client never sees this press.
                    FilterResult::Intercept(true)
                } else {
                    FilterResult::Forward
                }
            },
        );

        // Remember what is held so a VT switch can release it; the physical
        // release goes to whichever VT is in front, not to us.
        match key_state {
            KeyState::Pressed => {
                self.pressed_keys.insert(code);
            }
            KeyState::Released => {
                self.pressed_keys.remove(&code);
            }
        }

        if intercepted == Some(true) {
            tracing::info!("exit key pressed; shutting down");
            self.shutdown(EXIT_KEY_STATUS);
        }
    }

    /// Move the pointer, clamped to the output, and update pointer focus.
    fn move_pointer(&mut self, location: Point<f64, Logical>, time: u32) {
        let Some(pointer) = self.pointer.clone() else {
            return;
        };

        self.pointer_location = clamp_to_output(location, self.output_size());

        let focus = self.surface_under(self.pointer_location);
        let serial = SERIAL_COUNTER.next_serial();
        let pointer_location = self.pointer_location;

        pointer.motion(
            self,
            focus,
            &MotionEvent {
                location: pointer_location,
                serial,
                time,
            },
        );
        pointer.frame(self);

        // The cursor moved, so the composited image is stale.
        self.backend.damage();
        self.render();
    }

    fn on_pointer_button(&mut self, event: impl PointerButtonEvent<LibinputInputBackend>) {
        let Some(pointer) = self.pointer.clone() else {
            return;
        };
        let serial = SERIAL_COUNTER.next_serial();
        let button_state = event.state();

        // A press is also a focus request. With one client this only matters when
        // a dialog is stacked over its parent.
        if button_state == ButtonState::Pressed
            && let Some((surface, _)) = self.surface_under(self.pointer_location)
        {
            self.focus_surface(&surface);
        }

        pointer.button(
            self,
            &ButtonEvent {
                button: event.button_code(),
                state: button_state,
                serial,
                time: event.time_msec(),
            },
        );
        pointer.frame(self);
    }

    fn on_pointer_axis(&mut self, event: impl PointerAxisEvent<LibinputInputBackend>) {
        let Some(pointer) = self.pointer.clone() else {
            return;
        };

        let source = event.source();
        let mut frame = AxisFrame::new(event.time_msec()).source(source);

        for axis in [Axis::Horizontal, Axis::Vertical] {
            if let Some(v120) = event.amount_v120(axis) {
                frame = frame.v120(axis, v120 as i32);
            }
            let amount = event.amount(axis).unwrap_or(0.0);
            if amount != 0.0 {
                frame = frame.value(axis, amount);
            } else if source == AxisSource::Finger {
                // A finger source reporting zero means the gesture ended, and
                // clients need the stop event to end kinetic scrolling.
                frame = frame.stop(axis);
            }
        }

        pointer.axis(self, frame);
        pointer.frame(self);
    }

    fn on_touch_down(&mut self, event: impl TouchDownEvent<LibinputInputBackend>) {
        let Some(touch) = self.seat.get_touch() else {
            return;
        };
        let location = self.touch_location(&event);
        let Some(focus) = self.surface_under(location) else {
            return;
        };

        // Touching a window focuses it, matching what a click does.
        self.focus_surface(&focus.0);

        let serial = SERIAL_COUNTER.next_serial();
        touch.down(
            self,
            Some(focus),
            &DownEvent {
                slot: event.slot(),
                location,
                serial,
                time: event.time_msec(),
            },
        );
    }

    fn on_touch_motion(&mut self, event: impl TouchMotionEventTrait<LibinputInputBackend>) {
        let Some(touch) = self.seat.get_touch() else {
            return;
        };
        let location = self.touch_location(&event);
        let focus = self.surface_under(location);

        touch.motion(
            self,
            focus,
            &TouchMotionEvent {
                slot: event.slot(),
                location,
                time: event.time_msec(),
            },
        );
    }

    fn on_touch_up(&mut self, event: impl TouchUpEvent<LibinputInputBackend>) {
        let Some(touch) = self.seat.get_touch() else {
            return;
        };
        let serial = SERIAL_COUNTER.next_serial();
        touch.up(
            self,
            &UpEvent {
                slot: event.slot(),
                serial,
                time: event.time_msec(),
            },
        );
    }

    /// Map a normalised touch position onto the output.
    fn touch_location<E: AbsolutePositionEvent<LibinputInputBackend>>(
        &self,
        event: &E,
    ) -> Point<f64, Logical> {
        let size = self.output_size();
        (event.x_transformed(size.w), event.y_transformed(size.h)).into()
    }

    /// Hit-test the window stack from top to bottom.
    ///
    /// Every window is at the origin and fullscreen, so no per-window offset is
    /// needed. `WindowSurfaceType::ALL` makes Smithay test popups and subsurfaces
    /// as well, and honour input regions.
    fn surface_under(&self, location: Point<f64, Logical>) -> Option<Focus> {
        self.windows.iter().rev().find_map(|window| {
            window
                .surface_under(location, WindowSurfaceType::ALL)
                .map(|(surface, origin)| (surface.into(), origin.to_f64()))
        })
    }

    /// Raise and focus the window owning `target`, if it is not already on top.
    ///
    /// Does nothing while a keyboard grab is active. During a popup grab the
    /// click or touch belongs to the grab, which dismisses the menu; moving
    /// keyboard focus to a toplevel here would break exactly the menus the grab
    /// exists to make work. This guard lives here rather than at the call sites so
    /// it covers both the pointer and touch paths.
    fn focus_surface(&mut self, target: &FocusTarget) {
        if self
            .seat
            .get_keyboard()
            .is_some_and(|keyboard| keyboard.is_grabbed())
        {
            return;
        }

        // Resolve through subsurface and popup parents to the toplevel.
        let mut root = target.surface().clone();
        while let Some(parent) = get_parent(&root) {
            root = parent;
        }

        let Some(index) = self.windows.iter().position(|window| {
            window
                .toplevel()
                .is_some_and(|toplevel| toplevel.wl_surface() == &root)
        }) else {
            return;
        };

        if index + 1 == self.windows.len() {
            return;
        }

        let window = self.windows.remove(index);
        self.windows.push(window);
        self.refocus();
        self.backend.damage();
        self.render();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn size(w: i32, h: i32) -> Size<i32, Logical> {
        (w, h).into()
    }

    fn point(x: f64, y: f64) -> Point<f64, Logical> {
        (x, y).into()
    }

    #[test]
    fn a_position_inside_the_output_is_unchanged() {
        assert_eq!(
            clamp_to_output(point(100.0, 200.0), size(1920, 1080)),
            point(100.0, 200.0)
        );
    }

    #[test]
    fn the_origin_is_reachable() {
        assert_eq!(
            clamp_to_output(point(0.0, 0.0), size(1920, 1080)),
            point(0.0, 0.0)
        );
    }

    #[test]
    fn the_last_pixel_is_reachable_but_the_edge_is_not() {
        let out = size(1920, 1080);
        // One pixel inside the far edge is the furthest valid position.
        assert_eq!(
            clamp_to_output(point(1919.0, 1079.0), out),
            point(1919.0, 1079.0)
        );
        // Exactly at the size is one past the last pixel.
        assert_eq!(
            clamp_to_output(point(1920.0, 1080.0), out),
            point(1919.0, 1079.0)
        );
    }

    #[test]
    fn positions_past_the_far_edge_are_pulled_back() {
        assert_eq!(
            clamp_to_output(point(99999.0, 99999.0), size(2560, 1440)),
            point(2559.0, 1439.0)
        );
    }

    #[test]
    fn negative_positions_are_pulled_to_the_origin() {
        assert_eq!(
            clamp_to_output(point(-1.0, -500.0), size(1920, 1080)),
            point(0.0, 0.0)
        );
    }

    #[test]
    fn each_axis_clamps_independently() {
        // Off the right edge but vertically valid.
        assert_eq!(
            clamp_to_output(point(5000.0, 500.0), size(1920, 1080)),
            point(1919.0, 500.0)
        );
        // Above the top but horizontally valid.
        assert_eq!(
            clamp_to_output(point(500.0, -20.0), size(1920, 1080)),
            point(500.0, 0.0)
        );
    }

    #[test]
    fn fractional_positions_are_preserved_inside_the_output() {
        // Pointer motion is subpixel; clamping must not round it away.
        assert_eq!(
            clamp_to_output(point(10.5, 20.25), size(1920, 1080)),
            point(10.5, 20.25)
        );
    }

    /// A degenerate mode must not produce a negative clamp bound, which would
    /// make `clamp` panic on an inverted range.
    #[test]
    fn a_zero_sized_output_clamps_to_the_origin_without_panicking() {
        assert_eq!(
            clamp_to_output(point(10.0, 10.0), size(0, 0)),
            point(0.0, 0.0)
        );
        assert_eq!(
            clamp_to_output(point(-10.0, -10.0), size(0, 0)),
            point(0.0, 0.0)
        );
    }

    #[test]
    fn a_one_pixel_output_clamps_everything_to_the_origin() {
        assert_eq!(
            clamp_to_output(point(5.0, 5.0), size(1, 1)),
            point(0.0, 0.0)
        );
    }

    // A negative size is not testable: `Size::from` panics on construction, so
    // the type itself rules it out. The `.max(0.0)` guard in `clamp_to_output`
    // therefore only has to handle the zero case above.
}

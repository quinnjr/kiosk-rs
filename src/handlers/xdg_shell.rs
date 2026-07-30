//! `xdg-shell`, and with it the entire window policy.
//!
//! The policy is small enough to state exhaustively:
//!
//! - Every toplevel is fullscreen at the output's size. Clients cannot escape
//!   that geometry: `unset_fullscreen`, `maximize`, and `unmaximize` are
//!   acknowledged with a configure that keeps them fullscreen, and `move`,
//!   `resize`, and `minimize` are ignored (the protocol requires no reply).
//! - Toplevels form a stack. A new one goes on top and takes focus; a destroyed
//!   one is removed and focus falls to whatever is beneath.
//! - An empty stack does *not* exit. The compositor's lifetime is the child
//!   process's, so there is exactly one shutdown trigger.
//! - Popups are honoured, with the positioner constrained to the output.
//! - At most 32 toplevels are tracked; a client that exceeds the cap is sent a
//!   protocol error and disconnected.
//!
//! Rejecting the *second* toplevel was considered and rejected: a GTK dialog *is*
//! an `xdg_toplevel`, so refusing it breaks ordinary applications. The cap above is
//! a different policy — it exists only to stop a client exhausting memory, and sits
//! far above any real dialog stack.

use smithay::desktop::{
    PopupKeyboardGrab, PopupKind, PopupPointerGrab, PopupUngrabStrategy, Window,
    find_popup_root_surface,
};
use smithay::input::Seat;
use smithay::input::pointer::Focus;
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::protocol::{wl_output, wl_seat};
use smithay::utils::Serial;
use smithay::wayland::shell::xdg::decoration::XdgDecorationHandler;
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
};
use smithay::{delegate_xdg_decoration, delegate_xdg_shell};

/// `wl_surface.error` code 2. See the note at the use site: the protocol has no
/// resource-limit code reachable from a `wl_surface`, so this is the delivery
/// vehicle and the message string carries the reason.
const WL_SURFACE_ERROR_INVALID_SIZE: u32 = 2;

/// Cap on simultaneously-tracked toplevels.
///
/// A real dialog stack never approaches this; the limit exists so a client cannot
/// exhaust memory or turn the per-frame window walk into a livelock. Exceeding it
/// is a protocol error, which disconnects that client rather than killing us.
const MAX_TOPLEVELS: usize = 32;

use crate::focus::FocusTarget;
use crate::state::Kiosk;

impl XdgShellHandler for Kiosk {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        // A client that opens toplevels in a loop would otherwise grow this stack
        // without bound, and every entry is walked per frame and per input event.
        if self.windows.len() >= MAX_TOPLEVELS {
            tracing::warn!(
                limit = MAX_TOPLEVELS,
                "client exceeded the toplevel limit; disconnecting it"
            );
            // `post_error`, not `Client::kill`. `kill` looks like the right API —
            // "kill this client by triggering a protocol error" — but it only sets a
            // flag and hands the `ProtocolError` to `ClientData::disconnected`; it
            // never puts `wl_display.error` on the wire. The client would just see an
            // unexplained EOF and could not tell a limit violation from a compositor
            // crash. `post_error` builds the message, flushes, *then* kills.
            //
            // The code is imprecise and cannot be otherwise: an error code is
            // interpreted against the referenced object's interface, the only object
            // in hand is the `wl_surface`, and no protocol code on any object we hold
            // means "resource limit" (`no_memory` lives on `wl_display`, which
            // wayland-server does not expose as a `Resource`). The message string
            // carries the real reason; a delivered-but-mislabelled error is strictly
            // better for the client than a silent disconnect.
            surface.wl_surface().post_error(
                WL_SURFACE_ERROR_INVALID_SIZE,
                format!("too many toplevels (limit {MAX_TOPLEVELS})"),
            );
            return;
        }

        self.configure_fullscreen(&surface);
        surface.send_configure();

        self.windows.push(Window::new_wayland_window(surface));
        self.refocus();
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        self.windows
            .retain(|window| window.toplevel() != Some(&surface));
        self.refocus();

        // The window is gone but the screen still shows it.
        self.backend.damage();
        self.render();
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        self.constrain_popup(&surface);
        if let Err(err) = self.popups.track_popup(PopupKind::Xdg(surface)) {
            tracing::warn!(?err, "failed to track popup");
        }
        // The initial configure is sent from `CompositorHandler::commit`, which
        // also covers a popup that unmaps and remaps.
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        surface.with_pending_state(|state| {
            state.positioner = positioner;
        });
        self.constrain_popup(&surface);
        surface.send_repositioned(token);
    }

    fn popup_destroyed(&mut self, _surface: PopupSurface) {
        self.backend.damage();
        self.render();
    }

    /// Take a popup grab: route keyboard and pointer input to the popup chain so
    /// that a click or key outside it dismisses the whole chain.
    ///
    /// This is what makes a menu behave like a menu without the client having to
    /// notice the stray click itself.
    fn grab(&mut self, surface: PopupSurface, seat: wl_seat::WlSeat, serial: Serial) {
        let Some(seat) = Seat::<Self>::from_resource(&seat) else {
            tracing::warn!("popup grab for an unknown seat");
            return;
        };

        let popup = PopupKind::Xdg(surface);
        let root = match find_popup_root_surface(&popup) {
            Ok(root) => FocusTarget::from(root),
            Err(err) => {
                // The parent died between the request and now.
                tracing::debug!(?err, "popup grab without a live root surface");
                return;
            }
        };

        let mut grab = match self.popups.grab_popup(root, popup, &seat, serial) {
            Ok(grab) => grab,
            Err(err) => {
                tracing::debug!(?err, "popup grab refused");
                return;
            }
        };

        let previous = grab.previous_serial().unwrap_or(serial);

        // Check both devices before installing either. Installing the keyboard
        // grab and then refusing the pointer would leave the keyboard grabbed on a
        // chain we just dismissed — and `focus_surface` bails whenever a keyboard
        // grab is live, so clicks would stop raising windows too.
        let keyboard = seat.get_keyboard();
        let pointer = seat.get_pointer();

        let keyboard_ok = keyboard
            .as_ref()
            .is_none_or(|k| grab_allowed(k.is_grabbed(), k.has_grab(serial), k.has_grab(previous)));
        let pointer_ok = pointer
            .as_ref()
            .is_none_or(|p| grab_allowed(p.is_grabbed(), p.has_grab(serial), p.has_grab(previous)));

        if !keyboard_ok || !pointer_ok {
            grab.ungrab(PopupUngrabStrategy::All);
            return;
        }

        if let Some(keyboard) = keyboard {
            keyboard.set_focus(self, grab.current_grab(), serial);
            keyboard.set_grab(self, PopupKeyboardGrab::new(&grab), serial);
        }
        if let Some(pointer) = pointer {
            // `Focus::Keep`: the pointer is already over the surface that opened
            // the menu, and moving focus here would send a spurious leave.
            pointer.set_grab(self, PopupPointerGrab::new(&grab), serial, Focus::Keep);
        }
    }

    // The client cannot leave the kiosk geometry. The four state requests below
    // are answered with a configure that restates fullscreen, because the
    // protocol expects a response and silence hangs clients that wait for one.
    // `move`, `resize`, and `minimize` need no reply and are ignored.

    fn fullscreen_request(
        &mut self,
        surface: ToplevelSurface,
        _output: Option<wl_output::WlOutput>,
    ) {
        self.configure_fullscreen(&surface);
        surface.send_configure();
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        self.configure_fullscreen(&surface);
        surface.send_configure();
    }

    fn maximize_request(&mut self, surface: ToplevelSurface) {
        self.configure_fullscreen(&surface);
        surface.send_configure();
    }

    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        self.configure_fullscreen(&surface);
        surface.send_configure();
    }

    fn minimize_request(&mut self, _surface: ToplevelSurface) {
        // There is nowhere to minimize to, and `set_minimized` expects no reply.
    }

    fn move_request(&mut self, _surface: ToplevelSurface, _seat: wl_seat::WlSeat, _serial: Serial) {
        // `xdg_toplevel.move` starts an interactive drag and requires no reply.
        // The surface cannot leave the output, so there is nothing to grant.
    }

    fn resize_request(
        &mut self,
        _surface: ToplevelSurface,
        _seat: wl_seat::WlSeat,
        _serial: Serial,
        _edges: xdg_toplevel::ResizeEdge,
    ) {
        // As with `move`: no reply required, and the geometry is fixed.
    }
}

/// Force server-side decorations, which for this compositor means no
/// decorations at all. A kiosk application should not be drawing a title bar.
impl XdgDecorationHandler for Kiosk {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(zxdg_toplevel_decoration_v1::Mode::ServerSide);
        });
        toplevel.send_configure();
    }

    fn request_mode(
        &mut self,
        toplevel: ToplevelSurface,
        _mode: zxdg_toplevel_decoration_v1::Mode,
    ) {
        // The client's preference is noted and overruled.
        self.new_decoration(toplevel);
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        self.new_decoration(toplevel);
    }
}

/// May a client take a popup grab right now?
///
/// A grab is only legitimate if the client already owns the relevant input:
/// either nothing is grabbed at all, or the existing grab is one of ours — this
/// serial, or the serial of the grab this popup is nesting inside. Anything else
/// is a client trying to steal input while another grab is live, and the protocol
/// says to dismiss the popup rather than honour it.
fn grab_allowed(already_grabbed: bool, owns_serial: bool, owns_previous_serial: bool) -> bool {
    !already_grabbed || owns_serial || owns_previous_serial
}

impl Kiosk {
    /// Put a toplevel's pending state into the only geometry this compositor
    /// offers: fullscreen at the output's size.
    pub(crate) fn configure_fullscreen(&self, surface: &ToplevelSurface) {
        let size = self.output_size();
        surface.with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Fullscreen);
            state.states.unset(xdg_toplevel::State::Maximized);
            state.states.unset(xdg_toplevel::State::Resizing);
            state.size = Some(size);
            state.bounds = Some(size);
            // Advertise no window-management capabilities: there is no window
            // menu, no minimize target, and maximize is meaningless when
            // everything is already fullscreen.
            state.capabilities = Vec::new().into();
        });
    }

    /// Keep a popup inside the output. Constraining the positioner is the
    /// compositor's job, so without this a menu near the screen edge is drawn
    /// partly offscreen.
    ///
    /// The positioner works in the parent's window-geometry space, so the target
    /// rect is the output translated by the parent's geometry origin — not the
    /// output rect itself. Every window sits at the origin here, so only the
    /// parent's own `set_window_geometry` offset matters.
    fn constrain_popup(&self, surface: &PopupSurface) {
        let mut target = self.output_rect();
        if let Ok(root) = find_popup_root_surface(&PopupKind::Xdg(surface.clone()))
            && let Some(window) = self.window_for_surface(&root)
        {
            target.loc -= window.geometry().loc;
        }
        surface.with_pending_state(|state| {
            state.geometry = state.positioner.get_unconstrained_geometry(target);
        });
    }
}

delegate_xdg_shell!(Kiosk);
delegate_xdg_decoration!(Kiosk);

#[cfg(test)]
mod tests {
    use super::grab_allowed;

    #[test]
    fn a_first_grab_is_always_allowed() {
        // Nothing is grabbed, so the serials are irrelevant.
        assert!(grab_allowed(false, false, false));
        assert!(grab_allowed(false, true, false));
        assert!(grab_allowed(false, false, true));
    }

    #[test]
    fn nesting_inside_our_own_current_grab_is_allowed() {
        assert!(grab_allowed(true, true, false));
    }

    #[test]
    fn nesting_inside_the_parent_popups_grab_is_allowed() {
        // A submenu opening from a menu: the live grab belongs to the parent.
        assert!(grab_allowed(true, false, true));
    }

    /// The case this predicate exists for: another client holds the grab and this
    /// one owns neither serial, so it is trying to steal input.
    #[test]
    fn grabbing_over_someone_elses_grab_is_refused() {
        assert!(!grab_allowed(true, false, false));
    }
}

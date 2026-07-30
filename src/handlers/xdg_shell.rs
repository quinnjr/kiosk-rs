//! `xdg-shell`, and with it the entire window policy.
//!
//! The policy is small enough to state exhaustively:
//!
//! - Every toplevel is fullscreen at the output's size. Clients cannot escape
//!   that geometry: `set_fullscreen`, `unset_fullscreen`, `maximize`, and
//!   `unmaximize` are acknowledged with a configure that keeps them fullscreen,
//!   and `move`,
//!   `resize`, and `minimize` are ignored (the protocol requires no reply).
//! - Toplevels form a stack. A new one goes on top and takes focus; a destroyed
//!   one is removed and focus falls to whatever is beneath.
//! - An empty stack does *not* exit. The compositor's lifetime is the child
//!   process's, so there is exactly one shutdown trigger.
//! - Popups are honoured, with the positioner constrained to the output.
//! - At most 32 toplevels and 64 popups are tracked, and popup chains are capped at
//!   8 deep; a client that exceeds any of those is sent a protocol error and
//!   disconnected (see `new_popup` for why depth is the sharp limit). A popup created
//!   with a *null* parent is rejected outright: Smithay files those where the popup
//!   count cannot see them.
//!
//! Rejecting the *second* toplevel was considered and rejected: a GTK dialog *is*
//! an `xdg_toplevel`, so refusing it breaks ordinary applications. The cap above is
//! a different policy — it exists only to stop a client exhausting memory, and sits
//! far above any real dialog stack.

use smithay::desktop::{
    PopupKeyboardGrab, PopupKind, PopupManager, PopupPointerGrab, PopupUngrabStrategy, Window,
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

/// Cap on simultaneously-tracked popups, across all clients.
///
/// Same rationale as [`MAX_TOPLEVELS`], plus the recursion in Smithay's popup tree.
const MAX_POPUPS: usize = 64;

/// Cap on popup chain depth.
///
/// A menu → submenu → sub-submenu is three; nothing real approaches eight. See the
/// rationale at the enforcement site in `new_popup`.
pub(crate) const MAX_POPUP_DEPTH: usize = 8;

/// How deep this popup's parent chain already is.
///
/// Walks up through the manager, stopping one past the limit — the check itself
/// must not become the unbounded walk it exists to prevent.
fn popup_chain_depth(popups: &PopupManager, surface: &PopupSurface) -> usize {
    let mut depth = 0;
    let mut current = surface.get_parent_surface();
    while let Some(parent) = current {
        depth += 1;
        if depth > MAX_POPUP_DEPTH {
            break;
        }
        current = match popups.find_popup(&parent) {
            Some(PopupKind::Xdg(popup)) => popup.get_parent_surface(),
            _ => None,
        };
    }
    depth
}

/// Whether a new popup would exceed either cap.
///
/// Free function so the boundaries are testable without a GPU. `>=` on the count
/// (this popup would be the (n+1)th) and `>` on the depth (a chain of exactly
/// [`MAX_POPUP_DEPTH`] is allowed) are both deliberate, and both are the kind of
/// comparison a later edit silently gets wrong.
fn popup_limits_exceeded(count: usize, depth: usize) -> bool {
    count >= MAX_POPUPS || depth > MAX_POPUP_DEPTH
}

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
            // The cap is compositor-wide, not per client: nothing restricts the
            // socket to the spawned child, and real applications connect helper
            // processes. So the client that asks for one beyond the limit is the one
            // disconnected, which is not necessarily the one that filled it.
            tracing::warn!(
                limit = MAX_TOPLEVELS,
                tracked = self.windows.len(),
                "toplevel limit reached; disconnecting the requesting client"
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
                format!("compositor is tracking too many toplevels (limit {MAX_TOPLEVELS})"),
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
        self.backend.pacer.damage();
        self.render();
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        // `xdg_surface.get_popup` takes a *nullable* parent, and Smithay keeps it
        // nullable. A parent-less popup takes the other branch of
        // `PopupManager::track_popup`: it lands in `unmapped_popups` rather than a
        // `PopupTree`, and it is never positioned or drawn. This compositor has no
        // way to give it a parent later — that requires a protocol kiosk-rs does not
        // implement — so it would sit there forever being rescanned by
        // `popups.cleanup()` on every event loop iteration. Reject it outright.
        if surface.get_parent_surface().is_none() {
            tracing::warn!("client created a popup with no parent; disconnecting it");
            surface.wl_surface().post_error(
                WL_SURFACE_ERROR_INVALID_SIZE,
                "xdg_popup created without a parent surface",
            );
            return;
        }

        // Popups need the same cap as toplevels, and for a sharper reason: Smithay's
        // popup tree recurses once per chain level on insert, cleanup, send_done and
        // Drop, and `render_elements` walks it on every commit. An unbounded chain is
        // both a livelock (the event loop stops servicing input, so `--exit-key` dies)
        // and a stack overflow — and an overflow aborts rather than unwinds, so it
        // bypasses `catch_unwind` and skips the CRTC/VT restore entirely.
        let depth = popup_chain_depth(&self.popups, &surface);
        if popup_limits_exceeded(self.popup_count, depth) {
            tracing::warn!(
                count_limit = MAX_POPUPS,
                depth_limit = MAX_POPUP_DEPTH,
                depth,
                "client exceeded the popup limits; disconnecting it"
            );
            surface.wl_surface().post_error(
                WL_SURFACE_ERROR_INVALID_SIZE,
                format!("too many popups (limit {MAX_POPUPS}, max depth {MAX_POPUP_DEPTH})"),
            );
            return;
        }

        self.constrain_popup(&surface);
        match self.popups.track_popup(PopupKind::Xdg(surface)) {
            Ok(()) => self.popup_count += 1,
            Err(err) => tracing::warn!(?err, "failed to track popup"),
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
        // `saturating_sub`: a popup we *rejected* was never counted, but the client
        // still owns the object, so this can fire for one.
        self.popup_count = self.popup_count.saturating_sub(1);
        self.backend.pacer.damage();
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

    /// The caps' boundaries. Nothing else covers this arithmetic: matrix row 28 only
    /// confirms the depth cap is not *too tight* (three levels pass), and no test
    /// confirms either cap actually rejects — so `depth > MAX_POPUP_DEPTH * 10` or a
    /// `>=`/`>` slip on the count would survive the entire suite.
    #[test]
    fn the_popup_caps_reject_exactly_at_their_limits() {
        use super::{MAX_POPUP_DEPTH, MAX_POPUPS, popup_limits_exceeded};

        // Count: the cap is on what is already tracked, so the (n+1)th is refused.
        assert!(
            !popup_limits_exceeded(MAX_POPUPS - 1, 1),
            "refused the last permitted popup"
        );
        assert!(
            popup_limits_exceeded(MAX_POPUPS, 1),
            "admitted one past the count cap"
        );

        // Depth: a chain of exactly MAX_POPUP_DEPTH is allowed.
        assert!(
            !popup_limits_exceeded(0, MAX_POPUP_DEPTH),
            "refused a chain at the depth cap"
        );
        assert!(
            popup_limits_exceeded(0, MAX_POPUP_DEPTH + 1),
            "admitted one past the depth cap"
        );

        // A realistic menu chain is nowhere near either.
        assert!(
            !popup_limits_exceeded(3, 3),
            "refused an ordinary submenu chain"
        );
    }
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

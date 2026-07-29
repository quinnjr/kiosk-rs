//! Seat, focus, cursor image, and selection.
//!
//! The data device is registered even though there is a single client, because
//! toolkits expect the global to exist.

use std::os::unix::io::OwnedFd;

use smithay::input::pointer::CursorImageStatus;
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::reexports::wayland_server::Resource;
use smithay::wayland::output::OutputHandler;
use smithay::wayland::selection::SelectionHandler;
use smithay::wayland::selection::data_device::{
    ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
    set_data_device_focus,
};
use smithay::{delegate_data_device, delegate_output, delegate_seat};

use crate::focus::FocusTarget;
use crate::state::Kiosk;

impl SeatHandler for Kiosk {
    // One type for all three: see [`crate::focus`] for why it is a newtype over
    // `WlSurface` rather than an enum.
    type KeyboardFocus = FocusTarget;
    type PointerFocus = FocusTarget;
    type TouchFocus = FocusTarget;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&FocusTarget>) {
        tracing::debug!(focused = focused.is_some(), "keyboard focus changed");

        // The selection machinery filters `wl_data_device.selection` by the
        // clipboard focus, so without this the data device global exists but no
        // client ever receives a selection event and paste silently never works —
        // even within the single client.
        let client = focused.and_then(|target| target.surface().client());
        set_data_device_focus(&self.display_handle, seat, client);
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        self.cursor_status = image;
        self.backend.damage();
        self.render();
    }
}

impl SelectionHandler for Kiosk {
    type SelectionUserData = ();
}

impl DataDeviceHandler for Kiosk {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}

impl ClientDndGrabHandler for Kiosk {}

impl ServerDndGrabHandler for Kiosk {
    fn send(&mut self, _mime_type: String, _fd: OwnedFd, _seat: Seat<Self>) {
        // The compositor never offers a selection of its own, so there is
        // nothing to send.
    }
}

impl OutputHandler for Kiosk {}

delegate_seat!(Kiosk);
delegate_data_device!(Kiosk);
delegate_output!(Kiosk);

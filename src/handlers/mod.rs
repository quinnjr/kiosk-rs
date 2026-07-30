//! Wayland protocol handlers.
//!
//! One module per protocol's worth of trait implementations, each ending in the
//! `delegate_*!` macro that wires it to [`crate::state::Kiosk`].

mod compositor;
mod dmabuf;
mod seat;
pub(crate) mod xdg_shell;

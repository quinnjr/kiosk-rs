//! The single shared state type.
//!
//! Every Wayland global and the DRM backend live on [`Kiosk`]. Smithay's
//! `delegate_*!` macros in [`crate::handlers`] wire protocol dispatch to trait
//! implementations on this struct.
//!
//! There is deliberately no `Space`: with exactly one fullscreen window there is
//! nothing to lay out, so the render path collects elements from the window
//! stack at the origin.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::{Kind, RenderElementStates};
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::desktop::utils::{
    send_frames_surface_tree, surface_primary_scanout_output,
    update_surface_primary_scanout_output, with_surfaces_surface_tree,
};
use smithay::desktop::{PopupManager, Window};
use smithay::input::keyboard::Keycode;
use smithay::input::pointer::{CursorImageStatus, CursorImageSurfaceData, PointerHandle};
use smithay::input::{Seat, SeatState};
use smithay::output::Output;
use smithay::reexports::calloop::{LoopHandle, LoopSignal};
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State as ToplevelState;
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{IsAlive, Logical, Point, Rectangle, Size};
use smithay::wayland::compositor::{CompositorClientState, CompositorState, with_states};
use smithay::wayland::dmabuf::DmabufState;
use smithay::wayland::selection::data_device::DataDeviceState;
use smithay::wayland::shell::xdg::XdgShellState;
use smithay::wayland::shm::ShmState;

use crate::backend::drm::DrmBackend;
use crate::cli::Binding;
use crate::focus::FocusTarget;

/// The render element type. Windows, popups, and the cursor are all Wayland
/// surface trees, so one element type covers every layer.
pub type Element = WaylandSurfaceRenderElement<GlesRenderer>;

pub struct Kiosk {
    pub display_handle: DisplayHandle,

    // Protocol state.
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub shm_state: ShmState,
    pub seat_state: SeatState<Self>,
    pub data_device_state: DataDeviceState,
    pub dmabuf_state: DmabufState,

    // Input.
    pub seat: Seat<Self>,
    /// `Some` only if a pointer device existed at startup — capabilities are fixed
    /// for the session (see the test matrix's *Fixed input capabilities*), so a
    /// pointer plugged in later does not populate this. The cursor is drawn only
    /// when this is `Some`.
    pub pointer: Option<PointerHandle<Self>>,
    pub cursor_status: CursorImageStatus,
    pub pointer_location: Point<f64, Logical>,
    pub exit_key: Option<Binding>,
    /// Keys the client currently believes are held. Needed because the release of
    /// a `Ctrl+Alt+F<n>` chord is delivered to the incoming VT, not to us.
    pub pressed_keys: HashSet<Keycode>,
    /// The most recent *keyboard* event timestamp, in libinput's timebase. Pointer
    /// and touch events do not update it — only key events need it, so that a
    /// synthetic release is never stamped earlier than the press it releases.
    pub last_input_time: u32,

    // Shell.
    pub popups: PopupManager,
    /// Toplevel stack; the last entry is on top and holds focus.
    pub windows: Vec<Window>,

    // Output and rendering.
    pub output: Output,
    pub backend: DrmBackend,

    pub start_time: Instant,
    pub loop_signal: LoopSignal,
    /// Used to arm a one-shot retry timer when a frame fails. Upstream requires
    /// the caller to reschedule; see [`crate::pacing`].
    pub loop_handle: LoopHandle<'static, Kiosk>,
    /// True while a retry timer is pending, so a burst of failures arms one timer.
    pub retry_armed: bool,
    /// Frames dropped in a row. Reset by any successful render; used to escalate a
    /// permanently-failing display from tier 2 to tier 3 instead of retrying
    /// forever.
    pub consecutive_drops: u32,
    /// Set when the compositor should shut down, and with what status.
    pub exit_code: Option<i32>,
    /// True once the child has exited and been reaped. Distinguishes "the child
    /// ended, so we are ending" from "we are ending, so the child must be told".
    pub child_reaped: bool,
}

impl Kiosk {
    /// The output's logical size, which is also every toplevel's size.
    pub fn output_size(&self) -> Size<i32, Logical> {
        size_of(&self.output)
    }

    /// The output rect in logical coordinates, used to clamp the pointer and
    /// constrain popup positioners.
    pub fn output_rect(&self) -> Rectangle<i32, Logical> {
        rect_of(&self.output)
    }

    /// The focused window, i.e. the top of the stack.
    pub fn top_window(&self) -> Option<&Window> {
        self.windows.last()
    }

    /// Find the window whose *toplevel* surface is `surface`.
    ///
    /// Callers must pass a root surface. This deliberately does not walk trees:
    /// subsurface roots are resolved by the caller via `get_parent`, and popups by
    /// `find_popup_root_surface` — `get_parent` only follows the subsurface tree,
    /// so a popup surface would never resolve here.
    pub fn window_for_surface(&self, surface: &WlSurface) -> Option<&Window> {
        self.windows.iter().find(|window| {
            window
                .toplevel()
                .is_some_and(|toplevel| toplevel.wl_surface() == surface)
        })
    }

    /// Give keyboard focus to the top of the stack.
    ///
    /// An empty stack clears focus but does *not* exit: the compositor's
    /// lifetime is the child process's.
    pub fn refocus(&mut self) {
        let target = self
            .top_window()
            .and_then(|window| window.toplevel())
            .map(|toplevel| toplevel.wl_surface().clone());

        // Mark the newly focused toplevel activated and the rest not, so clients
        // draw the right focus styling.
        let focused = target.clone();
        for window in &self.windows {
            if let Some(toplevel) = window.toplevel() {
                let active = Some(toplevel.wl_surface()) == focused.as_ref();
                let changed = toplevel.with_pending_state(|state| {
                    let was = state.states.contains(ToplevelState::Activated);
                    if active {
                        state.states.set(ToplevelState::Activated);
                    } else {
                        state.states.unset(ToplevelState::Activated);
                    }
                    was != active
                });
                if changed {
                    toplevel.send_pending_configure();
                }
            }
        }

        if let Some(keyboard) = self.seat.get_keyboard() {
            let serial = smithay::utils::SERIAL_COUNTER.next_serial();
            keyboard.set_focus(self, target.map(FocusTarget::from), serial);
        }
    }

    /// Collect render elements front to back: cursor, then the window stack from
    /// top to bottom.
    ///
    /// Output scale is fixed at 1, so logical and physical coordinates coincide.
    pub fn render_elements(&mut self) -> Vec<Element> {
        self.drop_dead_cursor();
        let mut elements = Vec::new();

        // Cursor first so it draws above everything.
        //
        // Only a client-provided cursor surface is drawn. A client that asks for
        // a named cursor shape instead gets none, because rendering one would
        // mean loading an XCursor theme; in practice toolkits load the theme
        // themselves and hand us a surface.
        if self.pointer.is_some()
            && let CursorImageStatus::Surface(surface) = self.cursor_status.clone()
        {
            let hotspot = with_states(&surface, |states| {
                states
                    .data_map
                    .get::<CursorImageSurfaceData>()
                    .map(|data| data.lock().unwrap().hotspot)
                    .unwrap_or_else(|| (0, 0).into())
            });
            let location = self.pointer_location.to_i32_round::<i32>() - hotspot;
            elements.extend(
                smithay::backend::renderer::element::surface::render_elements_from_surface_tree(
                    &mut self.backend.renderer,
                    &surface,
                    location.to_physical(1),
                    1.0,
                    1.0,
                    Kind::Cursor,
                ),
            );
        }

        for window in self.windows.iter().rev() {
            let Some(toplevel) = window.toplevel() else {
                continue;
            };
            let surface = toplevel.wl_surface();

            // Popups draw above their parent toplevel. The window-geometry origin
            // term matters: positioner offsets are relative to the parent's
            // window geometry, and `Window::surface_under` hit-tests with the same
            // expression, so dropping it draws popups away from where clicks land.
            let window_loc = window.geometry().loc;
            for (popup, offset) in PopupManager::popups_for_surface(surface) {
                let location = (window_loc + offset - popup.geometry().loc).to_physical(1);
                elements.extend(
                    smithay::backend::renderer::element::surface::render_elements_from_surface_tree(
                        &mut self.backend.renderer,
                        popup.wl_surface(),
                        location,
                        1.0,
                        1.0,
                        Kind::Unspecified,
                    ),
                );
            }

            elements.extend(
                smithay::backend::renderer::element::surface::render_elements_from_surface_tree(
                    &mut self.backend.renderer,
                    surface,
                    (0, 0),
                    1.0,
                    1.0,
                    Kind::Unspecified,
                ),
            );
        }

        elements
    }

    /// Record which output each surface was scanned out on, so
    /// [`Self::send_frames`] can throttle correctly.
    pub fn update_scanout_state(&mut self, states: &RenderElementStates) {
        // One of the two read paths named in `drop_dead_cursor`'s doc. Guarding
        // here rather than relying on `render_elements` having run first means the
        // safety does not depend on call ordering inside `render`.
        self.drop_dead_cursor();

        for window in &self.windows {
            window.with_surfaces(|surface, surface_states| {
                update_surface_primary_scanout_output(
                    surface,
                    &self.output,
                    surface_states,
                    states,
                    |_, _, output, _| output,
                );
            });
        }

        if let CursorImageStatus::Surface(surface) = &self.cursor_status {
            with_surfaces_surface_tree(surface, |surface, surface_states| {
                update_surface_primary_scanout_output(
                    surface,
                    &self.output,
                    surface_states,
                    states,
                    |_, _, output, _| output,
                );
            });
        }
    }

    /// Release clients to draw their next frame.
    pub fn send_frames(&mut self) {
        self.drop_dead_cursor();
        let time = self.start_time.elapsed();
        for window in &self.windows {
            window.send_frame(&self.output, time, None, surface_primary_scanout_output);
        }
        if let CursorImageStatus::Surface(surface) = &self.cursor_status {
            send_frames_surface_tree(
                surface,
                &self.output,
                time,
                None,
                surface_primary_scanout_output,
            );
        }
    }

    /// Request shutdown with the given exit status.
    pub fn shutdown(&mut self, code: i32) {
        latch_exit_code(&mut self.exit_code, code);
        self.loop_signal.stop();
    }

    /// Drop a dead cursor surface.
    ///
    /// `cursor_status` holds a surface the client owns and may destroy at any
    /// time. Every read path (`with_states`, `with_surfaces_surface_tree`) unwraps
    /// the surface's user data, which panics on a destroyed resource — so a client
    /// could kill the compositor by destroying its cursor surface while the
    /// pointer is inside its window.
    fn drop_dead_cursor(&mut self) {
        if let CursorImageStatus::Surface(surface) = &self.cursor_status
            && !surface.alive()
        {
            self.cursor_status = CursorImageStatus::default_named();
        }
    }
}

/// First status wins.
///
/// The child's propagated status must not be overwritten by a later shutdown —
/// otherwise a `--exit-key` press during teardown would turn a child's `exit 42`
/// into a `0`, breaking the documented "exits with the application's own status".
fn latch_exit_code(current: &mut Option<i32>, code: i32) {
    if current.is_none() {
        *current = Some(code);
    }
}

/// The output's logical size, or `(0, 0)` if it has no mode.
///
/// A modeless output should be impossible — `build_output` always sets one — so
/// the fallback is logged rather than silently propagated: a zero size reaches
/// `configure_fullscreen`, and `size = Some((0,0))` in xdg-shell means "client
/// picks its own size", which is exactly the escape a kiosk must not offer.
pub(crate) fn size_of(output: &Output) -> Size<i32, Logical> {
    match output.current_mode() {
        Some(mode) => mode.size.to_logical(1),
        None => {
            tracing::error!(output = output.name(), "output has no mode");
            (0, 0).into()
        }
    }
}

/// The output rect in logical coordinates, anchored at the origin.
pub(crate) fn rect_of(output: &Output) -> Rectangle<i32, Logical> {
    Rectangle::from_size(size_of(output))
}

/// Per-client state. Smithay requires the compositor's per-client state to be
/// reachable from the client's data.
#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, client_id: ClientId) {
        tracing::debug!(?client_id, "client connected");
    }

    fn disconnected(&self, client_id: ClientId, reason: DisconnectReason) {
        tracing::debug!(?client_id, ?reason, "client disconnected");
    }
}

impl ClientState {
    pub fn new_arc() -> Arc<Self> {
        Arc::new(Self::default())
    }
}

#[cfg(test)]
mod tests {
    use smithay::output::{Mode, PhysicalProperties, Scale, Subpixel};

    use super::*;

    fn test_output(mode: Option<(i32, i32)>) -> Output {
        let output = Output::new(
            "TEST-1".to_string(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "test".into(),
                model: "test".into(),
            },
        );
        if let Some((w, h)) = mode {
            output.change_current_state(
                Some(Mode {
                    size: (w, h).into(),
                    refresh: 60_000,
                }),
                None,
                Some(Scale::Integer(1)),
                None,
            );
        }
        output
    }

    #[test]
    fn an_output_with_a_mode_reports_its_logical_size() {
        assert_eq!(
            size_of(&test_output(Some((2560, 1440)))),
            (2560, 1440).into()
        );
    }

    #[test]
    fn a_modeless_output_reports_zero_rather_than_panicking() {
        assert_eq!(size_of(&test_output(None)), (0, 0).into());
    }

    #[test]
    fn the_output_rect_is_anchored_at_the_origin() {
        // Popups are constrained against this rect, so the origin matters.
        let rect = rect_of(&test_output(Some((1920, 1080))));
        assert_eq!(rect.loc, (0, 0).into());
        assert_eq!(rect.size, (1920, 1080).into());
    }

    /// The contract the README leads with: kiosk exits with the application's own
    /// status. A later shutdown must not overwrite it.
    #[test]
    fn the_first_exit_code_wins() {
        let mut code = None;
        latch_exit_code(&mut code, 42);
        // e.g. --exit-key firing during teardown.
        latch_exit_code(&mut code, 0);
        assert_eq!(code, Some(42), "the child's status was overwritten");
    }

    #[test]
    fn a_zero_status_still_latches() {
        let mut code = None;
        latch_exit_code(&mut code, 0);
        latch_exit_code(&mut code, 1);
        assert_eq!(code, Some(0));
    }

    #[test]
    fn latching_an_error_after_success_is_refused() {
        let mut code = Some(0);
        latch_exit_code(&mut code, 1);
        assert_eq!(code, Some(0));
    }
}

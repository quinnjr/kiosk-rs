//! `wl_compositor`, `wl_shm`, and buffer handling.

use smithay::backend::renderer::utils::on_commit_buffer_handler;
use smithay::desktop::PopupKind;
use smithay::reexports::wayland_server::Client;
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    CompositorClientState, CompositorHandler, CompositorState, get_parent, is_sync_subsurface,
};
use smithay::wayland::shm::{ShmHandler, ShmState};
use smithay::{delegate_compositor, delegate_shm};

use crate::state::{ClientState, Kiosk};

impl CompositorHandler for Kiosk {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client
            .get_data::<ClientState>()
            .expect("client without ClientState")
            .compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);

        // A sync subsurface's contents only become visible when its parent
        // commits, so walk up to the root before doing any bookkeeping.
        // The root is needed twice — once for `on_commit`, once for the configure
        // check — and `window_for_surface` documents that callers must pass a root.
        let root = if is_sync_subsurface(surface) {
            None
        } else {
            let mut root = surface.clone();
            while let Some(parent) = get_parent(&root) {
                root = parent;
            }
            if let Some(window) = self.window_for_surface(&root) {
                window.on_commit();
            }
            Some(root)
        };

        self.popups.commit(surface);

        // xdg-shell requires the compositor to answer a surface's first commit
        // with a configure; the client will not attach a buffer until it has one.
        // Smithay does not do this for us, and it *clears* `initial_configure_sent`
        // when a mapped surface unmaps, so a hidden-then-reshown window needs the
        // handshake again. Doing it here rather than in `new_toplevel`/`new_popup`
        // covers both the first map and every remap.
        self.ensure_initial_configure(surface, root.as_ref());

        // New content: the frame is out of date.
        self.backend.damage();
        self.render();
    }
}

impl Kiosk {
    /// Send the initial configure for a toplevel or popup that has not had one.
    ///
    /// `root` is the caller's already-resolved subsurface root, or `None` for a sync
    /// subsurface. A surface has exactly one role, so a toplevel match rules out a
    /// popup — and skipping the popup lookup for a known toplevel avoids an
    /// allocating walk of every popup tree on the hot commit path.
    fn ensure_initial_configure(&mut self, surface: &WlSurface, root: Option<&WlSurface>) {
        if let Some(toplevel) = root
            .and_then(|root| self.window_for_surface(root))
            .and_then(|window| window.toplevel())
            .cloned()
        {
            if !toplevel.is_initial_configure_sent() {
                self.configure_fullscreen(&toplevel);
                toplevel.send_configure();
            }
            return;
        }

        if let Some(PopupKind::Xdg(popup)) = self.popups.find_popup(surface)
            && !popup.is_initial_configure_sent()
            && let Err(err) = popup.send_configure()
        {
            // The positioner was already rejected upstream, or the surface died.
            tracing::warn!(?err, "failed to configure popup");
        }
    }
}

impl BufferHandler for Kiosk {
    fn buffer_destroyed(&mut self, _buffer: &WlBuffer) {}
}

impl ShmHandler for Kiosk {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

delegate_compositor!(Kiosk);
delegate_shm!(Kiosk);

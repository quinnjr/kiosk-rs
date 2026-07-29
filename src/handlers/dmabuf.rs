//! Linux dmabuf import.
//!
//! Without this global, GPU-accelerated clients fall back to `wl_shm`, which
//! means a CPU copy of every frame. On the low-power hardware a kiosk usually
//! runs on that is the difference between smooth and unusable.

use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::renderer::ImportDma;
use smithay::delegate_dmabuf;
use smithay::wayland::dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier};

use crate::state::Kiosk;

impl DmabufHandler for Kiosk {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        dmabuf: Dmabuf,
        notifier: ImportNotifier,
    ) {
        match self.backend.renderer.import_dmabuf(&dmabuf, None) {
            Ok(_texture) => {
                // The import succeeded, so the buffer is usable. `GlesRenderer`
                // memoises dmabuf imports, so dropping the texture here is not
                // wasted work — the later attach reuses this same import.
                if let Err(err) = notifier.successful::<Kiosk>() {
                    // The client destroyed the params resource mid-import. Nothing
                    // to do, but a silent drop makes a hung client unexplainable.
                    tracing::warn!(?err, "failed to acknowledge dmabuf import");
                }
            }
            Err(err) => {
                tracing::warn!(?err, "failed to import client dmabuf");
                notifier.failed();
            }
        }
    }
}

delegate_dmabuf!(Kiosk);

//! The DRM/KMS backend: session, GBM, renderer, and frame submission.
//!
//! This is the only module that *sets up* the GPU: it owns the chosen card, the
//! renderer, and frame submission. [`super::discovery`] has already decided which
//! card and connector to use. Rendering *with* that renderer happens elsewhere —
//! [`crate::state::Kiosk::render_elements`] and [`crate::handlers`] both take
//! `&mut backend.renderer`.
//!
//! # Frame pacing
//!
//! Rendering is event driven rather than a fixed loop. The rules live in
//! [`crate::pacing::FramePacer`], which holds no GPU handles so that the
//! transitions are testable; this module only carries them out.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use rustix::fs::OFlags;
use smithay::backend::allocator::Fourcc;
use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
use smithay::backend::drm::compositor::{DrmCompositor, FrameFlags};
use smithay::backend::drm::exporter::gbm::GbmFramebufferExporter;
use smithay::backend::drm::{DrmDevice, DrmDeviceFd, DrmDeviceNotifier};
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::{Color32F, ImportDma};
use smithay::backend::session::Session;
use smithay::output::{Mode as OutputMode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::drm::control::{Device as ControlDevice, connector, crtc};
use smithay::utils::{DeviceFd, Transform};

use super::discovery::OutputCandidate;
use crate::pacing::{FramePacer, RenderDecision};
use crate::state::{Element, Kiosk};

/// The concrete [`DrmCompositor`] this backend uses.
///
/// The `()` is per-frame user data, which this compositor has no use for.
type Compositor =
    DrmCompositor<GbmAllocator<DrmDeviceFd>, GbmFramebufferExporter<DrmDeviceFd>, (), DrmDeviceFd>;

/// Colour used for the area not covered by a client surface.
const CLEAR_COLOR: Color32F = Color32F::new(0.0, 0.0, 0.0, 1.0);

/// Formats tried in order for the primary plane.
const COLOR_FORMATS: [Fourcc; 2] = [Fourcc::Xrgb8888, Fourcc::Argb8888];

/// How long to wait before retrying a dropped frame.
///
/// Roughly one retrace at 60Hz, which is what upstream's `queue_frame` docs
/// suggest. Exact timing does not matter: this only has to break the stall.
const RETRY_INTERVAL: Duration = Duration::from_millis(16);

/// Consecutive dropped frames before a transient fault is treated as permanent.
///
/// At [`RETRY_INTERVAL`] this is about a second. Retrying forever would trade a
/// visible freeze for an invisible one: the screen would still be stuck, while the
/// retry loop wrote thousands of warnings a minute into a log that has no rotation.
/// Exiting lets a supervisor restart us, which is the tier-3 contract.
const MAX_CONSECUTIVE_DROPS: u32 = 60;

/// Record a frame outcome against a drop budget; returns true when it is spent.
///
/// Every outcome goes through here rather than the success paths clearing the
/// counter inline — including the VT-resume reset in `main`. That is what makes the
/// reset rule testable: "60 consecutive"
/// only means anything if a success actually zeroes the budget, and a test cannot
/// observe that if the reset lives in `render`, which needs a live `DrmCompositor`.
/// Free function for the same reason — the off-by-one and the reset are the parts a
/// future change is most likely to get wrong, and neither needs a GPU to check.
pub(crate) fn note_frame(drops: &mut u32, succeeded: bool) -> bool {
    if succeeded {
        *drops = 0;
        return false;
    }
    *drops += 1;
    *drops >= MAX_CONSECUTIVE_DROPS
}

pub struct DrmBackend {
    pub drm: DrmDevice,
    pub renderer: GlesRenderer,
    pub compositor: Compositor,
    /// Kept so the native display the renderer was built from outlives it.
    /// Declared after `renderer` so it drops last — see `init`.
    #[allow(dead_code)]
    gbm: GbmDevice<DrmDeviceFd>,

    /// When to render and when to wait. See [`crate::pacing`] for the rules.
    pub pacer: FramePacer,
}

/// Everything produced by [`DrmBackend::init`] that the caller needs to finish
/// wiring up: the backend itself, the output global's source of truth, and the
/// DRM event source.
pub struct BackendInit {
    pub backend: DrmBackend,
    pub output: Output,
    pub notifier: DrmDeviceNotifier,
}

impl DrmBackend {
    /// Take DRM master on the candidate's card, set up rendering, and build the
    /// [`Output`] describing the chosen connector.
    pub fn init<S: Session>(session: &mut S, candidate: &OutputCandidate) -> Result<BackendInit> {
        let device_fd = open_device(session, &candidate.device)?;

        // `true` disables connectors we are not using, so a previously-configured
        // console or another compositor's leftovers do not stay lit.
        let (mut drm, notifier) = DrmDevice::new(device_fd.clone(), true)
            .with_context(|| format!("failed to open DRM device {}", candidate.device.display()))?;

        let gbm = GbmDevice::new(device_fd.clone()).context("failed to create GBM device")?;

        // SAFETY: `EGLDisplay` borrows the GBM device's native handle, so the
        // device must outlive it. `DrmBackend` stores a clone in its `gbm` field,
        // declared after `renderer`, and Rust drops fields in declaration order —
        // so the renderer (and its EGL display) is destroyed first.
        let egl_display =
            unsafe { EGLDisplay::new(gbm.clone()) }.context("failed to create EGL display")?;
        let egl_context = EGLContext::new(&egl_display).context("failed to create EGL context")?;
        // SAFETY: the context is current on this thread and not shared; the
        // compositor is single threaded.
        let renderer =
            unsafe { GlesRenderer::new(egl_context) }.context("failed to create GLES renderer")?;

        let connector_info = drm
            .get_connector(candidate.connector, false)
            .with_context(|| format!("failed to read connector {}", candidate.name))?;

        let mode = candidate.preferred_mode.ok_or_else(|| {
            anyhow!(
                "output {} reports no usable mode; it may have been unplugged",
                candidate.name
            )
        })?;

        let crtc = pick_crtc(&drm, &connector_info)
            .with_context(|| format!("no CRTC available for output {}", candidate.name))?;

        let surface = drm
            .create_surface(crtc, mode, &[candidate.connector])
            .with_context(|| format!("failed to create DRM surface for {}", candidate.name))?;

        let output = build_output(candidate, &connector_info, mode);

        let allocator = GbmAllocator::new(
            gbm.clone(),
            GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
        );
        let exporter = GbmFramebufferExporter::new(gbm.clone(), None);
        let renderer_formats = renderer.dmabuf_formats();

        let compositor = Compositor::new(
            &output,
            surface,
            None,
            allocator,
            exporter,
            COLOR_FORMATS,
            renderer_formats,
            drm.cursor_size(),
            Some(gbm.clone()),
        )
        .with_context(|| format!("failed to set up scanout for {}", candidate.name))?;

        tracing::info!(
            output = %candidate.name,
            device = %candidate.device.display(),
            mode = %format!("{}x{}@{}", mode.size().0, mode.size().1, mode.vrefresh()),
            "display configured"
        );

        Ok(BackendInit {
            backend: DrmBackend {
                drm,
                renderer,
                compositor,
                gbm,
                // Starts damaged, so the first frame paints black rather than
                // whatever the previous owner of the framebuffer left behind.
                pacer: FramePacer::new(),
            },
            output,
            notifier,
        })
    }

    /// Suspend on VT switch away. The DRM device loses master, so any commit
    /// from here on would fail.
    pub fn pause(&mut self) {
        self.pacer.pause();
        self.drm.pause();
    }

    /// Resume on VT switch back.
    ///
    /// The previous owner of the VT has reprogrammed the CRTC, so the compositor
    /// state must be reset and everything redrawn. Skipping this leaves the
    /// screen permanently black.
    pub fn resume(&mut self) -> Result<()> {
        // Mark active first. If either call below fails the caller shuts us down,
        // but leaving `active == false` on the way out would suppress input
        // dispatch — including the `--exit-key` check — turning a recoverable
        // failure into a machine with no way out.
        self.pacer.resume();

        self.drm
            .activate(true)
            .context("failed to reacquire DRM master")?;

        // Drop the flip the kernel will never report. `DrmCompositor` keeps its own
        // copy of "a flip is in flight" in `pending_frame`, and neither `activate`
        // nor `reset_state` clears it — only `frame_submitted` does. Leave it set and
        // the next `queue_frame` takes its `if pending_frame.is_none()` branch: it
        // enqueues, returns `Ok`, and never commits. The pacer would then believe a
        // flip is in flight, no vblank would ever arrive, and the screen would stay
        // frozen for good. `queued_frame` is always `None` here, so this cannot
        // submit anything.
        let _ = self.compositor.frame_submitted();
        self.compositor
            .reset_state()
            .context("failed to reset scanout state")?;
        self.compositor.reset_buffers();

        Ok(())
    }
}

impl Kiosk {
    /// Render and submit a frame if one is due.
    ///
    /// Safe to call from any event handler: it returns immediately when the
    /// session is paused, when nothing has changed, or when a flip is already in
    /// flight.
    pub fn render(&mut self) {
        if self.backend.pacer.poll() != RenderDecision::Render {
            return;
        }

        // Drop popups whose surfaces are gone before collecting elements,
        // otherwise dead popups linger in the render list.
        self.popups.cleanup();

        self.backend.pacer.begin_render();
        let elements: Vec<Element> = self.render_elements();

        let result = self.backend.compositor.render_frame(
            &mut self.backend.renderer,
            &elements,
            CLEAR_COLOR,
            FrameFlags::DEFAULT,
        );

        let frame = match result {
            Ok(frame) => frame,
            Err(err) => {
                // Losing master mid-render is expected around VT switches, so
                // this is a warning and a dropped frame, not a fatal error.
                tracing::warn!(?err, "frame render failed; dropping frame");
                self.backend.pacer.frame_failed();
                self.recover_from_dropped_frame();
                return;
            }
        };

        // Record scan-out before releasing clients: `send_frames` throttles on the
        // primary scan-out output, and a surface with none recorded gets no
        // callback at all.
        self.update_scanout_state(&frame.states);

        if frame.is_empty {
            // An empty frame is a success, not a drop: the render worked and there
            // was simply nothing new to show.
            note_frame(&mut self.consecutive_drops, true);
            // No pacer transition: an empty frame queues no flip, and damage is
            // deliberately left clear. Re-arming it would spin — `poll` would keep
            // returning `Render` for a frame that keeps coming out empty. The next
            // commit sets it.
            //
            // Clients still waiting on a frame callback must be released here
            // though, or a client that commits without visible damage waits forever
            // and never commits again — a permanent freeze.
            self.send_frames();
            return;
        }

        match self.backend.compositor.queue_frame(()) {
            Ok(()) => {
                note_frame(&mut self.consecutive_drops, true);
                self.backend.pacer.frame_queued();
            }
            Err(err) => {
                tracing::warn!(?err, "failed to queue frame");
                self.backend.pacer.frame_failed();
                self.recover_from_dropped_frame();
            }
        }
    }

    /// After a dropped frame, release the clients and schedule a retry.
    ///
    /// No flip was queued, so no vblank will arrive to drive the next render.
    /// Upstream is explicit that rescheduling is the caller's job: `queue_frame`'s
    /// docs recommend "a one-shot timer that will trigger after approximately one
    /// retrace duration". Without this a single transient `EBUSY` freezes the
    /// compositor until an input event happens to arrive.
    fn recover_from_dropped_frame(&mut self) {
        self.send_frames();

        if note_frame(&mut self.consecutive_drops, false) {
            tracing::error!(
                drops = self.consecutive_drops,
                "frames have failed continuously; the display is unusable"
            );
            self.shutdown(1);
            return;
        }

        if self.retry_armed {
            return;
        }

        let timer = Timer::from_duration(RETRY_INTERVAL);
        match self
            .loop_handle
            .insert_source(timer, |_, _, state: &mut Kiosk| {
                state.retry_armed = false;
                state.render();
                TimeoutAction::Drop
            }) {
            Ok(_) => self.retry_armed = true,
            Err(err) => {
                // Without a retry the screen would stall; a fatal exit at least
                // lets a supervisor restart us.
                tracing::error!(?err, "failed to schedule a frame retry");
                self.shutdown(1);
            }
        }
    }

    /// Handle a completed page flip: release clients to draw, then render again
    /// if damage arrived while the flip was in flight.
    pub fn on_vblank(&mut self) {
        self.backend.pacer.flip_completed();

        // Submission bookkeeping gets its own budget. It cannot share
        // `consecutive_drops`: the `render()` below succeeds on each of these cycles
        // and resets that counter, so the escalation would be unreachable in exactly
        // the scenario it exists for — a display whose bookkeeping fails every
        // vblank, warning 60 times a second forever into a log with no rotation.
        // Separate counters also stop one failure cycle from spending two units of
        // budget through `recover_from_dropped_frame`.
        match self.backend.compositor.frame_submitted() {
            Ok(_) => {
                note_frame(&mut self.submit_failures, true);
            }
            Err(err) => {
                tracing::warn!(?err, "failed to mark frame submitted");
                self.backend.pacer.frame_failed();
                if note_frame(&mut self.submit_failures, false) {
                    tracing::error!(
                        failures = self.submit_failures,
                        "frame submission has failed continuously; the display is unusable"
                    );
                    self.shutdown(1);
                    return;
                }
            }
        }

        self.send_frames();
        self.render();
    }
}

/// Open a DRM device through the session so it can be revoked on VT switch.
fn open_device<S: Session>(session: &mut S, path: &Path) -> Result<DrmDeviceFd> {
    let fd = session
        .open(
            path,
            OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
        )
        .map_err(|err| anyhow!("{err:?}"))
        .with_context(|| format!("failed to open {}", path.display()))?;
    Ok(DrmDeviceFd::new(DeviceFd::from(fd)))
}

/// Find a CRTC that can drive this connector.
///
/// A connector reaches a CRTC through an encoder, and each encoder advertises a
/// bitmask of CRTCs it can be routed to. With a single output any match works.
fn pick_crtc(drm: &DrmDevice, connector: &connector::Info) -> Result<crtc::Handle> {
    let resources = drm
        .resource_handles()
        .context("failed to read DRM resources")?;

    for encoder_handle in connector.encoders() {
        let encoder = match drm.get_encoder(*encoder_handle) {
            Ok(encoder) => encoder,
            Err(err) => {
                tracing::debug!(?encoder_handle, ?err, "skipping unreadable encoder");
                continue;
            }
        };

        if let Some(crtc) = resources
            .filter_crtcs(encoder.possible_crtcs())
            .into_iter()
            .next()
        {
            return Ok(crtc);
        }
    }

    bail!("connector has no usable encoder/CRTC combination")
}

/// Build the `wl_output` description from the connector.
fn build_output(
    candidate: &OutputCandidate,
    info: &connector::Info,
    mode: smithay::reexports::drm::control::Mode,
) -> Output {
    let (mm_width, mm_height) = info.size().unwrap_or((0, 0));

    let output = Output::new(
        candidate.name.clone(),
        PhysicalProperties {
            size: (mm_width as i32, mm_height as i32).into(),
            subpixel: Subpixel::Unknown,
            // The EDID would give real values, but nothing in a single-window
            // kiosk uses them.
            make: "Unknown".into(),
            model: "Unknown".into(),
        },
    );

    let wl_mode = OutputMode::from(mode);
    output.set_preferred(wl_mode);
    // Scale is fixed at 1: logical and physical coordinates coincide, which is
    // what the render path assumes.
    output.change_current_state(
        Some(wl_mode),
        Some(Transform::Normal),
        Some(smithay::output::Scale::Integer(1)),
        Some((0, 0).into()),
    );

    output
}

#[cfg(test)]
mod tests {
    use super::{MAX_CONSECUTIVE_DROPS, note_frame};

    #[test]
    fn one_drop_does_not_escalate() {
        let mut drops = 0;
        assert!(!note_frame(&mut drops, false));
        assert_eq!(drops, 1);
    }

    /// Pins the off-by-one: the escalation must fire *at* the limit, not before.
    #[test]
    fn the_budget_escalates_exactly_at_the_limit() {
        let mut drops = 0;
        for n in 1..MAX_CONSECUTIVE_DROPS {
            assert!(
                !note_frame(&mut drops, false),
                "escalated early, at drop {n}"
            );
        }
        assert!(
            note_frame(&mut drops, false),
            "did not escalate at the limit"
        );
        assert_eq!(drops, MAX_CONSECUTIVE_DROPS);
    }

    /// A flaky-but-recovering display must stay alive: the reset is what keeps it so.
    ///
    /// The success is reported *through* `note_frame` rather than by the test
    /// zeroing the counter itself. That is the whole point — a test that performs
    /// the reset by hand is byte-for-byte the off-by-one test above and cannot
    /// observe the reset going missing.
    #[test]
    fn a_success_resets_the_budget() {
        let mut drops = 0;
        for _ in 1..MAX_CONSECUTIVE_DROPS {
            note_frame(&mut drops, false);
        }
        assert_eq!(
            drops,
            MAX_CONSECUTIVE_DROPS - 1,
            "setup did not spend the budget"
        );

        assert!(
            !note_frame(&mut drops, true),
            "a success must never escalate"
        );
        assert_eq!(drops, 0, "a success must clear the budget");

        for n in 1..MAX_CONSECUTIVE_DROPS {
            assert!(
                !note_frame(&mut drops, false),
                "a reset budget escalated early, at drop {n}"
            );
        }
    }

    /// A success on an already-clear budget is not an underflow.
    #[test]
    fn repeated_successes_are_harmless() {
        let mut drops = 0;
        for _ in 0..3 {
            assert!(!note_frame(&mut drops, true));
            assert_eq!(drops, 0);
        }
    }
}

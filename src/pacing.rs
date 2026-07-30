//! The frame pacing state machine.
//!
//! Rendering is event driven rather than a fixed loop, and the rules are subtle
//! enough to be worth isolating from the GPU code that obeys them:
//!
//! - a client commit marks the frame damaged and asks for a render;
//! - a render submits a page flip only if something actually changed;
//! - the resulting vblank releases clients to draw again, and renders once more
//!   if new damage arrived while the flip was in flight.
//!
//! Two invariants matter, and both are easy to break by accident:
//!
//! 1. **Never queue two flips.** A second `queue_frame` before the vblank of the
//!    first is a protocol error against the kernel.
//! 2. **Never idle with damage outstanding.** Whenever the session is active,
//!    damage is set, and no flip is in flight, `poll` must authorise a render.
//!    Damage is cleared at `begin_render`, so damage arriving mid-render re-arms
//!    the flag; a genuinely empty frame idles until the next `damage()`, which is
//!    correct because restoring the flag there would spin.
//!
//! Note that invariant 2 is about this type only. Nothing here can guarantee that
//! somebody *calls* `poll` again — the caller owes a wake-up on every path that
//! does not queue a flip, since without a flip there is no vblank. See
//! `Kiosk::recover_from_dropped_frame` in `backend::drm`.
//!
//! Keeping this as a plain struct with no GPU handles means every transition is
//! directly testable, including the ones that only occur around a VT switch.

/// Whether a render should proceed, and why not when it should not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderDecision {
    /// Render and attempt to submit.
    Render,
    /// The session is paused; another VT owns the screen.
    Inactive,
    /// Nothing has changed since the last submitted frame.
    Clean,
    /// A page flip is already in flight.
    AwaitingFlip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FramePacer {
    /// False while the session is paused (another VT is in front).
    active: bool,
    /// Something has changed since the last submitted frame.
    damaged: bool,
    /// A page flip is in flight; the next render waits for its vblank.
    pending_flip: bool,
}

impl FramePacer {
    /// A pacer that starts active and damaged, so the compositor paints one frame
    /// immediately rather than leaving whatever the previous owner of the
    /// framebuffer put on screen.
    pub fn new() -> Self {
        Self {
            active: true,
            damaged: true,
            pending_flip: false,
        }
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Only the state machine itself needs these; callers act on [`Self::poll`].
    #[cfg(test)]
    pub fn is_damaged(&self) -> bool {
        self.damaged
    }

    #[cfg(test)]
    pub fn has_pending_flip(&self) -> bool {
        self.pending_flip
    }

    /// Record that the screen no longer matches what clients have drawn.
    pub fn damage(&mut self) {
        self.damaged = true;
    }

    /// Should a render happen now?
    pub fn poll(&self) -> RenderDecision {
        if !self.active {
            RenderDecision::Inactive
        } else if self.pending_flip {
            // Checked before `damaged`: a flip in flight is the stronger reason
            // to wait, and reporting `Clean` here would be misleading.
            RenderDecision::AwaitingFlip
        } else if !self.damaged {
            RenderDecision::Clean
        } else {
            RenderDecision::Render
        }
    }

    /// Called immediately before rendering, once [`Self::poll`] has allowed it.
    ///
    /// Clears the damage flag up front so that damage arriving *during* the
    /// render is not lost: the flag is set again by whoever delivers it, and the
    /// next vblank will pick it up.
    pub fn begin_render(&mut self) {
        debug_assert_eq!(self.poll(), RenderDecision::Render);
        self.damaged = false;
    }

    /// A frame was rendered and produced visible changes, so a flip is queued.
    pub fn frame_queued(&mut self) {
        debug_assert!(
            !self.pending_flip,
            "queued a second flip before the first vblank"
        );
        self.pending_flip = true;
    }

    /// Rendering or queueing failed. The frame is still owed, so restore damage
    /// and clear any flip we thought we had.
    pub fn frame_failed(&mut self) {
        self.damaged = true;
        self.pending_flip = false;
    }

    /// A queued flip completed.
    pub fn flip_completed(&mut self) {
        self.pending_flip = false;
    }

    /// The session was paused: another VT took the screen.
    pub fn pause(&mut self) {
        self.active = false;
    }

    /// The session was resumed.
    ///
    /// Everything must be redrawn because the previous owner reprogrammed the
    /// CRTC, and any flip queued before the switch will never report a vblank —
    /// so waiting for it would hang the compositor forever.
    pub fn resume(&mut self) {
        self.active = true;
        self.damaged = true;
        self.pending_flip = false;
    }
}

impl Default for FramePacer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_active_and_damaged_so_the_first_frame_paints() {
        let pacer = FramePacer::new();
        assert!(pacer.is_active());
        assert!(pacer.is_damaged());
        assert!(!pacer.has_pending_flip());
        assert_eq!(pacer.poll(), RenderDecision::Render);
    }

    /// `pause` must not touch damage: `resume` force-sets it, which would mask a
    /// `pause` that lost state if the two were ever tested only together.
    #[test]
    fn pausing_preserves_outstanding_damage() {
        let mut pacer = FramePacer::new();
        pacer.begin_render();
        pacer.frame_queued();
        pacer.flip_completed();
        pacer.damage();

        pacer.pause();

        assert!(
            pacer.is_damaged(),
            "pause dropped outstanding damage; resume would be masking it"
        );
    }

    /// Only `resume` may drop the pre-switch flip — `pause` must leave it, because
    /// the kernel may still own it at that point.
    #[test]
    fn pausing_does_not_forget_a_flip_the_kernel_still_owns() {
        let mut pacer = FramePacer::new();
        pacer.begin_render();
        pacer.frame_queued();

        pacer.pause();

        assert!(pacer.has_pending_flip());
    }

    #[test]
    #[should_panic(expected = "Render")]
    fn beginning_a_render_while_a_flip_is_in_flight_is_a_bug() {
        let mut pacer = FramePacer::new();
        pacer.begin_render();
        pacer.frame_queued();
        pacer.damage();
        pacer.begin_render();
    }

    #[test]
    #[should_panic(expected = "Render")]
    fn beginning_a_render_while_paused_is_a_bug() {
        let mut pacer = FramePacer::new();
        pacer.pause();
        pacer.begin_render();
    }

    #[test]
    #[should_panic(expected = "second flip")]
    fn queueing_two_flips_is_a_bug() {
        let mut pacer = FramePacer::new();
        pacer.begin_render();
        pacer.frame_queued();
        pacer.frame_queued();
    }

    /// Check the two invariants across every short event sequence, rather than at
    /// hand-picked points. This is what turns the scenario tests above into
    /// something closer to a proof.
    #[test]
    fn the_invariants_hold_across_every_short_event_sequence() {
        #[derive(Copy, Clone, Debug)]
        enum Ev {
            Damage,
            Render,
            Queued,
            Empty,
            Failed,
            Flip,
            Pause,
            Resume,
        }
        const ALL: [Ev; 8] = [
            Ev::Damage,
            Ev::Render,
            Ev::Queued,
            Ev::Empty,
            Ev::Failed,
            Ev::Flip,
            Ev::Pause,
            Ev::Resume,
        ];

        fn walk(pacer: FramePacer, depth: u32, trace: &mut Vec<Ev>, rendering: bool) {
            // Invariant 1: while a flip is in flight, no render is authorised.
            if pacer.has_pending_flip() {
                assert_ne!(pacer.poll(), RenderDecision::Render, "trace: {trace:?}");
            }
            // Invariant 2: active, damaged, and no flip pending => must render.
            if pacer.is_active() && pacer.is_damaged() && !pacer.has_pending_flip() {
                assert_eq!(
                    pacer.poll(),
                    RenderDecision::Render,
                    "idle with damage outstanding; trace: {trace:?}"
                );
            }
            if depth == 0 {
                return;
            }

            for ev in ALL {
                // Only legal transitions: a render must be authorised first, and
                // an outcome only follows a begun render.
                let legal = match ev {
                    Ev::Render => !rendering && pacer.poll() == RenderDecision::Render,
                    Ev::Queued | Ev::Empty | Ev::Failed => rendering,
                    _ => true,
                };
                if !legal {
                    continue;
                }

                let mut next = pacer;
                let mut next_rendering = rendering;
                match ev {
                    Ev::Damage => next.damage(),
                    Ev::Render => {
                        next.begin_render();
                        next_rendering = true;
                    }
                    Ev::Queued => {
                        next.frame_queued();
                        next_rendering = false;
                    }
                    // No pacer call: an empty frame queues no flip and deliberately
                    // does not re-arm damage, so it leaves pacer state untouched.
                    Ev::Empty => {
                        next_rendering = false;
                    }
                    Ev::Failed => {
                        next.frame_failed();
                        next_rendering = false;
                    }
                    Ev::Flip => next.flip_completed(),
                    Ev::Pause => next.pause(),
                    Ev::Resume => next.resume(),
                }
                trace.push(ev);
                walk(next, depth - 1, trace, next_rendering);
                trace.pop();
            }
        }

        walk(FramePacer::new(), 6, &mut Vec::new(), false);
    }

    #[test]
    fn a_clean_pacer_does_not_render() {
        let mut pacer = FramePacer::new();
        pacer.begin_render();
        assert_eq!(pacer.poll(), RenderDecision::Clean);
    }

    #[test]
    fn damage_makes_a_clean_pacer_render_again() {
        let mut pacer = FramePacer::new();
        pacer.begin_render();
        assert_eq!(pacer.poll(), RenderDecision::Clean);

        pacer.damage();
        assert_eq!(pacer.poll(), RenderDecision::Render);
    }

    /// Invariant 1: never queue two flips.
    #[test]
    fn a_pending_flip_blocks_further_renders_even_with_damage() {
        let mut pacer = FramePacer::new();
        pacer.begin_render();
        pacer.frame_queued();

        assert_eq!(pacer.poll(), RenderDecision::AwaitingFlip);

        // Fresh damage must not sneak a second flip past the first.
        pacer.damage();
        assert_eq!(pacer.poll(), RenderDecision::AwaitingFlip);
    }

    #[test]
    fn a_pending_flip_is_reported_ahead_of_cleanliness() {
        let mut pacer = FramePacer::new();
        pacer.begin_render();
        pacer.frame_queued();
        // Not damaged and flip pending: the flip is the more accurate reason.
        assert!(!pacer.is_damaged());
        assert_eq!(pacer.poll(), RenderDecision::AwaitingFlip);
    }

    #[test]
    fn the_vblank_after_a_flip_allows_the_next_render_when_damaged() {
        let mut pacer = FramePacer::new();
        pacer.begin_render();
        pacer.frame_queued();

        pacer.damage();
        pacer.flip_completed();
        assert_eq!(pacer.poll(), RenderDecision::Render);
    }

    #[test]
    fn the_vblank_after_a_flip_idles_when_nothing_changed() {
        let mut pacer = FramePacer::new();
        pacer.begin_render();
        pacer.frame_queued();
        pacer.flip_completed();
        assert_eq!(pacer.poll(), RenderDecision::Clean);
    }

    /// Damage arriving mid-render must survive to the next cycle.
    #[test]
    fn damage_during_a_render_is_not_lost() {
        let mut pacer = FramePacer::new();

        pacer.begin_render();
        assert!(!pacer.is_damaged(), "begin_render clears damage up front");

        // A client commits while we are rendering.
        pacer.damage();
        pacer.frame_queued();

        pacer.flip_completed();
        assert_eq!(
            pacer.poll(),
            RenderDecision::Render,
            "mid-render damage must schedule another frame"
        );
    }

    /// Invariant 2, negative case: an empty frame must not spin.
    #[test]
    fn an_empty_frame_does_not_busy_loop() {
        let mut pacer = FramePacer::new();
        pacer.begin_render();

        // No flip was queued, so no vblank is coming; if this returned Render the
        // compositor would render empty frames forever.
        assert_eq!(pacer.poll(), RenderDecision::Clean);
        assert!(!pacer.has_pending_flip());
    }

    #[test]
    fn a_failed_frame_is_retried() {
        let mut pacer = FramePacer::new();
        pacer.begin_render();
        pacer.frame_failed();

        assert!(pacer.is_damaged());
        assert!(!pacer.has_pending_flip());
        assert_eq!(pacer.poll(), RenderDecision::Render);
    }

    #[test]
    fn a_failure_after_queueing_clears_the_phantom_flip() {
        let mut pacer = FramePacer::new();
        pacer.begin_render();
        pacer.frame_queued();
        // The commit turned out to have failed.
        pacer.frame_failed();

        assert!(!pacer.has_pending_flip());
        assert_eq!(pacer.poll(), RenderDecision::Render);
    }

    #[test]
    fn a_paused_session_never_renders() {
        let mut pacer = FramePacer::new();
        pacer.pause();

        assert!(!pacer.is_active());
        assert_eq!(pacer.poll(), RenderDecision::Inactive);

        // Damage while paused is remembered but still does not render.
        pacer.damage();
        assert_eq!(pacer.poll(), RenderDecision::Inactive);
    }

    #[test]
    fn pausing_takes_priority_over_a_pending_flip() {
        let mut pacer = FramePacer::new();
        pacer.begin_render();
        pacer.frame_queued();
        pacer.pause();
        assert_eq!(pacer.poll(), RenderDecision::Inactive);
    }

    /// The VT-switch bug this struct exists to prevent: a flip queued before the
    /// switch never reports a vblank, so resuming must not wait for it.
    #[test]
    fn resuming_clears_a_flip_that_will_never_complete() {
        let mut pacer = FramePacer::new();
        pacer.begin_render();
        pacer.frame_queued();
        pacer.pause();

        pacer.resume();

        assert!(pacer.is_active());
        assert!(pacer.is_damaged(), "the CRTC was reprogrammed while away");
        assert!(
            !pacer.has_pending_flip(),
            "waiting on the pre-switch flip would hang forever"
        );
        assert_eq!(pacer.poll(), RenderDecision::Render);
    }

    #[test]
    fn resuming_from_clean_still_repaints() {
        let mut pacer = FramePacer::new();
        pacer.begin_render();
        assert_eq!(pacer.poll(), RenderDecision::Clean);

        pacer.pause();
        pacer.resume();
        assert_eq!(
            pacer.poll(),
            RenderDecision::Render,
            "the screen contents are gone after a VT switch"
        );
    }

    /// A full steady-state cycle: commit, render, flip, vblank, idle.
    #[test]
    fn steady_state_cycle() {
        let mut pacer = FramePacer::new();

        // Startup frame.
        assert_eq!(pacer.poll(), RenderDecision::Render);
        pacer.begin_render();
        pacer.frame_queued();
        pacer.flip_completed();
        assert_eq!(pacer.poll(), RenderDecision::Clean);

        // Three client frames.
        for _ in 0..3 {
            pacer.damage();
            assert_eq!(pacer.poll(), RenderDecision::Render);
            pacer.begin_render();
            pacer.frame_queued();
            assert_eq!(pacer.poll(), RenderDecision::AwaitingFlip);
            pacer.flip_completed();
            assert_eq!(pacer.poll(), RenderDecision::Clean);
        }

        assert!(!pacer.is_damaged());
        assert!(!pacer.has_pending_flip());
        assert!(pacer.is_active());
    }

    /// Repeated damage between flips must not accumulate into extra flips.
    #[test]
    fn repeated_damage_coalesces_into_one_frame() {
        let mut pacer = FramePacer::new();
        pacer.begin_render();
        pacer.frame_queued();
        pacer.flip_completed();

        for _ in 0..5 {
            pacer.damage();
        }

        assert_eq!(pacer.poll(), RenderDecision::Render);
        pacer.begin_render();
        pacer.frame_queued();
        pacer.flip_completed();
        // All five damage events collapsed into a single frame.
        assert_eq!(pacer.poll(), RenderDecision::Clean);
    }
}

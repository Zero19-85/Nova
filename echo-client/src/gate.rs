//! The keyframe gate: never hand a decoder a frame it cannot decode.
//!
//! A hardware decoder fed a P-frame before any keyframe does not error. It
//! produces garbage — smeared blocks that slowly correct themselves as later
//! frames happen to overwrite them — and on some Android decoders it wedges the
//! codec entirely rather than recovering. Either way the failure surfaces as
//! "the picture looks broken", which is the most expensive kind of bug to trace
//! because it looks like a *decode* problem when it is really a *sequencing*
//! problem.
//!
//! Nova forces an IDR at session start, so in the healthy case this gate opens
//! on the very first frame and costs one comparison per frame forever after.
//! It exists for the unhealthy cases, which are all real:
//!
//! - the grant landed while the host was mid-GOP, so the first datagrams to
//!   arrive belong to a P-frame;
//! - the session's opening IDR was lost beyond what FEC could repair;
//! - a queue overflow dropped frames, breaking the P-frame reference chain —
//!   the decoder is now referencing frames it never received.
//!
//! The third case is why [`KeyframeGate::close`] exists. A gate that only ever
//! opened would be a startup check; re-arming it on a discontinuity makes it a
//! standing invariant, which is what a decoder actually needs.
//!
//! ## Why this lives in the shared library
//!
//! The headless CLI has no decoder to protect, so it could skip the gate. It
//! does not, deliberately: the CLI is the only place this code gets exercised
//! before Android hardware exists, and a gate that is bypassed in the one build
//! anybody runs is a gate nobody has tested. The CLI reports
//! `frames_dropped_before_keyframe` in its summary, which turns the gate from
//! invisible machinery into a diagnostic — a nonzero count on a healthy link
//! means the host is not starting sessions with an IDR.

use crate::receiver::DecodedFrame;

/// Admits frames to a decoder only once a keyframe has established a reference
/// point, and re-arms whenever that reference is lost.
#[derive(Debug, Default)]
pub struct KeyframeGate {
    open: bool,
    dropped: u64,
}

impl KeyframeGate {
    /// A closed gate — the correct initial state, because at session start no
    /// reference frame exists yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Should this frame reach the decoder?
    ///
    /// Opens on the first keyframe and stays open. Frames refused before that
    /// point are counted rather than logged: at 120 fps a per-frame log line is
    /// its own denial of service, the same reasoning [`crate::receiver`] applies
    /// to rejected datagrams.
    pub fn admit(&mut self, frame: &DecodedFrame) -> bool {
        if self.open {
            return true;
        }
        if frame.is_keyframe() {
            self.open = true;
            return true;
        }
        self.dropped += 1;
        false
    }

    /// Re-arm after a discontinuity — a dropped frame, a decoder reset, a
    /// session restart.
    ///
    /// Call this whenever frames were skipped rather than delivered. Every
    /// P-frame after a gap references frames the decoder never saw, so the
    /// stream is undecodable until the next IDR regardless of what the gate
    /// does; what the gate adds is that the decoder is not asked to *try*.
    ///
    /// Note the standing limitation: Echo has no client→host path yet, so it
    /// cannot *request* an IDR. Recovery waits for the host's next scheduled
    /// one. Once input exists, this is the natural place to trigger that
    /// request.
    pub fn close(&mut self) {
        self.open = false;
    }

    /// Whether a keyframe has been seen and frames are flowing to the decoder.
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Frames refused while waiting for a keyframe, across the gate's lifetime.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(index: u32, frame_type: u8) -> DecodedFrame {
        DecodedFrame {
            index,
            frame_type,
            data: vec![0u8; 4],
            first_shard_at: std::time::Instant::now(),
        }
    }

    #[test]
    fn a_stream_that_opens_on_a_p_frame_is_held_until_the_first_keyframe() {
        let mut gate = KeyframeGate::new();
        assert!(!gate.is_open());

        // Exactly the case that produces garbage on a hardware decoder.
        assert!(!gate.admit(&frame(1, 1)));
        assert!(!gate.admit(&frame(2, 1)));
        assert_eq!(gate.dropped(), 2);

        assert!(gate.admit(&frame(3, 2)), "the keyframe itself must be admitted");
        assert!(gate.is_open());
    }

    #[test]
    fn once_open_the_gate_passes_p_frames() {
        let mut gate = KeyframeGate::new();
        assert!(gate.admit(&frame(1, 2)));
        for i in 2..10 {
            assert!(gate.admit(&frame(i, 1)));
        }
        assert_eq!(gate.dropped(), 0, "a healthy stream must never be gated");
    }

    #[test]
    fn closing_re_arms_so_a_broken_reference_chain_is_not_fed_to_the_decoder() {
        let mut gate = KeyframeGate::new();
        assert!(gate.admit(&frame(1, 2)));
        assert!(gate.admit(&frame(2, 1)));

        // A frame was dropped: every subsequent P-frame references something
        // the decoder never received.
        gate.close();
        assert!(!gate.is_open());
        assert!(!gate.admit(&frame(4, 1)));
        assert!(gate.admit(&frame(5, 2)), "the next IDR re-establishes the reference");
        assert_eq!(gate.dropped(), 1);
    }
}

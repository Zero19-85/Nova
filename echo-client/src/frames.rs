//! The frame queue between the Rust receive loop and a platform decoder feeder.
//!
//! Shared by every bridge rather than owned by one: `echo-android` feeds
//! MediaCodec from it and `echo-xbox` feeds a Media Foundation HEVC transform,
//! and the drop policy below is a property of *live streaming*, not of either
//! platform. It lived in `echo-android` until the Xbox port needed the same
//! behaviour; two copies of a policy this subtle would have diverged.
//!
//! ## Why bounded, and why drop-oldest
//!
//! The receive loop runs as fast as the network delivers; MediaCodec accepts
//! input as fast as the hardware decodes. Those are different rates, and when
//! the decoder is the slower one — a thermal throttle, a competing app, a
//! resolution the device is at the edge of handling — something has to give.
//!
//! An unbounded queue "gives" by growing, which converts a transient decode
//! stall into unbounded memory growth *and* into latency that never recovers:
//! every frame after the stall is displayed late by the depth of the backlog.
//! For a live stream that is strictly worse than dropping, because nobody wants
//! to watch a perfectly complete video that is four seconds behind their
//! controller.
//!
//! So the queue is small and drops the **oldest** frame when full. Dropping the
//! newest would be simpler and is wrong: it would preserve stale frames and
//! discard the current one, which is exactly backwards for a live stream.
//!
//! ## Dropping breaks the reference chain
//!
//! A dropped frame is not just a missing picture. Every P-frame after it
//! references frames the decoder never received, so the stream stays undecodable
//! until the next IDR — feeding it produces smearing that slowly corrects
//! itself, which viewers read as "the stream is broken".
//!
//! The queue therefore re-arms a [`KeyframeGate`] whenever it drops, and admits
//! nothing until the next keyframe. This is the *second* gate in the pipeline:
//! [`crate::receiver::run_receiver`] holds one for session start, and this
//! one covers loss that happens after the frame was already received. They are
//! genuinely different events, which is why one gate cannot cover both.
//!
//! That request does reach the host: [`FrameQueue::push`]'s overflow branch
//! records it, the feeder reports it through [`FrameSink::take_keyframe_request`],
//! and the receiver forwards it. Under Nova's infinite GOP nothing else would
//! ever produce the IDR that reopens the gate.

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::gate::KeyframeGate;
use crate::receiver::{DecodedFrame, FrameSink};

/// How much slack the queue absorbs, in TIME.
///
/// This used to be a frame count — `CAPACITY = 3`, documented as "~50 ms at
/// 60 fps". The number was right and the unit was wrong, and at 120 fps the
/// same three slots are 25 ms. Measured on an Xbox at 3840x2160@120 on
/// 2026-09-08: `worst_frame_age_ms` 88, `dropped_overflow` climbing steadily,
/// and the identical session at 60 fps completely clean.
///
/// The consequences of overflowing are much worse than the comment above it
/// implied, and worth spelling out because they arrive disguised:
///
///   1. a dropped frame makes `last_delivered` jump, and the receiver reads
///      that as TRANSIT LOSS — so the client asks the host to invalidate
///      frames that were never lost on the wire at all
///   2. the drop re-arms the keyframe gate, so more frames are refused behind
///      it (74 of them, against 39 overflow drops, in the measured session)
///   3. at 4K the host's DPB is only 5 frames, so a 4-5 frame invalidation
///      range immediately exceeds it: `RFI range 25082-25086 >= DPB 5 —
///      forcing IDR`, and `[LTR] no usable long-term reference`. Both repair
///      rungs are structurally unable to help, so every one of these costs a
///      full 4K keyframe
///   4. which the viewer sees as the picture blinking, at four or five
///      keyframes a second and ~20 Mbps of repair traffic
///
/// So the slack is a duration now, and the frame count follows the rate. It is
/// still a jitter absorber and not a buffer — 50 ms is the same latency budget
/// the original chose, just held constant across frame rates instead of
/// halving at exactly the rate this client exists to reach.
const SLACK_MS: u32 = 50;

/// Frames held before the decoder, sized from [`SLACK_MS`] at [`MAX_FPS`].
///
/// Against MAX_FPS rather than the negotiated rate for the same reason the
/// delay line is: over-sizing costs a few `Vec` slots, and under-sizing costs
/// the cascade above.
const CAPACITY: usize = (SLACK_MS * MAX_FPS).div_ceil(1000) as usize;

/// Frame rate the delay line sizes itself against.
///
/// Only ever used to convert a delay in milliseconds into a frame count, and
/// deliberately the highest rate Nova streams rather than the negotiated one: an
/// under-sized delay line would overflow, and overflow here is not a dropped
/// frame but a closed keyframe gate and a frozen picture. Over-sizing costs a
/// few `Vec` slots.
const MAX_FPS: u32 = 120;

/// Largest video delay that can be asked for: a third of a second.
///
/// Well past any audio latency a device plausibly imposes — the measured floor
/// on the dev hardware is 190 ms — and short enough that a nonsense value from
/// the sync engine cannot turn the picture into a slideshow.
const MAX_DELAY_MS: u32 = 350;

#[derive(Debug, Default, Clone, Copy)]
pub struct QueueStats {
    /// Frames evicted because the decoder was not keeping up.
    pub dropped_overflow: u64,
    /// Frames withheld while re-arming after a drop.
    pub dropped_waiting_keyframe: u64,
    pub delivered: u64,
    pub depth: usize,
    /// Milliseconds the most recent frame spent between its first shard
    /// arriving and reaching the decoder.
    pub last_frame_age_ms: u32,
    /// The worst such figure this session. Kept because latency that builds and
    /// then drains leaves no trace in an instantaneous reading — and "it lags,
    /// then catches up when I stop" is exactly that shape.
    pub worst_frame_age_ms: u32,
}

struct Inner {
    frames: VecDeque<DecodedFrame>,
    gate: KeyframeGate,
    closed: bool,
    stats: QueueStats,
    /// Set when the gate re-armed, cleared when the request has been passed on.
    ///
    /// Without this the gate is a trap rather than a guard: Nova runs an
    /// infinite GOP, so "wait for the next keyframe" means "wait forever".
    /// Live 2026-08-15: one overflow while the decoder warmed up froze the
    /// picture on the first frame for the rest of the session.
    keyframe_wanted: bool,
    /// Inclusive range of wire indices this queue dropped, cleared when the
    /// request has been passed on.
    ///
    /// The cheap counterpart to `keyframe_wanted`. An overflow drop is the
    /// one loss the client can describe exactly: the frame arrived, was
    /// authenticated, and was then discarded here, so its index is known.
    /// That is precisely what the encoder needs to re-point a reference
    /// instead of coding a whole intra frame.
    ///
    /// Widened rather than replaced while a request is outstanding: two
    /// overflows before the next receive-loop tick broke the chain in both
    /// places, and repairing only the later one leaves the earlier gap in the
    /// DPB forever.
    lost_frames: Option<(u32, u32)>,
    /// How long to hold each frame before the decoder may have it — the A/V
    /// sync delay. Zero disables the delay line entirely.
    delay: Duration,
    /// `CAPACITY` plus however many frames `delay` holds in flight. Recomputed
    /// whenever the delay changes.
    capacity: usize,
}

/// A bounded, drop-oldest frame queue with a blocking, timed pop.
///
/// The blocking pop is the whole point of the pull model: Kotlin's feeder thread
/// parks *inside* Rust rather than polling, so there is no busy-wait and no
/// callback into the JVM on the frame path — no `AttachCurrentThread`, no
/// `GlobalRef`, and no per-frame allocation.
pub struct FrameQueue {
    inner: Mutex<Inner>,
    ready: Condvar,
}

impl Default for FrameQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameQueue {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                frames: VecDeque::with_capacity(CAPACITY),
                // Opens on the first keyframe. `run_receiver`'s gate has
                // already guaranteed that, so in the healthy case this opens
                // immediately and never closes again.
                gate: KeyframeGate::new(),
                closed: false,
                stats: QueueStats::default(),
                keyframe_wanted: false,
                lost_frames: None,
                delay: Duration::ZERO,
                capacity: CAPACITY,
            }),
            ready: Condvar::new(),
        }
    }

    /// Hold every frame `delay_ms` past its arrival before the decoder may have
    /// it — the A/V sync engine's one control. Returns the value applied.
    ///
    /// ## Why the delay goes on VIDEO
    ///
    /// Audio latency on this path is ~190 ms and most of it is a hardware floor:
    /// the device's own output stage costs ~70 ms with the fast mixer path
    /// already granted, and the jitter buffer's 80 ms is burst insurance that was
    /// measured to be necessary. Video, by contrast, is rendered as soon as it
    /// decodes. So audio cannot be pulled forward and video can be pushed back;
    /// that asymmetry decides which side moves.
    ///
    /// ## Why capacity has to move with it
    ///
    /// [`CAPACITY`] is 3 because a backlog means the decoder is behind, and the
    /// right answer is to drop and re-gate. A delay line makes a backlog the
    /// NORMAL state — 190 ms at 60 fps is eleven frames in flight — so leaving
    /// capacity at 3 would trip the overflow path on every frame, and overflow
    /// here closes the keyframe gate. The picture would freeze permanently the
    /// moment sync was switched on.
    ///
    /// Sized against [`MAX_FPS`] rather than the negotiated rate: too many slots
    /// costs a few pointers, too few costs the stream.
    pub fn set_delay_ms(&self, delay_ms: u32) -> u32 {
        let delay_ms = delay_ms.min(MAX_DELAY_MS);
        let mut inner = self.lock();
        inner.delay = Duration::from_millis(delay_ms as u64);
        inner.capacity = CAPACITY + (delay_ms * MAX_FPS).div_ceil(1000) as usize;
        drop(inner);
        // A shortened delay can make frames due immediately; a waiter parked on
        // the old deadline must re-evaluate rather than sleep through it.
        self.ready.notify_all();
        delay_ms
    }

    /// The delay currently applied, in milliseconds.
    pub fn delay_ms(&self) -> u32 {
        self.lock().delay.as_millis() as u32
    }

    /// Whether a drop has left this queue waiting for a keyframe, clearing the
    /// flag so one request produces one ask.
    ///
    /// Repeatable by design: if the requested keyframe is itself lost, the next
    /// overflow sets the flag again and recovery is retried.
    pub fn take_keyframe_request(&self) -> bool {
        let mut inner = self.lock();
        std::mem::replace(&mut inner.keyframe_wanted, false)
    }

    /// The range of wire indices this queue dropped, clearing it so one loss
    /// produces one ask.
    ///
    /// Repeatable for the same reason as `take_keyframe_request`: if the
    /// recovery frame is itself lost, the next overflow re-arms it.
    pub fn take_invalidation_request(&self) -> Option<(u32, u32)> {
        let mut inner = self.lock();
        inner.lost_frames.take()
    }

    /// Ask the host for a keyframe on the next receive-loop turn.
    ///
    /// Exists for the Android Surface lifecycle. Backgrounding destroys the
    /// SurfaceView's Surface and returning creates a new one; the decoder is
    /// re-attached with `setOutputSurface`, which preserves its reference
    /// chain — but "preserves" is a decoder-implementation promise, not a
    /// guarantee, and under Nova's infinite GOP a chain that did break is
    /// unrecoverable without asking. One IDR is cheap; a permanently frozen
    /// picture is not.
    ///
    /// **Deliberately does NOT close the gate.** Overflow does, because there
    /// the chain is known to be broken and admitting P-frames would feed the
    /// decoder garbage. Here it is merely *suspect*: `setOutputSurface` keeps
    /// the decoder able to use the frames already in flight, so closing the
    /// gate would discard good frames — and, worse, would introduce a new
    /// freeze mode, since a lost IDR leaves nothing to re-arm the request and
    /// the gate would stay shut forever. Flagging only is strictly safer: if
    /// the IDR never comes, the stream is no worse off than before the ask.
    pub fn request_keyframe(&self) {
        let mut inner = self.lock();
        if inner.closed {
            return;
        }
        inner.keyframe_wanted = true;
    }

    /// Offer a frame. Never blocks — the receive loop must not be stalled by a
    /// slow decoder, because the socket keeps filling either way.
    pub fn push(&self, frame: DecodedFrame) {
        let mut inner = self.lock();
        if inner.closed {
            return;
        }
        if inner.frames.len() >= inner.capacity {
            let dropped = inner.frames.pop_front().map(|f| f.index);
            inner.stats.dropped_overflow += 1;
            // The chain is broken; nothing is decodable until the host repairs
            // it -- under an infinite GOP that only happens if we ask.
            inner.gate.close();
            match dropped {
                // We know exactly which frame the decoder will never see, so
                // ask for the cheap repair. The host answers with a type-5
                // recovery frame, or falls back to a keyframe by itself if
                // NVENC cannot re-point that far -- either one re-opens the
                // gate, so this is never a way to get stuck.
                Some(index) => {
                    let (first, last) = inner.lost_frames.unwrap_or((index, index));
                    inner.lost_frames = Some((first.min(index), last.max(index)));
                }
                // An empty queue cannot report what it lost; only a full
                // keyframe is guaranteed to repair an unknown gap.
                None => inner.keyframe_wanted = true,
            }
        }
        if !inner.gate.admit(&frame) {
            inner.stats.dropped_waiting_keyframe += 1;
            return;
        }
        inner.frames.push_back(frame);
        drop(inner);
        self.ready.notify_one();
    }

    /// Take the next frame, waiting up to `timeout`.
    ///
    /// `None` means "nothing yet" or "closed" — the caller distinguishes them
    /// with [`FrameQueue::is_closed`]. A timeout is not an error: it is the
    /// normal state of a feeder thread that is keeping up.
    pub fn pop_timeout(&self, timeout: Duration) -> Option<DecodedFrame> {
        let deadline = Instant::now() + timeout;
        let mut inner = self.lock();
        loop {
            // The delay line. A frame is not eligible until `delay` has elapsed
            // since its first shard landed, which is the same clock
            // `last_frame_age_ms` already reports — so the delay is measured
            // from arrival, not from when this thread happened to ask, and it
            // cannot accumulate scheduling jitter on top of itself.
            //
            // Deliberately NOT a sleep before returning the frame: parking with
            // the lock released lets `push` keep filling and `set_delay_ms` cut
            // the wait short, and it keeps the "is it due" test in one place.
            let due_in = if inner.delay.is_zero() {
                Duration::ZERO
            } else {
                match inner.frames.front() {
                    Some(f) => inner.delay.saturating_sub(f.first_shard_at.elapsed()),
                    None => Duration::ZERO,
                }
            };

            if due_in.is_zero() {
                if let Some(frame) = inner.frames.pop_front() {
                    inner.stats.delivered += 1;
                    // How long this frame spent inside the client, from its
                    // first shard landing on the socket to being handed to the
                    // decoder. With sync on, this INCLUDES the delay — which is
                    // correct and is the point: it is the video half of the A/V
                    // latency, measured on one clock with no agreement needed
                    // with the host about time.
                    let age = frame.first_shard_at.elapsed();
                    inner.stats.last_frame_age_ms = age.as_millis() as u32;
                    inner.stats.worst_frame_age_ms =
                        inner.stats.worst_frame_age_ms.max(age.as_millis() as u32);
                    return Some(frame);
                }
            }
            if inner.closed {
                return None;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            // Wake for whichever comes first: the caller's timeout, or the head
            // frame becoming due. Sleeping the full timeout when a frame is due
            // sooner would turn the delay into the caller's poll interval — the
            // exact mistake `learn_ticker` made on the host, where a 500 ms
            // drain became the input path's latency.
            let wait = if due_in.is_zero() { remaining } else { remaining.min(due_in) };
            // Condvar wakeups can be spurious, so the loop re-checks rather
            // than trusting the wake.
            let (guard, _) = self
                .ready
                .wait_timeout(inner, wait)
                .unwrap_or_else(|e| e.into_inner());
            inner = guard;
        }
    }

    /// Wake every waiter and refuse further frames. Idempotent, so the session
    /// teardown path and an explicit close cannot fight.
    pub fn close(&self) {
        let mut inner = self.lock();
        inner.closed = true;
        inner.frames.clear();
        drop(inner);
        self.ready.notify_all();
    }

    pub fn is_closed(&self) -> bool {
        self.lock().closed
    }

    pub fn stats(&self) -> QueueStats {
        let inner = self.lock();
        QueueStats { depth: inner.frames.len(), ..inner.stats }
    }

    /// A poisoned lock here means another thread panicked while holding it. The
    /// data behind it is a queue of frames — there is no invariant a panic could
    /// have left half-applied, so recovering is strictly better than propagating
    /// a panic into a JNI boundary.
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Adapts the queue to the receive loop's sink interface.
pub struct QueueSink(pub std::sync::Arc<FrameQueue>);

impl FrameSink for QueueSink {
    fn on_frame(&mut self, frame: DecodedFrame) {
        self.0.push(frame);
    }

    fn take_keyframe_request(&mut self) -> bool {
        self.0.take_keyframe_request()
    }

    fn take_invalidation_request(&mut self) -> Option<(u32, u32)> {
        self.0.take_invalidation_request()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// The trap the whole `capacity` field exists to avoid.
    ///
    /// CAPACITY is 3 because a backlog normally means the decoder is behind, and
    /// the answer is to drop and re-gate. A delay line makes a backlog the
    /// NORMAL state — 190 ms at 60 fps is eleven frames in flight — so a fixed
    /// capacity would trip the overflow path on every frame, and overflow here
    /// closes the keyframe gate. Under Nova's infinite GOP that is a permanently
    /// frozen picture the moment sync is switched on.
    #[test]
    fn a_delay_line_does_not_overflow_the_queue_or_close_the_gate() {
        let q = FrameQueue::new();
        q.set_delay_ms(190);

        // What 190 ms of 60 fps actually holds in flight: twelve frames, four
        // times the undelayed capacity of 3.
        let in_flight = 190 * 60 / 1000 + 1;
        q.push(frame(1, 2, 16));
        for i in 2..=in_flight {
            q.push(frame(i, 1, 16));
        }

        let s = q.stats();
        assert_eq!(s.dropped_overflow, 0, "the delay line must not read as a slow decoder");
        assert_eq!(s.dropped_waiting_keyframe, 0, "and must not close the gate");
        assert!(!q.take_keyframe_request(), "no repair should have been asked for");
    }

    /// The delay is measured from the frame's ARRIVAL, so it is a fixed offset
    /// rather than something that compounds with how often the caller asks.
    #[test]
    fn a_frame_is_withheld_until_its_delay_has_elapsed() {
        let q = FrameQueue::new();
        q.set_delay_ms(60);
        q.push(frame(1, 2, 16));

        // Too early: the frame exists but is not due, and the caller waits.
        assert!(q.pop_timeout(Duration::from_millis(10)).is_none());
        assert_eq!(q.stats().depth, 1, "withheld, not dropped");

        // Waiting past the delay yields it — and the pop must not sleep the
        // whole timeout when the frame comes due sooner.
        let start = Instant::now();
        let got = q.pop_timeout(Duration::from_millis(500));
        assert_eq!(got.map(|f| f.index), Some(1));
        assert!(start.elapsed() < Duration::from_millis(300), "slept past the due time");
    }

    /// Turning sync off must release what is already held, not strand it behind
    /// a deadline set under the old delay.
    #[test]
    fn shortening_the_delay_releases_held_frames_immediately() {
        let q = FrameQueue::new();
        q.set_delay_ms(300);
        q.push(frame(1, 2, 16));
        assert!(q.pop_timeout(Duration::from_millis(5)).is_none());

        q.set_delay_ms(0);
        assert_eq!(
            q.pop_timeout(Duration::from_millis(50)).map(|f| f.index),
            Some(1),
            "a frame held under the old delay must come free at once"
        );
    }

    #[test]
    fn the_delay_is_clamped_to_something_sane() {
        let q = FrameQueue::new();
        assert_eq!(q.set_delay_ms(10_000), MAX_DELAY_MS, "a nonsense value must not stick");
        assert_eq!(q.delay_ms(), MAX_DELAY_MS);
    }

    /// With sync off the queue must behave exactly as it always has — same
    /// capacity, same drop policy, no timing behaviour at all.
    #[test]
    fn zero_delay_is_the_original_queue() {
        let q = FrameQueue::new();
        assert_eq!(q.delay_ms(), 0);
        q.push(frame(1, 2, 16));
        // Relative to CAPACITY, not a hardcoded 6. The literal was written when
        // CAPACITY was 3 and quietly stopped testing anything the moment the
        // queue was sized from a duration instead of a frame count — six frames
        // fit exactly, nothing overflowed, and the assertion failed for the one
        // reason it was never meant to catch.
        for i in 2..=(CAPACITY as u32 + 2) {
            q.push(frame(i, 1, 16));
        }
        assert!(q.stats().dropped_overflow > 0, "still a shallow drop-oldest queue");
        // And it never withholds.
        assert!(q.pop_timeout(Duration::from_millis(1)).is_some());
    }

    fn frame(index: u32, frame_type: u8, len: usize) -> DecodedFrame {
        DecodedFrame {
            index,
            frame_type,
            data: vec![index as u8; len],
            // "Arrived now" — these tests are about queue ordering and drop
            // policy, not about age, and a frame built at test time is
            // genuinely zero milliseconds old.
            first_shard_at: std::time::Instant::now(),
        }
    }

    #[test]
    fn frames_come_out_in_the_order_they_went_in() {
        let q = FrameQueue::new();
        q.push(frame(1, 2, 4));
        q.push(frame(2, 1, 4));
        assert_eq!(q.pop_timeout(Duration::from_millis(10)).unwrap().index, 1);
        assert_eq!(q.pop_timeout(Duration::from_millis(10)).unwrap().index, 2);
    }

    #[test]
    fn a_full_queue_drops_the_oldest_frame_not_the_newest() {
        let q = FrameQueue::new();
        q.push(frame(1, 2, 4)); // keyframe opens the gate
        for i in 2..=CAPACITY as u32 {
            q.push(frame(i, 1, 4));
        }
        // This overflows. The stale frame must go, not the fresh one — and the
        // drop must re-arm the gate, so the P-frame that caused the overflow is
        // itself withheld as undecodable.
        q.push(frame(99, 1, 4));

        let stats = q.stats();
        assert_eq!(stats.dropped_overflow, 1);
        assert_eq!(stats.dropped_waiting_keyframe, 1, "a post-drop P-frame is not decodable");
        assert_eq!(
            q.pop_timeout(Duration::from_millis(10)).unwrap().index,
            2,
            "frame 1 was the oldest and should have been evicted"
        );
    }

    /// An overflow drop names the frame it lost, so the host can re-point a
    /// reference instead of coding a whole intra frame. The expensive repair
    /// must NOT also be requested -- asking for both would pay for the IDR
    /// anyway and make the cheap path pointless.
    #[test]
    fn an_overflow_drop_names_the_frame_it_lost() {
        let q = FrameQueue::new();
        q.push(frame(1, 2, 4));
        for i in 2..=(CAPACITY as u32) {
            q.push(frame(i, 1, 4));
        }
        assert_eq!(q.take_invalidation_request(), None, "nothing has been dropped yet");

        q.push(frame(99, 1, 4)); // fills past capacity, evicting frame 1
        assert_eq!(q.stats().dropped_overflow, 1);
        assert_eq!(
            q.take_invalidation_request(),
            Some((1, 1)),
            "the evicted frame's index is exactly what the encoder needs"
        );
        assert!(!q.take_keyframe_request(), "a named loss must not also cost an IDR");
        assert_eq!(q.take_invalidation_request(), None, "one loss, one ask");
    }

    /// Two drops before the receive loop's next tick must both be repaired.
    /// Reporting only the later one leaves the earlier gap in the DPB, and
    /// the picture stays broken in a way no further request would fix.
    #[test]
    fn drops_that_arrive_together_widen_into_one_range() {
        let q = FrameQueue::new();
        q.push(frame(1, 2, 4));
        for i in 2..=(CAPACITY as u32) {
            q.push(frame(i, 1, 4));
        }
        q.push(frame(98, 1, 4)); // evicts 1, and closes the gate

        // A keyframe re-opens the gate, so the queue can fill and break a
        // second time before the receive loop next collects the request.
        q.push(frame(50, 2, 4));
        for i in 51..=(CAPACITY as u32 + 49) {
            q.push(frame(i, 1, 4));
        }
        q.push(frame(99, 1, 4)); // evicts 2

        assert_eq!(q.stats().dropped_overflow, 2);
        assert_eq!(
            q.take_invalidation_request(),
            Some((1, 2)),
            "both losses must be covered by the range"
        );
    }

    /// A recovery frame is a P-frame the encoder coded against a reference it
    /// proved survives, so it re-opens the gate exactly like a keyframe. If it
    /// did not, the answer to an invalidation would be refused as undecodable
    /// and the picture would stay frozen while the host believed it had
    /// already repaired the stream.
    #[test]
    fn a_recovery_frame_reopens_the_gate_after_a_drop() {
        let q = FrameQueue::new();
        q.push(frame(1, 2, 4));
        for i in 2..=(CAPACITY as u32 + 2) {
            q.push(frame(i, 1, 4));
        }
        assert!(q.stats().dropped_overflow > 0, "the gate should now be closed");
        while q.pop_timeout(Duration::from_millis(10)).is_some() {}

        let refused = q.stats().dropped_waiting_keyframe;
        q.push(frame(200, 1, 4));
        assert_eq!(
            q.stats().dropped_waiting_keyframe,
            refused + 1,
            "an ordinary P-frame is still undecodable"
        );

        q.push(frame(201, 5, 4)); // the host's RFI recovery frame
        assert_eq!(
            q.pop_timeout(Duration::from_millis(10)).map(|f| f.index),
            Some(201),
            "a type-5 recovery frame must be admitted"
        );
    }

    #[test]
    fn the_queue_recovers_on_the_next_keyframe_after_a_drop() {
        let q = FrameQueue::new();
        q.push(frame(1, 2, 4));
        for i in 2..=(CAPACITY as u32 + 2) {
            q.push(frame(i, 1, 4));
        }
        assert!(q.stats().dropped_overflow > 0);

        // Drain, then present an IDR: the gate must open again.
        while q.pop_timeout(Duration::from_millis(1)).is_some() {}
        q.push(frame(500, 2, 4));
        assert_eq!(q.pop_timeout(Duration::from_millis(10)).unwrap().index, 500);
    }

    #[test]
    fn a_timed_pop_returns_rather_than_hanging_when_nothing_arrives() {
        let q = FrameQueue::new();
        let start = Instant::now();
        assert!(q.pop_timeout(Duration::from_millis(30)).is_none());
        assert!(start.elapsed() >= Duration::from_millis(25), "must actually wait");
    }

    #[test]
    fn closing_wakes_a_blocked_feeder_thread() {
        // The teardown case: without this, closing a session would leave
        // Kotlin's feeder thread parked in Rust until its timeout expired.
        let q = Arc::new(FrameQueue::new());
        let waiter = {
            let q = q.clone();
            std::thread::spawn(move || q.pop_timeout(Duration::from_secs(30)))
        };
        std::thread::sleep(Duration::from_millis(50));
        q.close();
        assert!(waiter.join().expect("feeder thread must not panic").is_none());
        assert!(q.is_closed());
    }

    #[test]
    fn a_closed_queue_accepts_nothing_further() {
        let q = FrameQueue::new();
        q.close();
        q.push(frame(1, 2, 4));
        assert!(q.pop_timeout(Duration::from_millis(5)).is_none());
    }
}

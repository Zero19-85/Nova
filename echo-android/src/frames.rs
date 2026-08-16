//! The frame queue between the Rust receive loop and Kotlin's decoder feeder.
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
//! [`echo_client::receiver::run_receiver`] holds one for session start, and this
//! one covers loss that happens after the frame was already received. They are
//! genuinely different events, which is why one gate cannot cover both.
//!
//! Standing limitation: Echo has no client→host path yet, so it cannot request
//! an IDR — recovery waits for the host's next scheduled one. When input lands,
//! [`FrameQueue::push`]'s overflow branch is where that request belongs.

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use echo_client::gate::KeyframeGate;
use echo_client::receiver::{DecodedFrame, FrameSink};

/// Frames held before the decoder. Deliberately shallow: this is a jitter
/// absorber, not a buffer. At 60 fps it is ~50 ms of slack, which covers
/// scheduling noise without hiding a decoder that has genuinely fallen behind.
const CAPACITY: usize = 3;

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
            }),
            ready: Condvar::new(),
        }
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

    /// Offer a frame. Never blocks — the receive loop must not be stalled by a
    /// slow decoder, because the socket keeps filling either way.
    pub fn push(&self, frame: DecodedFrame) {
        let mut inner = self.lock();
        if inner.closed {
            return;
        }
        if inner.frames.len() >= CAPACITY {
            inner.frames.pop_front();
            inner.stats.dropped_overflow += 1;
            // The chain is broken; nothing is decodable until the next IDR —
            // and under an infinite GOP one only exists if we ask for it.
            inner.gate.close();
            inner.keyframe_wanted = true;
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
            if let Some(frame) = inner.frames.pop_front() {
                inner.stats.delivered += 1;
                // How long this frame spent inside the client, from its first
                // shard landing on the socket to being handed to the decoder.
                //
                // Reported because a pointer that trails the hand feels the
                // same whether the delay is in the input path or in the video
                // returning, and every previous attempt to tell those apart was
                // a guess. This measures the video half on one clock, so it
                // needs no agreement with the host about time.
                let age = frame.first_shard_at.elapsed();
                inner.stats.last_frame_age_ms = age.as_millis() as u32;
                inner.stats.worst_frame_age_ms = inner.stats.worst_frame_age_ms.max(age.as_millis() as u32);
                return Some(frame);
            }
            if inner.closed {
                return None;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            // Condvar wakeups can be spurious, so the loop re-checks rather
            // than trusting the wake.
            let (guard, _) = self
                .ready
                .wait_timeout(inner, remaining)
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

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

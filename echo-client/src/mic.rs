//! Client-side microphone accounting.
//!
//! The host reports what it *received* and what its jitter buffer did with it.
//! Neither number can answer the question that matters when a user says the
//! microphone sounds broken: **did the client ever produce the audio?** An
//! encoder that stalls, a capture thread the system throttled, and a path that
//! drops packets all look identical from the host — silence.
//!
//! So the client keeps its own half of the measurement, and the session report
//! carries it to the host log where the two sit side by side. This is the same
//! discipline `input`'s counters exist for, and it was earned the same way:
//! every question in the input debugging session had half its answer on each
//! machine, and correlating them meant reading numbers off a phone screen.
//!
//! ## The gap is the diagnostic
//!
//! Packet *count* says the encoder is alive; the longest **gap** between
//! packets says whether it is keeping up. A 20 ms encoder that occasionally
//! goes 300 ms without emitting is producing an audible dropout that no
//! host-side counter can distinguish from network loss — the host simply never
//! sees those sequences, exactly as if they had been dropped in flight. The
//! worst gap is the one number that separates those two causes, so it is
//! tracked even though nothing else here needs a clock.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

static PACKETS: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);
static REFUSED: AtomicU64 = AtomicU64::new(0);
static WORST_GAP_MS: AtomicU32 = AtomicU32::new(0);
static LAST_PACKET: Mutex<Option<Instant>> = Mutex::new(None);

/// What the client knows about its own microphone.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MicClientStats {
    /// Encoded packets handed to the transport.
    pub packets: u64,
    /// Total encoded bytes — the honest measure of what the uplink is paying.
    pub bytes: u64,
    /// Payloads the transport refused (empty or oversized). Always zero in a
    /// healthy session; nonzero means the encoder is emitting something the
    /// channel will not carry, which is a bug rather than a condition.
    pub refused: u64,
    /// Longest observed gap between consecutive packets, in milliseconds.
    pub worst_gap_ms: u32,
}

/// Record one encoded packet on its way out.
pub fn record_sent(bytes: usize) {
    PACKETS.fetch_add(1, Ordering::Relaxed);
    BYTES.fetch_add(bytes as u64, Ordering::Relaxed);

    let now = Instant::now();
    let mut last = LAST_PACKET.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(previous) = last.replace(now) {
        let gap = now.duration_since(previous).as_millis().min(u32::MAX as u128) as u32;
        WORST_GAP_MS.fetch_max(gap, Ordering::Relaxed);
    }
}

/// Record a payload the transport would not carry.
pub fn record_refused() {
    REFUSED.fetch_add(1, Ordering::Relaxed);
}

pub fn stats() -> MicClientStats {
    MicClientStats {
        packets: PACKETS.load(Ordering::Relaxed),
        bytes: BYTES.load(Ordering::Relaxed),
        refused: REFUSED.load(Ordering::Relaxed),
        worst_gap_ms: WORST_GAP_MS.load(Ordering::Relaxed),
    }
}

/// Clear the counters at the start of a session.
///
/// These are process-global, so without this the second session of a process
/// would inherit the first one's worst gap — including the large one every
/// session ends with when the microphone stops before the session does. A stale
/// worst-gap is worse than none: it looks like evidence.
pub fn reset() {
    PACKETS.store(0, Ordering::Relaxed);
    BYTES.store(0, Ordering::Relaxed);
    REFUSED.store(0, Ordering::Relaxed);
    WORST_GAP_MS.store(0, Ordering::Relaxed);
    *LAST_PACKET.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The counters are process-global, so these run as one test — separate
    /// `#[test]` functions share the statics and would race each other.
    #[test]
    fn counters_accumulate_and_reset() {
        reset();
        assert_eq!(stats(), MicClientStats::default());

        record_sent(40);
        record_sent(38);
        record_refused();

        let s = stats();
        assert_eq!(s.packets, 2);
        assert_eq!(s.bytes, 78);
        assert_eq!(s.refused, 1);

        // The first packet has no predecessor, so it cannot contribute a gap.
        // Two packets sent back to back give a gap near zero, not a spurious
        // large one — which is what a `None` mishandled as `0` would produce.
        assert!(s.worst_gap_ms < 1_000, "an immediate second packet is not a stall");

        reset();
        assert_eq!(stats(), MicClientStats::default(), "a new session starts clean");
    }
}

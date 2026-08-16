//! Sealed, unreliable microphone datagrams — client to host.
//!
//! ## Why this is not [`crate::input_channel`] with a different tag
//!
//! The two are siblings: both travel client → host, both are sealed with the
//! session's [`SessionKeys`], both ride the punched media socket, and both
//! deliberately refuse the reliable control channel. Everything *below* the
//! payload is the same idea. But two of `input_channel`'s decisions are wrong
//! for audio, and copying them wholesale would have been the easy mistake.
//!
//! **Redundancy.** Every input datagram repeats the last
//! [`REDUNDANCY`](crate::input_channel::REDUNDANCY) packets, because input
//! carries *state transitions*: a lost key-up strands a key held down on the
//! host until something else releases it, so a packet has to survive a loss.
//! Audio carries no state. A lost 20 ms packet is 20 ms of concealment and the
//! next packet is already correct — there is nothing to strand. Repeating each
//! packet three times would triple the upstream cost of the microphone forever
//! to insure a loss that heals itself in a twentieth of a second, on the one
//! link (a phone's uplink) least able to spare it. So there is no redundancy
//! here, and that is a considered omission rather than an unfinished one.
//!
//! **Strict ordering.** `InputReceiver` keeps a single high-water mark and
//! drops anything at or behind it, which is right for input: an older pointer
//! delta describes a position that has already been superseded, so delivering
//! it late would move the cursor *backwards*. An audio packet that arrives late
//! but still ahead of the playout point is perfectly good — the jitter buffer
//! puts it back in its place by sequence number and nobody hears anything. A
//! high-water mark would discard it and manufacture a gap that the network did
//! not actually cause. So this receiver keeps a [sliding window](MicReceiver)
//! instead, and reports reordering separately from loss.
//!
//! ## What the sequence number is
//!
//! One datagram carries exactly one payload, so the datagram counter *is* the
//! sequence number and there is only one of them. `input_channel` needs two —
//! a per-datagram counter for the nonce and a per-packet sequence for the
//! deduplicator — precisely because its datagrams carry several packets. Here
//! that distinction would be two names for the same integer.
//!
//! Bundling several packets per datagram was considered and rejected: it would
//! buy a lower packet rate at the cost of adding the bundle's own duration to
//! the latency of every packet in it, which is the one thing a microphone path
//! cannot spend. At 20 ms frames this is 50 datagrams per second.
//!
//! ## The window is the replay defence
//!
//! Same property `input_channel` has, by the same reasoning: a datagram
//! replayed by an attacker carries a sequence number the receiver has already
//! recorded, so it is dropped by the deduplicator with no separate replay
//! window to keep in agreement. The window here is 64 wide, so a replay is
//! caught for 64 packets — 1.3 seconds at 20 ms frames — after which the
//! sequence is out of the window and refused as *late* instead. Either way it
//! is not delivered, which is the property that matters.
//!
//! ## Wire format
//!
//! ```text
//!   [0xE4][flags u8][seq u32 BE][sealed …]
//!
//!   sealed = AES-128-GCM(STREAM_MIC, seq) over the payload bytes
//! ```
//!
//! The sequence is in the clear because the receiver needs it to derive the
//! nonce before it can decrypt — the same reason `rtp.rs` sends the frame index
//! unencrypted. It is nonetheless *authenticated*: it goes into both the nonce
//! and the associated data, so a datagram whose sequence was altered in flight
//! fails the tag rather than being reordered into a lie. `flags` is reserved and
//! sent as zero so a later need is additive rather than a format break.
//!
//! This module knows nothing about Opus, or about audio at all. It carries
//! opaque payload bytes, exactly as `input_channel` carries opaque GameStream
//! packets — the codec belongs to the two endpoints, not to the transport.

use crate::demux::ECHO_MIC;
use crate::media_crypto::{CryptoError, SessionKeys, CRYPTO_OVERHEAD, STREAM_MIC};

/// Bytes before the sealed payload: tag, flags, sequence.
pub const HEADER_LEN: usize = 6;

/// Largest payload one datagram may carry.
///
/// 1275 bytes is the largest packet Opus will ever emit, so this never rejects
/// a legitimate frame while keeping the sealed datagram (1275 + 6 + 16 = 1297,
/// plus 28 for IP and UDP) comfortably inside a 1500-byte MTU. A payload that
/// exceeds it is a bug on the sending side, not a large frame.
pub const MAX_PAYLOAD: usize = 1275;

/// How far behind the newest sequence a packet may arrive and still be
/// delivered.
///
/// Sized in packets rather than milliseconds because this layer does not know
/// the frame duration. At 20 ms frames it is 1.3 seconds — far beyond any
/// reordering a real path produces, and comfortably longer than the host's
/// jitter buffer, so the window is never the thing that discards a packet the
/// buffer could still have used.
pub const WINDOW_PACKETS: u32 = 64;

/// The AAD's frame-type slot. Audio has no frame types; a fixed value keeps the
/// tag committing to a constant rather than leaving a byte an attacker could
/// vary.
const PURPOSE: u8 = 0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MicError {
    /// Not a microphone datagram, or shorter than a header plus a tag.
    Malformed,
    /// The tag did not verify: forged, corrupted, or sealed under another key.
    Authentication,
    /// Nothing to send, or a datagram that opened to nothing. Reachable on the
    /// receive side only with a valid key, so it means a version mismatch
    /// between peers rather than an attack.
    EmptyPayload,
    /// Larger than [`MAX_PAYLOAD`]. Refused rather than truncated: half an
    /// Opus packet is not a quieter Opus packet, it is a decoder error.
    PayloadTooLarge(usize),
    /// The 32-bit sequence space is exhausted. Continuing would repeat a GCM
    /// nonce, which leaks the authentication subkey, so the sender stops
    /// instead. Unreachable in practice: at 50 packets per second this is
    /// nearly three years of continuous speech in one session.
    Exhausted,
}

impl std::fmt::Display for MicError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed => write!(f, "not a well-formed microphone datagram"),
            Self::Authentication => write!(
                f,
                "microphone datagram failed authentication — forged, corrupted, or not ours"
            ),
            Self::EmptyPayload => write!(f, "microphone datagram carried no payload"),
            Self::PayloadTooLarge(n) => {
                write!(f, "microphone payload of {n} bytes exceeds the {MAX_PAYLOAD}-byte limit")
            }
            Self::Exhausted => write!(f, "microphone sequence space exhausted — session must restart"),
        }
    }
}

impl std::error::Error for MicError {}

impl From<CryptoError> for MicError {
    fn from(e: CryptoError) -> Self {
        match e {
            CryptoError::Truncated => Self::Malformed,
            _ => Self::Authentication,
        }
    }
}

// ── Sending ─────────────────────────────────────────────────────────────────

/// The client half: turns encoded audio packets into sealed datagrams.
pub struct MicSender {
    keys: SessionKeys,
    next_seq: u32,
}

impl MicSender {
    pub fn new(keys: SessionKeys) -> Self {
        // Starts at 1 for the reason `rtp.rs` starts frame indices there: zero
        // stays available as "nothing yet" on the receiver, with no separate
        // sentinel for the two sides to keep in agreement.
        Self { keys, next_seq: 1 }
    }

    /// The sequence the next successful [`datagram`](Self::datagram) will carry.
    pub fn next_sequence(&self) -> u32 {
        self.next_seq
    }

    /// Seal one payload into one datagram.
    ///
    /// A rejected payload — empty or oversized — does **not** consume a
    /// sequence number. That is deliberate twice over: a burned sequence would
    /// waste a nonce, and more importantly the receiver reads a missing
    /// sequence as packet loss, so a local validation failure would be
    /// reported to the far end as a network problem.
    pub fn datagram(&mut self, payload: &[u8]) -> Result<Vec<u8>, MicError> {
        if payload.is_empty() {
            return Err(MicError::EmptyPayload);
        }
        if payload.len() > MAX_PAYLOAD {
            return Err(MicError::PayloadTooLarge(payload.len()));
        }
        if self.next_seq == u32::MAX {
            return Err(MicError::Exhausted);
        }

        let seq = self.next_seq;
        self.next_seq += 1;

        let sealed = self.keys.seal(STREAM_MIC, seq, PURPOSE, payload);
        let mut datagram = Vec::with_capacity(HEADER_LEN + sealed.len());
        datagram.push(ECHO_MIC);
        datagram.push(0); // flags, reserved
        datagram.extend_from_slice(&seq.to_be_bytes());
        datagram.extend_from_slice(&sealed);
        Ok(datagram)
    }
}

// ── Receiving ───────────────────────────────────────────────────────────────

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MicStats {
    /// Datagrams that opened successfully.
    pub accepted: u64,
    /// Datagrams that failed the tag check, or were malformed — forged, or from
    /// a stale session.
    pub rejected: u64,
    /// Payloads delivered to the renderer.
    pub applied: u64,
    /// Delivered, but behind the newest sequence seen. The path is reordering.
    /// Harmless in itself — the jitter buffer sorts them — but a rising count
    /// is what distinguishes "the network is delivering out of order" from
    /// "the network is dropping", which look identical downstream.
    pub reordered: u64,
    /// Already seen. On this channel there is no redundancy, so unlike the
    /// input path a nonzero count is **not** the mechanism working: it means
    /// the path is duplicating packets, or something is replaying them.
    pub duplicates: u64,
    /// Arrived more than [`WINDOW_PACKETS`] behind and could not be placed.
    pub late: u64,
}

impl MicStats {
    /// Sequence numbers never seen at all, inferred from the highest accepted.
    ///
    /// The one number that says whether the *path* is losing audio, as opposed
    /// to the renderer discarding it — and it cannot be derived downstream,
    /// because by then the gap has already been concealed.
    pub fn lost(&self, highest_seq: u32) -> u64 {
        u64::from(highest_seq)
            .saturating_sub(self.applied)
            .saturating_sub(self.duplicates)
    }
}

/// One payload, with the sequence the sender gave it.
///
/// The sequence travels onward because the jitter buffer needs it: it is what
/// orders a reordered packet, sizes a gap for concealment, and distinguishes a
/// late arrival from a fresh one. Discarding it here would force the renderer
/// to re-derive timing from arrival order, which is precisely the thing a
/// jittery path destroys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MicPacket {
    pub seq: u32,
    pub payload: Vec<u8>,
    /// True if this arrived behind a sequence already delivered.
    pub reordered: bool,
}

/// The host half: opens datagrams and yields payloads that have not been seen.
///
/// Deduplication uses a sliding window rather than a high-water mark — see the
/// module documentation for why that difference from
/// [`crate::input_channel::InputReceiver`] is deliberate. `window` is a bitmap
/// of the [`WINDOW_PACKETS`] sequences at and below `highest`: bit 0 is
/// `highest` itself, bit *n* is `highest - n`.
#[derive(Debug)]
pub struct MicReceiver {
    keys: SessionKeys,
    /// Highest sequence accepted. Zero until the first packet.
    highest: u32,
    window: u64,
    stats: MicStats,
}

/// What the window made of a sequence number.
#[derive(Debug, PartialEq, Eq)]
enum Placement {
    /// Ahead of everything seen.
    Fresh,
    /// Behind the newest, but inside the window and not yet seen.
    Reordered,
    /// Already recorded.
    Duplicate,
    /// Too far behind to place.
    TooLate,
}

impl MicReceiver {
    pub fn new(keys: SessionKeys) -> Self {
        Self { keys, highest: 0, window: 0, stats: MicStats::default() }
    }

    pub fn stats(&self) -> MicStats {
        self.stats
    }

    /// Highest sequence accepted so far; zero before the first packet.
    pub fn highest_sequence(&self) -> u32 {
        self.highest
    }

    /// Open one datagram.
    ///
    /// `Ok(None)` means the datagram was authentic but carried nothing new — a
    /// duplicate or an arrival too late to place. That is a normal event on a
    /// real path and is counted, not an error. `Err` means the datagram was not
    /// ours.
    pub fn open(&mut self, datagram: &[u8]) -> Result<Option<MicPacket>, MicError> {
        if datagram.first() != Some(&ECHO_MIC) || datagram.len() < HEADER_LEN + CRYPTO_OVERHEAD {
            self.stats.rejected += 1;
            return Err(MicError::Malformed);
        }
        let seq = u32::from_be_bytes([datagram[2], datagram[3], datagram[4], datagram[5]]);
        if seq == 0 {
            // No sender emits zero — it is the receiver's "nothing yet"
            // sentinel. Refused before the decrypt it could never pass anyway.
            self.stats.rejected += 1;
            return Err(MicError::Malformed);
        }

        let payload = match self.keys.open(STREAM_MIC, seq, PURPOSE, &datagram[HEADER_LEN..]) {
            Ok(p) => p,
            Err(e) => {
                self.stats.rejected += 1;
                return Err(e.into());
            }
        };
        self.stats.accepted += 1;

        if payload.is_empty() {
            return Err(MicError::EmptyPayload);
        }

        // Placement runs only after authentication. Advancing the window on an
        // unverified sequence would let anyone who can write to this socket
        // push `highest` forward and make every genuine packet behind it read
        // as late — a denial of service costing one forged datagram.
        match self.place(seq) {
            Placement::Fresh | Placement::Reordered => {}
            Placement::Duplicate => {
                self.stats.duplicates += 1;
                return Ok(None);
            }
            Placement::TooLate => {
                self.stats.late += 1;
                return Ok(None);
            }
        }
        let reordered = seq != self.highest;
        if reordered {
            self.stats.reordered += 1;
        }
        self.stats.applied += 1;
        Ok(Some(MicPacket { seq, payload, reordered }))
    }

    /// Record `seq` in the sliding window and report what it was.
    fn place(&mut self, seq: u32) -> Placement {
        if self.highest == 0 {
            self.highest = seq;
            self.window = 1;
            return Placement::Fresh;
        }

        // `wrapping_sub` plus a half-space comparison keeps this correct across
        // the sequence space wrapping. Nothing will reach 2^32 packets, but a
        // comparison that misbehaves there would be a silent, unreproducible
        // audio dropout rather than an obvious failure.
        let ahead = seq.wrapping_sub(self.highest);
        if ahead != 0 && ahead <= u32::MAX / 2 {
            self.window = if ahead >= WINDOW_PACKETS {
                1 // the jump cleared the whole window
            } else {
                (self.window << ahead) | 1
            };
            self.highest = seq;
            return Placement::Fresh;
        }

        let behind = self.highest.wrapping_sub(seq);
        if behind >= WINDOW_PACKETS {
            return Placement::TooLate;
        }
        let bit = 1u64 << behind;
        if self.window & bit != 0 {
            Placement::Duplicate
        } else {
            self.window |= bit;
            Placement::Reordered
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demux::{classify, Class};

    fn pair() -> (MicSender, MicReceiver) {
        let keys = SessionKeys::generate();
        (MicSender::new(keys.clone()), MicReceiver::new(keys))
    }

    /// A plausible Opus packet: the TOC byte plus a little payload.
    fn opus(n: u8) -> Vec<u8> {
        let mut p = vec![0x78]; // config 15, mono, one frame
        p.extend_from_slice(&[n; 39]);
        p
    }

    #[test]
    fn a_sealed_datagram_round_trips_and_classifies_as_microphone() {
        let (mut tx, mut rx) = pair();
        let datagram = tx.datagram(&opus(1)).unwrap();

        assert_eq!(classify(&datagram), Class::EchoMic);
        let packet = rx.open(&datagram).unwrap().expect("a fresh packet");
        assert_eq!(packet.payload, opus(1));
        assert_eq!(packet.seq, 1);
        assert!(!packet.reordered);
    }

    /// The very first packet of a session must be delivered with no warm-up.
    ///
    /// Worth its own test for the reason the input channel has one: the sender
    /// starts at sequence 1 while the receiver starts `highest` at 0, and those
    /// two initialisations live in different structs. Set to the same value,
    /// the first packet would read as already-seen and be dropped — which
    /// sounds like a clipped first syllable and would be blamed on the encoder.
    #[test]
    fn the_first_packet_of_a_session_is_delivered_immediately() {
        let (mut tx, mut rx) = pair();
        let datagram = tx.datagram(&opus(1)).unwrap();

        assert!(rx.open(&datagram).unwrap().is_some(), "no warm-up is permitted");
        assert_eq!(rx.stats().applied, 1);
        assert_eq!(rx.stats().duplicates, 0);
        assert_eq!(rx.stats().late, 0);
    }

    /// The difference from `input_channel` that this module exists for: a
    /// packet that arrives out of order is still good audio, and dropping it
    /// would manufacture a gap the network did not cause.
    #[test]
    fn a_reordered_packet_is_delivered_and_reported_as_reordered() {
        let (mut tx, mut rx) = pair();
        let first = tx.datagram(&opus(1)).unwrap();
        let second = tx.datagram(&opus(2)).unwrap();
        let third = tx.datagram(&opus(3)).unwrap();

        // Arrive 1, 3, 2 — the commonest reordering there is.
        assert_eq!(rx.open(&first).unwrap().unwrap().seq, 1);
        assert_eq!(rx.open(&third).unwrap().unwrap().seq, 3);

        let late_arrival = rx.open(&second).unwrap().expect("still good audio");
        assert_eq!(late_arrival.seq, 2);
        assert_eq!(late_arrival.payload, opus(2), "the payload must survive intact");
        assert!(late_arrival.reordered, "the caller needs to know it was out of order");

        assert_eq!(rx.stats().applied, 3, "all three must reach the renderer");
        assert_eq!(rx.stats().reordered, 1);
        assert_eq!(rx.stats().late, 0);
    }

    /// A replayed datagram must deliver nothing. This is the property that
    /// stops a passive observer of the punched path from injecting recorded
    /// speech into the host's microphone.
    #[test]
    fn a_replayed_datagram_delivers_nothing() {
        let (mut tx, mut rx) = pair();
        let datagram = tx.datagram(&opus(1)).unwrap();

        assert!(rx.open(&datagram).unwrap().is_some());
        assert_eq!(rx.open(&datagram).unwrap(), None, "a replay must be inert");
        assert_eq!(rx.open(&datagram).unwrap(), None, "…every time");
        assert_eq!(rx.stats().duplicates, 2);
        assert_eq!(rx.stats().applied, 1);
    }

    /// The window is finite, so the guarantee it provides is stated as a fact
    /// rather than assumed: a replay stays caught for exactly `WINDOW_PACKETS`,
    /// and beyond that it is refused as late instead. Either way it is not
    /// delivered, which is the property that actually matters.
    #[test]
    fn the_window_covers_exactly_the_documented_reordering_depth() {
        let (mut tx, mut rx) = pair();
        let first = tx.datagram(&opus(1)).unwrap();
        rx.open(&first).unwrap().unwrap();

        // Advance to the far edge of the window: sequence 1 + (WINDOW - 1).
        for _ in 1..WINDOW_PACKETS {
            let d = tx.datagram(&opus(9)).unwrap();
            rx.open(&d).unwrap();
        }
        assert_eq!(rx.highest_sequence(), WINDOW_PACKETS);
        assert_eq!(rx.open(&first).unwrap(), None, "still inside the window: a duplicate");
        assert_eq!(rx.stats().duplicates, 1);

        // One more packet pushes sequence 1 out of reach entirely.
        let d = tx.datagram(&opus(9)).unwrap();
        rx.open(&d).unwrap();
        assert_eq!(rx.open(&first).unwrap(), None, "out of the window: late, not delivered");
        assert_eq!(rx.stats().late, 1);
        assert_eq!(rx.stats().duplicates, 1, "and not miscounted as a duplicate");
    }

    /// A gap larger than the window must clear it rather than leaving stale
    /// bits that would mark fresh sequences as already-seen.
    #[test]
    fn a_jump_past_the_window_clears_it_instead_of_shifting_stale_bits() {
        let keys = SessionKeys::generate();
        let mut rx = MicReceiver::new(keys.clone());
        let mut tx = MicSender::new(keys);

        rx.open(&tx.datagram(&opus(1)).unwrap()).unwrap().unwrap();

        // Skip far ahead — a long silence, or a burst loss.
        for _ in 0..WINDOW_PACKETS * 3 {
            let _ = tx.datagram(&opus(0)).unwrap(); // never delivered
        }
        let far = tx.datagram(&opus(2)).unwrap();
        assert!(rx.open(&far).unwrap().is_some());

        // Everything at the new position must be fresh, not shadowed by a bit
        // left over from before the jump.
        for _ in 0..WINDOW_PACKETS {
            let d = tx.datagram(&opus(3)).unwrap();
            assert!(rx.open(&d).unwrap().is_some(), "a stale window bit would drop this");
        }
        assert_eq!(rx.stats().duplicates, 0);
    }

    /// Authorization is possession of the session key.
    #[test]
    fn a_datagram_from_a_foreign_key_is_refused() {
        let (mut tx, _) = pair();
        let (_, mut rx) = pair(); // a different session entirely

        let datagram = tx.datagram(&opus(1)).unwrap();
        assert_eq!(rx.open(&datagram), Err(MicError::Authentication));
        assert_eq!(rx.stats().rejected, 1);
        assert_eq!(rx.stats().applied, 0);
    }

    /// Flipping any byte must fail the tag rather than producing plausible
    /// noise — which, unlike a corrupted video frame, would be *audible*.
    #[test]
    fn a_tampered_datagram_is_refused() {
        let (mut tx, mut rx) = pair();
        let mut datagram = tx.datagram(&opus(1)).unwrap();
        let last = datagram.len() - 1;
        datagram[last] ^= 0x01;
        assert_eq!(rx.open(&datagram), Err(MicError::Authentication));
    }

    /// The sequence is transmitted in the clear, so it must be authenticated —
    /// otherwise an attacker could renumber a datagram in flight and either
    /// replay it forever or push the window past every genuine packet.
    #[test]
    fn the_cleartext_sequence_is_covered_by_the_tag() {
        let (mut tx, mut rx) = pair();
        let mut datagram = tx.datagram(&opus(1)).unwrap();
        datagram[5] = 9; // renumber it
        assert_eq!(rx.open(&datagram), Err(MicError::Authentication));
        assert_eq!(rx.highest_sequence(), 0, "a forgery must not advance the window");
    }

    /// A forged datagram must not be able to push the window forward — that
    /// would cost one packet to silence a whole session.
    #[test]
    fn a_forgery_cannot_advance_the_window_and_strand_genuine_packets() {
        let keys = SessionKeys::generate();
        let mut rx = MicReceiver::new(keys.clone());
        let mut tx = MicSender::new(keys);
        let mut attacker = MicSender::new(SessionKeys::generate());

        rx.open(&tx.datagram(&opus(1)).unwrap()).unwrap().unwrap();

        // The attacker seals under their own key at a wildly future sequence.
        for _ in 0..500 {
            let _ = attacker.datagram(&opus(0));
        }
        let forged = attacker.datagram(&opus(0)).unwrap();
        assert!(rx.open(&forged).is_err());
        assert_eq!(rx.highest_sequence(), 1, "the window must not have moved");

        // The genuine stream continues undisturbed.
        assert!(rx.open(&tx.datagram(&opus(2)).unwrap()).unwrap().is_some());
    }

    /// The sequence derives the nonce, so it must never repeat within a
    /// session — reuse would leak the GCM authentication subkey.
    #[test]
    fn every_datagram_carries_a_fresh_sequence() {
        let (mut tx, _) = pair();
        let mut seen = std::collections::HashSet::new();
        for n in 0..500u16 {
            let d = tx.datagram(&opus(n as u8)).unwrap();
            let seq = u32::from_be_bytes([d[2], d[3], d[4], d[5]]);
            assert!(seen.insert(seq), "sequence {seq} reused");
        }
    }

    /// A locally-rejected payload must not burn a sequence number: the gap
    /// would reach the host as packet loss and be blamed on the network.
    #[test]
    fn a_rejected_payload_does_not_consume_a_sequence_number() {
        let (mut tx, mut rx) = pair();
        assert_eq!(tx.datagram(&[]), Err(MicError::EmptyPayload));
        assert_eq!(
            tx.datagram(&vec![0u8; MAX_PAYLOAD + 1]),
            Err(MicError::PayloadTooLarge(MAX_PAYLOAD + 1))
        );
        assert_eq!(tx.next_sequence(), 1, "neither may have advanced the sequence");

        // …and the stream that follows starts where the host expects it to.
        let packet = rx.open(&tx.datagram(&opus(1)).unwrap()).unwrap().unwrap();
        assert_eq!(packet.seq, 1);
        assert_eq!(rx.stats().lost(rx.highest_sequence()), 0, "no loss may be implied");
    }

    #[test]
    fn a_payload_at_the_size_limit_is_accepted() {
        let (mut tx, mut rx) = pair();
        let big = vec![0x7au8; MAX_PAYLOAD];
        let datagram = tx.datagram(&big).unwrap();
        assert_eq!(rx.open(&datagram).unwrap().unwrap().payload, big);
    }

    #[test]
    fn garbage_is_rejected_without_panicking() {
        let (_, mut rx) = pair();
        assert_eq!(rx.open(&[]), Err(MicError::Malformed));
        assert_eq!(rx.open(&[ECHO_MIC]), Err(MicError::Malformed));
        assert_eq!(rx.open(&[0x90, 0, 0, 0, 0, 0, 0, 0]), Err(MicError::Malformed));
        // Right tag and long enough, but not sealed by anyone.
        assert_eq!(rx.open(&[ECHO_MIC; 64]), Err(MicError::Authentication));

        // Right tag, long enough, sequence zero — the sentinel no sender emits.
        let mut zero_seq = vec![ECHO_MIC, 0, 0, 0, 0, 0];
        zero_seq.extend_from_slice(&[0u8; CRYPTO_OVERHEAD]);
        assert_eq!(rx.open(&zero_seq), Err(MicError::Malformed));

        // None of that may have disturbed the window.
        assert_eq!(rx.highest_sequence(), 0);
    }

    /// Loss is inferred rather than observed, so the arithmetic is asserted
    /// against a stream with a known hole in it.
    #[test]
    fn loss_is_inferred_from_the_sequence_the_sender_reached() {
        let (mut tx, mut rx) = pair();
        for n in 0..10u8 {
            let datagram = tx.datagram(&opus(n)).unwrap();
            // Drop sequences 4 and 7.
            if n == 3 || n == 6 {
                continue;
            }
            rx.open(&datagram).unwrap();
        }
        assert_eq!(rx.stats().applied, 8);
        assert_eq!(rx.stats().lost(rx.highest_sequence()), 2);
    }
}

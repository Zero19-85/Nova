//! Sealed, unreliable, unordered input datagrams — client to host.
//!
//! ## Why input does not use the reliable control channel
//!
//! It did, and it was unusable. [`crate::rudp`] is a reliable *ordered* channel
//! with a window of [`MAX_IN_FLIGHT`](crate::rudp::MAX_IN_FLIGHT) unacknowledged
//! messages, which is exactly right for the handful of session commands it was
//! built for and exactly wrong for a pointer:
//!
//! - **The window is a rate limit.** Eight messages per round trip is a ceiling
//!   of a few hundred per second on a good path and far less on a bad one. A
//!   mouse generates more than that, so the surplus queued in the driver's
//!   backlog and drained afterwards — the host's cursor kept moving after the
//!   user's hand stopped (live 2026-08-16, "it drags heavily").
//! - **Ordering makes one lost datagram stall every input behind it.** A
//!   retransmit costs [`RETRY_INITIAL`](crate::rudp::RETRY_INITIAL) at minimum,
//!   during which nothing is delivered. On a path already carrying 20 Mbps of
//!   video, loss is routine.
//!
//! Both problems come from guaranteeing something input does not want. A lost
//! mouse delta is worth *nothing* a frame later; the next one supersedes it.
//! So input gets its own datagram type with no acknowledgements, no ordering
//! guarantee, and no window.
//!
//! ## What replaces reliability
//!
//! Motion is self-healing — deltas accumulate, so a dropped one is a small
//! position error the next packet corrects. Button and key events are not: a
//! lost key-up strands a key held on the host. Rather than reintroduce
//! acknowledgement for those, every datagram **repeats the last
//! [`REDUNDANCY`] packets it already sent**, and the receiver deduplicates by
//! per-packet sequence number. A packet therefore survives up to `REDUNDANCY`
//! consecutive losses at a cost of a few dozen bytes per datagram, with no
//! round trip anywhere. This is the standard trade for real-time input, and it
//! is why the sequence number is per *packet* rather than per datagram.
//!
//! Deduplication is also the replay defence: a datagram replayed by an attacker
//! carries sequence numbers the receiver has already applied, so it yields
//! nothing. No separate replay window is needed, which is deliberate — one
//! mechanism doing both jobs cannot disagree with itself.
//!
//! ## Authorization is the key, not the address
//!
//! These datagrams arrive on the media socket, which anyone who has seen the
//! punched path can write to, and they end up in `SendInput` on a machine where
//! Nova's Master runs as LocalSystem. Nothing about the source address is
//! trusted. The payload is sealed with the session's [`SessionKeys`] — minted
//! by the host when the session started and handed to the client over mutual
//! TLS — so opening one successfully *is* proof that the sender is the device
//! that was granted the session. A forged or replayed datagram fails the GCM
//! tag and is counted, not injected.
//!
//! ## Wire format
//!
//! ```text
//!   [0xE3][flags u8][counter u32 BE][sealed …]
//!
//!   sealed = AES-128-GCM(STREAM_INPUT, counter) over:
//!     [base_seq u32 BE][count u8]([len u16 BE][packet bytes] × count)
//! ```
//!
//! The counter is in the clear because the receiver needs it to derive the
//! nonce before it can decrypt — the same reason `rtp.rs` sends the frame index
//! unencrypted. It reveals only how many input datagrams have been sent, which
//! anyone counting packets already knows. `flags` is reserved and sent as zero
//! so a later need is additive rather than a format break.

use std::collections::VecDeque;

use crate::demux::ECHO_INPUT;
use crate::media_crypto::{CryptoError, SessionKeys, CRYPTO_OVERHEAD, STREAM_INPUT};

/// Bytes before the sealed payload: tag, flags, counter.
pub const HEADER_LEN: usize = 6;

/// Previously-sent packets repeated in each datagram.
///
/// Three tolerates two consecutive losses of any single packet, which on a
/// Wi-Fi path carrying video is generous — bursts longer than that are visible
/// in the video too. Raising it costs bytes on every datagram forever, so it is
/// a poor place to buy insurance.
pub const REDUNDANCY: usize = 3;

/// Largest plaintext batch, chosen so the sealed datagram stays under a WAN MTU
/// with room for IP and UDP headers. Input packets are 9–16 bytes, so this is
/// roughly 60 of them — far more than one 8 ms tick can produce.
const MAX_PLAINTEXT: usize = 1024;

/// The AAD's frame-type slot. Input has no frame types; a fixed value keeps the
/// tag committing to "this is input" rather than leaving a byte an attacker
/// could vary.
const PURPOSE: u8 = 0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputError {
    /// Not an input datagram, or shorter than a header plus a tag.
    Malformed,
    /// The tag did not verify: forged, corrupted, or sealed under another key.
    Authentication,
    /// The plaintext did not parse — only reachable with a key, so it means a
    /// version mismatch between peers rather than an attack.
    BadBatch,
    /// The 32-bit counter or sequence space is exhausted. Reusing either would
    /// repeat a GCM nonce, which leaks the authentication subkey, so the sender
    /// stops instead. Unreachable in practice: at 125 datagrams per second this
    /// is over a year of continuous input in one session.
    Exhausted,
}

impl std::fmt::Display for InputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed => write!(f, "not a well-formed input datagram"),
            Self::Authentication => {
                write!(f, "input datagram failed authentication — forged, corrupted, or not ours")
            }
            Self::BadBatch => write!(f, "input batch did not parse"),
            Self::Exhausted => write!(f, "input sequence space exhausted — session must restart"),
        }
    }
}

impl std::error::Error for InputError {}

impl From<CryptoError> for InputError {
    fn from(e: CryptoError) -> Self {
        match e {
            CryptoError::Truncated => Self::Malformed,
            _ => Self::Authentication,
        }
    }
}

// ── Sending ─────────────────────────────────────────────────────────────────

/// The client half: turns input packets into sealed datagrams.
pub struct InputSender {
    keys: SessionKeys,
    counter: u32,
    next_seq: u32,
    /// Recently sent packets, oldest first, with contiguous sequence numbers.
    /// Bounded by [`REDUNDANCY`], so this never grows.
    recent: VecDeque<Vec<u8>>,
    /// Sequence number of `recent`'s front.
    recent_base: u32,
}

impl InputSender {
    pub fn new(keys: SessionKeys) -> Self {
        Self {
            keys,
            // Both start at 1 for the reason `rtp.rs` starts frame indices
            // there: zero stays available as "nothing yet" on the receiver,
            // with no separate sentinel to keep in agreement.
            counter: 1,
            next_seq: 1,
            recent: VecDeque::new(),
            recent_base: 1,
        }
    }

    /// Seal `packets` into datagrams, each also repeating up to [`REDUNDANCY`]
    /// previously-sent packets.
    ///
    /// Returns more than one datagram only if the batch is large enough to
    /// exceed [`MAX_PLAINTEXT`]; a coalesced tick produces exactly one.
    pub fn datagrams(&mut self, packets: Vec<Vec<u8>>) -> Result<Vec<Vec<u8>>, InputError> {
        if packets.is_empty() {
            return Ok(Vec::new());
        }

        let mut out = Vec::new();
        // Everything already queued for repetition, plus the new packets. The
        // receiver drops whatever it has seen, so including the old ones costs
        // bytes and nothing else.
        let mut pending: Vec<(u32, Vec<u8>)> = self
            .recent
            .iter()
            .enumerate()
            .map(|(i, p)| (self.recent_base.wrapping_add(i as u32), p.clone()))
            .collect();

        for packet in packets {
            if self.next_seq == u32::MAX {
                return Err(InputError::Exhausted);
            }
            let seq = self.next_seq;
            self.next_seq += 1;

            self.recent.push_back(packet.clone());
            if self.recent.len() > REDUNDANCY {
                self.recent.pop_front();
                self.recent_base = self.recent_base.wrapping_add(1);
            } else if self.recent.len() == 1 {
                self.recent_base = seq;
            }
            pending.push((seq, packet));
        }

        // Sequence numbers must be contiguous within a datagram — `base_seq`
        // plus position is how the receiver derives each one, which is what
        // keeps the batch header to five bytes instead of four per packet.
        for chunk in contiguous_chunks(pending) {
            out.push(self.seal_batch(&chunk)?);
        }
        Ok(out)
    }

    fn seal_batch(&mut self, batch: &[(u32, Vec<u8>)]) -> Result<Vec<u8>, InputError> {
        if self.counter == u32::MAX {
            return Err(InputError::Exhausted);
        }
        let counter = self.counter;
        self.counter += 1;

        let mut plaintext = Vec::with_capacity(MAX_PLAINTEXT);
        plaintext.extend_from_slice(&batch[0].0.to_be_bytes());
        plaintext.push(batch.len() as u8);
        for (_, packet) in batch {
            plaintext.extend_from_slice(&(packet.len() as u16).to_be_bytes());
            plaintext.extend_from_slice(packet);
        }

        let sealed = self.keys.seal(STREAM_INPUT, counter, PURPOSE, &plaintext);
        let mut datagram = Vec::with_capacity(HEADER_LEN + sealed.len());
        datagram.push(ECHO_INPUT);
        datagram.push(0); // flags, reserved
        datagram.extend_from_slice(&counter.to_be_bytes());
        datagram.extend_from_slice(&sealed);
        Ok(datagram)
    }
}

/// Split into runs of contiguous sequence numbers that each fit
/// [`MAX_PLAINTEXT`], preserving order.
fn contiguous_chunks(packets: Vec<(u32, Vec<u8>)>) -> Vec<Vec<(u32, Vec<u8>)>> {
    let mut chunks: Vec<Vec<(u32, Vec<u8>)>> = Vec::new();
    let mut current: Vec<(u32, Vec<u8>)> = Vec::new();
    let mut bytes = 5; // base_seq + count

    for (seq, packet) in packets {
        let cost = 2 + packet.len();
        let breaks_run = current
            .last()
            .is_some_and(|(last, _)| seq != last.wrapping_add(1));
        // 255 because `count` is one byte; the size check is what usually fires.
        if !current.is_empty() && (breaks_run || bytes + cost > MAX_PLAINTEXT || current.len() == 255)
        {
            chunks.push(std::mem::take(&mut current));
            bytes = 5;
        }
        bytes += cost;
        current.push((seq, packet));
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

// ── Receiving ───────────────────────────────────────────────────────────────

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct InputStats {
    /// Datagrams that opened successfully.
    pub accepted: u64,
    /// Datagrams that failed the tag check — forged, or from a stale session.
    pub rejected: u64,
    /// Packets delivered to the injector.
    pub applied: u64,
    /// Packets already seen, dropped by the redundancy deduplicator. A healthy
    /// lossless path drops roughly [`REDUNDANCY`] per datagram; that is the
    /// mechanism working, not a problem.
    pub duplicates: u64,
}

/// The host half: opens datagrams and yields packets that have not been applied.
#[derive(Debug)]
pub struct InputReceiver {
    keys: SessionKeys,
    /// Highest sequence delivered. Zero until the first packet.
    applied_through: u32,
    stats: InputStats,
}

impl InputReceiver {
    pub fn new(keys: SessionKeys) -> Self {
        Self { keys, applied_through: 0, stats: InputStats::default() }
    }

    pub fn stats(&self) -> InputStats {
        self.stats
    }

    /// Open one datagram, returning the packets that are new, in order.
    ///
    /// Packets already applied are dropped silently — that is the redundancy
    /// scheme working. An unordered arrival that is *behind* the high-water
    /// mark is dropped for the same reason, which is correct for input: the
    /// state it described has already been superseded.
    pub fn open(&mut self, datagram: &[u8]) -> Result<Vec<Vec<u8>>, InputError> {
        if datagram.first() != Some(&ECHO_INPUT) || datagram.len() < HEADER_LEN + CRYPTO_OVERHEAD {
            self.stats.rejected += 1;
            return Err(InputError::Malformed);
        }
        let counter = u32::from_be_bytes([datagram[2], datagram[3], datagram[4], datagram[5]]);

        let plaintext = match self.keys.open(STREAM_INPUT, counter, PURPOSE, &datagram[HEADER_LEN..])
        {
            Ok(p) => p,
            Err(e) => {
                self.stats.rejected += 1;
                return Err(e.into());
            }
        };
        self.stats.accepted += 1;

        let (base_seq, packets) = parse_batch(&plaintext).ok_or(InputError::BadBatch)?;

        let mut fresh = Vec::with_capacity(packets.len());
        for (offset, packet) in packets.into_iter().enumerate() {
            let seq = base_seq.wrapping_add(offset as u32);
            // Fresh means strictly ahead of the high-water mark: `ahead == 0`
            // is the packet we last applied arriving again, which is the common
            // case (every datagram repeats it) and must be dropped. `wrapping_sub`
            // keeps the comparison correct across the sequence space wrapping,
            // which nothing will reach but which must not misbehave if it does.
            let ahead = seq.wrapping_sub(self.applied_through);
            if seq == 0 || ahead == 0 || ahead > u32::MAX / 2 {
                self.stats.duplicates += 1;
                continue;
            }
            self.applied_through = seq;
            self.stats.applied += 1;
            fresh.push(packet);
        }
        Ok(fresh)
    }
}

fn parse_batch(plaintext: &[u8]) -> Option<(u32, Vec<Vec<u8>>)> {
    if plaintext.len() < 5 {
        return None;
    }
    let base_seq = u32::from_be_bytes([plaintext[0], plaintext[1], plaintext[2], plaintext[3]]);
    let count = plaintext[4] as usize;

    let mut packets = Vec::with_capacity(count);
    let mut at = 5;
    for _ in 0..count {
        if at + 2 > plaintext.len() {
            return None;
        }
        let len = u16::from_be_bytes([plaintext[at], plaintext[at + 1]]) as usize;
        at += 2;
        if at + len > plaintext.len() {
            return None;
        }
        packets.push(plaintext[at..at + len].to_vec());
        at += len;
    }
    Some((base_seq, packets))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demux::{classify, Class};

    fn pair() -> (InputSender, InputReceiver) {
        let keys = SessionKeys::generate();
        (InputSender::new(keys.clone()), InputReceiver::new(keys))
    }

    fn packet(n: u8) -> Vec<u8> {
        vec![n; 12]
    }

    #[test]
    fn a_sealed_datagram_round_trips_and_classifies_as_input() {
        let (mut tx, mut rx) = pair();
        let out = tx.datagrams(vec![packet(1), packet(2)]).unwrap();
        assert_eq!(out.len(), 1, "a small batch is one datagram");
        assert_eq!(classify(&out[0]), Class::EchoInput);
        assert_eq!(rx.open(&out[0]).unwrap(), vec![packet(1), packet(2)]);
    }

    /// The very first packet of a session must be applied, with no warm-up.
    ///
    /// Worth its own test because the sender starts at sequence 1 while the
    /// receiver starts its high-water mark at 0, and those two initialisations
    /// live in different structs. If they were ever set to the same value the
    /// first packet would read as "already applied" and be dropped — which on a
    /// live session looks like a dead zone before the pointer starts moving,
    /// and would be blamed on the network rather than on an off-by-one.
    #[test]
    fn the_first_packet_of_a_session_is_applied_immediately() {
        let (mut tx, mut rx) = pair();
        let out = tx.datagrams(vec![packet(1)]).unwrap();

        assert_eq!(rx.open(&out[0]).unwrap(), vec![packet(1)], "no warm-up is permitted");
        assert_eq!(rx.stats().applied, 1);
        assert_eq!(rx.stats().duplicates, 0, "nothing may be discarded before the first packet");
    }

    /// The point of the whole module: a packet must survive losing the datagram
    /// that first carried it. A dropped key-up is a key stuck down on the host.
    #[test]
    fn a_packet_survives_losing_the_datagram_that_first_carried_it() {
        let (mut tx, mut rx) = pair();

        let lost = tx.datagrams(vec![packet(1)]).unwrap();
        let next = tx.datagrams(vec![packet(2)]).unwrap();

        // `lost` never reaches the receiver.
        drop(lost);
        let delivered = rx.open(&next[0]).unwrap();
        assert_eq!(
            delivered,
            vec![packet(1), packet(2)],
            "the repeat must carry the lost packet, in its original order"
        );
    }

    /// …and the repeats must not be applied twice when nothing was lost. A
    /// double-applied click is a double click.
    #[test]
    fn repeats_are_dropped_when_the_original_arrived() {
        let (mut tx, mut rx) = pair();

        let first = tx.datagrams(vec![packet(1)]).unwrap();
        let second = tx.datagrams(vec![packet(2)]).unwrap();

        assert_eq!(rx.open(&first[0]).unwrap(), vec![packet(1)]);
        assert_eq!(rx.open(&second[0]).unwrap(), vec![packet(2)], "only the new one");
        assert!(rx.stats().duplicates > 0, "the repeat must be counted, not applied");
    }

    /// Redundancy tolerates REDUNDANCY consecutive losses, and no more. Stated
    /// as a test so the guarantee is a fact rather than a comment.
    #[test]
    fn redundancy_covers_exactly_the_documented_number_of_losses() {
        let (mut tx, mut rx) = pair();

        let first = tx.datagrams(vec![packet(1)]).unwrap();
        drop(first);
        // REDUNDANCY more datagrams; the last still repeats packet 1.
        let mut last = Vec::new();
        for n in 0..REDUNDANCY {
            last = tx.datagrams(vec![packet(n as u8 + 2)]).unwrap();
        }
        let delivered = rx.open(&last[0]).unwrap();
        assert_eq!(delivered[0], packet(1), "still recoverable at the edge of the window");

        // One further loss puts it out of reach — asserted so nobody assumes
        // the scheme is stronger than it is.
        let (mut tx, mut rx) = pair();
        drop(tx.datagrams(vec![packet(1)]).unwrap());
        let mut last = Vec::new();
        for n in 0..=REDUNDANCY {
            last = tx.datagrams(vec![packet(n as u8 + 2)]).unwrap();
        }
        assert!(!rx.open(&last[0]).unwrap().contains(&packet(1)));
    }

    /// A replayed datagram must inject nothing. This is the property that keeps
    /// a passive observer of the punched path from replaying a click.
    #[test]
    fn a_replayed_datagram_applies_nothing() {
        let (mut tx, mut rx) = pair();
        let out = tx.datagrams(vec![packet(1)]).unwrap();

        assert_eq!(rx.open(&out[0]).unwrap().len(), 1);
        assert!(rx.open(&out[0]).unwrap().is_empty(), "a replay must be inert");
        assert!(rx.open(&out[0]).unwrap().is_empty(), "…every time");
    }

    /// Authorization is possession of the session key. A datagram sealed under
    /// any other key must not reach the injector.
    #[test]
    fn a_datagram_from_a_foreign_key_is_refused() {
        let (mut tx, _) = pair();
        let (_, mut rx) = pair(); // a different session entirely

        let out = tx.datagrams(vec![packet(1)]).unwrap();
        assert_eq!(rx.open(&out[0]), Err(InputError::Authentication));
        assert_eq!(rx.stats().rejected, 1);
        assert_eq!(rx.stats().applied, 0);
    }

    /// Flipping any byte of the sealed region must fail the tag rather than
    /// producing a plausible-but-wrong packet.
    #[test]
    fn a_tampered_datagram_is_refused() {
        let (mut tx, mut rx) = pair();
        let mut out = tx.datagrams(vec![packet(1)]).unwrap().remove(0);
        let last = out.len() - 1;
        out[last] ^= 0x01;
        assert_eq!(rx.open(&out), Err(InputError::Authentication));
    }

    /// The counter is what derives the nonce, so it must never repeat within a
    /// session — reuse would leak the GCM authentication subkey.
    #[test]
    fn every_datagram_carries_a_fresh_counter() {
        let (mut tx, _) = pair();
        let mut seen = std::collections::HashSet::new();
        for n in 0..200u8 {
            for d in tx.datagrams(vec![packet(n)]).unwrap() {
                let counter = u32::from_be_bytes([d[2], d[3], d[4], d[5]]);
                assert!(seen.insert(counter), "counter {counter} reused");
            }
        }
    }

    /// A burst too large for one datagram must split rather than being
    /// truncated or refused — silently losing the tail would strand keys.
    #[test]
    fn an_oversized_batch_splits_into_several_datagrams() {
        let (mut tx, mut rx) = pair();
        let burst: Vec<Vec<u8>> = (0..200).map(|n| packet(n as u8)).collect();

        let out = tx.datagrams(burst.clone()).unwrap();
        assert!(out.len() > 1, "200 packets cannot fit one datagram");

        let mut delivered = Vec::new();
        for d in &out {
            delivered.extend(rx.open(d).unwrap());
        }
        assert_eq!(delivered, burst, "every packet, exactly once, in order");
    }

    #[test]
    fn garbage_is_rejected_without_panicking() {
        let (_, mut rx) = pair();
        assert_eq!(rx.open(&[]), Err(InputError::Malformed));
        assert_eq!(rx.open(&[ECHO_INPUT]), Err(InputError::Malformed));
        assert_eq!(rx.open(&[0x90, 0, 0, 0, 0, 0, 0, 0]), Err(InputError::Malformed));
        assert_eq!(rx.open(&[ECHO_INPUT; 64]), Err(InputError::Authentication));
    }

    #[test]
    fn an_empty_batch_sends_nothing() {
        let (mut tx, _) = pair();
        assert!(tx.datagrams(Vec::new()).unwrap().is_empty());
    }
}

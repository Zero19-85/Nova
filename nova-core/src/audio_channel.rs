//! Sealed host → client game audio, unreliable and unordered.
//!
//! The mirror of [`crate::mic_channel`], pointing the other way: the microphone
//! carries the client's voice to the host, this carries the host's game audio to
//! the client. The two are deliberately separate modules rather than one
//! parameterised by direction, for the reason [`crate::demux`] gives for their
//! separate tags — they are produced and consumed by different subsystems at
//! different rates, and sharing a stream id would let a captured datagram of one
//! be replayed as the other.
//!
//! ## Why this exists when Nova already sends audio
//!
//! Nova has carried WASAPI loopback → Opus → RTP on port 48000 since long before
//! Echo, and a Moonlight client still receives exactly that. An Echo client does
//! not: it holds one hole-punched path and listens on nothing else, so audio has
//! to arrive as a sealed datagram on that path like everything else Echo
//! carries. **The capture and the encode are shared** — the same Worker
//! pipeline, the same Opus bytes, forked at the Master. Only the framing below
//! is new.
//!
//! ## Wire format
//!
//! ```text
//!   byte 0      ECHO_AUDIO (0xE5) — the demux tag
//!   byte 1      flags, reserved, always zero
//!   bytes 2..6  sequence, big-endian u32
//!   bytes 6..   sealed = AES-128-GCM(STREAM_AUDIO, seq) over the payload bytes
//! ```
//!
//! Byte-identical in shape to a microphone datagram, and that is intentional: a
//! client that already parses one needs no new parser, only a different key
//! stream and a different destination. The sequence travels in the clear because
//! the receiver needs it to derive the nonce before it can decrypt, and is
//! nonetheless authenticated — it is committed to by both the nonce and the
//! associated data, so altering it in flight fails the tag rather than
//! reordering the stream into a lie.
//!
//! This module knows nothing about Opus, about stereo, or about audio at all. It
//! carries opaque payload bytes; the codec belongs to the two endpoints, not to
//! the transport.

use crate::demux::ECHO_AUDIO;
use crate::media_crypto::{CryptoError, SessionKeys, CRYPTO_OVERHEAD, STREAM_AUDIO};

/// Bytes before the sealed payload: tag, flags, sequence.
pub const HEADER_LEN: usize = 6;

/// Largest payload one datagram may carry.
///
/// 1275 bytes is the largest packet Opus will ever emit, so this never rejects a
/// legitimate frame while keeping the sealed datagram (1275 + 6 + 16 = 1297,
/// plus 28 for IP and UDP) comfortably inside a 1500-byte MTU. Downstream audio
/// is stereo at 128 kbps, so a 20 ms frame is around 320 bytes — this bound is
/// not close to being the binding constraint, and a payload that exceeds it is a
/// bug on the sending side rather than a large frame.
pub const MAX_PAYLOAD: usize = 1275;

/// How far behind the newest sequence a packet may arrive and still be
/// delivered.
///
/// Sized in packets rather than milliseconds because this layer does not know
/// the frame duration. At 20 ms frames it is 1.3 seconds — far beyond any
/// reordering a real path produces, and comfortably longer than the client's
/// jitter buffer, so the window is never the thing that discards a packet the
/// buffer could still have used.
pub const WINDOW_PACKETS: u32 = 64;

/// The AAD's frame-type slot. Audio has no frame types; a fixed value keeps the
/// tag committing to a constant rather than leaving a byte an attacker could
/// vary.
const PURPOSE: u8 = 0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioError {
    /// Not a game-audio datagram, or shorter than a header plus a tag.
    Malformed,
    /// The tag did not verify: forged, corrupted, or sealed under another key.
    Authentication,
    /// Nothing to send, or a datagram that opened to nothing. Reachable on the
    /// receive side only with a valid key, so it means a version mismatch
    /// between peers rather than an attack.
    EmptyPayload,
    /// Larger than [`MAX_PAYLOAD`]. Refused rather than truncated: half an Opus
    /// packet is not a quieter Opus packet, it is a decoder error.
    PayloadTooLarge(usize),
    /// The 32-bit sequence space is exhausted. Continuing would repeat a GCM
    /// nonce, which leaks the authentication subkey, so the sender stops
    /// instead. Unreachable in practice: at 50 packets per second this is nearly
    /// three years of continuous audio in one session.
    Exhausted,
}

impl std::fmt::Display for AudioError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed => write!(f, "not a well-formed game-audio datagram"),
            Self::Authentication => write!(
                f,
                "game-audio datagram failed authentication — forged, corrupted, or not ours"
            ),
            Self::EmptyPayload => write!(f, "game-audio datagram carried no payload"),
            Self::PayloadTooLarge(n) => {
                write!(f, "game-audio payload of {n} bytes exceeds the {MAX_PAYLOAD}-byte limit")
            }
            Self::Exhausted => {
                write!(f, "game-audio sequence space exhausted — session must restart")
            }
        }
    }
}

impl std::error::Error for AudioError {}

impl From<CryptoError> for AudioError {
    fn from(e: CryptoError) -> Self {
        match e {
            CryptoError::Truncated => Self::Malformed,
            _ => Self::Authentication,
        }
    }
}

// ── Sending (host) ───────────────────────────────────────────────────────────

/// The host half: turns encoded audio packets into sealed datagrams.
///
/// Owns its own sequence counter, which is the whole reason this is a struct
/// rather than a free function. That counter must **not** be shared with
/// `audio::AudioTxState`: that one belongs to the GameStream wire on port 48000
/// and advances whenever a Moonlight client is being served. Two consumers of
/// one counter would leave gaps in each other's sequence space, which a receiver
/// reads as packet loss — the host would manufacture a loss report for a network
/// that dropped nothing, and the jitter buffer would conceal audio that was
/// never missing.
#[derive(Debug)]
pub struct AudioSender {
    keys: SessionKeys,
    next_seq: u32,
}

impl AudioSender {
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
    /// A rejected payload — empty or oversized — does **not** consume a sequence
    /// number. Deliberate twice over: a burned sequence wastes a nonce, and more
    /// importantly the receiver reads a missing sequence as packet loss, so a
    /// local validation failure would be reported to the far end as a network
    /// problem.
    pub fn datagram(&mut self, payload: &[u8]) -> Result<Vec<u8>, AudioError> {
        if payload.is_empty() {
            return Err(AudioError::EmptyPayload);
        }
        if payload.len() > MAX_PAYLOAD {
            return Err(AudioError::PayloadTooLarge(payload.len()));
        }
        if self.next_seq == u32::MAX {
            return Err(AudioError::Exhausted);
        }

        let seq = self.next_seq;
        self.next_seq += 1;

        let sealed = self.keys.seal(STREAM_AUDIO, seq, PURPOSE, payload);
        let mut datagram = Vec::with_capacity(HEADER_LEN + sealed.len());
        datagram.push(ECHO_AUDIO);
        datagram.push(0); // flags, reserved
        datagram.extend_from_slice(&seq.to_be_bytes());
        datagram.extend_from_slice(&sealed);
        Ok(datagram)
    }
}

// ── Receiving (client) ───────────────────────────────────────────────────────

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct AudioStats {
    /// Datagrams that opened successfully.
    pub accepted: u64,
    /// Datagrams that failed the tag check, or were malformed — forged, or from
    /// a previous session whose keys no longer apply.
    pub rejected: u64,
    /// Opened, but arrived behind a sequence already delivered.
    pub duplicates: u64,
    /// Opened and delivered, but arrived out of order (ahead of the window's
    /// floor, behind the highest seen). Counted separately from loss because it
    /// is not loss: the packet was used.
    pub reordered: u64,
    /// Opened but too old to use — further behind than [`WINDOW_PACKETS`].
    pub late: u64,
}

impl AudioStats {
    /// Packets the network never delivered, inferred from the gap between the
    /// highest sequence seen and how many distinct ones arrived.
    ///
    /// Inferred rather than counted because a receiver cannot observe a packet
    /// that never came; the sequence space is the only evidence there is.
    pub fn lost(&self, highest_seq: u32) -> u64 {
        // Sequences start at 1, so `highest_seq` is also how many were sent.
        (highest_seq as u64).saturating_sub(self.accepted)
    }
}

/// One authenticated audio packet, with the sequence it arrived under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioPacket {
    pub seq: u32,
    pub payload: Vec<u8>,
}

/// The client half: opens sealed datagrams, rejecting forgeries and replays.
///
/// Uses a **sliding window rather than a high-water mark**, which is the
/// deliberate reversal of [`crate::input_channel`]'s rule and the same choice
/// [`crate::mic_channel`] makes. Dropping everything at or behind the mark is
/// right for a pointer, where an older delta describes a position already
/// superseded. It is wrong for audio: a packet that arrives late but still ahead
/// of playout is perfectly good, and discarding it manufactures a gap the
/// network did not cause.
pub struct AudioReceiver {
    keys: SessionKeys,
    /// Highest sequence opened so far; the window's ceiling.
    highest: u32,
    /// Sequences delivered inside the current window, as a bitmap relative to
    /// `highest` — bit 0 is `highest`, bit n is `highest - n`.
    seen: u64,
    stats: AudioStats,
}

impl AudioReceiver {
    pub fn new(keys: SessionKeys) -> Self {
        Self { keys, highest: 0, seen: 0, stats: AudioStats::default() }
    }

    pub fn stats(&self) -> AudioStats {
        self.stats
    }

    /// The highest sequence opened so far — what [`AudioStats::lost`] needs.
    pub fn highest_sequence(&self) -> u32 {
        self.highest
    }

    /// Open one datagram.
    ///
    /// `Ok(Some(packet))` is a new, authentic packet. `Ok(None)` is authentic
    /// but unusable — a duplicate or too far behind — which is not an error and
    /// must not be logged as one. `Err` means it never authenticated.
    ///
    /// **Window placement happens only after the tag verifies**, and that
    /// ordering is load-bearing rather than incidental. These datagrams arrive
    /// on a socket anyone who has seen the punched path can write to. Advancing
    /// `highest` on an unverified sequence would let one forged datagram
    /// carrying a huge sequence push the window past everything genuine, so
    /// every real packet afterwards reads as late — a whole-session denial of
    /// service for the cost of a single UDP send.
    pub fn open(&mut self, datagram: &[u8]) -> Result<Option<AudioPacket>, AudioError> {
        if datagram.len() < HEADER_LEN + CRYPTO_OVERHEAD || datagram[0] != ECHO_AUDIO {
            self.stats.rejected += 1;
            return Err(AudioError::Malformed);
        }
        let seq = u32::from_be_bytes([datagram[2], datagram[3], datagram[4], datagram[5]]);
        if seq == 0 {
            // Zero is the receiver's "nothing yet" sentinel and is never sent.
            self.stats.rejected += 1;
            return Err(AudioError::Malformed);
        }

        let payload = match self.keys.open(STREAM_AUDIO, seq, PURPOSE, &datagram[HEADER_LEN..]) {
            Ok(p) => p,
            Err(e) => {
                self.stats.rejected += 1;
                return Err(e.into());
            }
        };
        if payload.is_empty() {
            self.stats.rejected += 1;
            return Err(AudioError::EmptyPayload);
        }

        // Authenticated — only now may this datagram move the window.
        if seq > self.highest {
            let advance = seq - self.highest;
            self.seen = if advance >= 64 { 0 } else { self.seen << advance };
            self.seen |= 1;
            self.highest = seq;
        } else {
            let behind = self.highest - seq;
            if behind >= WINDOW_PACKETS {
                self.stats.late += 1;
                return Ok(None);
            }
            let bit = 1u64 << behind;
            if self.seen & bit != 0 {
                self.stats.duplicates += 1;
                return Ok(None);
            }
            self.seen |= bit;
            self.stats.reordered += 1;
        }

        self.stats.accepted += 1;
        Ok(Some(AudioPacket { seq, payload }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (AudioSender, AudioReceiver) {
        let keys = SessionKeys::generate();
        (AudioSender::new(keys.clone()), AudioReceiver::new(keys))
    }

    #[test]
    fn round_trips_and_numbers_from_one() {
        let (mut tx, mut rx) = pair();
        let d = tx.datagram(b"a frame of game audio").expect("seals");
        assert_eq!(d[0], ECHO_AUDIO, "carries the audio tag, not the mic's");
        let got = rx.open(&d).expect("opens").expect("is new");
        assert_eq!(got.seq, 1);
        assert_eq!(got.payload, b"a frame of game audio");
    }

    /// The bug this whole module's separate sequence counter exists to prevent,
    /// asserted at the layer that would show it: gaps read as loss.
    #[test]
    fn a_gap_in_sequence_is_reported_as_loss() {
        let (mut tx, mut rx) = pair();
        let first = tx.datagram(b"one").unwrap();
        let _dropped_in_flight = tx.datagram(b"two").unwrap();
        let third = tx.datagram(b"three").unwrap();

        rx.open(&first).unwrap().unwrap();
        rx.open(&third).unwrap().unwrap();

        assert_eq!(rx.stats().accepted, 2);
        assert_eq!(rx.stats().lost(rx.highest_sequence()), 1);
    }

    /// The reversal of `input_channel`'s rule: a late-but-in-window packet is
    /// still good audio and must be delivered, not discarded.
    #[test]
    fn reordered_packet_is_delivered_not_dropped() {
        let (mut tx, mut rx) = pair();
        let first = tx.datagram(b"one").unwrap();
        let second = tx.datagram(b"two").unwrap();

        rx.open(&second).unwrap().expect("newest arrives first");
        let late = rx.open(&first).unwrap().expect("the earlier one is still usable");
        assert_eq!(late.payload, b"one");
        assert_eq!(rx.stats().reordered, 1);
        assert_eq!(rx.stats().late, 0, "in-window is not late");
    }

    #[test]
    fn a_replayed_datagram_is_refused_once_delivered() {
        let (mut tx, mut rx) = pair();
        let d = tx.datagram(b"once").unwrap();
        assert!(rx.open(&d).unwrap().is_some());
        assert!(rx.open(&d).unwrap().is_none(), "replay delivers nothing");
        assert_eq!(rx.stats().duplicates, 1);
    }

    #[test]
    fn a_packet_further_back_than_the_window_is_late() {
        let (mut tx, mut rx) = pair();
        let first = tx.datagram(b"ancient").unwrap();
        for _ in 0..WINDOW_PACKETS + 1 {
            let d = tx.datagram(b"newer").unwrap();
            rx.open(&d).unwrap();
        }
        assert!(rx.open(&first).unwrap().is_none());
        assert_eq!(rx.stats().late, 1);
    }

    /// Foreign keys must not open our audio — the same property the mic channel
    /// has, tested here rather than assumed to carry over.
    #[test]
    fn another_sessions_key_cannot_open_it() {
        let (mut tx, _rx) = pair();
        let d = tx.datagram(b"private").unwrap();
        let mut stranger = AudioReceiver::new(SessionKeys::generate());
        assert_eq!(stranger.open(&d), Err(AudioError::Authentication));
    }

    /// The cross-stream property `STREAM_AUDIO` being distinct from
    /// `STREAM_MIC` buys: a captured downstream datagram cannot be replayed
    /// into the upstream microphone path, or vice versa. Both directions,
    /// because a one-directional test would pass with the ids swapped.
    #[test]
    fn audio_and_mic_datagrams_cannot_cross() {
        let keys = SessionKeys::generate();

        let mut audio_tx = AudioSender::new(keys.clone());
        let sealed_audio = audio_tx.datagram(b"game audio").unwrap();
        let mut mic_rx = crate::mic_channel::MicReceiver::new(keys.clone());
        // Re-tag so it is well-formed for the mic parser: only the stream id
        // should be what stops it.
        let mut as_mic = sealed_audio.clone();
        as_mic[0] = crate::demux::ECHO_MIC;
        assert!(mic_rx.open(&as_mic).is_err(), "audio must not open as mic");

        let mut mic_tx = crate::mic_channel::MicSender::new(keys.clone());
        let sealed_mic = mic_tx.datagram(b"a voice").unwrap();
        let mut audio_rx = AudioReceiver::new(keys);
        let mut as_audio = sealed_mic.clone();
        as_audio[0] = ECHO_AUDIO;
        assert!(audio_rx.open(&as_audio).is_err(), "mic must not open as audio");
    }

    #[test]
    fn oversized_and_empty_payloads_do_not_burn_a_sequence() {
        let (mut tx, _rx) = pair();
        assert_eq!(tx.datagram(b""), Err(AudioError::EmptyPayload));
        let big = vec![0u8; MAX_PAYLOAD + 1];
        assert_eq!(tx.datagram(&big), Err(AudioError::PayloadTooLarge(MAX_PAYLOAD + 1)));
        assert_eq!(tx.next_sequence(), 1, "a local refusal must not look like loss");
    }

    #[test]
    fn a_forged_sequence_cannot_move_the_window() {
        let (mut tx, mut rx) = pair();
        let good = tx.datagram(b"real").unwrap();
        rx.open(&good).unwrap().unwrap();

        // A forgery claiming a sequence far ahead. If the window advanced before
        // authentication, every genuine packet after this would read as late.
        let mut forged = good.clone();
        forged[2..6].copy_from_slice(&1_000_000u32.to_be_bytes());
        assert!(rx.open(&forged).is_err());
        assert_eq!(rx.highest_sequence(), 1, "an unverified sequence moved the window");

        let next = tx.datagram(b"still fine").unwrap();
        assert!(rx.open(&next).unwrap().is_some());
    }
}

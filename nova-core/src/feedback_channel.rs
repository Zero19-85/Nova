//! Client → host video feedback: which frames the client actually decoded.
//!
//! ## Why this channel exists
//!
//! Nova's cheap loss repairs — reference-frame invalidation and long-term
//! references — both work by pointing a P-frame at some *older* picture instead
//! of the frame before it. That only repairs anything if the client still holds
//! the picture being pointed at. Reference a frame it never received and the
//! repair frame is itself undecodable, which is strictly worse than having sent
//! a keyframe.
//!
//! GameStream has no message for this, so with Moonlight the host has to guess
//! (it falls back to the oldest reference it holds, on the reasoning that age is
//! the only evidence available). Echo owns both ends, so it can simply be told:
//! this carries the newest frame index the client's decoder accepted, and the
//! encoder then repairs against a picture it *knows* is there.
//!
//! ## Why its own tag and stream id
//!
//! For the reason [`ECHO_MIC`](crate::demux::ECHO_MIC) is separate from
//! [`ECHO_INPUT`](crate::demux::ECHO_INPUT): a distinct demux tag means no
//! dispatcher has to re-inspect a datagram to find out which subsystem wants
//! it, and a distinct stream id means a captured feedback datagram cannot be
//! replayed into the input or microphone paths, or vice versa.
//!
//! ## Authorization is the key, not the address
//!
//! Same rule as every other client-to-host channel. These datagrams arrive on
//! the media socket, which anyone who has seen the punched path can write to,
//! and what they influence is which reference the encoder trusts. Nothing about
//! the source address is trusted: the payload is sealed with the session's
//! [`SessionKeys`], so opening one successfully *is* proof the sender is the
//! device that was granted the session. A forged or replayed datagram fails the
//! GCM tag and is discarded.
//!
//! The host additionally treats the acknowledged index as **monotonic** — see
//! [`FeedbackReceiver::open`]. A replayed old datagram therefore cannot walk the
//! watermark backwards and retire a reference the client demonstrably holds.
//!
//! ## Wire format
//!
//! ```text
//!   [0xE6][flags u8][counter u32 BE][sealed …]
//!
//!   sealed = AES-128-GCM(STREAM_FEEDBACK, counter) over:
//!     [acked_frame u32 BE]
//! ```
//!
//! The counter is in the clear because the receiver needs it to derive the
//! nonce before it can decrypt, exactly as the input and microphone channels do.
//! `flags` is reserved and sent as zero so a later need is additive rather than
//! a format break.

use crate::demux::ECHO_FEEDBACK;
use crate::media_crypto::{CryptoError, SessionKeys, CRYPTO_OVERHEAD, STREAM_FEEDBACK};

/// Bytes before the sealed payload: tag, flags, counter.
pub const HEADER_LEN: usize = 6;

/// Plaintext length: one big-endian u32 frame index.
const PLAINTEXT_LEN: usize = 4;

/// Total datagram size. Fixed, so a receiver can reject a wrong-sized datagram
/// before spending a decryption on it.
pub const DATAGRAM_LEN: usize = HEADER_LEN + PLAINTEXT_LEN + CRYPTO_OVERHEAD;

/// The AAD's frame-type slot. Feedback has no frame types; a fixed value keeps
/// the tag committing to "this is feedback" rather than leaving a byte an
/// attacker could vary.
const PURPOSE: u8 = 0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedbackError {
    /// Not a feedback datagram, or not exactly [`DATAGRAM_LEN`] bytes.
    Malformed,
    /// The tag did not verify: forged, corrupted, or sealed under another key.
    Authentication,
    /// The 32-bit counter space is exhausted. Reusing a counter would repeat a
    /// GCM nonce, which leaks the authentication subkey, so the sender stops
    /// instead. Unreachable in practice: at one report per frame at 120 fps
    /// this is over a year of continuous streaming in one session.
    Exhausted,
}

impl std::fmt::Display for FeedbackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed => write!(f, "not a well-formed feedback datagram"),
            Self::Authentication => write!(
                f,
                "feedback datagram failed authentication — forged, corrupted, or not ours"
            ),
            Self::Exhausted => {
                write!(f, "feedback counter space exhausted — session must restart")
            }
        }
    }
}

impl std::error::Error for FeedbackError {}

/// Client side: seals "I decoded up to frame N" for the host.
pub struct FeedbackSender {
    keys: SessionKeys,
    counter: u32,
}

impl FeedbackSender {
    pub fn new(keys: SessionKeys) -> Self {
        Self { keys, counter: 0 }
    }

    /// Seal one report. `acked_frame` is the newest wire frame index the
    /// decoder has accepted.
    ///
    /// Reports are deliberately **idempotent and lossy-tolerant**: each one
    /// carries an absolute watermark rather than a delta, so a lost datagram
    /// costs nothing but freshness and the next one recovers the full picture.
    /// That is why this channel needs no retransmission of its own.
    pub fn seal(&mut self, acked_frame: u32) -> Result<Vec<u8>, FeedbackError> {
        let counter = self.counter;
        self.counter = self.counter.checked_add(1).ok_or(FeedbackError::Exhausted)?;

        let sealed = self
            .keys
            .seal(STREAM_FEEDBACK, counter, PURPOSE, &acked_frame.to_be_bytes());

        let mut out = Vec::with_capacity(HEADER_LEN + sealed.len());
        out.push(ECHO_FEEDBACK);
        out.push(0); // flags, reserved
        out.extend_from_slice(&counter.to_be_bytes());
        out.extend_from_slice(&sealed);
        Ok(out)
    }
}

/// Host side: opens feedback datagrams and tracks the acknowledged watermark.
#[derive(Debug)]
pub struct FeedbackReceiver {
    keys: SessionKeys,
    acked: u32,
}

impl FeedbackReceiver {
    pub fn new(keys: SessionKeys) -> Self {
        Self { keys, acked: 0 }
    }

    /// The newest frame index the client has confirmed decoding, or 0 if it has
    /// not reported yet.
    pub fn acked_frame(&self) -> u32 {
        self.acked
    }

    /// Open one datagram and advance the watermark.
    ///
    /// Returns the new watermark when this report moved it, and `None` when it
    /// did not. **The watermark only ever moves forward**, which is what makes
    /// a replayed or reordered datagram harmless: an attacker who captures an
    /// old report cannot use it to retire a reference the client still holds,
    /// and ordinary UDP reordering cannot either. Authentication alone would not
    /// give that property, because a replay is a *valid* datagram.
    pub fn open(&mut self, datagram: &[u8]) -> Result<Option<u32>, FeedbackError> {
        if datagram.len() != DATAGRAM_LEN || datagram.first() != Some(&ECHO_FEEDBACK) {
            return Err(FeedbackError::Malformed);
        }
        let counter = u32::from_be_bytes([datagram[2], datagram[3], datagram[4], datagram[5]]);

        let plaintext = match self
            .keys
            .open(STREAM_FEEDBACK, counter, PURPOSE, &datagram[HEADER_LEN..])
        {
            Ok(p) => p,
            Err(CryptoError::Authentication) => return Err(FeedbackError::Authentication),
            Err(_) => return Err(FeedbackError::Malformed),
        };
        if plaintext.len() != PLAINTEXT_LEN {
            return Err(FeedbackError::Malformed);
        }
        let acked = u32::from_be_bytes([plaintext[0], plaintext[1], plaintext[2], plaintext[3]]);

        if acked > self.acked {
            self.acked = acked;
            Ok(Some(acked))
        } else {
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One session's keys, shared by both ends of a round trip.
    fn keys() -> SessionKeys {
        SessionKeys::generate()
    }

    #[test]
    fn a_report_round_trips() {
        let k = keys();
        let mut tx = FeedbackSender::new(k.clone());
        let mut rx = FeedbackReceiver::new(k);

        let dg = tx.seal(1234).expect("seals");
        assert_eq!(dg.len(), DATAGRAM_LEN, "size is fixed so it can be pre-checked");
        assert_eq!(dg[0], ECHO_FEEDBACK);
        assert_eq!(rx.open(&dg), Ok(Some(1234)));
        assert_eq!(rx.acked_frame(), 1234);
    }

    /// The property the encoder depends on: a stale report can never retire a
    /// reference the client has proven it holds.
    #[test]
    fn the_watermark_never_moves_backwards() {
        let k = keys();
        let mut tx = FeedbackSender::new(k.clone());
        let mut rx = FeedbackReceiver::new(k);

        let first = tx.seal(500).expect("seals");
        let second = tx.seal(900).expect("seals");

        assert_eq!(rx.open(&second), Ok(Some(900)));
        // The older report arrives late — reordering, or a deliberate replay.
        assert_eq!(rx.open(&first), Ok(None), "an older report must not move it");
        assert_eq!(rx.acked_frame(), 900);
    }

    /// Sealed under a different session's keys: opening must fail rather than
    /// silently accepting an index from someone else's stream.
    #[test]
    fn a_datagram_from_another_session_is_refused() {
        let mut tx = FeedbackSender::new(SessionKeys::generate());
        let mut rx = FeedbackReceiver::new(keys());

        let dg = tx.seal(42).expect("seals");
        assert_eq!(rx.open(&dg), Err(FeedbackError::Authentication));
        assert_eq!(rx.acked_frame(), 0, "a refused datagram changes nothing");
    }

    #[test]
    fn a_corrupted_tag_is_refused() {
        let k = keys();
        let mut tx = FeedbackSender::new(k.clone());
        let mut rx = FeedbackReceiver::new(k);

        let mut dg = tx.seal(77).expect("seals");
        *dg.last_mut().expect("non-empty") ^= 0xFF;
        assert_eq!(rx.open(&dg), Err(FeedbackError::Authentication));
    }

    #[test]
    fn a_wrong_sized_datagram_is_rejected_before_decryption() {
        let mut rx = FeedbackReceiver::new(keys());
        assert_eq!(rx.open(&[ECHO_FEEDBACK, 0, 0, 0, 0, 1]), Err(FeedbackError::Malformed));
        assert_eq!(rx.open(&[]), Err(FeedbackError::Malformed));
    }
}

//! Multiplexing over the punched UDP socket.
//!
//! Once Echo runs everything over one hole-punched path, that socket carries
//! four unrelated kinds of datagram at once, and Nova's media port carries a
//! fifth because it still serves Moonlight at the same time:
//!
//! | Kind | Discriminator |
//! |---|---|
//! | STUN (keepalive, punch probes) | first two bits `00` **and** magic cookie |
//! | Moonlight RTP video | `0x90` — RTP version 2 with the extension bit |
//! | Moonlight client "ping" | ASCII `PING` (`0x50…`) |
//! | Echo media | [`ECHO_MEDIA`] |
//! | Echo control | [`ECHO_CONTROL`] / [`ECHO_CONTROL_ACK`] |
//!
//! ## Why the tag costs zero bytes
//!
//! The obvious implementation prepends a byte, which shortens every payload,
//! changes the shard size Reed-Solomon works over, and eats MTU headroom. None
//! of that is necessary, because byte 0 of a datagram is *already* the
//! discriminator: RTP puts its version there, STUN's leading bits are there,
//! and `rtp.rs` writes byte 0 **after** FEC parity has been computed (parity is
//! calculated with bytes 0..16 and 28..32 zeroed — Sunshine's layout, which
//! moonlight-common-c relies on when it regenerates headers on recovered
//! packets).
//!
//! So an Echo session writes its tag into that same slot instead of the RTP
//! version. The consequences are all good ones: datagram size is unchanged, so
//! MTU headroom is untouched; the tag sits outside FEC coverage, so it neither
//! affects nor is affected by reconstruction; and classification stays a
//! single-byte test on the hot path. An Echo client does not need the RTP
//! version field — it is not speaking RTP to anything.
//!
//! Values `0xC0..=0xFF` are used because that range is unreachable for every
//! other kind above: STUN is `0x00..=0x3F`, RTP version 2 is `0x80..=0xBF`, and
//! `PING` starts `0x50`. [`tags_cannot_collide`](tests) asserts it rather than
//! trusting the table.

/// Echo media: the GameStream `NV_VIDEO_PACKET` layout, payload sealed with
/// AES-128-GCM at the frame level.
pub const ECHO_MEDIA: u8 = 0xE0;
/// Echo control: a reliable-delivery frame (see [`crate::rudp`]).
pub const ECHO_CONTROL: u8 = 0xE1;
/// Acknowledgement of a control frame.
pub const ECHO_CONTROL_ACK: u8 = 0xE2;

/// What a datagram arriving on the shared socket is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// RFC 8489 binding request or response.
    Stun,
    /// Echo media datagram.
    EchoMedia,
    /// Echo reliable-control frame.
    EchoControl,
    /// Acknowledgement of a control frame.
    EchoControlAck,
    /// Anything else: Moonlight RTP, a client ping, or noise. Deliberately one
    /// bucket — Nova's existing paths already know how to tell those apart,
    /// and this classifier must not start second-guessing them.
    Other,
}

const MAGIC_COOKIE: u32 = 0x2112_A442;

/// Classify a datagram by its first byte (plus STUN's magic cookie).
///
/// Cheap enough for the receive path: one bounds check and at most one 4-byte
/// comparison.
pub fn classify(buf: &[u8]) -> Class {
    match buf.first() {
        None => Class::Other,
        Some(&ECHO_MEDIA) => Class::EchoMedia,
        Some(&ECHO_CONTROL) => Class::EchoControl,
        Some(&ECHO_CONTROL_ACK) => Class::EchoControlAck,
        Some(&b) if b & 0xC0 == 0 => {
            // Leading bits say "could be STUN"; only the cookie settles it.
            // Without this check a stray datagram starting with a low byte
            // would be handed to the STUN parser as if it were a probe.
            if buf.len() >= 20
                && u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]) == MAGIC_COOKIE
            {
                Class::Stun
            } else {
                Class::Other
            }
        }
        Some(_) => Class::Other,
    }
}

/// True for any datagram belonging to Echo's own protocols — the test the
/// host's media socket uses to decide whether a datagram belongs to the Echo
/// transport rather than to Moonlight.
pub fn is_echo(buf: &[u8]) -> bool {
    matches!(
        classify(buf),
        Class::EchoMedia | Class::EchoControl | Class::EchoControlAck
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The property the whole scheme rests on. If an Echo tag could ever be
    /// read as STUN or RTP (or vice versa), a control frame would be parsed as
    /// a video packet — or worse, a punch probe would be answered with a
    /// session command.
    #[test]
    fn tags_cannot_collide_with_stun_or_rtp() {
        for tag in [ECHO_MEDIA, ECHO_CONTROL, ECHO_CONTROL_ACK] {
            assert_eq!(tag & 0xC0, 0xC0, "Echo tags must live above RTP's range");
            assert_ne!(tag & 0xC0, 0x00, "…and outside STUN's");
            assert_ne!(tag & 0xC0, 0x80, "…and outside RTP version 2's");
        }
        // Moonlight's ping and RTP must both land in `Other`.
        assert_eq!(classify(b"PING"), Class::Other);
        let mut rtp = vec![0u8; 40];
        rtp[0] = 0x90; // V=2, X=1 — what rtp.rs writes
        assert_eq!(classify(&rtp), Class::Other);
    }

    #[test]
    fn a_real_stun_message_is_recognised_and_a_lookalike_is_not() {
        let mut stun = vec![0u8; 20];
        stun[0] = 0x00;
        stun[1] = 0x01; // binding request
        stun[4..8].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
        assert_eq!(classify(&stun), Class::Stun);

        // Same leading bits, no cookie: must NOT reach the STUN parser.
        let mut lookalike = vec![0u8; 40];
        lookalike[0] = 0x01;
        assert_eq!(classify(&lookalike), Class::Other);

        // Cookie but too short to be a header.
        assert_eq!(classify(&stun[..12]), Class::Other);
    }

    #[test]
    fn echo_tags_classify_and_are_recognised_as_ours() {
        assert_eq!(classify(&[ECHO_MEDIA, 0, 0]), Class::EchoMedia);
        assert_eq!(classify(&[ECHO_CONTROL, 0, 0]), Class::EchoControl);
        assert_eq!(classify(&[ECHO_CONTROL_ACK, 0, 0]), Class::EchoControlAck);

        assert!(is_echo(&[ECHO_MEDIA]));
        assert!(is_echo(&[ECHO_CONTROL]));
        assert!(!is_echo(b"PING"));
        assert!(!is_echo(&[]));
    }

    #[test]
    fn an_empty_datagram_is_classified_without_panicking() {
        assert_eq!(classify(&[]), Class::Other);
        assert_eq!(classify(&[0x00]), Class::Other);
    }
}

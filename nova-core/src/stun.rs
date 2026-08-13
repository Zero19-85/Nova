//! STUN (RFC 8489) binding codec and NAT mapping classification.
//!
//! Shared by both peers: the Nova host and the Echo client each discover their
//! own **server-reflexive** address — the public `IP:port` a NAT has mapped for
//! them — and each must recognise the other's probes during a hole punch. One
//! implementation, so the two can never disagree about the wire format.
//!
//! ## Socket-agnostic on purpose
//!
//! A NAT mapping belongs to a **socket**, not to a host. A binding request sent
//! from a scratch socket discovers the mapping *for that scratch socket*, which
//! then closes and takes the mapping with it. To be useful, the request must
//! leave from the socket that will carry traffic — which on the host is
//! `rtp.rs`'s media socket, owned by a dedicated send thread with its own
//! buffers, DSCP marking, and learned target.
//!
//! So this module owns no socket. It is a codec plus a demultiplexer:
//!
//! - [`build_binding_request`] / [`parse_binding_response`] are pure functions
//!   over byte slices, usable from any receive path.
//! - [`is_stun_message`] is the demux predicate that lets STUN share a port
//!   with media — the reason that is safe is documented on the function.
//! - [`discover_mapped_address`] is the convenience path for a socket the
//!   caller owns exclusively.
//!
//! This is also why Nova hand-rolls STUN rather than adopting an ICE crate:
//! measured against this tree, `webrtc-ice` costs +21 crates, `str0m` +19, the
//! `stun` crate +8 — and all of them are built around *owning* the transport,
//! which is backwards for a peer whose media socket already exists. Nova and
//! Echo control both ends of the connection, so none of ICE's interoperability
//! machinery (role conflict resolution, long-term credentials, TURN
//! allocation, trickle negotiation with a browser) buys anything.
//!
//! ## Not ICE
//!
//! No connectivity checks, no candidate pairing, no nomination. This answers
//! one question — "what does the outside world see this socket as?" — plus a
//! second, cheap and high-value one: [`classify_mapping`] compares what two
//! independent STUN servers report and says whether hole punching can work at
//! all. A NAT with endpoint-*dependent* mapping (classically "symmetric")
//! assigns a different public port per destination, so a candidate learned
//! from a STUN server is worthless for reaching a peer; those sessions need a
//! relay. Knowing that *before* attempting a connection is what lets a client
//! fail over quickly instead of timing out.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use rand::Rng;

/// RFC 8489 magic cookie. Its fixed value is what makes STUN safely
/// distinguishable from other traffic on a shared port.
const MAGIC_COOKIE: u32 = 0x2112_A442;

const MSG_BINDING_REQUEST: u16 = 0x0001;
const MSG_BINDING_SUCCESS: u16 = 0x0101;
const MSG_BINDING_ERROR: u16 = 0x0111;

const ATTR_MAPPED_ADDRESS: u16 = 0x0001;
const ATTR_ERROR_CODE: u16 = 0x0009;
const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;

const HEADER_LEN: usize = 20;
const TXID_LEN: usize = 12;

/// Retransmission schedule. RFC 8489 §6.2.1 specifies an RTO with exponential
/// backoff; three attempts over ~3.5 s is the useful part of that curve for an
/// interactive "can I be reached?" probe — a STUN server that has not answered
/// by then is down, not slow.
pub const RETRY_DELAYS: [Duration; 3] = [
    Duration::from_millis(500),
    Duration::from_millis(1000),
    Duration::from_millis(2000),
];

/// Public STUN servers used when no operator override is configured.
///
/// Two *independent* operators, deliberately: [`classify_mapping`] compares
/// the mapping reported by two servers, and two addresses belonging to the
/// same provider can share a front end, which would make an endpoint-dependent
/// NAT look endpoint-independent and send Echo down a hole-punch path that
/// cannot work.
pub const DEFAULT_STUN_SERVERS: [&str; 2] = ["stun.l.google.com:19302", "stun.cloudflare.com:3478"];

/// A 12-byte STUN transaction ID.
///
/// Carried in the request and echoed in the response; checking it is what
/// stops an off-path attacker (or a stale datagram from a previous probe) from
/// convincing Nova it has a public address it does not have.
pub type TransactionId = [u8; TXID_LEN];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StunError {
    /// Not a STUN message at all, or truncated below a parseable size.
    Malformed(&'static str),
    /// A valid STUN message whose transaction ID is not the one we sent.
    TransactionMismatch,
    /// The server answered with a binding error response.
    ErrorResponse { code: u16, reason: String },
    /// Parsed cleanly but carried no address attribute.
    NoAddress,
    /// No response within the retransmission schedule.
    Timeout,
    /// Socket-level failure.
    Io(String),
}

impl std::fmt::Display for StunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StunError::Malformed(w) => write!(f, "malformed STUN message ({w})"),
            StunError::TransactionMismatch => write!(f, "STUN transaction ID mismatch"),
            StunError::ErrorResponse { code, reason } => {
                write!(f, "STUN error response {code}: {reason}")
            }
            StunError::NoAddress => write!(f, "STUN response carried no mapped address"),
            StunError::Timeout => write!(f, "no STUN response"),
            StunError::Io(e) => write!(f, "STUN socket error: {e}"),
        }
    }
}

/// How this NAT assigns public ports — the single fact that decides whether a
/// direct peer-to-peer connection is possible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MappingBehavior {
    /// The same public `IP:port` regardless of destination. A candidate
    /// learned from a STUN server is therefore also valid for reaching a peer:
    /// hole punching works.
    EndpointIndependent,
    /// A different public port per destination ("symmetric"). The address a
    /// STUN server reports is useless for reaching anyone else, so a direct
    /// connection cannot be established and the session must be relayed.
    EndpointDependent,
    /// Not enough successful probes to tell. Treat as "attempt direct, be
    /// ready to fall back" rather than as either answer.
    Unknown,
}

/// The outcome of probing one socket against one or more STUN servers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WanCandidate {
    /// The server-reflexive address: what the outside world sees.
    pub mapped: SocketAddr,
    /// Which STUN server reported it.
    pub via: SocketAddr,
}

// ── Wire codec ──────────────────────────────────────────────────────────────

/// A fresh, cryptographically-random transaction ID.
///
/// Randomness here is a security property, not a uniqueness convenience: it is
/// the only thing binding a response to our request on an unauthenticated
/// channel.
pub fn new_transaction_id() -> TransactionId {
    let mut id = [0u8; TXID_LEN];
    rand::thread_rng().fill(&mut id);
    id
}

/// Encode a STUN Binding Request — a bare 20-byte header with no attributes,
/// which is all RFC 8489 requires to learn a reflexive address.
pub fn build_binding_request(txid: &TransactionId) -> [u8; HEADER_LEN] {
    let mut out = [0u8; HEADER_LEN];
    out[0..2].copy_from_slice(&MSG_BINDING_REQUEST.to_be_bytes());
    out[2..4].copy_from_slice(&0u16.to_be_bytes()); // no attributes
    out[4..8].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
    out[8..20].copy_from_slice(txid);
    out
}

/// Encode a Binding **Success Response** reporting `mapped` back to a peer.
///
/// The host is not only a STUN client: during a hole punch each side answers
/// the other's binding requests, which is how both learn that the path is
/// open. It is also what a test double or a development relay needs in order
/// to stand in for a real STUN server.
pub fn build_binding_response(txid: &TransactionId, mapped: SocketAddr) -> Vec<u8> {
    // XOR-MAPPED-ADDRESS only — the legacy plain form exists for old servers
    // to send, not for us to emit.
    let (family, addr_bytes): (u8, Vec<u8>) = match mapped.ip() {
        IpAddr::V4(v4) => (
            0x01,
            (u32::from_be_bytes(v4.octets()) ^ MAGIC_COOKIE)
                .to_be_bytes()
                .to_vec(),
        ),
        IpAddr::V6(v6) => {
            let mut key = [0u8; 16];
            key[0..4].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
            key[4..16].copy_from_slice(txid);
            let octets = v6.octets();
            (0x02, (0..16).map(|i| octets[i] ^ key[i]).collect())
        }
    };
    let value_len = 4 + addr_bytes.len();

    let mut out = Vec::with_capacity(HEADER_LEN + 4 + value_len);
    out.extend_from_slice(&MSG_BINDING_SUCCESS.to_be_bytes());
    out.extend_from_slice(&((4 + value_len) as u16).to_be_bytes());
    out.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
    out.extend_from_slice(txid);
    out.extend_from_slice(&ATTR_XOR_MAPPED_ADDRESS.to_be_bytes());
    out.extend_from_slice(&(value_len as u16).to_be_bytes());
    out.push(0x00); // reserved
    out.push(family);
    out.extend_from_slice(&(mapped.port() ^ (MAGIC_COOKIE >> 16) as u16).to_be_bytes());
    out.extend_from_slice(&addr_bytes);
    out
}

/// Cheap, allocation-free test for "is this datagram STUN?", for ports that
/// carry both STUN and media.
///
/// Sharing one port is safe because the two protocols are unambiguous in their
/// first bytes:
///
/// - STUN's leading two bits are always `00` (message types are 14-bit), so
///   the first byte is `0x00`–`0x3F`.
/// - RTP's first byte carries version 2 in its top two bits (`0x80` or `0xB0`
///   with padding/extension), so it can never fall in that range.
///
/// The magic cookie is then checked as well, so even a malformed peer cannot
/// have a media packet mistaken for a binding response.
pub fn is_stun_message(buf: &[u8]) -> bool {
    buf.len() >= HEADER_LEN
        && buf[0] & 0xC0 == 0
        && u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]) == MAGIC_COOKIE
}

/// Decode a Binding Response and return the reflexive address it reports.
///
/// Verifies the transaction ID against `txid` before trusting anything in the
/// message. Accepts `XOR-MAPPED-ADDRESS` (RFC 8489) and falls back to the
/// legacy `MAPPED-ADDRESS` (RFC 3489) that a few old servers still send —
/// XOR-MAPPED exists precisely because some NATs rewrite IP addresses they
/// find verbatim in payloads, so the plain form is a fallback, never a
/// preference.
pub fn parse_binding_response(buf: &[u8], txid: &TransactionId) -> Result<SocketAddr, StunError> {
    if !is_stun_message(buf) {
        return Err(StunError::Malformed("not a STUN message"));
    }
    let msg_type = u16::from_be_bytes([buf[0], buf[1]]);
    let body_len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
    if buf.len() < HEADER_LEN + body_len {
        return Err(StunError::Malformed("body shorter than declared length"));
    }
    if buf[8..20] != txid[..] {
        return Err(StunError::TransactionMismatch);
    }

    let body = &buf[HEADER_LEN..HEADER_LEN + body_len];
    if msg_type == MSG_BINDING_ERROR {
        return Err(parse_error_attribute(body));
    }
    if msg_type != MSG_BINDING_SUCCESS {
        return Err(StunError::Malformed("not a binding response"));
    }

    let mut fallback: Option<SocketAddr> = None;
    for (attr_type, value) in AttributeIter::new(body) {
        match attr_type {
            ATTR_XOR_MAPPED_ADDRESS => {
                if let Some(addr) = decode_address(value, true, txid) {
                    return Ok(addr);
                }
            }
            ATTR_MAPPED_ADDRESS => {
                if fallback.is_none() {
                    fallback = decode_address(value, false, txid);
                }
            }
            _ => {}
        }
    }
    fallback.ok_or(StunError::NoAddress)
}

/// Walks `[type u16][len u16][value, padded to 4]` records, stopping at the
/// first malformed one rather than trusting a length that runs off the end.
struct AttributeIter<'a> {
    body: &'a [u8],
    at: usize,
}

impl<'a> AttributeIter<'a> {
    fn new(body: &'a [u8]) -> Self {
        Self { body, at: 0 }
    }
}

impl<'a> Iterator for AttributeIter<'a> {
    type Item = (u16, &'a [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        if self.at + 4 > self.body.len() {
            return None;
        }
        let attr_type = u16::from_be_bytes([self.body[self.at], self.body[self.at + 1]]);
        let len = u16::from_be_bytes([self.body[self.at + 2], self.body[self.at + 3]]) as usize;
        let start = self.at + 4;
        let end = start.checked_add(len)?;
        if end > self.body.len() {
            return None; // declared length overruns the body — stop, don't panic
        }
        // Values are padded to a 4-byte boundary; the padding is not part of
        // the value but must be stepped over.
        self.at = start + len + ((4 - (len % 4)) % 4);
        Some((attr_type, &self.body[start..end]))
    }
}

/// `[reserved u8][family u8][port u16][address]`, optionally XOR-obscured.
fn decode_address(value: &[u8], xor: bool, txid: &TransactionId) -> Option<SocketAddr> {
    if value.len() < 4 {
        return None;
    }
    let family = value[1];
    let raw_port = u16::from_be_bytes([value[2], value[3]]);
    // The port is XORed with the high 16 bits of the magic cookie.
    let port = if xor { raw_port ^ (MAGIC_COOKIE >> 16) as u16 } else { raw_port };

    match family {
        0x01 => {
            let octets: [u8; 4] = value.get(4..8)?.try_into().ok()?;
            let raw = u32::from_be_bytes(octets);
            let addr = if xor { raw ^ MAGIC_COOKIE } else { raw };
            Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(addr)), port))
        }
        0x02 => {
            let octets: [u8; 16] = value.get(4..20)?.try_into().ok()?;
            let addr = if xor {
                // IPv6 is XORed with the cookie followed by the transaction ID.
                let mut key = [0u8; 16];
                key[0..4].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
                key[4..16].copy_from_slice(txid);
                let mut out = [0u8; 16];
                for i in 0..16 {
                    out[i] = octets[i] ^ key[i];
                }
                out
            } else {
                octets
            };
            Some(SocketAddr::new(IpAddr::V6(Ipv6Addr::from(addr)), port))
        }
        _ => None,
    }
}

fn parse_error_attribute(body: &[u8]) -> StunError {
    for (attr_type, value) in AttributeIter::new(body) {
        if attr_type == ATTR_ERROR_CODE && value.len() >= 4 {
            // [reserved u16][class u8][number u8][reason UTF-8]
            let code = (value[2] as u16 & 0x07) * 100 + value[3] as u16;
            let reason = String::from_utf8_lossy(&value[4..]).into_owned();
            return StunError::ErrorResponse { code, reason };
        }
    }
    StunError::ErrorResponse { code: 0, reason: "unspecified".to_string() }
}

// ── Probing ─────────────────────────────────────────────────────────────────

/// Ask one STUN server what it sees `socket` as.
///
/// **The mapping discovered belongs to `socket`.** Pass the socket that will
/// actually carry traffic, or the answer describes a mapping that ceases to
/// exist the moment the probe socket is dropped.
///
/// Retransmits per [`RETRY_DELAYS`] because the probe rides UDP and a single
/// lost datagram must not read as "no public address". Datagrams that are not
/// STUN, or that carry another transaction's ID, are skipped rather than
/// failing the probe — on a shared media port they are simply the media.
pub async fn discover_mapped_address(
    socket: &tokio::net::UdpSocket,
    server: SocketAddr,
) -> Result<WanCandidate, StunError> {
    let txid = new_transaction_id();
    let request = build_binding_request(&txid);
    let mut buf = [0u8; 1500];

    for delay in RETRY_DELAYS {
        socket
            .send_to(&request, server)
            .await
            .map_err(|e| StunError::Io(e.to_string()))?;

        let deadline = tokio::time::Instant::now() + delay;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break; // retransmit
            }
            match tokio::time::timeout(remaining, socket.recv_from(&mut buf)).await {
                Ok(Ok((n, from))) => {
                    if from != server || !is_stun_message(&buf[..n]) {
                        continue; // media, or another peer — not ours
                    }
                    match parse_binding_response(&buf[..n], &txid) {
                        Ok(mapped) => return Ok(WanCandidate { mapped, via: server }),
                        // A stale response from an earlier attempt: keep waiting.
                        Err(StunError::TransactionMismatch) => continue,
                        Err(e) => return Err(e),
                    }
                }
                // See punch.rs: on Windows an ICMP port-unreachable for an
                // earlier datagram surfaces as ECONNRESET on the next recv of
                // a UDP socket. One unreachable STUN server must not fail a
                // probe that other servers may still answer.
                Ok(Err(e))
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::ConnectionRefused
                    ) =>
                {
                    continue
                }
                Ok(Err(e)) => return Err(StunError::Io(e.to_string())),
                Err(_) => break, // this attempt's window elapsed
            }
        }
    }
    Err(StunError::Timeout)
}

/// Decide whether direct peer-to-peer is possible for `socket`, by comparing
/// what two independent STUN servers report.
///
/// Identical mappings ⇒ the NAT assigns one public port per socket regardless
/// of destination, so the address is usable for reaching an Echo client.
/// Differing mappings ⇒ endpoint-dependent, and no amount of candidate
/// exchange will make a direct connection work; the session needs a relay.
///
/// Returns the classification alongside every candidate gathered, so a caller
/// that only wanted the address still gets it.
pub async fn classify_mapping(
    socket: &tokio::net::UdpSocket,
    servers: &[SocketAddr],
) -> (MappingBehavior, Vec<WanCandidate>) {
    let mut candidates = Vec::new();
    for server in servers {
        match discover_mapped_address(socket, *server).await {
            Ok(c) => candidates.push(c),
            Err(e) => println!("🌐 STUN probe via {server} failed: {e}"),
        }
    }
    let behavior = match candidates.as_slice() {
        [] | [_] => MappingBehavior::Unknown,
        [first, rest @ ..] => {
            if rest.iter().all(|c| c.mapped == first.mapped) {
                MappingBehavior::EndpointIndependent
            } else {
                MappingBehavior::EndpointDependent
            }
        }
    };
    (behavior, candidates)
}

#[cfg(test)]
mod tests {
    use super::*;


    #[test]
    fn binding_request_has_the_rfc_header_shape() {
        let txid = [0xAA; TXID_LEN];
        let req = build_binding_request(&txid);
        assert_eq!(u16::from_be_bytes([req[0], req[1]]), MSG_BINDING_REQUEST);
        assert_eq!(u16::from_be_bytes([req[2], req[3]]), 0, "no attributes");
        assert_eq!(
            u32::from_be_bytes([req[4], req[5], req[6], req[7]]),
            MAGIC_COOKIE
        );
        assert_eq!(&req[8..20], &txid[..]);
        assert!(is_stun_message(&req));
    }

    /// Independent check of the XOR decode: the wire bytes below are the
    /// obscured form, hand-computed, and the parser must recover the plain
    /// address RFC 5769's IPv4 sample describes (192.0.2.1:32853).
    ///   port 32853 (0x8055) ^ 0x2112             = 0xA147
    ///   addr 192.0.2.1 (0xC0000201) ^ 0x2112A442 = 0xE112A643
    #[test]
    fn xor_mapped_address_decodes_to_the_plain_address() {
        let txid = [0x42; TXID_LEN];
        let mut msg = Vec::new();
        msg.extend_from_slice(&MSG_BINDING_SUCCESS.to_be_bytes());
        msg.extend_from_slice(&12u16.to_be_bytes()); // one 12-byte attribute
        msg.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        msg.extend_from_slice(&txid);
        msg.extend_from_slice(&ATTR_XOR_MAPPED_ADDRESS.to_be_bytes());
        msg.extend_from_slice(&8u16.to_be_bytes());
        msg.extend_from_slice(&[0x00, 0x01]); // reserved, IPv4
        msg.extend_from_slice(&[0xA1, 0x47]); // x-port
        msg.extend_from_slice(&[0xE1, 0x12, 0xA6, 0x43]); // x-address

        let addr = parse_binding_response(&msg, &txid).expect("decode");
        assert_eq!(addr, "192.0.2.1:32853".parse().unwrap());
    }

    #[test]
    fn legacy_mapped_address_is_accepted_when_xor_is_absent() {
        let txid = [0x11; TXID_LEN];
        let mut msg = Vec::new();
        msg.extend_from_slice(&MSG_BINDING_SUCCESS.to_be_bytes());
        msg.extend_from_slice(&12u16.to_be_bytes());
        msg.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        msg.extend_from_slice(&txid);
        msg.extend_from_slice(&ATTR_MAPPED_ADDRESS.to_be_bytes());
        msg.extend_from_slice(&8u16.to_be_bytes());
        msg.extend_from_slice(&[0x00, 0x01]);
        msg.extend_from_slice(&8080u16.to_be_bytes());
        msg.extend_from_slice(&[203, 0, 113, 9]);

        assert_eq!(
            parse_binding_response(&msg, &txid).unwrap(),
            "203.0.113.9:8080".parse().unwrap()
        );
    }

    /// The check that stops an off-path datagram from forging our public
    /// address.
    #[test]
    fn a_response_for_another_transaction_is_rejected() {
        let ours = [0x01; TXID_LEN];
        let theirs = [0x02; TXID_LEN];
        let mut msg = Vec::new();
        msg.extend_from_slice(&MSG_BINDING_SUCCESS.to_be_bytes());
        msg.extend_from_slice(&12u16.to_be_bytes());
        msg.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        msg.extend_from_slice(&theirs);
        msg.extend_from_slice(&ATTR_XOR_MAPPED_ADDRESS.to_be_bytes());
        msg.extend_from_slice(&8u16.to_be_bytes());
        msg.extend_from_slice(&[0x00, 0x01, 0xA1, 0x47, 0xE1, 0x12, 0xA6, 0x43]);

        assert_eq!(
            parse_binding_response(&msg, &ours),
            Err(StunError::TransactionMismatch)
        );
    }

    /// The property that lets STUN and RTP share port 47998 — see
    /// `is_stun_message`.
    #[test]
    fn stun_and_rtp_are_unambiguous_on_a_shared_port() {
        assert!(is_stun_message(&build_binding_request(&[0; TXID_LEN])));

        // A real RTP header: version 2 in the top two bits ⇒ first byte 0x80.
        let mut rtp = vec![0x80, 0x60];
        rtp.extend_from_slice(&[0u8; 30]);
        assert!(!is_stun_message(&rtp), "RTP must never look like STUN");

        // Right leading bits, wrong cookie (RFC 3489-era or spoofed).
        let mut no_cookie = build_binding_request(&[0; TXID_LEN]).to_vec();
        no_cookie[4] = 0x00;
        assert!(!is_stun_message(&no_cookie));

        assert!(!is_stun_message(&[]), "empty datagram");
        assert!(!is_stun_message(&[0u8; 8]), "shorter than a header");
    }

    /// Hostile input must be refused, never panic: this parser runs on bytes
    /// from the open internet, inside the LocalSystem service.
    #[test]
    fn truncated_and_overlong_attributes_are_refused_not_panicked_on() {
        let txid = [0x33; TXID_LEN];
        let mut header = Vec::new();
        header.extend_from_slice(&MSG_BINDING_SUCCESS.to_be_bytes());
        header.extend_from_slice(&8u16.to_be_bytes());
        header.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        header.extend_from_slice(&txid);

        // Declared body length exceeds what is present.
        assert!(matches!(
            parse_binding_response(&header, &txid),
            Err(StunError::Malformed(_))
        ));

        // Attribute claims 0xFFFF bytes inside an 8-byte body.
        let mut lying = header.clone();
        lying.extend_from_slice(&ATTR_XOR_MAPPED_ADDRESS.to_be_bytes());
        lying.extend_from_slice(&0xFFFFu16.to_be_bytes());
        lying.extend_from_slice(&[0x00, 0x01, 0xA1, 0x47]);
        assert_eq!(parse_binding_response(&lying, &txid), Err(StunError::NoAddress));

        // Address attribute truncated mid-address.
        let mut short = header.clone();
        short.extend_from_slice(&ATTR_XOR_MAPPED_ADDRESS.to_be_bytes());
        short.extend_from_slice(&4u16.to_be_bytes());
        short.extend_from_slice(&[0x00, 0x01, 0xA1, 0x47]);
        assert_eq!(parse_binding_response(&short, &txid), Err(StunError::NoAddress));

        // Every truncation of a well-formed message must be refused cleanly.
        let mut full = header.clone();
        full[2..4].copy_from_slice(&12u16.to_be_bytes());
        full.extend_from_slice(&ATTR_XOR_MAPPED_ADDRESS.to_be_bytes());
        full.extend_from_slice(&8u16.to_be_bytes());
        full.extend_from_slice(&[0x00, 0x01, 0xA1, 0x47, 0xE1, 0x12, 0xA6, 0x43]);
        for cut in 0..full.len() {
            let _ = parse_binding_response(&full[..cut], &txid); // must not panic
        }
    }

    #[test]
    fn error_responses_surface_their_code() {
        let txid = [0x55; TXID_LEN];
        let reason = b"Bad Request";
        let attr_len = 4 + reason.len();
        let padded = attr_len + ((4 - (attr_len % 4)) % 4);
        let mut msg = Vec::new();
        msg.extend_from_slice(&MSG_BINDING_ERROR.to_be_bytes());
        msg.extend_from_slice(&((4 + padded) as u16).to_be_bytes());
        msg.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        msg.extend_from_slice(&txid);
        msg.extend_from_slice(&ATTR_ERROR_CODE.to_be_bytes());
        msg.extend_from_slice(&(attr_len as u16).to_be_bytes());
        msg.extend_from_slice(&[0x00, 0x00, 0x04, 0x00]); // class 4, number 0 => 400
        msg.extend_from_slice(reason);
        msg.resize(HEADER_LEN + 4 + padded, 0);

        assert_eq!(
            parse_binding_response(&msg, &txid),
            Err(StunError::ErrorResponse { code: 400, reason: "Bad Request".to_string() })
        );
    }

    /// End-to-end over loopback: proves the request we build is one a real
    /// STUN implementation would accept, and that the receive path skips
    /// non-STUN traffic arriving on the same socket.
    #[tokio::test]
    async fn probe_round_trips_against_a_local_stun_responder() {
        let server = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server_addr = server.local_addr().unwrap();
        let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let client_addr = client.local_addr().unwrap();

        tokio::spawn(async move {
            let mut buf = [0u8; 1500];
            let (n, from) = server.recv_from(&mut buf).await.unwrap();
            assert!(is_stun_message(&buf[..n]));
            let txid: TransactionId = buf[8..20].try_into().unwrap();

            // Reply describing the sender, the way a real server would.
            let SocketAddr::V4(v4) = from else { panic!("v4 expected") };
            let xport = v4.port() ^ (MAGIC_COOKIE >> 16) as u16;
            let xaddr = u32::from_be_bytes(v4.ip().octets()) ^ MAGIC_COOKIE;
            let mut msg = Vec::new();
            msg.extend_from_slice(&MSG_BINDING_SUCCESS.to_be_bytes());
            msg.extend_from_slice(&12u16.to_be_bytes());
            msg.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
            msg.extend_from_slice(&txid);
            msg.extend_from_slice(&ATTR_XOR_MAPPED_ADDRESS.to_be_bytes());
            msg.extend_from_slice(&8u16.to_be_bytes());
            msg.extend_from_slice(&[0x00, 0x01]);
            msg.extend_from_slice(&xport.to_be_bytes());
            msg.extend_from_slice(&xaddr.to_be_bytes());
            // Media-shaped noise first: the probe must ignore it, not fail.
            let _ = server.send_to(&[0x80, 0x60, 0, 0, 0, 0, 0, 0], from).await;
            server.send_to(&msg, from).await.unwrap();
        });

        let got = discover_mapped_address(&client, server_addr).await.expect("probe");
        assert_eq!(got.mapped, client_addr, "reflexive address is the probing socket's");
        assert_eq!(got.via, server_addr);
    }

    /// Live probe against the real internet — the only way to know the codec
    /// interoperates with servers we did not write. Ignored by default (needs
    /// outbound UDP/3478 and public STUN reachability), matching this
    /// codebase's other live diagnostics:
    ///   cargo test --lib echo::wan::tests::live_ -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "hits public STUN servers over the real internet — run explicitly"]
    async fn live_stun_reports_this_hosts_public_address() {
        use tokio::net::lookup_host;

        let mut servers = Vec::new();
        for name in DEFAULT_STUN_SERVERS {
            match lookup_host(name).await {
                Ok(addrs) => {
                    if let Some(a) = addrs.filter(|a| a.is_ipv4()).next() {
                        println!("resolved {name} → {a}");
                        servers.push(a);
                    }
                }
                Err(e) => println!("could not resolve {name}: {e}"),
            }
        }
        assert!(!servers.is_empty(), "no STUN server resolved — check DNS");

        let socket = tokio::net::UdpSocket::bind("0.0.0.0:0").await.unwrap();
        println!("local socket: {}", socket.local_addr().unwrap());
        let (behavior, candidates) = classify_mapping(&socket, &servers).await;
        for c in &candidates {
            println!("reflexive {} (via {})", c.mapped, c.via);
        }
        println!("NAT mapping behavior: {behavior:?}");
        assert!(!candidates.is_empty(), "no STUN server answered");
    }
    #[test]
    fn mapping_classification_needs_two_agreeing_observations() {
        let a: SocketAddr = "198.51.100.7:41000".parse().unwrap();
        let b: SocketAddr = "198.51.100.7:41001".parse().unwrap();
        let s1: SocketAddr = "1.1.1.1:3478".parse().unwrap();
        let s2: SocketAddr = "8.8.8.8:3478".parse().unwrap();

        let same = vec![
            WanCandidate { mapped: a, via: s1 },
            WanCandidate { mapped: a, via: s2 },
        ];
        let differing = vec![
            WanCandidate { mapped: a, via: s1 },
            WanCandidate { mapped: b, via: s2 },
        ];
        // Mirrors classify_mapping's decision on gathered candidates.
        let judge = |c: &[WanCandidate]| match c {
            [] | [_] => MappingBehavior::Unknown,
            [first, rest @ ..] => {
                if rest.iter().all(|x| x.mapped == first.mapped) {
                    MappingBehavior::EndpointIndependent
                } else {
                    MappingBehavior::EndpointDependent
                }
            }
        };
        assert_eq!(judge(&same), MappingBehavior::EndpointIndependent);
        assert_eq!(judge(&differing), MappingBehavior::EndpointDependent);
        assert_eq!(judge(&same[..1]), MappingBehavior::Unknown, "one probe proves nothing");
        assert_eq!(judge(&[]), MappingBehavior::Unknown);
    }
}

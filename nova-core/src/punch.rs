//! UDP hole punching: simultaneous open between two peers behind NATs.
//!
//! Shared by both sides, because the algorithm is symmetric — there is no
//! "client" and "server" in a punch, only two peers doing the same thing at
//! the same time. That symmetry is the mechanism, not an implementation
//! convenience:
//!
//! 1. Each peer has learned its own public address from a STUN server
//!    ([`crate::stun`]) and traded it through the relay.
//! 2. Both then send to the other's address at once. The **first outbound
//!    packet from each side creates that side's NAT mapping**, so the other
//!    side's packets — which would otherwise be dropped as unsolicited — now
//!    arrive at a mapping that already exists.
//! 3. The early packets from each side usually *are* dropped, because they
//!    arrive before the far NAT has its mapping. That is expected. Retrying
//!    for a few seconds is what makes it work, not a workaround.
//!
//! ## Why the probes are STUN
//!
//! A punch probe has to be distinguishable from media on a socket that will
//! carry both, and it needs a request/response pair so each side can tell
//! "my packet arrived" from "I received something". A STUN binding
//! request/response is exactly that, already implemented, and already
//! demultiplexable from RTP (see [`crate::stun::is_stun_message`]). Using
//! anything bespoke here would mean inventing a second framing with the same
//! properties.
//!
//! ## What "success" means
//!
//! Either direction confirms a path:
//!
//! - A binding **response** to one of our requests proves our packets reach
//!   the peer *and* theirs reach us — a full round trip.
//! - A binding **request** from the peer proves at minimum that their packets
//!   reach us, and we answer it so their side completes too.
//!
//! Both are reported, because a NAT that permits one direction first is
//! normal; the caller wants the address, and it is the same address either
//! way.
//!
//! ## What this is not
//!
//! Not ICE. There is no candidate prioritisation, no role negotiation, no
//! nomination round, and no TURN fallback. With endpoint-dependent
//! ("symmetric") NAT on either side this will simply fail — see
//! [`crate::stun::MappingBehavior`] — and the honest response is to relay the
//! session, which is a separate piece of work.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::UdpSocket;

use crate::stun::{
    build_binding_request, build_binding_response, is_stun_message, new_transaction_id,
    parse_binding_response, StunError, TransactionId,
};

/// How often each peer re-sends to every candidate while punching.
///
/// Frequent enough that the window where both mappings exist is hit quickly —
/// the two sides start at different instants, so the overlap has to be sampled
/// densely — and sparse enough not to look like a flood to a NAT that rate
/// limits. At 25 ms a two-second blast is ~80 attempts per candidate, which is
/// far more overlap than any realistic timing skew needs.
pub const PROBE_INTERVAL: Duration = Duration::from_millis(25);

/// How long to keep trying before giving up.
///
/// Punches that work usually work in well under a second; several seconds
/// covers a slow relay round trip and one peer starting late. Beyond that the
/// answer is almost always "this NAT will not permit it", and failing quickly
/// lets a caller fall back to a relay while the user is still waiting.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(8);

/// How a path was confirmed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PunchProof {
    /// We received a response to our own request: a full round trip.
    RoundTrip,
    /// We received the peer's request (and answered it): their packets reach
    /// us, and our reply completes their side.
    PeerProbe,
}

/// A confirmed open path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PunchResult {
    /// The address to send to — the source of the packet that confirmed the
    /// path, which is not necessarily the candidate we aimed at (a NAT may
    /// reveal a different port than the peer predicted).
    pub peer: SocketAddr,
    pub proof: PunchProof,
    /// Probe rounds spent before confirmation, for diagnostics.
    pub rounds: u32,
}

/// Cadence and duration of a blast.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PunchConfig {
    /// Gap between rounds. Every round re-sends to every candidate.
    pub interval: Duration,
    /// Total time before giving up.
    pub timeout: Duration,
}

impl Default for PunchConfig {
    fn default() -> Self {
        Self { interval: PROBE_INTERVAL, timeout: DEFAULT_TIMEOUT }
    }
}

/// The transport a punch runs over.
///
/// This trait exists because the two peers reach the wire differently and the
/// algorithm must nevertheless be **one implementation**. Echo owns its UDP
/// socket outright. Nova does not: its media socket belongs to `rtp.rs`, sends
/// go through that sender, and inbound STUN arrives on a channel fed by the
/// demux hook. Duplicating the loop for each would guarantee the two sides
/// eventually disagree about timing or latching — in a protocol whose entire
/// premise is that both sides do the same thing at the same time.
pub trait PunchIo {
    /// Best-effort send. Failures are ignored on purpose: one unreachable
    /// candidate (a v6 address on a v4 socket, a dead route) must not abort
    /// the attempt on the others.
    fn send_to(&self, data: &[u8], to: SocketAddr);

    /// Next datagram, or `None` if nothing arrives within `timeout`.
    ///
    /// Implementations must swallow transport errors that are *expected*
    /// during a punch — notably `ConnectionReset` on Windows, which is how an
    /// ICMP port-unreachable for an earlier datagram surfaces on the next UDP
    /// receive, and which happens routinely before the far NAT has its
    /// mapping.
    ///
    /// Written as an explicit `impl Future + Send` rather than `async fn`
    /// because the `Send` bound is load-bearing, not incidental: Nova drives
    /// its punch from a `tokio::spawn`ed task, so a non-`Send` future here
    /// would fail to compile at a call site far from the implementation. An
    /// `async fn` in the impl satisfies this signature as long as it really is
    /// `Send`, which is exactly the property worth enforcing.
    fn recv(
        &mut self,
        timeout: Duration,
    ) -> impl std::future::Future<Output = Option<(Vec<u8>, SocketAddr)>> + Send;
}

/// A punch over a socket the caller owns.
pub struct UdpPunchIo<'a> {
    socket: &'a UdpSocket,
    buf: [u8; 1500],
}

impl<'a> UdpPunchIo<'a> {
    pub fn new(socket: &'a UdpSocket) -> Self {
        Self { socket, buf: [0u8; 1500] }
    }
}

impl PunchIo for UdpPunchIo<'_> {
    fn send_to(&self, data: &[u8], to: SocketAddr) {
        // `try_send_to` keeps this synchronous, matching the trait: a blast
        // round must not await between candidates, or the "simultaneous" in
        // simultaneous open stops being true under load.
        let _ = self.socket.try_send_to(data, to);
    }

    async fn recv(&mut self, timeout: Duration) -> Option<(Vec<u8>, SocketAddr)> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            match tokio::time::timeout(remaining, self.socket.recv_from(&mut self.buf)).await {
                Ok(Ok((n, from))) => return Some((self.buf[..n].to_vec(), from)),
                // See the trait docs: routine during a punch, never fatal.
                Ok(Err(e))
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::ConnectionRefused
                    ) => {}
                Ok(Err(_)) => return None,
                Err(_) => return None,
            }
        }
    }
}

/// Punch toward `candidates` on `socket` until a path is confirmed or the
/// timeout elapses.
///
/// `socket` must be the socket that will carry traffic afterwards — the
/// mapping opened belongs to it, and using a different one would mean punching
/// a hole and then walking past it.
pub async fn punch(
    socket: &UdpSocket,
    candidates: &[SocketAddr],
    timeout: Duration,
) -> Result<PunchResult, StunError> {
    let mut io = UdpPunchIo::new(socket);
    punch_io(&mut io, candidates, PunchConfig { timeout, ..Default::default() }).await
}

/// The blast: fire at every candidate every `cfg.interval` until a path is
/// confirmed or `cfg.timeout` elapses. **Both peers run exactly this.**
///
/// Non-STUN datagrams are ignored rather than treated as errors: on a real
/// media socket they are the media.
pub async fn punch_io<T: PunchIo + ?Sized>(
    io: &mut T,
    candidates: &[SocketAddr],
    cfg: PunchConfig,
) -> Result<PunchResult, StunError> {
    // Deduplicate, preserving order. A peer behind an endpoint-independent NAT
    // reports the SAME address from every STUN server it asked, so its offer
    // routinely lists one address several times — blasting each copy would
    // multiply the packet rate at a NAT for no additional coverage, which is
    // exactly the behaviour that gets a source rate-limited.
    let mut targets: Vec<SocketAddr> = Vec::with_capacity(candidates.len());
    for c in candidates {
        if !targets.contains(c) {
            targets.push(*c);
        }
    }
    if targets.is_empty() {
        return Err(StunError::Io("no peer candidates to punch toward".to_string()));
    }
    let candidates = &targets[..];

    // One transaction ID for the whole attempt: every retransmission to every
    // candidate is the same logical request, so a response to any of them
    // matches.
    let txid: TransactionId = new_transaction_id();
    let request = build_binding_request(&txid);
    let deadline = tokio::time::Instant::now() + cfg.timeout;
    let mut rounds = 0u32;

    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err(StunError::Timeout);
        }

        for candidate in candidates {
            io.send_to(&request, *candidate);
        }
        rounds += 1;

        let round_end = (tokio::time::Instant::now() + cfg.interval).min(deadline);
        loop {
            let remaining = round_end.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            let Some((datagram, from)) = io.recv(remaining).await else {
                break; // nothing this round
            };
            if !is_stun_message(&datagram) {
                continue; // media, or noise
            }

            // A response to OUR request: the round trip closed. Latch here.
            if parse_binding_response(&datagram, &txid).is_ok() {
                return Ok(PunchResult { peer: from, proof: PunchProof::RoundTrip, rounds });
            }

            // Otherwise: the peer probing us. Answering is what completes
            // THEIR side, so it is a cooperative obligation, not an
            // optimisation — a peer that never hears back keeps blasting until
            // it gives up, even though our own NAT is already open to it.
            if let Some(peer_txid) = binding_request_txid(&datagram) {
                io.send_to(&build_binding_response(&peer_txid, from), from);
                return Ok(PunchResult { peer: from, proof: PunchProof::PeerProbe, rounds });
            }
        }

        // Hold the cadence even when `recv` returns early (a closed transport,
        // or a burst of noise): without this the loop would spin at full speed
        // and turn a blast into a flood.
        tokio::time::sleep_until(round_end).await;
    }
}

/// The transaction ID of an inbound binding **request**, or `None` if this is
/// not one.
///
/// Callers on a live media socket need this to answer a peer's probes for as
/// long as the path is in use; a NAT that stops seeing traffic in one
/// direction may drop the mapping even while the other direction is busy.
pub fn binding_request_txid(datagram: &[u8]) -> Option<TransactionId> {
    if !is_stun_message(datagram) {
        return None;
    }
    // Message type 0x0001 = Binding Request.
    if u16::from_be_bytes([datagram[0], datagram[1]]) != 0x0001 {
        return None;
    }
    datagram[8..20].try_into().ok()
}

/// Answer a peer's binding request, reporting where we saw it from.
///
/// Split out so a live media loop can keep honouring probes after the initial
/// punch without re-entering [`punch`].
pub async fn answer_probe(socket: &UdpSocket, datagram: &[u8], from: SocketAddr) -> bool {
    match binding_request_txid(datagram) {
        Some(txid) => {
            let response = build_binding_response(&txid, from);
            socket.send_to(&response, from).await.is_ok()
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real thing, both sides at once on loopback: two sockets that have
    /// never heard of each other, punching simultaneously. Neither is a
    /// server; whichever confirms first does so by one of the two proofs.
    #[tokio::test]
    async fn two_peers_punching_simultaneously_both_confirm() {
        let a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let a_addr = a.local_addr().unwrap();
        let b_addr = b.local_addr().unwrap();

        let ta = tokio::spawn(async move { punch(&a, &[b_addr], Duration::from_secs(5)).await });
        let tb = tokio::spawn(async move { punch(&b, &[a_addr], Duration::from_secs(5)).await });

        let ra = ta.await.unwrap().expect("peer A must confirm a path");
        let rb = tb.await.unwrap().expect("peer B must confirm a path");
        assert_eq!(ra.peer, b_addr, "A learns B's address");
        assert_eq!(rb.peer, a_addr, "B learns A's address");
    }

    /// Punching at an address nobody is listening on must fail cleanly and
    /// promptly, so a caller can fall back to a relay rather than hang.
    #[tokio::test]
    async fn punching_into_the_void_times_out_rather_than_hanging() {
        let a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        // Port 9 (discard) on loopback: nothing will answer.
        let nowhere: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let started = std::time::Instant::now();
        let err = punch(&a, &[nowhere], Duration::from_millis(400))
            .await
            .expect_err("must not succeed");
        assert_eq!(err, StunError::Timeout);
        assert!(started.elapsed() < Duration::from_secs(3), "must give up near the deadline");
    }

    #[tokio::test]
    async fn no_candidates_is_an_error_not_an_infinite_wait() {
        let a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        assert!(punch(&a, &[], Duration::from_secs(1)).await.is_err());
    }

    /// A peer behind an endpoint-independent NAT offers the same address once
    /// per STUN server it asked. Blasting each copy doubles the packet rate at
    /// the NAT for no extra coverage.
    #[tokio::test]
    async fn duplicate_candidates_are_blasted_once_not_twice() {
        let peer: SocketAddr = "203.0.113.9:47998".parse().unwrap();
        let other: SocketAddr = "198.51.100.4:47998".parse().unwrap();
        let sent = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        drop(tx);
        let mut io = ChannelPunchIo { sent: sent.clone(), inbox: rx };

        let cfg = PunchConfig {
            interval: Duration::from_millis(40),
            timeout: Duration::from_millis(100),
        };
        let _ = punch_io(&mut io, &[peer, peer, other, peer], cfg).await;

        let sent = sent.lock().unwrap();
        assert!(!sent.is_empty(), "should have sent at least one round");
        let to_peer = sent.iter().filter(|(_, to)| *to == peer).count();
        let to_other = sent.iter().filter(|(_, to)| *to == other).count();
        assert_eq!(to_peer, to_other, "each distinct address gets exactly one packet per round");
    }

    /// Media sharing the socket must be ignored, not mistaken for a probe.
    #[tokio::test]
    async fn media_on_the_punching_socket_is_ignored() {
        let a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let a_addr = a.local_addr().unwrap();
        let b_addr = b.local_addr().unwrap();

        // B sends RTP-shaped noise, then eventually a real probe.
        tokio::spawn(async move {
            for _ in 0..5 {
                let _ = b.send_to(&[0x80, 0x60, 0, 0, 0, 0, 0, 0], a_addr).await;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let _ = b
                .send_to(&build_binding_request(&[9u8; 12]), a_addr)
                .await;
            tokio::time::sleep(Duration::from_millis(500)).await;
        });

        let result = punch(&a, &[b_addr], Duration::from_secs(3))
            .await
            .expect("the real probe must be found among the noise");
        assert_eq!(result.proof, PunchProof::PeerProbe);
    }

    /// A transport shaped like Nova's: sends go one way, receives arrive on a
    /// channel rather than from the socket. Proves `punch_io` is genuinely
    /// transport-agnostic — the reason the algorithm could be shared instead
    /// of written twice.
    struct ChannelPunchIo {
        sent: std::sync::Arc<std::sync::Mutex<Vec<(Vec<u8>, SocketAddr)>>>,
        inbox: tokio::sync::mpsc::UnboundedReceiver<(Vec<u8>, SocketAddr)>,
    }

    impl PunchIo for ChannelPunchIo {
        fn send_to(&self, data: &[u8], to: SocketAddr) {
            self.sent.lock().unwrap().push((data.to_vec(), to));
        }
        async fn recv(&mut self, timeout: Duration) -> Option<(Vec<u8>, SocketAddr)> {
            tokio::time::timeout(timeout, self.inbox.recv()).await.ok().flatten()
        }
    }

    #[tokio::test]
    async fn the_same_algorithm_runs_over_a_non_socket_transport() {
        let peer: SocketAddr = "203.0.113.7:47998".parse().unwrap();
        let sent = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut io = ChannelPunchIo { sent: sent.clone(), inbox: rx };

        // Feed a response to whatever transaction the punch invents, the way
        // the peer would once its NAT lets our packets through.
        let sent_for_task = sent.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(30)).await;
                let first = sent_for_task.lock().unwrap().first().cloned();
                if let Some((request, _)) = first {
                    let txid: TransactionId = request[8..20].try_into().unwrap();
                    let _ = tx.send((build_binding_response(&txid, peer), peer));
                    return;
                }
            }
        });

        let result = punch_io(&mut io, &[peer], PunchConfig::default())
            .await
            .expect("must confirm over a channel transport too");
        assert_eq!(result.peer, peer);
        assert_eq!(result.proof, PunchProof::RoundTrip);

        // Every datagram it emitted went to the candidate, and all shared one
        // transaction — retransmissions of one logical request, not a new
        // request each round.
        let sent = sent.lock().unwrap();
        assert!(!sent.is_empty());
        let first_txid = &sent[0].0[8..20];
        for (payload, to) in sent.iter() {
            assert_eq!(*to, peer);
            assert_eq!(&payload[8..20], first_txid, "one transaction for the whole blast");
        }
    }

    /// The blast must hold its cadence rather than spinning when the transport
    /// yields nothing — otherwise a dead peer turns into a packet flood.
    #[tokio::test]
    async fn a_silent_transport_paces_rather_than_floods() {
        let peer: SocketAddr = "203.0.113.8:1234".parse().unwrap();
        let sent = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        // Sender dropped immediately: `recv` returns None straight away, every
        // time. Without pacing this would spin as fast as the CPU allows.
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        drop(tx);
        let mut io = ChannelPunchIo { sent: sent.clone(), inbox: rx };

        let cfg = PunchConfig {
            interval: Duration::from_millis(25),
            timeout: Duration::from_millis(250),
        };
        assert!(punch_io(&mut io, &[peer], cfg).await.is_err());

        let rounds = sent.lock().unwrap().len();
        // ~10 rounds expected in 250 ms at 25 ms; allow generous slack for a
        // loaded CI box, but a spin would be in the thousands.
        assert!(rounds <= 40, "cadence not held: {rounds} rounds in 250ms");
        assert!(rounds >= 2, "should have blasted more than once: {rounds}");
    }

    #[test]
    fn binding_requests_are_told_apart_from_responses_and_junk() {
        let txid = [3u8; 12];
        assert_eq!(binding_request_txid(&build_binding_request(&txid)), Some(txid));
        // A response is not a request.
        let response = build_binding_response(&txid, "203.0.113.1:1234".parse().unwrap());
        assert_eq!(binding_request_txid(&response), None);
        assert_eq!(binding_request_txid(&[0x80, 0x60, 0, 0]), None);
        assert_eq!(binding_request_txid(&[]), None);
    }
}

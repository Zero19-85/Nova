//! Reliable, ordered delivery for Echo's control messages over UDP.
//!
//! Media tolerates loss — that is what FEC and P-frame recovery are for.
//! Session state does not: a `start_session` that evaporates leaves a client
//! waiting for a stream the host never started, and a `stop_session` that
//! evaporates leaves the host holding a pipeline nobody is watching, which
//! blocks the *next* client, including a Moonlight one. So control gets
//! acknowledgement and retransmission, and media does not.
//!
//! ## What this is, and what it deliberately is not
//!
//! It is a **reliable ordered message channel**: each message is sequenced,
//! retransmitted until acknowledged, and delivered to the application exactly
//! once and in order. It is *not* a congestion-controlled byte stream. Control
//! traffic is a handful of small messages per session at human cadence, so
//! there is nothing to congestion-control and no benefit to pretending
//! otherwise.
//!
//! Two consequences follow, both deliberate:
//!
//! - **One message per datagram.** [`MAX_PAYLOAD`] is the hard cap and
//!   oversized messages are refused rather than fragmented. Nova's control
//!   messages are a few hundred bytes; a fragmentation layer would be code
//!   with no caller.
//! - **A bounded send window.** At most [`MAX_IN_FLIGHT`] unacknowledged
//!   messages, so a peer that stops acknowledging cannot make the sender
//!   buffer without limit.
//!
//! ## Transport-agnostic by construction
//!
//! Like [`crate::punch`], this owns no socket. It consumes datagrams and
//! produces datagrams; the caller moves the bytes. That is what lets the host
//! drive it through `rtp.rs`'s media socket (which it does not own) while the
//! client drives it through its own — one implementation, two very different
//! transports, and a test suite that can drop packets at will.
//!
//! ## Wire format
//!
//! ```text
//!   DATA:  [0xE1][flags][seq u32 BE][payload …]
//!   ACK:   [0xE2][flags][seq u32 BE]
//! ```
//!
//! `flags` is reserved and sent as zero; it exists so a future need (a close
//! marker, a keepalive bit) is additive rather than a format break.
//!
//! Every received DATA is acknowledged, **including duplicates** — a duplicate
//! usually means our previous ACK was the packet that got lost, so answering
//! it again is the whole recovery mechanism rather than wasted traffic.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use crate::demux::{ECHO_CONTROL, ECHO_CONTROL_ACK};

/// Bytes of framing before a control payload.
pub const HEADER_LEN: usize = 6;

/// Largest control payload accepted in one message.
///
/// Chosen to keep the whole datagram (payload + header) comfortably under a
/// 1400-byte WAN MTU, so a control message is never fragmented by the network.
/// Fragmented UDP is worse than large UDP: one lost fragment discards the
/// entire datagram, which for a retransmitting channel means a message that
/// keeps failing for a reason no counter explains.
pub const MAX_PAYLOAD: usize = 1200;

/// Unacknowledged messages allowed in flight.
pub const MAX_IN_FLIGHT: usize = 8;

/// First retransmission delay. Deliberately longer than a LAN round trip and
/// shorter than a human notices, so the common case (nothing was lost) never
/// retransmits and the uncommon one recovers before a user reaches for the
/// reconnect button.
pub const RETRY_INITIAL: Duration = Duration::from_millis(150);
/// Ceiling for the doubling backoff.
pub const RETRY_MAX: Duration = Duration::from_millis(1200);
/// Attempts before a message is declared undeliverable.
///
/// With the backoff above this is roughly 8 seconds of trying — long enough to
/// ride out a Wi-Fi roam or a NAT hiccup, short enough that a genuinely dead
/// path is reported rather than retried forever.
pub const MAX_ATTEMPTS: u32 = 8;

/// Reordering tolerance: how far ahead of the next expected sequence a message
/// may arrive and still be buffered. Beyond this, the peer is not merely
/// reordering — it is out of sync, or someone is spraying sequence numbers.
const MAX_REORDER: u32 = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RudpError {
    /// Payload exceeds [`MAX_PAYLOAD`].
    TooLarge { len: usize },
    /// The send window is full: the peer has stopped acknowledging.
    WindowFull,
    /// A message was retransmitted [`MAX_ATTEMPTS`] times without an ACK. The
    /// path is gone; the caller should tear the session down rather than keep
    /// pretending.
    PeerUnresponsive { seq: u32 },
}

impl std::fmt::Display for RudpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge { len } => {
                write!(f, "control message of {len} bytes exceeds the {MAX_PAYLOAD}-byte limit")
            }
            Self::WindowFull => write!(
                f,
                "{MAX_IN_FLIGHT} control messages are already awaiting acknowledgement"
            ),
            Self::PeerUnresponsive { seq } => write!(
                f,
                "control message {seq} went unacknowledged after {MAX_ATTEMPTS} attempts — \
                 the path to the peer is gone"
            ),
        }
    }
}

impl std::error::Error for RudpError {}

struct Pending {
    seq: u32,
    datagram: Vec<u8>,
    next_attempt: Instant,
    attempts: u32,
}

/// One end of a reliable control channel.
///
/// Drive it by feeding every inbound datagram to [`on_datagram`](Self::on_datagram)
/// and repeatedly draining [`poll_transmit`](Self::poll_transmit); send whatever
/// it returns. It never blocks and never sleeps — [`next_timeout`](Self::next_timeout)
/// says when it next wants to be polled.
pub struct ControlChannel {
    next_seq: u32,
    pending: Vec<Pending>,
    /// Datagrams ready to go out (new messages, retransmits, and ACKs).
    outbox: Vec<Vec<u8>>,
    /// Next sequence number to deliver to the application.
    next_expected: u32,
    /// Received out of order, held until the gap fills.
    reorder: BTreeMap<u32, Vec<u8>>,
    /// Highest sequence delivered, so a late duplicate is dropped rather than
    /// delivered twice.
    stats: ChannelStats,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ChannelStats {
    pub sent: u64,
    pub retransmitted: u64,
    pub delivered: u64,
    pub duplicates_dropped: u64,
    pub acks_sent: u64,
    pub malformed: u64,
}

impl Default for ControlChannel {
    fn default() -> Self {
        Self::new()
    }
}

impl ControlChannel {
    pub fn new() -> Self {
        Self {
            // Sequence 0 is never used, for the same reason `rtp.rs` starts
            // frame indices at 1: it makes "nothing yet" and "the first one"
            // distinguishable everywhere without a sentinel.
            next_seq: 1,
            pending: Vec::new(),
            outbox: Vec::new(),
            next_expected: 1,
            reorder: BTreeMap::new(),
            stats: ChannelStats::default(),
        }
    }

    pub fn stats(&self) -> ChannelStats {
        self.stats
    }

    /// Messages awaiting acknowledgement.
    pub fn in_flight(&self) -> usize {
        self.pending.len()
    }

    /// Queue a message for reliable delivery. It is transmitted on the next
    /// [`poll_transmit`](Self::poll_transmit) and retried until acknowledged.
    pub fn send(&mut self, payload: &[u8], now: Instant) -> Result<u32, RudpError> {
        if payload.len() > MAX_PAYLOAD {
            return Err(RudpError::TooLarge { len: payload.len() });
        }
        if self.pending.len() >= MAX_IN_FLIGHT {
            return Err(RudpError::WindowFull);
        }
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1).max(1);

        let mut datagram = Vec::with_capacity(HEADER_LEN + payload.len());
        datagram.push(ECHO_CONTROL);
        datagram.push(0); // flags, reserved
        datagram.extend_from_slice(&seq.to_be_bytes());
        datagram.extend_from_slice(payload);

        self.outbox.push(datagram.clone());
        self.stats.sent += 1;
        self.pending.push(Pending {
            seq,
            datagram,
            next_attempt: now + RETRY_INITIAL,
            attempts: 1,
        });
        Ok(seq)
    }

    /// Feed one inbound datagram. Returns any payloads that became deliverable,
    /// **in order**.
    ///
    /// Datagrams that are not ours are ignored rather than rejected: this
    /// channel shares a socket with media and STUN, and the caller is expected
    /// to route by [`crate::demux::classify`] — a stray one arriving here is a
    /// routing bug, not a protocol violation worth tearing a session down for.
    pub fn on_datagram(&mut self, datagram: &[u8], now: Instant) -> Vec<Vec<u8>> {
        let _ = now;
        match datagram.first() {
            Some(&ECHO_CONTROL_ACK) => {
                if datagram.len() < HEADER_LEN {
                    self.stats.malformed += 1;
                    return Vec::new();
                }
                let seq = u32::from_be_bytes([datagram[2], datagram[3], datagram[4], datagram[5]]);
                self.pending.retain(|p| p.seq != seq);
                Vec::new()
            }
            Some(&ECHO_CONTROL) => {
                if datagram.len() < HEADER_LEN {
                    self.stats.malformed += 1;
                    return Vec::new();
                }
                let seq = u32::from_be_bytes([datagram[2], datagram[3], datagram[4], datagram[5]]);

                // Acknowledge FIRST and unconditionally, including for a
                // duplicate: a duplicate normally means our previous ACK was
                // what got lost, and staying silent would leave the peer
                // retransmitting until it gave up on a message we already have.
                self.outbox.push(ack_datagram(seq));
                self.stats.acks_sent += 1;

                // Already delivered, or absurdly far ahead. `wrapping_sub`
                // keeps this correct across the sequence space wrapping, which
                // a long-lived session will eventually do.
                let ahead = seq.wrapping_sub(self.next_expected);
                if seq == 0 || ahead >= MAX_REORDER {
                    self.stats.duplicates_dropped += 1;
                    return Vec::new();
                }
                if self.reorder.contains_key(&seq) {
                    self.stats.duplicates_dropped += 1;
                    return Vec::new();
                }
                self.reorder.insert(seq, datagram[HEADER_LEN..].to_vec());

                // Drain whatever the arrival completed.
                let mut ready = Vec::new();
                while let Some(payload) = self.reorder.remove(&self.next_expected) {
                    self.next_expected = self.next_expected.wrapping_add(1).max(1);
                    self.stats.delivered += 1;
                    ready.push(payload);
                }
                ready
            }
            _ => {
                self.stats.malformed += 1;
                Vec::new()
            }
        }
    }

    /// Datagrams to put on the wire now: newly queued messages, ACKs, and any
    /// retransmission whose timer expired.
    ///
    /// Returns `Err` when a message has exhausted [`MAX_ATTEMPTS`] — at which
    /// point the path is gone and the caller should end the session rather than
    /// keep polling.
    pub fn poll_transmit(&mut self, now: Instant) -> Result<Vec<Vec<u8>>, RudpError> {
        let mut out = std::mem::take(&mut self.outbox);

        for p in self.pending.iter_mut() {
            if now < p.next_attempt {
                continue;
            }
            if p.attempts >= MAX_ATTEMPTS {
                return Err(RudpError::PeerUnresponsive { seq: p.seq });
            }
            // Exponential backoff: a path that just dropped a packet is often
            // about to drop the retry too, and hammering it makes that likelier
            // rather than less likely.
            let backoff = RETRY_INITIAL
                .saturating_mul(1 << p.attempts.min(4))
                .min(RETRY_MAX);
            p.attempts += 1;
            p.next_attempt = now + backoff;
            out.push(p.datagram.clone());
            self.stats.retransmitted += 1;
        }
        Ok(out)
    }

    /// When this channel next needs polling, or `None` if nothing is pending.
    /// Lets the caller sleep instead of spinning a timer.
    pub fn next_timeout(&self, now: Instant) -> Option<Duration> {
        self.pending
            .iter()
            .map(|p| p.next_attempt.saturating_duration_since(now))
            .min()
    }
}

// ── Byte-stream adapter ─────────────────────────────────────────────────────

/// Backlog of application chunks the driver may hold while the send window is
/// full, before declaring the peer hopeless.
///
/// Bounded because [`RudpStream`]'s writes never block: a peer that stops
/// acknowledging must cost a bounded amount of memory and then a clean error,
/// not an ever-growing queue. 64 chunks is ~76 KB — far more than a TLS
/// handshake plus every command a session will ever send, so reaching it means
/// the path is gone rather than merely slow.
const MAX_BACKLOG: usize = 64;

/// A reliable ordered **byte stream** over [`ControlChannel`].
///
/// This exists so that `rustls` can run over the punched UDP path. TLS needs a
/// reliable ordered stream and does not care what provides it, so layering it
/// here gives Echo the same mutual-TLS authentication the LAN control port
/// already uses — against the same pairing trust store, with the same
/// certificates — without inventing a single new cryptographic primitive.
///
/// That is the whole design argument. The alternative was a bespoke
/// challenge-response over raw datagrams, which means hand-rolling security
/// critical crypto for a command surface running as LocalSystem; or shipping a
/// key through the signaling relay, which would make the relay able to
/// impersonate either peer and defeat the point of peer-to-peer.
///
/// Writes are chunked to [`MAX_PAYLOAD`] and never block — the driver absorbs
/// them, subject to [`MAX_BACKLOG`]. Reads yield whatever the driver has
/// delivered, in order.
pub struct RudpStream {
    out_tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    in_rx: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    /// Partially consumed payload from a previous read.
    residue: Option<(Vec<u8>, usize)>,
}

impl RudpStream {
    /// Build a stream and the two channel ends [`drive`] needs.
    pub fn new() -> (Self, StreamPlumbing) {
        let (out_tx, out_rx) = tokio::sync::mpsc::unbounded_channel();
        let (in_tx, in_rx) = tokio::sync::mpsc::unbounded_channel();
        (
            Self { out_tx, in_rx, residue: None },
            StreamPlumbing { app_out: out_rx, app_in: in_tx },
        )
    }
}

/// The driver's half of a [`RudpStream`].
pub struct StreamPlumbing {
    app_out: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    app_in: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
}

impl tokio::io::AsyncRead for RudpStream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        use std::task::Poll;

        // Finish whatever a previous read left over before asking for more.
        if let Some((data, pos)) = self.residue.take() {
            let n = (data.len() - pos).min(buf.remaining());
            buf.put_slice(&data[pos..pos + n]);
            if pos + n < data.len() {
                self.residue = Some((data, pos + n));
            }
            return Poll::Ready(Ok(()));
        }

        match self.in_rx.poll_recv(cx) {
            Poll::Ready(Some(data)) => {
                let n = data.len().min(buf.remaining());
                buf.put_slice(&data[..n]);
                if n < data.len() {
                    self.residue = Some((data, n));
                }
                Poll::Ready(Ok(()))
            }
            // Driver gone: a clean EOF, which is what TLS expects when a peer
            // closes. Reporting an error here would turn every normal
            // disconnect into a logged failure.
            Poll::Ready(None) => Poll::Ready(Ok(())),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl tokio::io::AsyncWrite for RudpStream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        use std::task::Poll;
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        // One chunk per call: the caller loops, and capping here is what keeps
        // every reliable message inside one unfragmented datagram.
        let n = buf.len().min(MAX_PAYLOAD);
        match self.out_tx.send(buf[..n].to_vec()) {
            Ok(()) => Poll::Ready(Ok(n)),
            Err(_) => Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "the reliable-control driver has stopped",
            ))),
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        // Delivery is the driver's job and is guaranteed by retransmission, so
        // there is nothing here to force. Blocking until every byte were
        // acknowledged would stall TLS mid-handshake for a full round trip on
        // every flush.
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        self.in_rx.close();
        std::task::Poll::Ready(Ok(()))
    }
}

/// Run a [`ControlChannel`] against a real transport, feeding a [`RudpStream`].
///
/// `send` puts one datagram on the wire; `inbound` supplies datagrams already
/// demultiplexed as ours. Deliberately not given a socket: the host's channel
/// rides `rtp.rs`'s media socket, which it does not own, while the client's
/// rides its own. Same driver, two transports — the same reason
/// [`crate::punch`] takes a trait.
///
/// Returns when either side closes, or with an error when the peer stops
/// acknowledging.
pub async fn drive<F>(
    mut channel: ControlChannel,
    mut inbound: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    plumbing: StreamPlumbing,
    send: F,
) -> Result<ChannelStats, RudpError>
where
    F: Fn(&[u8]),
{
    let StreamPlumbing { mut app_out, app_in } = plumbing;
    let mut backlog: std::collections::VecDeque<Vec<u8>> = std::collections::VecDeque::new();
    let mut app_open = true;

    loop {
        let now = Instant::now();

        // Move as much backlog into the send window as it will take. The
        // window is what bounds outstanding retransmission work; the backlog
        // is what stops a full window from becoming lost data.
        while let Some(front) = backlog.front() {
            match channel.send(front, now) {
                Ok(_) => {
                    backlog.pop_front();
                }
                Err(RudpError::WindowFull) => break,
                Err(e) => return Err(e),
            }
        }

        for datagram in channel.poll_transmit(now)? {
            send(&datagram);
        }

        // Nothing left to do and nobody left to talk to.
        if !app_open && backlog.is_empty() && channel.in_flight() == 0 {
            return Ok(channel.stats());
        }

        let timeout = channel
            .next_timeout(now)
            .unwrap_or(Duration::from_millis(250))
            .max(Duration::from_millis(1));

        tokio::select! {
            datagram = inbound.recv() => {
                match datagram {
                    Some(d) => {
                        for payload in channel.on_datagram(&d, Instant::now()) {
                            // Receiver gone (the stream was dropped): stop
                            // rather than deliver into the void.
                            if app_in.send(payload).is_err() {
                                return Ok(channel.stats());
                            }
                        }
                    }
                    None => return Ok(channel.stats()), // transport closed
                }
            }
            chunk = app_out.recv(), if app_open => {
                match chunk {
                    Some(c) => {
                        if backlog.len() >= MAX_BACKLOG {
                            // The peer has not acknowledged anything for long
                            // enough to fill the backlog. Failing here is the
                            // honest answer: retransmission cannot help a path
                            // that is gone, and buffering further would only
                            // delay the same conclusion at greater cost.
                            return Err(RudpError::PeerUnresponsive { seq: 0 });
                        }
                        backlog.push_back(c);
                    }
                    None => app_open = false, // writer dropped
                }
            }
            _ = tokio::time::sleep(timeout) => {}
        }
    }
}

fn ack_datagram(seq: u32) -> Vec<u8> {
    let mut d = Vec::with_capacity(HEADER_LEN);
    d.push(ECHO_CONTROL_ACK);
    d.push(0);
    d.extend_from_slice(&seq.to_be_bytes());
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drives two endpoints against each other through a link that can be told
    /// to drop specific datagrams — the only way to test a retransmission layer
    /// honestly, since on a working link none of the interesting code runs.
    struct Link {
        a: ControlChannel,
        b: ControlChannel,
        now: Instant,
        /// Drop the Nth datagram in each direction (0-based), by call order.
        drop_a_to_b: Vec<usize>,
        drop_b_to_a: Vec<usize>,
        count_a_to_b: usize,
        count_b_to_a: usize,
    }

    impl Link {
        fn new() -> Self {
            Self {
                a: ControlChannel::new(),
                b: ControlChannel::new(),
                now: Instant::now(),
                drop_a_to_b: Vec::new(),
                drop_b_to_a: Vec::new(),
                count_a_to_b: 0,
                count_b_to_a: 0,
            }
        }

        /// One exchange round. Returns whatever B delivered, then whatever A
        /// delivered.
        fn pump(&mut self) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
            let mut to_b = Vec::new();
            for d in self.a.poll_transmit(self.now).expect("a transmits") {
                let n = self.count_a_to_b;
                self.count_a_to_b += 1;
                if !self.drop_a_to_b.contains(&n) {
                    to_b.push(d);
                }
            }
            let mut to_a = Vec::new();
            for d in self.b.poll_transmit(self.now).expect("b transmits") {
                let n = self.count_b_to_a;
                self.count_b_to_a += 1;
                if !self.drop_b_to_a.contains(&n) {
                    to_a.push(d);
                }
            }

            let mut b_got = Vec::new();
            for d in to_b {
                b_got.extend(self.b.on_datagram(&d, self.now));
            }
            let mut a_got = Vec::new();
            for d in to_a {
                a_got.extend(self.a.on_datagram(&d, self.now));
            }
            (b_got, a_got)
        }

        fn advance(&mut self, d: Duration) {
            self.now += d;
        }
    }

    #[test]
    fn a_message_crosses_a_clean_link_once() {
        let mut link = Link::new();
        link.a.send(b"start_session", link.now).unwrap();

        let (b_got, _) = link.pump();
        assert_eq!(b_got, vec![b"start_session".to_vec()]);

        // B's ACK reaches A, so nothing stays pending and nothing is resent.
        let (b_got, _) = link.pump();
        assert!(b_got.is_empty(), "no duplicate delivery");
        assert_eq!(link.a.in_flight(), 0, "the ACK cleared the send window");
        assert_eq!(link.a.stats().retransmitted, 0);
    }

    /// The reason this module exists: a dropped control message must still
    /// arrive, exactly once.
    #[test]
    fn a_dropped_message_is_retransmitted_and_delivered_exactly_once() {
        let mut link = Link::new();
        link.drop_a_to_b = vec![0]; // lose the first transmission outright
        link.a.send(b"stop_session", link.now).unwrap();

        let (b_got, _) = link.pump();
        assert!(b_got.is_empty(), "the message was dropped in flight");

        // Before the retry timer, nothing happens — a retransmit layer that
        // fires immediately is just a flood.
        let (b_got, _) = link.pump();
        assert!(b_got.is_empty());

        link.advance(RETRY_INITIAL + Duration::from_millis(10));
        let (b_got, _) = link.pump();
        assert_eq!(b_got, vec![b"stop_session".to_vec()], "the retry got through");
        assert_eq!(link.a.stats().retransmitted, 1);

        // And the recovered message is not delivered a second time.
        link.advance(Duration::from_secs(1));
        let (b_got, _) = link.pump();
        assert!(b_got.is_empty());
    }

    /// A lost ACK looks identical to a lost message from the sender's side, so
    /// it retransmits — and the receiver must answer the duplicate rather than
    /// stay silent, or the sender retries until it gives up on a message that
    /// was delivered the first time.
    #[test]
    fn a_lost_ack_causes_a_retransmit_that_is_answered_not_redelivered() {
        let mut link = Link::new();
        link.drop_b_to_a = vec![0]; // B's ACK never arrives
        link.a.send(b"hello", link.now).unwrap();

        let (b_got, _) = link.pump();
        assert_eq!(b_got.len(), 1, "B received it the first time");
        assert_eq!(link.a.in_flight(), 1, "…but A never learned that");

        link.advance(RETRY_INITIAL + Duration::from_millis(10));
        let (b_got, _) = link.pump();
        assert!(b_got.is_empty(), "the duplicate must NOT be delivered twice");
        assert_eq!(link.b.stats().duplicates_dropped, 1);

        // The second ACK gets through and settles it.
        let (_, _) = link.pump();
        assert_eq!(link.a.in_flight(), 0);
        assert_eq!(link.b.stats().delivered, 1, "delivered exactly once");
    }

    /// Ordering is the other half of the guarantee: a session teardown that
    /// arrives before the setup it follows would be applied to the wrong state.
    #[test]
    fn messages_are_delivered_in_order_even_when_they_arrive_out_of_order() {
        let mut a = ControlChannel::new();
        let mut b = ControlChannel::new();
        let now = Instant::now();

        a.send(b"first", now).unwrap();
        a.send(b"second", now).unwrap();
        a.send(b"third", now).unwrap();
        let mut datagrams = a.poll_transmit(now).unwrap();
        assert_eq!(datagrams.len(), 3);

        // Deliver third, first, second.
        datagrams.swap(0, 2);
        let mut delivered = Vec::new();
        for d in &datagrams {
            delivered.extend(b.on_datagram(d, now));
        }
        assert_eq!(
            delivered,
            vec![b"first".to_vec(), b"second".to_vec(), b"third".to_vec()],
            "the application must never see them out of order"
        );
    }

    /// A peer that has stopped answering must be reported, not retried
    /// forever: the session is over and the host needs to release the
    /// pipeline for the next client.
    #[test]
    fn an_unresponsive_peer_is_eventually_reported() {
        let mut ch = ControlChannel::new();
        let mut now = Instant::now();
        ch.send(b"anyone there?", now).unwrap();

        for _ in 0..MAX_ATTEMPTS {
            now += RETRY_MAX;
            if let Err(e) = ch.poll_transmit(now) {
                assert!(matches!(e, RudpError::PeerUnresponsive { seq: 1 }));
                return;
            }
        }
        panic!("an unanswered message must eventually be declared undeliverable");
    }

    /// Backpressure rather than unbounded buffering: a peer that stops
    /// acknowledging must not let the sender grow memory without limit.
    #[test]
    fn the_send_window_is_bounded() {
        let mut ch = ControlChannel::new();
        let now = Instant::now();
        for i in 0..MAX_IN_FLIGHT {
            ch.send(format!("msg {i}").as_bytes(), now).expect("within the window");
        }
        assert_eq!(ch.send(b"one too many", now), Err(RudpError::WindowFull));
    }

    #[test]
    fn oversized_messages_are_refused_rather_than_fragmented() {
        let mut ch = ControlChannel::new();
        let now = Instant::now();
        let big = vec![0u8; MAX_PAYLOAD + 1];
        assert_eq!(ch.send(&big, now), Err(RudpError::TooLarge { len: big.len() }));
        // The boundary itself is allowed, and stays inside a 1400-byte MTU.
        assert!(ch.send(&vec![0u8; MAX_PAYLOAD], now).is_ok());
        assert!(MAX_PAYLOAD + HEADER_LEN <= 1400);
    }

    /// This channel shares a socket with media, STUN, and internet noise.
    /// Nothing it is handed may panic it or corrupt its sequence state.
    #[test]
    fn junk_datagrams_are_counted_and_ignored() {
        let mut ch = ControlChannel::new();
        let now = Instant::now();
        for junk in [
            vec![],
            vec![ECHO_CONTROL],              // tag but no header
            vec![ECHO_CONTROL_ACK, 0, 0],    // truncated ACK
            vec![0x90, 1, 2, 3, 4, 5, 6],    // an RTP datagram misrouted here
            vec![0xFF; 40],
        ] {
            assert!(ch.on_datagram(&junk, now).is_empty());
        }
        assert!(ch.stats().malformed > 0);
        assert_eq!(ch.stats().delivered, 0);

        // …and a real message still works afterwards.
        let mut peer = ControlChannel::new();
        peer.send(b"still fine", now).unwrap();
        let datagrams = peer.poll_transmit(now).unwrap();
        let delivered: Vec<_> = datagrams.iter().flat_map(|d| ch.on_datagram(d, now)).collect();
        assert_eq!(delivered, vec![b"still fine".to_vec()]);
    }

    /// The point of the whole adapter: a real `rustls` mutual-TLS handshake,
    /// completed over this transport, across a link that drops packets.
    ///
    /// If this passes, Echo's WAN control channel has exactly the
    /// authentication the LAN port has — same certificates, same trust store,
    /// same library — with no bespoke crypto anywhere. It is also the only
    /// honest way to test the adapter: TLS is far less forgiving of a
    /// reordered, duplicated, or truncated stream than any assertion this file
    /// could write by hand.
    #[tokio::test]
    async fn mutual_tls_completes_over_a_lossy_rudp_link() {
        use crate::identity::{client_config_pinned, parse_fingerprint, server_config_require_client_cert, Identity};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let dir = std::env::temp_dir().join(format!("nova-rudp-tls-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let host = Identity::load_or_create(&dir, "tls-host", "nova").unwrap();
        let client = Identity::load_or_create(&dir, "tls-client", "echo").unwrap();
        let host_pin = parse_fingerprint(&host.fingerprint).unwrap();

        let (c2s_tx, c2s_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let (s2c_tx, s2c_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();

        let (client_stream, client_plumbing) = RudpStream::new();
        let (server_stream, server_plumbing) = RudpStream::new();

        // Drop every 7th datagram from the client and every 5th from the host.
        // Without retransmission a TLS handshake does not survive a single one.
        let client_drops = std::sync::atomic::AtomicUsize::new(0);
        let host_drops = std::sync::atomic::AtomicUsize::new(0);

        let client_driver = tokio::spawn(async move {
            drive(ControlChannel::new(), s2c_rx, client_plumbing, move |d| {
                let n = client_drops.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if n % 7 != 6 {
                    let _ = c2s_tx.send(d.to_vec());
                }
            })
            .await
        });
        let server_driver = tokio::spawn(async move {
            drive(ControlChannel::new(), c2s_rx, server_plumbing, move |d| {
                let n = host_drops.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if n % 5 != 4 {
                    let _ = s2c_tx.send(d.to_vec());
                }
            })
            .await
        });

        let acceptor = tokio_rustls::TlsAcceptor::from(std::sync::Arc::new(
            server_config_require_client_cert(&host).unwrap(),
        ));
        let connector = tokio_rustls::TlsConnector::from(std::sync::Arc::new(
            client_config_pinned(&client, host_pin).unwrap(),
        ));

        let server = tokio::spawn(async move {
            let mut tls = acceptor.accept(server_stream).await.expect("server handshake");
            // The peer certificate is what authorization is decided on — if it
            // did not survive the transport, nothing downstream can be trusted.
            let peer = {
                let (_, conn) = tls.get_ref();
                conn.peer_certificates()
                    .and_then(|c| c.first())
                    .map(|c| crate::identity::fingerprint(c.as_ref()))
            };
            let mut line = vec![0u8; 64];
            let n = tls.read(&mut line).await.expect("server read");
            tls.write_all(b"{\"ok\":true}\n").await.expect("server write");
            tls.flush().await.unwrap();
            (peer, String::from_utf8_lossy(&line[..n]).to_string())
        });

        let name = rustls_pki_types::ServerName::try_from("nova.host").unwrap();
        let mut tls = connector.connect(name, client_stream).await.expect("client handshake");
        tls.write_all(b"{\"command\":\"hello\"}\n").await.expect("client write");
        tls.flush().await.unwrap();
        let mut reply = vec![0u8; 64];
        let n = tls.read(&mut reply).await.expect("client read");

        let (peer_fp, request) = server.await.unwrap();
        assert_eq!(
            peer_fp.as_deref(),
            Some(client.fingerprint.as_str()),
            "the client's certificate must arrive intact — it IS the authorization"
        );
        assert_eq!(request, "{\"command\":\"hello\"}\n");
        assert_eq!(&reply[..n], b"{\"ok\":true}\n");

        drop(tls);
        let _ = tokio::time::timeout(Duration::from_secs(2), client_driver).await;
        let _ = tokio::time::timeout(Duration::from_secs(2), server_driver).await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A message larger than one datagram must still cross intact — the
    /// adapter chunks it, and TLS records routinely exceed [`MAX_PAYLOAD`].
    #[tokio::test]
    async fn the_stream_adapter_carries_more_than_one_datagram_worth() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (a2b_tx, a2b_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let (b2a_tx, b2a_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let (mut writer, w_plumbing) = RudpStream::new();
        let (mut reader, r_plumbing) = RudpStream::new();

        tokio::spawn(drive(ControlChannel::new(), b2a_rx, w_plumbing, move |d| {
            let _ = a2b_tx.send(d.to_vec());
        }));
        tokio::spawn(drive(ControlChannel::new(), a2b_rx, r_plumbing, move |d| {
            let _ = b2a_tx.send(d.to_vec());
        }));

        let payload: Vec<u8> = (0..(MAX_PAYLOAD * 3 + 17)).map(|i| i as u8).collect();
        let expected = payload.clone();
        tokio::spawn(async move {
            writer.write_all(&payload).await.unwrap();
            writer.flush().await.unwrap();
            // Hold the stream open; dropping it would close the driver before
            // the tail is acknowledged.
            tokio::time::sleep(Duration::from_secs(3)).await;
        });

        let mut got = vec![0u8; expected.len()];
        tokio::time::timeout(Duration::from_secs(5), reader.read_exact(&mut got))
            .await
            .expect("a multi-datagram write must arrive")
            .expect("read");
        assert_eq!(got, expected);
    }

    /// Both directions at once, over a link that loses a quarter of everything
    /// — the closest thing to a WAN this suite can build.
    #[test]
    fn a_lossy_bidirectional_exchange_still_converges() {
        let mut link = Link::new();
        link.drop_a_to_b = vec![0, 3, 6, 9];
        link.drop_b_to_a = vec![1, 4, 7];

        link.a.send(b"a1", link.now).unwrap();
        link.a.send(b"a2", link.now).unwrap();
        link.b.send(b"b1", link.now).unwrap();

        let mut from_a = Vec::new();
        let mut from_b = Vec::new();
        for _ in 0..40 {
            let (b_got, a_got) = link.pump();
            from_a.extend(b_got);
            from_b.extend(a_got);
            link.advance(RETRY_MAX);
        }

        assert_eq!(from_a, vec![b"a1".to_vec(), b"a2".to_vec()]);
        assert_eq!(from_b, vec![b"b1".to_vec()]);
        assert_eq!(link.a.in_flight(), 0, "everything was acknowledged");
        assert_eq!(link.b.in_flight(), 0);
    }
}

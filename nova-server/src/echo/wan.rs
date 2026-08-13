//! WAN candidate gathering and NAT keepalive — the host-bound half of Nova's
//! NAT traversal.
//!
//! The protocol itself (RFC 8489 binding codec, mapping classification) lives
//! in [`nova_core::stun`], because both peers speak it. What stays here is the
//! part that is inseparable from *being the host*: the gathering loop drives
//! `rtp.rs`'s media socket, and a NAT mapping belongs to a socket, so the
//! probe must egress from the one the stream will use. That coupling is why
//! this module cannot move into the shared crate — and why no off-the-shelf
//! ICE agent fits Nova, since they all want to own the transport.
//!
//! Responses come back on that same socket and are separated from client pings
//! by `rtp.rs`'s demux hook (see `nova_core::stun::is_stun_message` for why
//! that is unambiguous), which forwards them here.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use nova_core::stun::{
    parse_binding_response, MappingBehavior, StunError,
    RETRY_DELAYS,
};

// Re-exported so `rtp.rs`'s demux hook and this module's tests reach the shared
// codec through one path — there must never be two notions of "is this STUN".
pub use nova_core::stun::{
    build_binding_request, build_binding_response, is_stun_message, new_transaction_id,
    TransactionId, WanCandidate, DEFAULT_STUN_SERVERS,
};

// ── Gathering loop ──────────────────────────────────────────────────────────

/// How often a binding request goes out to hold the NAT pinhole open.
///
/// UDP mappings are commonly reclaimed after 30 s idle (some consumer routers
/// are harsher). 25 s keeps a comfortable margin under the common floor
/// without generating meaningful traffic: two 20-byte datagrams a minute.
///
/// This is *only* about keeping the mapping alive between sessions. While a
/// stream is running the media itself keeps the pinhole open far more
/// effectively than any keepalive could.
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(25);

/// How long to wait for the first candidate before giving up on a probe cycle.
const PROBE_TIMEOUT: Duration = Duration::from_secs(4);

/// Punch transport over Nova's media socket.
///
/// The host cannot use [`nova_core::punch::UdpPunchIo`]: `rtp.rs` owns the
/// socket, sends go out through [`crate::rtp::RtpSender::send_raw`], and
/// inbound STUN arrives on the channel the demux hook feeds. Implementing the
/// transport rather than reimplementing the loop is what keeps host and client
/// running byte-for-byte the same algorithm.
struct RtpPunchIo<'a> {
    rtp: &'a Arc<std::sync::Mutex<crate::rtp::RtpSender>>,
    inbox: &'a mut tokio::sync::mpsc::UnboundedReceiver<(Vec<u8>, SocketAddr)>,
}

impl nova_core::punch::PunchIo for RtpPunchIo<'_> {
    fn send_to(&self, data: &[u8], to: SocketAddr) {
        // Lock scope is the syscall only — the media path takes this same
        // mutex for every frame, and a blast round must never hold it across
        // an await.
        let tx = self.rtp.lock().unwrap_or_else(|e| e.into_inner());
        let _ = tx.send_raw(data, to);
    }

    async fn recv(&mut self, timeout: Duration) -> Option<(Vec<u8>, SocketAddr)> {
        tokio::time::timeout(timeout, self.inbox.recv()).await.ok().flatten()
    }
}

/// Controls for the gathering loop, held by the signaling client.
pub struct GatherHandle {
    /// Peer candidates to blast at, delivered from the signaling client when
    /// the relay hands over an offer. Routed through the gatherer rather than
    /// punched from the signaling task because the gatherer is the sole owner
    /// of the STUN inbox — two consumers of that channel would race for each
    /// other's responses.
    punch_tx: tokio::sync::mpsc::UnboundedSender<Vec<SocketAddr>>,
    /// The peer address a punch confirmed, if any.
    latched: Arc<std::sync::Mutex<Option<SocketAddr>>>,
    /// Poke to force an immediate probe — the signaling client fires this the
    /// moment a relay connection is established, so a freshly-connected host
    /// publishes a fresh address rather than a possibly-stale cached one.
    probe_now: Arc<tokio::sync::Notify>,
    /// Bumped whenever the mapped address CHANGES (initial discovery counts).
    /// The signaling client watches this and republishes only then — a
    /// keepalive that found the same address is not news, and pushing it every
    /// 25 s would be pure noise at the relay.
    changes: tokio::sync::watch::Receiver<u64>,
}

impl GatherHandle {
    /// Request an immediate probe.
    pub fn probe_now(&self) {
        self.probe_now.notify_one();
    }

    /// A clone of the change-generation watcher.
    pub fn changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.changes.clone()
    }

    /// Start blasting at a peer's candidates.
    ///
    /// Called when the relay delivers an offer. Passive answering alone is not
    /// enough against a restrictive NAT: if only one side ever *initiates*,
    /// the other side's NAT may still have no outbound mapping toward us when
    /// our packets arrive, and drops them. Both sides must initiate.
    pub fn punch_toward(&self, candidates: Vec<SocketAddr>) {
        // Deduplicate here rather than relying on `punch_io`'s own guard, so
        // the log line reports what is actually blasted. A peer behind an
        // endpoint-independent NAT offers the same address once per STUN
        // server it asked, and "blasting at 2 candidates" when both are the
        // same address is the kind of log that costs an hour during a field
        // test.
        let mut unique: Vec<SocketAddr> = Vec::with_capacity(candidates.len());
        for c in candidates {
            if !unique.contains(&c) {
                unique.push(c);
            }
        }
        if unique.is_empty() {
            return;
        }
        let _ = self.punch_tx.send(unique);
    }

    /// The peer address a punch confirmed, if one has.
    pub fn latched_peer(&self) -> Option<SocketAddr> {
        *self.latched.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Shared handle to the latched peer, for the session manager.
    ///
    /// Handing over the cell rather than a copy is deliberate: a punch can
    /// complete (or be redone after a NAT rebind) at any moment, and a session
    /// started later must see the current path, not whatever was latched when
    /// the manager happened to be constructed.
    pub fn latched_handle(&self) -> Arc<std::sync::Mutex<Option<SocketAddr>>> {
        self.latched.clone()
    }
}

/// Start the STUN gathering + NAT-keepalive loop on Nova's **media socket**.
///
/// Wiring, and why each piece is where it is:
///
/// - Requests egress via [`RtpSender::send_raw`], so the mapping discovered is
///   the one the media stream will actually use.
/// - Responses arrive back on that same socket and are separated from client
///   pings by `rtp.rs`'s demux hook, which forwards them here. Note that the
///   drain runs on `media_supervisor`'s 500 ms `learn_ticker`, which ticks
///   whether or not a client or Worker exists — so the keepalive keeps working
///   while Nova sits idle, which is precisely when the pinhole would otherwise
///   lapse.
/// - The caller decides whether this runs at all. It is started only when a
///   signaling relay is configured: with no relay there is no WAN path, and a
///   LAN-only install must not emit traffic to third-party STUN servers every
///   25 seconds.
/// `servers` are hostnames (`host:port`), resolved lazily and re-resolved
/// after a failure so a DNS change or a dead server cannot wedge the loop.
pub fn spawn_gatherer(
    rtp_sender: Arc<std::sync::Mutex<crate::rtp::RtpSender>>,
    candidates: Arc<std::sync::Mutex<Vec<WanCandidate>>>,
    servers: Vec<String>,
) -> GatherHandle {
    let (inbox_tx, mut inbox) = tokio::sync::mpsc::unbounded_channel();
    rtp_sender
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .set_stun_inbox(inbox_tx);

    let probe_now = Arc::new(tokio::sync::Notify::new());
    let (change_tx, change_rx) = tokio::sync::watch::channel(0u64);
    let (punch_tx, mut punch_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<SocketAddr>>();
    let latched: Arc<std::sync::Mutex<Option<SocketAddr>>> = Arc::new(std::sync::Mutex::new(None));
    let handle = GatherHandle {
        probe_now: probe_now.clone(),
        changes: change_rx,
        punch_tx,
        latched: latched.clone(),
    };

    tokio::spawn(async move {
        let names = servers;
        let mut servers: Vec<SocketAddr> = Vec::new();
        let mut ticker = tokio::time::interval(KEEPALIVE_INTERVAL);
        // A missed tick (host suspended, or a probe that overran) must not
        // produce a burst of catch-up probes — the same frame-deadline lesson
        // the capture loop already learned.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last_mapped: Option<SocketAddr> = None;
        let mut generation = 0u64;
        // Classification is worth one extra probe when the address is new, and
        // nothing at all on the steady-state keepalives.
        let mut classify_next = true;

        loop {
            tokio::select! {
                _ = ticker.tick() => {}
                _ = probe_now.notified() => {}
                // A peer punching toward us. Answering is a cooperative
                // obligation, not an optimisation: our reply is what completes
                // THEIR side of the simultaneous open, and it also opens our
                // own NAT toward them. Ignoring these would leave the host
                // reachable in principle and unreachable in practice.
                Some((payload, from)) = inbox.recv() => {
                    if let Some(txid) = nova_core::punch::binding_request_txid(&payload) {
                        let response = nova_core::stun::build_binding_response(&txid, from);
                        let sent = {
                            let tx = rtp_sender.lock().unwrap_or_else(|e| e.into_inner());
                            tx.send_raw(&response, from).is_ok()
                        };
                        println!("🥊 Punch probe from {from} — {}",
                            if sent { "answered" } else { "reply failed" });
                    }
                    continue; // not a keepalive tick; don't start a probe cycle
                }
                // An offer arrived: blast at the peer. This is the host-side
                // half of the simultaneous open — answering probes is not
                // enough on its own, because a restrictive peer NAT drops our
                // packets until IT has sent outbound to us, and it only does
                // that once its own blast is running against a host that is
                // also blasting back.
                Some(peers) = punch_rx.recv() => {
                    println!("🥊 Offer received — blasting at {} candidate(s): {}",
                        peers.len(),
                        peers.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(", "));
                    let cfg = nova_core::punch::PunchConfig::default();
                    let mut io = RtpPunchIo { rtp: &rtp_sender, inbox: &mut inbox };
                    match nova_core::punch::punch_io(&mut io, &peers, cfg).await {
                        Ok(result) => {
                            println!("✅ Punch succeeded: path open to {} after {} round(s) ({:?})",
                                result.peer, result.rounds, result.proof);
                            *latched.lock().unwrap_or_else(|e| e.into_inner()) = Some(result.peer);
                            // Latched as the confirmed ECHO peer, deliberately
                            // NOT installed as RtpSender's media target: that
                            // target belongs to whatever session is live, and
                            // overwriting it here would redirect an in-flight
                            // Moonlight stream to the Echo peer mid-frame.
                            // Handing media over is the Echo session's job,
                            // once one exists.
                        }
                        Err(e) => println!("❌ Punch failed: {e} — the peer may be behind an \
                            endpoint-dependent NAT, which needs a relay"),
                    }
                    continue;
                }
            }

            if servers.is_empty() {
                servers = resolve_servers(&names).await;
                if servers.is_empty() {
                    println!("🌐 STUN: no server resolved — retrying at the next keepalive");
                    continue;
                }
            }

            // Steady state is ONE request to the primary server: enough to
            // hold the mapping open and to notice a change.
            let primary = servers[0];
            let mapped = match probe(&rtp_sender, &mut inbox, primary).await {
                Ok(addr) => addr,
                Err(e) => {
                    println!("🌐 STUN probe via {primary} failed: {e}");
                    // Rotate: a dead primary must not wedge the keepalive
                    // forever, and re-resolution picks up DNS changes.
                    servers.rotate_left(1);
                    if servers.len() < 2 {
                        servers.clear();
                    }
                    continue;
                }
            };

            if last_mapped == Some(mapped) {
                continue; // pinhole refreshed, nothing to publish
            }

            // New or changed address: this is the only path that touches the
            // published candidate list or wakes the signaling client.
            let mut gathered = vec![WanCandidate { mapped, via: primary }];
            if classify_next && servers.len() > 1 {
                match probe(&rtp_sender, &mut inbox, servers[1]).await {
                    Ok(second) => {
                        let behavior = if second == mapped {
                            MappingBehavior::EndpointIndependent
                        } else {
                            MappingBehavior::EndpointDependent
                        };
                        if behavior == MappingBehavior::EndpointDependent {
                            println!(
                                "🌐 NAT is endpoint-dependent (symmetric): {mapped} via {primary} \
                                 but {second} via {} — direct peer-to-peer will NOT work here; \
                                 these sessions need a relay",
                                servers[1]
                            );
                            gathered.push(WanCandidate { mapped: second, via: servers[1] });
                        } else {
                            println!("🌐 NAT mapping is endpoint-independent — hole punching viable");
                        }
                    }
                    Err(e) => println!("🌐 STUN classification probe failed: {e}"),
                }
                classify_next = false;
            }

            match last_mapped {
                None => println!("🌐 WAN address discovered: {mapped} (via {primary})"),
                Some(prev) => {
                    println!("🌐 WAN address changed: {prev} → {mapped} — republishing");
                    // A new public address invalidates the classification too.
                    classify_next = true;
                }
            }
            last_mapped = Some(mapped);
            *candidates.lock().unwrap_or_else(|e| e.into_inner()) = gathered;
            generation += 1;
            // Send failure just means the signaling client is gone; the
            // keepalive must carry on regardless, since the pinhole still
            // matters for a relay that may reconnect.
            let _ = change_tx.send(generation);
        }
    });

    handle
}

/// Resolve STUN server hostnames to v4 addresses.
async fn resolve_servers(names: &[String]) -> Vec<SocketAddr> {
    let mut out = Vec::new();
    for name in names {
        match tokio::net::lookup_host(name.as_str()).await {
            // v4 only: the mapping being kept alive belongs to Nova's media
            // socket, which is bound to 0.0.0.0.
            Ok(addrs) => out.extend(addrs.filter(SocketAddr::is_ipv4).take(1)),
            Err(e) => println!("🌐 STUN: could not resolve {name}: {e}"),
        }
    }
    out
}

/// One binding transaction over the media socket.
///
/// Sends through `RtpSender::send_raw` and reads replies from the demux inbox
/// rather than from a socket directly — the media socket is owned by
/// `rtp.rs`, and this module never takes it over (see the module docs).
async fn probe(
    rtp_sender: &Arc<std::sync::Mutex<crate::rtp::RtpSender>>,
    inbox: &mut tokio::sync::mpsc::UnboundedReceiver<(Vec<u8>, SocketAddr)>,
    server: SocketAddr,
) -> Result<SocketAddr, StunError> {
    // Discard anything left from an earlier transaction so a stale reply can't
    // be mistaken for this one's (the txid check would catch it, but draining
    // keeps the channel from growing across a long outage).
    while inbox.try_recv().is_ok() {}

    let txid = new_transaction_id();
    let request = build_binding_request(&txid);

    for delay in RETRY_DELAYS {
        {
            // Lock scope kept to the syscall: the media path takes this same
            // mutex for every frame.
            let tx = rtp_sender.lock().unwrap_or_else(|e| e.into_inner());
            tx.send_raw(&request, server)
                .map_err(|e| StunError::Io(e.to_string()))?;
        }

        let deadline = tokio::time::Instant::now() + delay.min(PROBE_TIMEOUT);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break; // retransmit
            }
            match tokio::time::timeout(remaining, inbox.recv()).await {
                Ok(Some((payload, from))) => {
                    if from != server {
                        // Another peer's STUN. If it is a punch probe, answer
                        // it here too — a peer must not go unanswered merely
                        // because it arrived while we happened to be mid-probe.
                        if let Some(txid) = nova_core::punch::binding_request_txid(&payload) {
                            let response = nova_core::stun::build_binding_response(&txid, from);
                            let tx = rtp_sender.lock().unwrap_or_else(|e| e.into_inner());
                            let _ = tx.send_raw(&response, from);
                        }
                        continue;
                    }
                    match parse_binding_response(&payload, &txid) {
                        Ok(mapped) => return Ok(mapped),
                        Err(StunError::TransactionMismatch) => continue,
                        Err(e) => return Err(e),
                    }
                }
                // The demux hook's sender was dropped — RtpSender is gone.
                Ok(None) => return Err(StunError::Io("STUN inbox closed".to_string())),
                Err(_) => break, // this attempt's window elapsed
            }
        }
    }
    Err(StunError::Timeout)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cadence spec, end to end through the real media socket: probes go
    /// out via `RtpSender::send_raw`, come back through `rtp.rs`'s demux hook,
    /// and — the part that matters — the relay is only woken when the mapped
    /// address actually CHANGES. A keepalive that rediscovers the same address
    /// must publish nothing.
    #[tokio::test]
    async fn gathering_publishes_on_change_and_stays_silent_on_keepalives() {
        use std::sync::atomic::{AtomicU16, Ordering};

        // A STUN server whose reported port we can move at will, standing in
        // for a NAT rebind.
        let server = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server_addr = server.local_addr().unwrap();
        let reported_port = Arc::new(AtomicU16::new(40000));
        tokio::spawn({
            let reported_port = reported_port.clone();
            async move {
                let mut buf = [0u8; 1500];
                loop {
                    let Ok((n, from)) = server.recv_from(&mut buf).await else { return };
                    if !is_stun_message(&buf[..n]) {
                        continue;
                    }
                    let txid: TransactionId = buf[8..20].try_into().unwrap();
                    let port = reported_port.load(Ordering::Relaxed);
                    let reported = SocketAddr::from(([203, 0, 113, 5], port));
                    let _ = server
                        .send_to(&build_binding_response(&txid, reported), from)
                        .await;
                }
            }
        });

        let rtp = Arc::new(std::sync::Mutex::new(
            crate::rtp::RtpSender::new(0).expect("bind media socket"),
        ));
        let candidates = Arc::new(std::sync::Mutex::new(Vec::new()));
        let gather = spawn_gatherer(
            rtp.clone(),
            candidates.clone(),
            vec![server_addr.to_string()],
        );
        let mut changes = gather.changes();

        // The demux hook lives in `try_learn_target`, which in production is
        // driven by media_supervisor's 500 ms ticker. Stand that in here.
        let pump = tokio::spawn({
            let rtp = rtp.clone();
            async move {
                loop {
                    rtp.lock().unwrap().try_learn_target();
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        });

        // 1. Initial discovery must publish.
        gather.probe_now();
        tokio::time::timeout(Duration::from_secs(5), changes.changed())
            .await
            .expect("initial discovery must publish within 5s")
            .expect("watch alive");
        assert_eq!(
            candidates.lock().unwrap()[0].mapped,
            "203.0.113.5:40000".parse::<SocketAddr>().unwrap()
        );

        // 2. A keepalive that finds the SAME address must publish nothing.
        gather.probe_now();
        gather.probe_now();
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(
            !changes.has_changed().unwrap(),
            "an unchanged address must not wake the relay"
        );

        // 3. A NAT rebind must publish again.
        reported_port.store(40001, Ordering::Relaxed);
        gather.probe_now();
        tokio::time::timeout(Duration::from_secs(5), changes.changed())
            .await
            .expect("a changed address must publish within 5s")
            .expect("watch alive");
        assert_eq!(
            candidates.lock().unwrap()[0].mapped,
            "203.0.113.5:40001".parse::<SocketAddr>().unwrap()
        );

        pump.abort();
    }

    /// The keepalive must stay comfortably under the common 30 s NAT idle
    /// timeout — this constant is the whole reason the loop exists.
    #[test]
    fn keepalive_stays_under_the_common_nat_timeout() {
        assert!(
            KEEPALIVE_INTERVAL < Duration::from_secs(30),
            "UDP mappings are commonly reclaimed at 30 s idle"
        );
        assert!(
            KEEPALIVE_INTERVAL >= Duration::from_secs(15),
            "probing far more often than needed is just traffic"
        );
    }
}

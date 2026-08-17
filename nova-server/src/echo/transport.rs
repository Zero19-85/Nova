//! Echo's WAN control channel: mutual TLS over the punched UDP path.
//!
//! ## Why this exists at all
//!
//! Echo's control channel used to be a TCP listener on port 48011. That works
//! on a LAN and cannot work across the internet without port forwarding, which
//! is exactly the configuration burden the whole WAN effort exists to remove:
//! the hole punch opens a **UDP** path, and nothing about it helps a TCP
//! connection.
//!
//! The obvious fix — accept commands as plain UDP datagrams from the latched
//! peer — is the one that must not be taken. Port 48011's real protection was
//! never TCP; it was mutual TLS against the pairing trust store, refusing a
//! peer before a single command byte was read. A raw UDP command port would
//! replace that with a source address, which is spoofable, on a socket that is
//! now internet-reachable by design, guarding commands that reconfigure
//! displays and retarget the media stream on a LocalSystem service.
//!
//! So the transport is layered instead of replaced:
//!
//! ```text
//!   NDJSON commands      ← identical to the LAN port, same dispatcher
//!   rustls mutual TLS    ← identical trust store, identical certificates
//!   RudpStream           ← reliable ordered byte stream (nova_core::rudp)
//!   demux tag 0xE1/0xE2  ← shares the media socket (nova_core::demux)
//!   punched UDP path
//! ```
//!
//! Every security property of the LAN port survives, with no new cryptography
//! written anywhere: the alternative designs were a bespoke challenge-response
//! (hand-rolled crypto in front of a SYSTEM command surface) or shipping a key
//! through the signaling relay (which would let the relay impersonate either
//! peer, defeating peer-to-peer).
//!
//! ## Why it rides the media socket
//!
//! A NAT mapping belongs to a socket. Control has to leave from the same socket
//! the punch opened, or it needs its own mapping to punch, keep alive, and lose
//! independently. That socket belongs to `rtp.rs`, so this module never owns
//! it: datagrams leave through `RtpSender::send_raw` and arrive through the
//! demux inbox `rtp.rs` feeds.
//!
//! ## One peer at a time
//!
//! Nova serves one session, so this accepts one tunnel. While a tunnel is live,
//! control datagrams from other addresses are ignored — which also bounds what
//! an unauthenticated peer can make this process do: at most one TLS handshake
//! at a time, and none at all while a real client is connected.
//!
//! **The slot must therefore be reclaimable.** Nothing beneath TLS times out, so
//! a peer that simply stops talking leaves `serve_connection` blocked on a read
//! that never completes and the slot held until Nova restarts — observed live
//! (2026-08-15) as one successful tunnel followed by five successful punches
//! that the host silently ignored, with only `tls handshake eof` on the client.
//! [`TUNNEL_IDLE_TIMEOUT`] bounds it when nobody is waiting, and
//! [`CONTENDED_IDLE_TIMEOUT`] bounds it much sooner when somebody is — patience
//! is only worth having while it costs nobody anything.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nova_core::rudp::{drive, ControlChannel, RudpStream};

use crate::echo::rpc::{self, EchoIdentity};
use crate::echo::session::SessionManager;

/// How long a peer has to complete the TLS handshake before its tunnel is torn
/// down. Generous next to a LAN handshake because this one crosses the
/// internet and retransmits, but bounded so a half-open tunnel cannot hold the
/// single connection slot indefinitely.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);

/// How long a tunnel may go without a single datagram before it is treated as
/// dead and its slot reclaimed.
///
/// This exists because a client that simply stops talking used to hold the one
/// tunnel slot **forever**: nothing below the TLS layer times out, so
/// `serve_connection` waits on a read that will never complete, `busy` stays
/// set, and every later punch is silently ignored. A client vanishing without a
/// close_notify is the normal case, not the exceptional one — Android kills
/// backgrounded apps, and a refused client goes quiet by design.
///
/// A granted session pings every 500 ms (`echo-client/src/receiver.rs`), so this
/// is 60× the live cadence and cannot reap a working tunnel.
const TUNNEL_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// How long an incumbent tunnel may be silent before a peer that is *actively
/// asking* for the slot takes it.
///
/// [`TUNNEL_IDLE_TIMEOUT`] is the right patience when nobody is waiting: it must
/// clear well past any plausible hiccup, because reaping a live tunnel drops a
/// working stream. But when another peer is knocking, that patience is paid by a
/// user standing in front of the machine wondering why the app will not connect.
///
/// Live 2026-08-17: a client closed and reopened, arrived on a new source port,
/// and was refused for the remainder of the 30 s while the host held a tunnel
/// its owner had already abandoned. The operator's workaround — close the app
/// and try again — could not work, because closing it is what started the wait.
///
/// Safe at five seconds because the measurement underneath it is now accurate: a
/// granted session pings every 500 ms and those pings are recorded against the
/// pinned peer specifically (see `RtpSender::last_rx_pinned`), so a live session
/// reads as ~0.5 s idle no matter who else is sending to the port. Before that
/// fix a contender's own packets erased the incumbent's liveness, and this
/// shortcut would have handed away healthy tunnels.
const CONTENDED_IDLE_TIMEOUT: Duration = Duration::from_secs(5);

/// How often the accept loop checks the live tunnel for idleness. Only needs to
/// be fine enough that a reclaimed slot is available before a user retries.
const IDLE_SWEEP: Duration = Duration::from_secs(5);

/// How often the input arrival rate is reported while input is flowing.
///
/// This exists to answer one question that nothing else can: input travels
/// client → socket → Master → pipe → Worker → `SendInput`, and a pointer that
/// lags behind the hand looks identical from the outside no matter which of
/// those segments is queueing. The **arrival rate measured here** splits that
/// chain in half. If it matches the client's send cadence, everything upstream
/// is healthy and the delay is downstream; if it is a fraction of it, the
/// client or the path is the bottleneck and the host is blameless.
///
/// Silent when no input is arriving, so an idle session logs nothing.
const INPUT_REPORT_INTERVAL: Duration = Duration::from_secs(1);

/// The tunnel currently being served.
struct ActiveTunnel {
    peer: SocketAddr,
    sink: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    /// Last **control** datagram seen from `peer`. Dropping [`ActiveTunnel`]
    /// closes `sink`, which ends the driver, which unblocks the TLS read — so
    /// eviction here cascades all the way to `serve_tunnel` returning and
    /// clearing `busy`.
    last_seen: Instant,
}

impl ActiveTunnel {
    /// How long this tunnel has been silent, counting **all** traffic from the
    /// peer, not just control datagrams.
    ///
    /// A granted Echo session keeps its NAT mapping alive with a raw `b"PING"`
    /// every 500 ms on the media socket. That is not control traffic and never
    /// reaches this module, so judging by `last_seen` alone declares a perfectly
    /// healthy stream dead — which is exactly what happened live on 2026-08-15:
    /// a working session went black at the 30 s mark. `rtp.rs` sees every
    /// datagram on the port and is the only place that knows the peer is alive.
    fn idle_for(&self, rtp: &Mutex<crate::rtp::RtpSender>) -> Duration {
        let control_idle = self.last_seen.elapsed();
        let media_idle = rtp
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .idle_since(self.peer);
        // Whichever channel spoke most recently is the true measure of life.
        match media_idle {
            Some(m) => control_idle.min(m),
            None => control_idle,
        }
    }
}

/// Run the WAN control endpoint.
///
/// `inbound` receives every control datagram `rtp.rs` demultiplexes off the
/// media socket; `rtp_sender` is how replies leave.
pub fn spawn(
    mut inbound: tokio::sync::mpsc::UnboundedReceiver<(Vec<u8>, SocketAddr)>,
    rtp_sender: Arc<Mutex<crate::rtp::RtpSender>>,
    handler: Arc<rpc::Handler>,
    sessions: Arc<SessionManager>,
) {
    tokio::spawn(async move {
        // True while a tunnel is being served. Not a mutex: the accept loop is
        // the only writer, and the served task clears it on exit.
        let busy = Arc::new(AtomicBool::new(false));
        // The tunnel currently being served.
        let mut active: Option<ActiveTunnel> = None;
        let mut sweep = tokio::time::interval(IDLE_SWEEP);
        // Rate-limit the "slot taken" notice: a rejected client retransmits.
        let mut last_busy_notice: Option<Instant> = None;
        // Same, for input that could not be injected.
        let mut last_input_notice: Option<Instant> = None;
        // Input arrival accounting — see INPUT_REPORT_INTERVAL.
        let mut input_window = Instant::now();
        let (mut datagrams, mut applied, mut duplicates, mut refused) = (0u32, 0u32, 0u32, 0u32);

        println!("🛡️  Echo WAN control ready — mutual TLS over the punched path");

        loop {
            let (datagram, from) = tokio::select! {
                received = inbound.recv() => match received {
                    Some(v) => v,
                    None => break, // demux gone; the process is shutting down
                },
                _ = sweep.tick() => {
                    // Reap a tunnel whose peer stopped talking. Without this the
                    // slot is held until Nova restarts.
                    if let Some(t) = &active {
                        if t.idle_for(&rtp_sender) > TUNNEL_IDLE_TIMEOUT {
                            println!(
                                "⌛ Echo WAN: tunnel from {} silent for {}s — reclaiming the slot",
                                t.peer,
                                t.idle_for(&rtp_sender).as_secs()
                            );
                            active = None;
                        }
                    }

                    // Reaping the tunnel reclaims a SLOT. It says nothing to the
                    // session, which until this existed was the whole problem:
                    // a phone that lost signal left the host encoding and
                    // transmitting to an address nobody was listening at, with
                    // no path to ever stop — `stop` needs a client well enough
                    // to ask, and `force_end` needs an operator at the tray.
                    //
                    // The two steps are read in two separate lock acquisitions
                    // rather than one nested pair. Every other path in this
                    // system takes the session lock and then the RTP lock
                    // (`start` → `plane.begin`, `force_end` → `plane.end`);
                    // holding the session lock while asking RTP a question would
                    // be the sole inversion, and the sole opportunity for a
                    // deadlock.
                    let media_idle = sessions
                        .live_peer()
                        .and_then(|peer| rtp_sender.lock().unwrap_or_else(|e| e.into_inner()).idle_since(peer));
                    sessions.sweep(media_idle, TUNNEL_IDLE_TIMEOUT);
                    continue;
                }
            };

            // Input is split off before anything else looks at this datagram.
            //
            // It shares the inbox with control but has nothing to do with the
            // tunnel: it carries its own authentication, so it must not open
            // one, must not be able to evict one, and must not count toward the
            // busy slot. Nor does it refresh `last_seen` — the idle sweep is
            // asking whether the *tunnel* is alive, and a peer that only ever
            // sent input would otherwise hold the slot forever.
            if nova_core::demux::classify(&datagram) == nova_core::demux::Class::EchoInput {
                datagrams += 1;
                // Timed across the whole open-and-inject, because the point of
                // measuring here is to catch this step being expensive enough
                // to back the queue up — an assumption nobody has tested.
                let began = Instant::now();
                match sessions.inject_sealed_input(&datagram) {
                    Ok(count) => {
                        applied += count as u32;
                        // Every datagram repeats REDUNDANCY packets, so a
                        // healthy lossless path shows duplicates ≈ 3× datagrams.
                        // Far fewer means datagrams are being lost, which is the
                        // signature of a path problem rather than a queue.
                        duplicates += (4u32.saturating_sub(count as u32)).min(3);
                    }
                    Err(why) => {
                        refused += 1;
                        // Rate-limited: a spray of forged datagrams must not
                        // become a log-flooding amplifier, and a real one is a
                        // steady condition rather than an event.
                        if last_input_notice.map_or(true, |t: Instant| {
                            t.elapsed() > Duration::from_secs(10)
                        }) {
                            println!("🚫 Echo input from {from} not injected: {why}");
                            last_input_notice = Some(Instant::now());
                        }
                    }
                }
                let cost = began.elapsed();

                if input_window.elapsed() >= INPUT_REPORT_INTERVAL {
                    println!(
                        "⌨️  Echo input/s: {datagrams} datagrams, {applied} applied, \
                         {duplicates} repeats, {refused} refused, last inject {}µs",
                        cost.as_micros()
                    );
                    input_window = Instant::now();
                    datagrams = 0;
                    applied = 0;
                    duplicates = 0;
                    refused = 0;
                }
                continue;
            }

            // Route to the live tunnel.
            if let Some(t) = &mut active {
                if t.peer == from {
                    t.last_seen = Instant::now();
                    if t.sink.send(datagram).is_err() {
                        active = None; // driver finished
                    }
                    continue;
                }
                if busy.load(Ordering::Relaxed) {
                    // Someone is asking for a slot the incumbent may no longer
                    // be using. Contention is not itself proof the incumbent is
                    // dead — but it IS the moment to stop being patient about
                    // finding out, so the incumbent gets a much shorter leash
                    // than the unattended timeout.
                    let idle = t.idle_for(&rtp_sender);
                    if idle > CONTENDED_IDLE_TIMEOUT {
                        println!(
                            "🔀 Echo WAN: {from} wants a tunnel and {} has been silent {}s — \
                             handing the slot over",
                            t.peer,
                            idle.as_secs()
                        );
                        // Falls through to the shared `active = None` below,
                        // which drops the record. That closes the sink, ends the
                        // driver, unblocks the served task's TLS read and lets it
                        // return and clear `busy` — asynchronously, so THIS
                        // datagram still falls out at the `busy` check below. The
                        // newcomer gets in on a retransmit its RUDP layer is
                        // already sending. The win is the wait dropping from 30 s
                        // to 5, not the elimination of a round trip.
                    } else {
                        // A live incumbent. Silence here is what made the
                        // wedged-slot bug invisible: five successful punches
                        // produced no host-side line at all, while the client
                        // saw only `tls handshake eof`.
                        if last_busy_notice.map_or(true, |t| t.elapsed() > Duration::from_secs(10)) {
                            println!(
                                "🚧 Echo WAN: {from} wants a tunnel but {} holds the slot \
                                 (idle {}s, handover at {}s)",
                                t.peer,
                                idle.as_secs(),
                                CONTENDED_IDLE_TIMEOUT.as_secs()
                            );
                            last_busy_notice = Some(Instant::now());
                        }
                        continue;
                    }
                }
                // Either the slot was never really busy, or the handover above
                // released it. Drop the stale record and open a fresh tunnel.
                active = None;
            }
            if busy.load(Ordering::Relaxed) {
                continue;
            }

            // A new peer's first control datagram opens a tunnel. Note what is
            // NOT trusted here: nothing about `from` authorizes anything. It
            // selects which address the TLS handshake is attempted with, and
            // the handshake decides everything that matters.
            let (dg_tx, dg_rx) = tokio::sync::mpsc::unbounded_channel();
            let _ = dg_tx.send(datagram);
            active = Some(ActiveTunnel { peer: from, sink: dg_tx, last_seen: Instant::now() });
            busy.store(true, Ordering::Relaxed);

            tokio::spawn(serve_tunnel(
                from,
                dg_rx,
                rtp_sender.clone(),
                handler.clone(),
                sessions.clone(),
                busy.clone(),
            ));
        }
    });
}

async fn serve_tunnel(
    peer: SocketAddr,
    inbound: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    rtp_sender: Arc<Mutex<crate::rtp::RtpSender>>,
    handler: Arc<rpc::Handler>,
    sessions: Arc<SessionManager>,
    busy: Arc<AtomicBool>,
) {
    println!("🔗 Echo WAN: control tunnel opening from {peer}");
    let (stream, plumbing) = RudpStream::new();

    // The driver owns the reliable layer and moves bytes through the media
    // socket. `send_raw` takes the RtpSender lock for the duration of one
    // syscall only — the same discipline the punch loop uses, because the
    // media path takes that lock for every frame.
    let driver = tokio::spawn(drive(ControlChannel::new(), inbound, plumbing, move |d| {
        let tx = rtp_sender.lock().unwrap_or_else(|e| e.into_inner());
        let _ = tx.send_raw(d, peer);
    }));

    let served = async {
        let Some(tls_config) = crate::pairing::client_auth_tls_config() else {
            println!("⚠️  Echo WAN: no TLS identity published yet — refusing {peer}");
            return;
        };
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls_config));

        let tls = match tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await {
            Ok(Ok(t)) => t,
            Ok(Err(e)) => {
                println!("⚠️  Echo WAN: TLS handshake with {peer} failed: {e}");
                return;
            }
            Err(_) => {
                println!("⚠️  Echo WAN: TLS handshake with {peer} timed out");
                return;
            }
        };

        // Authorization against the LIVE pairing trust store — the identical
        // check the LAN port makes, via the identical function. A device
        // revoked in the tray is refused here on its next connection too.
        let peer_der = {
            let (_, conn) = tls.get_ref();
            conn.peer_certificates()
                .and_then(|c| c.first())
                .map(|c| c.as_ref().to_vec())
        };
        let identity = match rpc::authorize(peer_der.as_deref(), crate::pairing::trusted_device_name) {
            Ok(id) => id,
            Err(why) => {
                println!("⛔ Echo WAN: {peer} denied — {why}");
                return;
            }
        };
        println!("🔓 Echo WAN: {peer} authenticated as \"{}\"", identity.device_name);

        match rpc::serve_connection(tls, handler, &identity).await {
            Ok(()) => println!("🔌 Echo WAN: {peer} disconnected"),
            Err(e) => println!("🔌 Echo WAN: {peer} closed: {e}"),
        }

        // The control channel dying means the client is GONE — not that it is
        // finished. Those are different, and the difference is the whole of the
        // detach feature: a client that deliberately stops sends `stop_session`
        // over this tunnel first (a full teardown, unchanged), while one that
        // simply vanishes gets its display held for the grace period instead.
        //
        // This used to call `stop`, i.e. a full teardown, which rearranged the
        // operator's monitors the instant a phone lost signal. It must not do so
        // again: the sweep detaches at the idle timeout and then drops the
        // tunnel, which lands HERE microseconds later — a teardown here would
        // silently un-do every detach the moment it happened.
        release_session_of(&sessions, &identity, "control tunnel closed");
    };

    served.await;
    driver.abort();
    busy.store(false, Ordering::Relaxed);
}

/// Detach the Echo session held by `identity`, if it holds a live one.
fn release_session_of(sessions: &SessionManager, identity: &EchoIdentity, why: &str) {
    // Owner-checked inside, which is exactly right here: if some other device
    // holds the session, this disconnect must not touch it. A false return —
    // no session, someone else's, or one the sweep already detached — is not
    // worth a line: a control tunnel closing with nothing live behind it is the
    // ordinary end of a client that already said goodbye.
    if sessions.detach_on_disconnect(identity, why) {
        println!("⏸️  Echo: session detached — {why}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::echo::rpc::{DisplayOrchestrator, DisplayRequest, DisplaySeat};
    use crate::echo::session::{MediaPlane, StreamParams};
    use crate::rtsp::ClientInfo;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    struct OneVirtualSeat;
    impl DisplayOrchestrator for OneVirtualSeat {
        fn seats(&self) -> Vec<DisplaySeat> {
            vec![DisplaySeat {
                id: "primary".into(),
                label: "Virtual display".into(),
                width: 1920,
                height: 1080,
                refresh_hz: 60,
                is_primary: true,
                reconfigurable: true,
                virtual_display: true,
                hdr_active: false,
                hdr_capable: true,
            }]
        }
        fn apply(&self, _: &DisplayRequest) -> Result<(), String> {
            Ok(())
        }
    }

    struct RecordingPlane;
    impl MediaPlane for RecordingPlane {
        fn begin(
            &self,
            _peer: SocketAddr,
            _params: &StreamParams,
            _device_name: &str,
            _rikey: [u8; 16],
            _rikeyid: u32,
        ) -> Result<(), crate::echo::session::HandoffError> {
            Ok(())
        }
        fn end(&self, _mode: crate::echo::session::EndMode) {}
        fn request_idr(&self) {}
        fn inject_input(&self, _packet: Vec<u8>) {}
    }

    /// The whole batch, end to end: NDJSON commands crossing a **lossy**
    /// reliable tunnel, hitting the same handler the LAN port uses, with the
    /// anti-hijack gate deciding over that tunnel exactly as it does locally.
    ///
    /// TLS is proven separately over this same transport in
    /// `nova_core::rudp`'s `mutual_tls_completes_over_a_lossy_rudp_link`; this
    /// covers the layer above it, so between them the full stack is exercised
    /// without either test needing to stand up both halves at once.
    #[tokio::test]
    async fn the_command_surface_answers_over_a_lossy_tunnel_and_the_gate_still_holds() {
        use nova_core::rudp::{drive, ControlChannel as RudpChannel, RudpStream};

        let client_info = Arc::new(Mutex::new(None::<ClientInfo>));
        let sessions = Arc::new(SessionManager::new(
            Arc::new(RecordingPlane),
            client_info.clone(),
            Arc::new(Mutex::new(Some("203.0.113.7:47998".parse().unwrap()))),
            // This test is about the tunnel, not the budget or the reaper.
            crate::echo::session::SessionPolicy {
                audio_reserve_kbps: 0,
                detach_grace: Duration::from_secs(600),
            },
        ));
        let handler = rpc::build_handler(
            Arc::new(OneVirtualSeat),
            client_info.clone(),
            Some(sessions.clone()),
        );

        let (host_stream, host_plumbing) = RudpStream::new();
        let (peer_stream, peer_plumbing) = RudpStream::new();
        let (h2p_tx, h2p_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let (p2h_tx, p2h_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();

        // Drop every 4th datagram each way. Without retransmission a command
        // would simply vanish and this test would hang.
        let hd = std::sync::atomic::AtomicUsize::new(0);
        let pd = std::sync::atomic::AtomicUsize::new(0);
        tokio::spawn(drive(RudpChannel::new(), p2h_rx, host_plumbing, move |d| {
            if hd.fetch_add(1, Ordering::Relaxed) % 4 != 3 {
                let _ = h2p_tx.send(d.to_vec());
            }
        }));
        tokio::spawn(drive(RudpChannel::new(), h2p_rx, peer_plumbing, move |d| {
            if pd.fetch_add(1, Ordering::Relaxed) % 4 != 3 {
                let _ = p2h_tx.send(d.to_vec());
            }
        }));

        let identity = EchoIdentity {
            fingerprint: "a".repeat(64),
            device_name: "Xbox".into(),
        };
        tokio::spawn(async move {
            let _ = rpc::serve_connection(host_stream, handler, &identity).await;
        });

        let (read, mut write) = tokio::io::split(peer_stream);
        let mut lines = BufReader::new(read).lines();
        let call = |cmd: &str| -> Vec<u8> { format!("{cmd}\n").into_bytes() };

        // 1. hello — the surface answers at all, over loss.
        write.write_all(&call(r#"{"id":1,"command":"hello"}"#)).await.unwrap();
        write.flush().await.unwrap();
        let reply = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
            .await
            .expect("a command must survive a lossy tunnel")
            .unwrap()
            .expect("reply");
        let v: serde_json::Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["result"]["device_name"], "Xbox");
        assert_eq!(v["result"]["capabilities"]["sessions"], true);

        // 2. The anti-hijack gate, decided over the tunnel: a live Moonlight
        //    client must block an Echo handoff exactly as it does on the LAN.
        *client_info.lock().unwrap() = Some(ClientInfo {
            streaming_active: true,
            ..Default::default()
        });
        write.write_all(&call(r#"{"id":2,"command":"start_session"}"#)).await.unwrap();
        write.flush().await.unwrap();
        let reply = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
            .await
            .expect("reply arrives")
            .unwrap()
            .expect("reply");
        let v: serde_json::Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"]["code"], "moonlight_active");
        assert!(!sessions.echo_holds_media(), "nothing may have been retargeted");

        // 3. With that client gone, the same request is granted and carries
        //    usable key material.
        client_info.lock().unwrap().as_mut().unwrap().streaming_active = false;
        write.write_all(&call(r#"{"id":3,"command":"start_session","res":"1080p"}"#)).await.unwrap();
        write.flush().await.unwrap();
        let reply = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
            .await
            .expect("reply arrives")
            .unwrap()
            .expect("reply");
        let v: serde_json::Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(v["ok"], true, "expected a grant, got {v}");
        assert_eq!(v["result"]["peer"], "203.0.113.7:47998");
        let key = v["result"]["media_key"].as_str().expect("media key");
        assert!(
            nova_core::media_crypto::SessionKeys::from_hex(key).is_ok(),
            "the grant must carry usable key material"
        );
        assert!(sessions.echo_holds_media());
    }

    /// A LocalSystem command port must not be reachable from the internet now
    /// that WAN clients have their own authenticated path.
    #[test]
    fn the_lan_port_admits_only_private_addresses() {
        for lan in ["127.0.0.1:48011", "192.168.1.50:48011", "10.0.0.4:48011", "172.16.3.9:48011"] {
            assert!(rpc::is_lan_peer_for_test(&lan.parse().unwrap()), "{lan} is LAN");
        }
        for wan in ["8.8.8.8:48011", "73.213.125.252:48011", "172.32.0.1:48011"] {
            assert!(!rpc::is_lan_peer_for_test(&wan.parse().unwrap()), "{wan} is not LAN");
        }
    }

    /// The handshake timeout must be long enough for a retransmitting
    /// transport to complete a TLS handshake across the internet, and short
    /// enough that a stalled peer cannot hold the single tunnel slot for long.
    #[test]
    fn the_handshake_timeout_suits_a_retransmitting_wan_transport() {
        assert!(
            HANDSHAKE_TIMEOUT >= Duration::from_secs(10),
            "a TLS handshake over a lossy WAN path needs room for retransmission"
        );
        assert!(
            HANDSHAKE_TIMEOUT <= Duration::from_secs(30),
            "a stalled peer must not hold the tunnel slot indefinitely"
        );
    }
}

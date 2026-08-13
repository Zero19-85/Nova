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

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nova_core::rudp::{drive, ControlChannel, RudpStream};

use crate::echo::rpc::{self, EchoIdentity};
use crate::echo::session::SessionManager;

/// How long a peer has to complete the TLS handshake before its tunnel is torn
/// down. Generous next to a LAN handshake because this one crosses the
/// internet and retransmits, but bounded so a half-open tunnel cannot hold the
/// single connection slot indefinitely.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);

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
        // Datagram sink for the tunnel currently being served.
        let mut active: Option<(SocketAddr, tokio::sync::mpsc::UnboundedSender<Vec<u8>>)> = None;

        println!("🛡️  Echo WAN control ready — mutual TLS over the punched path");

        while let Some((datagram, from)) = inbound.recv().await {
            // Route to the live tunnel.
            if let Some((peer, sink)) = &active {
                if *peer == from {
                    if sink.send(datagram).is_err() {
                        active = None; // driver finished
                    }
                    continue;
                }
                if busy.load(Ordering::Relaxed) {
                    continue; // another peer, while one is served — ignore
                }
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
            active = Some((from, dg_tx));
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

        // The control channel dying IS the session ending. Without this, a
        // client that loses its connection mid-stream leaves the pipeline
        // marked as held — blocking the next client, including a Moonlight
        // one, until Nova restarts. A WAN client vanishing is the expected
        // case, not the exceptional one.
        release_session_of(&sessions, &identity, "control tunnel closed");
    };

    served.await;
    driver.abort();
    busy.store(false, Ordering::Relaxed);
}

/// End the Echo session held by `identity`, if it holds one.
fn release_session_of(sessions: &SessionManager, identity: &EchoIdentity, why: &str) {
    // `stop` is owner-checked, which is exactly right here: if some other
    // device holds the session, this disconnect must not end it.
    if sessions.stop(identity).is_ok() {
        println!("🛑 Echo: session released — {why}");
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
        fn end(&self) {}
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
        let mut call = |cmd: &str| -> Vec<u8> { format!("{cmd}\n").into_bytes() };

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

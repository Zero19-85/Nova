//! Signaling client — how Nova and an Echo client find each other across the
//! internet before either can send a packet to the other.
//!
//! ## Why a relay exists at all
//!
//! Hole punching requires both peers to learn each other's server-reflexive
//! addresses ([`super::wan`]) and then start sending simultaneously. That is a
//! chicken-and-egg problem: neither can tell the other anything until a path
//! already exists. A signaling relay breaks it — a small always-reachable
//! service both sides connect *outbound* to, which forwards candidates. It
//! carries no media, only a handful of small messages at session setup.
//!
//! ## Why long-poll and not WebSocket
//!
//! A WebSocket would cost eight additional crates (`tokio-tungstenite`,
//! `tungstenite`, `sha1`, `data-encoding`, `chacha20`, …). HTTPS long-poll
//! costs **zero**: `hyper` and `tokio-rustls` are already in the tree for the
//! pairing server. What WebSocket buys — server push and low per-message
//! framing overhead on a long-lived stream — is not what signaling needs:
//! traffic is a few messages when a session starts, and server push is exactly
//! what a held-open request provides. If signaling ever becomes chatty, this
//! is the decision to revisit.
//!
//! ## Authentication: Nova's pairing identity, in the client role
//!
//! Nova presents `pairing.rs`'s `SERVER_IDENTITY` — the same certificate it
//! serves to Moonlight on 47984 and to Echo on 48011 — as its **client**
//! certificate to the relay. The relay therefore enrols hosts by fingerprint
//! exactly as Nova enrols clients, and a host's identity is the same value
//! everywhere it appears.
//!
//! The relay's own certificate is **pinned by SHA-256**, not validated against
//! a public CA set. This is deliberate and is the stronger choice: it costs no
//! dependency (`webpki-roots` would be one more), and it means trusting one
//! key we operate rather than every CA on the internet for a service that
//! brokers connections into people's machines. The trade is that rotating the
//! relay's certificate requires shipping a new pin — acceptable for
//! infrastructure we control, and a rotation we would plan anyway.
//!
//! ## Envelope
//!
//! The same newline-delimited JSON shape as [`super::rpc`], viewed from the
//! other side: Nova writes `{"id":…,"command":…,…}` and reads
//! `{"id":…,"ok":…,"result"|"error":…}`. One codec, one mental model. The
//! types are mirrored rather than shared because each side only ever needs one
//! direction (rpc deserializes requests and serializes responses; this does
//! the reverse), and mirroring keeps both sets of `serde` attributes honest
//! about the direction they actually travel.
//!
//! ## Status
//!
//! The transport, authentication, envelope, and reconnect behaviour are
//! complete and tested end-to-end against a real TLS relay in this module's
//! tests. What is deliberately *not* here is the hole-punching itself: this
//! client announces Nova's presence and long-polls for a peer, and the
//! candidate exchange it carries becomes meaningful once the punch loop
//! exists. Until a relay URL is configured the whole module is inert.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::header::{CONNECTION, CONTENT_TYPE, HOST};
use hyper::{Method, Request as HttpRequest};
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use tokio_rustls::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use tokio_rustls::rustls::{self, ClientConfig, DigitallySignedStruct, SignatureScheme};
use tokio_rustls::TlsConnector;

use crate::config::SignalingConfig;

use super::wan::WanCandidate;

/// Protocol version Nova announces to the relay. Distinct from
/// [`super::rpc::PROTOCOL_VERSION`]: the two surfaces version independently.
pub const SIGNALING_PROTOCOL_VERSION: u32 = 1;

/// Longest response body accepted from the relay. Signaling messages are a few
/// hundred bytes; anything approaching this is a relay malfunctioning or
/// hostile, and this runs inside the LocalSystem service.
const MAX_RESPONSE_BYTES: usize = 256 * 1024;

/// Reconnect backoff bounds. A relay outage must degrade to a slow retry, not
/// a hot loop against a service that is already struggling.
const BACKOFF_BASE: Duration = Duration::from_secs(1);
const BACKOFF_CAP: Duration = Duration::from_secs(60);

/// Ceiling on how long a single long-poll may be held open. Middleboxes and
/// load balancers commonly cut idle HTTP requests at 60 s, so a poll that
/// waits longer than this fails in a way that looks like an error rather than
/// a timeout.
const MAX_POLL_SECS: u32 = 55;

// ── Envelope ────────────────────────────────────────────────────────────────

/// Nova → relay. Mirrors `rpc::RpcRequest` in the opposite direction.
#[derive(Debug, Clone, Serialize)]
struct SignalRequest {
    id: u64,
    command: &'static str,
    #[serde(flatten)]
    params: Map<String, Value>,
}

/// Relay → Nova. Mirrors `rpc::RpcResponse`.
#[derive(Debug, Clone, Deserialize)]
struct SignalResponse {
    #[serde(default)]
    id: Option<u64>,
    ok: bool,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<SignalError>,
}

#[derive(Debug, Clone, Deserialize)]
struct SignalError {
    #[serde(default)]
    code: String,
    #[serde(default)]
    message: String,
}

impl std::fmt::Display for SignalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

fn encode_request(req: &SignalRequest) -> Result<Vec<u8>, String> {
    let mut body = serde_json::to_vec(req).map_err(|e| format!("encode {}: {e}", req.command))?;
    body.push(b'\n'); // newline-delimited, same as echo::rpc
    Ok(body)
}

/// Parse one NDJSON response line. Tolerates (and ignores) trailing lines so a
/// relay that batches events into several lines cannot desync the client.
fn decode_response(body: &[u8]) -> Result<SignalResponse, String> {
    let text = std::str::from_utf8(body).map_err(|e| format!("response not UTF-8: {e}"))?;
    let line = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .ok_or_else(|| "empty response body".to_string())?;
    serde_json::from_str(line).map_err(|e| format!("malformed response: {e}"))
}

// ── Certificate pinning ─────────────────────────────────────────────────────

/// Accepts exactly one server certificate: the one whose DER hashes to `pin`.
///
/// Chain building, name validation, and expiry are all irrelevant under a pin
/// — the certificate is not being *trusted*, it is being *recognised*. The
/// signature checks below are still delegated to rustls's real verifiers,
/// because those prove the peer holds the matching private key, which is the
/// property that actually matters.
struct PinnedRelayCert {
    pin: [u8; 32],
    algs: rustls::crypto::WebPkiSupportedAlgorithms,
}

impl std::fmt::Debug for PinnedRelayCert {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PinnedRelayCert({}…)", hex::encode(&self.pin[..8]))
    }
}

impl ServerCertVerifier for PinnedRelayCert {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let got: [u8; 32] = Sha256::digest(end_entity.as_ref()).into();
        // Constant-time compare: a pin check that leaks its progress through
        // timing is a pin check an attacker can walk byte by byte.
        let equal = got
            .iter()
            .zip(self.pin.iter())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0;
        if equal {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(format!(
                "relay certificate {}… does not match the configured pin {}…",
                hex::encode(&got[..8]),
                hex::encode(&self.pin[..8])
            )))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algs)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

/// Build the mutual-TLS client config: Nova's pairing identity as the client
/// certificate, the relay recognised by pin.
///
/// Takes the identity explicitly rather than reaching into `pairing.rs` so the
/// whole stack is testable against a throwaway identity — see this module's
/// tests.
pub fn build_client_tls(
    cert_der: &[u8],
    key_der: &[u8],
    relay_pin: [u8; 32],
) -> Result<ClientConfig, String> {
    let verifier = PinnedRelayCert {
        pin: relay_pin,
        algs: rustls::crypto::aws_lc_rs::default_provider().signature_verification_algorithms,
    };
    let cert = CertificateDer::from(cert_der.to_vec());
    let key = PrivateKeyDer::Pkcs8(key_der.to_vec().into());
    ClientConfig::builder_with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
        .dangerous() // "dangerous" = custom verifier; the pin IS the verification
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_client_auth_cert(vec![cert], key)
        .map_err(|e| format!("client TLS config: {e}"))
}

/// Decode a 64-character hex SHA-256 pin.
pub fn parse_pin(hex_pin: &str) -> Result<[u8; 32], String> {
    let bytes = hex::decode(hex_pin.trim())
        .map_err(|e| format!("relay_cert_sha256 is not valid hex: {e}"))?;
    bytes
        .try_into()
        .map_err(|_| "relay_cert_sha256 must be a 32-byte (64 hex character) SHA-256".to_string())
}

// ── Connection target ───────────────────────────────────────────────────────

/// Where and how to reach the relay, resolved once from configuration so a
/// malformed URL fails loudly at startup instead of on every retry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayTarget {
    pub host: String,
    pub port: u16,
    /// Request path, always beginning with `/`.
    pub path: String,
}

impl RelayTarget {
    pub fn parse(url: &str) -> Result<Self, String> {
        let parsed = url::Url::parse(url.trim()).map_err(|e| format!("invalid relay url: {e}"))?;
        if parsed.scheme() != "https" {
            // The client certificate IS Nova's identity; sending it over
            // anything but TLS would hand that identity to the network.
            return Err(format!(
                "relay url must be https (got {}) — signaling carries Nova's identity certificate",
                parsed.scheme()
            ));
        }
        let host = parsed
            .host_str()
            .ok_or_else(|| "relay url has no host".to_string())?
            .to_string();
        let port = parsed.port().unwrap_or(443);
        let path = match parsed.path() {
            "" => "/".to_string(),
            p => p.to_string(),
        };
        Ok(RelayTarget { host, port, path })
    }

    fn authority(&self) -> String {
        if self.port == 443 {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

/// Reconnect delay for attempt `n` (0-based): 1 s doubling to a 60 s ceiling.
///
/// Deterministic and separately testable; the caller adds jitter so a fleet of
/// hosts recovering from a relay outage does not stampede in lockstep.
pub fn backoff_delay(attempt: u32) -> Duration {
    let secs = BACKOFF_BASE
        .as_secs()
        .saturating_mul(1u64.checked_shl(attempt.min(16)).unwrap_or(u64::MAX));
    Duration::from_secs(secs.min(BACKOFF_CAP.as_secs()))
}

// ── Client ──────────────────────────────────────────────────────────────────

/// A live connection to the relay: one TLS session carrying sequential
/// NDJSON request/response pairs.
struct RelayConnection {
    sender: hyper::client::conn::http1::SendRequest<Full<Bytes>>,
    target: RelayTarget,
    next_id: u64,
}

impl RelayConnection {
    async fn connect(target: &RelayTarget, tls: Arc<ClientConfig>) -> Result<Self, String> {
        let stream = tokio::net::TcpStream::connect((target.host.as_str(), target.port))
            .await
            .map_err(|e| format!("connect {}:{}: {e}", target.host, target.port))?;
        let _ = stream.set_nodelay(true);

        let server_name = ServerName::try_from(target.host.clone())
            .map_err(|e| format!("invalid server name {}: {e}", target.host))?;
        let tls_stream = TlsConnector::from(tls)
            .connect(server_name, stream)
            .await
            .map_err(|e| format!("relay TLS handshake: {e}"))?;

        let (sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tls_stream))
            .await
            .map_err(|e| format!("http handshake: {e}"))?;
        // The connection future drives the socket; it completes when the
        // connection closes. Nothing else can make progress without it.
        tokio::spawn(async move {
            if let Err(e) = conn.await {
                println!("📡 Signaling: relay connection ended ({e})");
            }
        });

        Ok(RelayConnection { sender, target: target.clone(), next_id: 1 })
    }

    /// One request/response round trip.
    async fn call(
        &mut self,
        command: &'static str,
        params: Map<String, Value>,
    ) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        let body = encode_request(&SignalRequest { id, command, params })?;

        let req = HttpRequest::builder()
            .method(Method::POST)
            .uri(self.target.path.clone())
            .header(HOST, self.target.authority())
            .header(CONTENT_TYPE, "application/x-ndjson")
            // Explicit keep-alive: announce and every subsequent poll reuse one
            // TLS session, so a poll cycle costs no handshake.
            .header(CONNECTION, "keep-alive")
            .body(Full::new(Bytes::from(body)))
            .map_err(|e| format!("build request: {e}"))?;

        let res = self
            .sender
            .send_request(req)
            .await
            .map_err(|e| format!("{command} request failed: {e}"))?;
        let status = res.status();
        let collected = res
            .into_body()
            .collect()
            .await
            .map_err(|e| format!("{command} response body: {e}"))?
            .to_bytes();
        if collected.len() > MAX_RESPONSE_BYTES {
            return Err(format!(
                "{command} response of {} bytes exceeds the {MAX_RESPONSE_BYTES}-byte limit",
                collected.len()
            ));
        }
        if !status.is_success() {
            return Err(format!("{command} rejected by relay: HTTP {status}"));
        }

        let decoded = decode_response(&collected)?;
        if decoded.id.is_some() && decoded.id != Some(id) {
            // Sequential HTTP means responses cannot legitimately arrive out
            // of order; a mismatch means the relay is confused about which
            // request it is answering, and acting on it would be worse than
            // reconnecting.
            return Err(format!(
                "{command} response id {:?} does not match request id {id}",
                decoded.id
            ));
        }
        if !decoded.ok {
            return Err(match decoded.error {
                Some(e) => format!("{command}: {e}"),
                None => format!("{command}: relay reported failure without a reason"),
            });
        }
        Ok(decoded.result.unwrap_or(Value::Null))
    }
}

/// Everything the signaling loop needs to identify this host.
#[derive(Clone)]
pub struct HostIdentity {
    /// Nova's certificate fingerprint — the same value the relay and every
    /// paired client know it by.
    pub fingerprint: String,
    pub cert_der: Vec<u8>,
    pub key_der: Vec<u8>,
}

/// How long to wait at startup for `pairing.rs` to publish Nova's identity —
/// same race, same reasoning as `echo::rpc`'s wait: the pairing server is
/// spawned moments earlier, but on a fresh install it must generate a
/// certificate first.
const IDENTITY_WAIT: Duration = Duration::from_secs(60);

/// How often to re-announce presence on a healthy connection.
///
/// Independent of the poll cycle on purpose: polling proves the connection is
/// alive, but says nothing to a directory that has already dropped us. Well
/// under any sensible relay TTL so a single missed round is harmless.
const REANNOUNCE_INTERVAL: Duration = Duration::from_secs(60);

/// Resolve Nova's pairing identity, waiting for the pairing server to publish
/// it.
async fn await_identity() -> Option<HostIdentity> {
    let deadline = tokio::time::Instant::now() + IDENTITY_WAIT;
    loop {
        if let Some((cert_der, key_der)) = crate::pairing::server_identity() {
            let fingerprint = crate::pairing::fingerprint_of_cert(&cert_der);
            return Some(HostIdentity { fingerprint, cert_der, key_der });
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Start the signaling client. Returns immediately; does nothing at all when
/// no relay is configured.
///
/// `candidates` is the shared cache the WAN layer fills with server-reflexive
/// addresses; whatever is in it at announce time is what the relay publishes.
pub fn spawn(
    cfg: &SignalingConfig,
    candidates: Arc<Mutex<Vec<WanCandidate>>>,
    gather: super::wan::GatherHandle,
) {
    if cfg.url.trim().is_empty() {
        println!("📡 Signaling: no relay configured — Echo WAN connections are disabled (LAN only)");
        return;
    }
    let target = match RelayTarget::parse(&cfg.url) {
        Ok(t) => t,
        Err(e) => {
            println!("❌ Signaling: {e}");
            return;
        }
    };
    let pin = match parse_pin(&cfg.relay_cert_sha256) {
        Ok(p) => p,
        Err(e) => {
            println!("❌ Signaling: {e} — refusing to connect without a relay certificate pin");
            return;
        }
    };
    let poll_secs = cfg.poll_timeout_secs.clamp(5, MAX_POLL_SECS);

    println!(
        "📡 Signaling: relaying through https://{} (pin {}…, poll {poll_secs}s)",
        target.authority(),
        &cfg.relay_cert_sha256[..16.min(cfg.relay_cert_sha256.len())]
    );

    tokio::spawn(async move {
        let Some(identity) = await_identity().await else {
            println!(
                "❌ Signaling: no TLS identity published after {IDENTITY_WAIT:?} — \
                 not connecting (is the pairing server running in this process?)"
            );
            return;
        };
        let tls = match build_client_tls(&identity.cert_der, &identity.key_der, pin) {
            Ok(c) => Arc::new(c),
            Err(e) => {
                println!("❌ Signaling: {e}");
                return;
            }
        };
        println!(
            "📡 Signaling: presenting Nova's pairing identity {}… to the relay",
            &identity.fingerprint[..16.min(identity.fingerprint.len())]
        );

        let mut attempt = 0u32;
        loop {
            match run_session(&target, tls.clone(), &identity, &candidates, poll_secs, &gather).await {
                Ok(()) => {
                    // Clean end (relay closed the connection): reconnect
                    // promptly, this is normal long-poll churn.
                    attempt = 0;
                }
                Err(e) => {
                    println!("📡 Signaling: {e}");
                    attempt = attempt.saturating_add(1);
                }
            }
            let base = backoff_delay(attempt);
            // Jitter up to 25%: a relay coming back after an outage must not
            // be hit by every host at the same instant.
            let jitter = Duration::from_millis(
                (rand::random::<u64>() % (base.as_millis() as u64 / 4 + 1)).min(15_000),
            );
            tokio::time::sleep(base + jitter).await;
        }
    });
}

/// One connection's lifetime: announce, then long-poll until something breaks
/// or Nova's public address changes.
async fn run_session(
    target: &RelayTarget,
    tls: Arc<ClientConfig>,
    identity: &HostIdentity,
    candidates: &Arc<Mutex<Vec<WanCandidate>>>,
    poll_secs: u32,
    gather: &super::wan::GatherHandle,
) -> Result<(), String> {
    let mut conn = RelayConnection::connect(target, tls).await?;

    // Probe the moment the relay connection is up, so what we announce is a
    // freshly-confirmed mapping rather than one cached from before whatever
    // outage caused this reconnect. `changes` is marked seen FIRST so the
    // resulting discovery registers as a change and republishes below.
    let mut changes = gather.changes();
    changes.mark_unchanged();
    gather.probe_now();

    let announced = announce(&mut conn, identity, candidates).await?;
    println!("📡 Signaling: announced to relay as {announced}");
    let mut last_announce = tokio::time::Instant::now();

    loop {
        // A change that landed while we were not polling can be published on
        // this connection directly — no request is in flight, so the
        // sequential HTTP/1.1 connection is free.
        if changes.has_changed().unwrap_or(false) {
            changes.mark_unchanged();
            update(&mut conn, identity, candidates).await?;
        }

        // Periodic re-announce. A relay expires hosts it has not heard from,
        // and polling alone does not prove presence to a directory that has
        // already forgotten us — an evicted host would poll contentedly
        // forever while being invisible to every lookup (confirmed live
        // 2026-08-13: a tethered client got `host_offline` while this Worker's
        // own log showed a healthy signaling session). Re-announcing on a
        // timer means presence is re-established without depending on the
        // relay noticing, or on this connection ever cycling.
        if last_announce.elapsed() >= REANNOUNCE_INTERVAL {
            announce(&mut conn, identity, candidates).await?;
            last_announce = tokio::time::Instant::now();
        }

        tokio::select! {
            polled = poll(&mut conn, identity, poll_secs) => {
                match polled {
                    Ok(events) => {
                        for event in events {
                            handle_event(&event, gather);
                        }
                    }
                    // The relay is telling us it has no record of this host.
                    // Re-announce on the SAME connection rather than tearing
                    // the session down: the connection is healthy, only the
                    // directory entry is missing.
                    Err(e) if e.contains("not_announced") => {
                        println!("📡 Signaling: relay has no record of us — re-announcing");
                        let announced = announce(&mut conn, identity, candidates).await?;
                        last_announce = tokio::time::Instant::now();
                        println!("📡 Signaling: re-announced to relay as {announced}");
                    }
                    Err(e) => return Err(e),
                }
            }
            // A change DURING a poll cannot be published on this connection:
            // hyper's HTTP/1.1 sender is sequential, and abandoning an
            // in-flight response would desync it. Ending the session is also
            // the honest response — a changed public address usually means a
            // NAT rebind, which frequently killed this TCP connection anyway.
            // The reconnect re-announces with the new address.
            changed = changes.changed() => {
                return match changed {
                    Ok(()) => {
                        println!("📡 Signaling: WAN address changed mid-poll — reconnecting to republish");
                        Ok(())
                    }
                    // The gatherer is gone; keep the session running rather
                    // than tearing down a healthy relay connection.
                    Err(_) => std::future::pending().await,
                };
            }
        }
    }
}

async fn announce(
    conn: &mut RelayConnection,
    identity: &HostIdentity,
    candidates: &Arc<Mutex<Vec<WanCandidate>>>,
) -> Result<String, String> {
    let listed = candidate_json(candidates);

    let mut params = Map::new();
    params.insert("protocol_version".into(), json!(SIGNALING_PROTOCOL_VERSION));
    params.insert("fingerprint".into(), json!(identity.fingerprint));
    params.insert("candidates".into(), Value::Array(listed));
    let result = conn.call("announce", params).await?;
    Ok(result
        .get("host_id")
        .and_then(Value::as_str)
        .unwrap_or(&identity.fingerprint[..16.min(identity.fingerprint.len())])
        .to_string())
}

/// Republish candidates after the gatherer reported a changed mapping.
///
/// Distinct command from `announce` so the relay can tell "a host just came
/// online" from "a known host's address moved" — the first may need to create
/// state, the second only to replace it.
async fn update(
    conn: &mut RelayConnection,
    identity: &HostIdentity,
    candidates: &Arc<Mutex<Vec<WanCandidate>>>,
) -> Result<(), String> {
    let listed = candidate_json(candidates);
    let count = listed.len();
    let mut params = Map::new();
    params.insert("fingerprint".into(), json!(identity.fingerprint));
    params.insert("candidates".into(), Value::Array(listed));
    conn.call("update", params).await?;
    println!("📡 Signaling: republished {count} candidate(s) after a WAN address change");
    Ok(())
}

fn candidate_json(candidates: &Arc<Mutex<Vec<WanCandidate>>>) -> Vec<Value> {
    candidates
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|c| json!({ "addr": c.mapped.to_string(), "via": c.via.to_string() }))
        .collect()
}

async fn poll(
    conn: &mut RelayConnection,
    identity: &HostIdentity,
    poll_secs: u32,
) -> Result<Vec<Value>, String> {
    let mut params = Map::new();
    params.insert("fingerprint".into(), json!(identity.fingerprint));
    params.insert("wait_secs".into(), json!(poll_secs));
    let result = conn.call("poll", params).await?;
    Ok(result
        .get("events")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default())
}

/// React to one relay event.
///
/// An `offer` hands the peer's candidates to the gatherer, which owns the
/// media socket's STUN inbox and runs the blast. Dispatching rather than
/// punching inline matters: the poll loop must stay responsive (it is holding
/// the only relay connection), and two consumers of the STUN inbox would race
/// for each other's responses.
fn handle_event(event: &Value, gather: &super::wan::GatherHandle) {
    let kind = event.get("event").and_then(Value::as_str).unwrap_or("unknown");
    match kind {
        "offer" => {
            let peer = event.get("peer_fingerprint").and_then(Value::as_str).unwrap_or("?");
            let candidates = nova_core::relay::parse_candidates(event.get("candidates"));
            if candidates.is_empty() {
                println!(
                    "📡 Signaling: offer from peer {}… carried no usable candidates — ignoring",
                    &peer[..16.min(peer.len())]
                );
                return;
            }
            println!(
                "📡 Signaling: connection offer from peer {}… with {} candidate(s) — punching",
                &peer[..16.min(peer.len())],
                candidates.len()
            );
            gather.punch_toward(candidates);
        }
        other => println!("📡 Signaling: unhandled relay event \"{other}\""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::{CertificateParams, KeyPair};
    use std::net::SocketAddr;

    fn self_signed(name: &str) -> (Vec<u8>, Vec<u8>) {
        let key = KeyPair::generate().expect("keypair");
        let params = CertificateParams::new(vec![name.to_string()]).expect("params");
        let cert = params.self_signed(&key).expect("self-sign");
        (cert.der().to_vec(), key.serialize_der())
    }

    fn sha256(bytes: &[u8]) -> [u8; 32] {
        Sha256::digest(bytes).into()
    }

    #[test]
    fn relay_url_must_be_https_and_parses_host_port_path() {
        let t = RelayTarget::parse("https://relay.example.com/v1/signal").unwrap();
        assert_eq!(t.host, "relay.example.com");
        assert_eq!(t.port, 443);
        assert_eq!(t.path, "/v1/signal");
        assert_eq!(t.authority(), "relay.example.com", "default port omitted");

        let t = RelayTarget::parse("https://relay.example.com:8443/x").unwrap();
        assert_eq!(t.port, 8443);
        assert_eq!(t.authority(), "relay.example.com:8443");

        // Plaintext would put Nova's identity certificate on the wire.
        assert!(RelayTarget::parse("http://relay.example.com/v1").is_err());
        assert!(RelayTarget::parse("not a url").is_err());
    }

    #[test]
    fn pins_must_be_32_byte_hex() {
        assert!(parse_pin(&"ab".repeat(32)).is_ok());
        assert!(parse_pin(&format!("  {}  ", "ab".repeat(32))).is_ok());
        assert!(parse_pin("abcd").is_err(), "too short");
        assert!(parse_pin(&"zz".repeat(32)).is_err(), "not hex");
        assert!(parse_pin("").is_err());
    }

    #[test]
    fn backoff_climbs_then_holds_at_the_ceiling() {
        assert_eq!(backoff_delay(0), Duration::from_secs(1));
        assert_eq!(backoff_delay(1), Duration::from_secs(2));
        assert_eq!(backoff_delay(2), Duration::from_secs(4));
        let mut prev = Duration::ZERO;
        for n in 0..64 {
            let d = backoff_delay(n);
            assert!(d >= prev, "must never decrease");
            assert!(d <= BACKOFF_CAP, "must never exceed the cap");
            prev = d;
        }
        assert_eq!(backoff_delay(63), BACKOFF_CAP, "saturates rather than overflowing");
    }

    #[test]
    fn envelope_matches_the_echo_rpc_shape() {
        let mut params = Map::new();
        params.insert("fingerprint".into(), json!("abc"));
        let body = encode_request(&SignalRequest { id: 7, command: "announce", params }).unwrap();
        assert!(body.ends_with(b"\n"), "newline-delimited like echo::rpc");
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["id"], 7);
        assert_eq!(v["command"], "announce");
        assert_eq!(v["fingerprint"], "abc", "params are flattened to the top level");

        let ok = decode_response(b"{\"id\":7,\"ok\":true,\"result\":{\"host_id\":\"h\"}}\n").unwrap();
        assert!(ok.ok);
        assert_eq!(ok.result.unwrap()["host_id"], "h");

        let err = decode_response(b"{\"id\":7,\"ok\":false,\"error\":{\"code\":\"nope\",\"message\":\"denied\"}}").unwrap();
        assert!(!err.ok);
        assert_eq!(err.error.unwrap().code, "nope");

        assert!(decode_response(b"").is_err());
        assert!(decode_response(b"not json").is_err());
    }

    /// The pin decides trust, so both directions matter: the right certificate
    /// must be accepted and any other one refused, regardless of whether it is
    /// otherwise valid.
    #[test]
    fn the_pin_accepts_only_the_pinned_certificate() {
        let (relay_der, _) = self_signed("relay.example.com");
        let (impostor_der, _) = self_signed("relay.example.com");

        let verifier = PinnedRelayCert {
            pin: sha256(&relay_der),
            algs: rustls::crypto::aws_lc_rs::default_provider().signature_verification_algorithms,
        };
        let name = ServerName::try_from("relay.example.com").unwrap();
        let now = UnixTime::since_unix_epoch(Duration::from_secs(1_800_000_000));

        assert!(
            verifier
                .verify_server_cert(&CertificateDer::from(relay_der), &[], &name, &[], now)
                .is_ok(),
            "the pinned certificate must be accepted"
        );
        assert!(
            verifier
                .verify_server_cert(&CertificateDer::from(impostor_der), &[], &name, &[], now)
                .is_err(),
            "a different certificate for the same name must be refused"
        );
    }

    /// End-to-end over loopback against a real TLS server: proves the pin, the
    /// client certificate, hyper-over-rustls, and the NDJSON envelope all work
    /// together — the parts most likely to be individually plausible and
    /// jointly broken.
    #[tokio::test]
    async fn announces_to_a_real_tls_relay_presenting_novas_identity() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (relay_cert, relay_key) = self_signed("localhost");
        let (nova_cert, nova_key) = self_signed("nova");
        let nova_fp = hex::encode(sha256(&nova_cert));

        // Relay side: require a client certificate, then report which one it saw.
        let server_cfg = rustls::ServerConfig::builder_with_protocol_versions(&[
            &rustls::version::TLS13,
        ])
        .with_client_cert_verifier(crate::pairing::test_accept_any_client_cert())
        .with_single_cert(
            vec![CertificateDer::from(relay_cert.clone())],
            PrivateKeyDer::Pkcs8(relay_key.into()),
        )
        .expect("server config");

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_cfg));

        let seen_fp = Arc::new(Mutex::new(String::new()));
        let seen_body = Arc::new(Mutex::new(String::new()));
        tokio::spawn({
            let seen_fp = seen_fp.clone();
            let seen_body = seen_body.clone();
            async move {
                let (tcp, _) = listener.accept().await.unwrap();
                let mut tls = acceptor.accept(tcp).await.expect("server handshake");
                {
                    let (_, conn) = tls.get_ref();
                    let der = conn.peer_certificates().unwrap()[0].as_ref().to_vec();
                    *seen_fp.lock().unwrap() = hex::encode(sha256(&der));
                }
                // Minimal HTTP/1.1: read the request, echo a signaling response.
                let mut buf = vec![0u8; 8192];
                let n = tls.read(&mut buf).await.unwrap();
                *seen_body.lock().unwrap() = String::from_utf8_lossy(&buf[..n]).into_owned();
                let payload = b"{\"id\":1,\"ok\":true,\"result\":{\"host_id\":\"relay-assigned\"}}\n";
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nContent-Length: {}\r\n\r\n",
                    payload.len()
                );
                tls.write_all(head.as_bytes()).await.unwrap();
                tls.write_all(payload).await.unwrap();
                tls.flush().await.unwrap();
                // Hold the connection open briefly so the client can read it all.
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        });

        let tls = build_client_tls(&nova_cert, &nova_key, sha256(&relay_cert)).expect("client tls");
        let target = RelayTarget {
            host: "localhost".to_string(),
            port: addr.port(),
            path: "/v1/signal".to_string(),
        };
        let mut conn = RelayConnection::connect(&target, Arc::new(tls))
            .await
            .expect("connect to relay");

        let identity = HostIdentity {
            fingerprint: nova_fp.clone(),
            cert_der: nova_cert,
            key_der: nova_key,
        };
        let candidates = Arc::new(Mutex::new(vec![WanCandidate {
            mapped: "203.0.113.5:47998".parse::<SocketAddr>().unwrap(),
            via: "1.1.1.1:3478".parse::<SocketAddr>().unwrap(),
        }]));

        let host_id = announce(&mut conn, &identity, &candidates)
            .await
            .expect("announce");
        assert_eq!(host_id, "relay-assigned", "relay's assigned id is used");

        assert_eq!(
            *seen_fp.lock().unwrap(),
            nova_fp,
            "the relay must see Nova's own identity certificate"
        );
        let body = seen_body.lock().unwrap().clone();
        assert!(body.contains("POST /v1/signal"), "request line: {body}");
        assert!(body.contains("\"command\":\"announce\""));
        assert!(body.contains("203.0.113.5:47998"), "candidates are published");
    }

    /// A relay whose certificate is not the pinned one must fail the
    /// handshake, not merely log a warning.
    #[tokio::test]
    async fn a_relay_with_the_wrong_certificate_is_refused() {
        let (relay_cert, relay_key) = self_signed("localhost");
        let (other_cert, _) = self_signed("localhost");
        let (nova_cert, nova_key) = self_signed("nova");

        let server_cfg = rustls::ServerConfig::builder_with_protocol_versions(&[
            &rustls::version::TLS13,
        ])
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(relay_cert)],
            PrivateKeyDer::Pkcs8(relay_key.into()),
        )
        .expect("server config");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_cfg));
        tokio::spawn(async move {
            if let Ok((tcp, _)) = listener.accept().await {
                let _ = acceptor.accept(tcp).await; // will fail; that's the point
            }
        });

        // Pin the OTHER certificate.
        let tls = build_client_tls(&nova_cert, &nova_key, sha256(&other_cert)).unwrap();
        let target = RelayTarget {
            host: "localhost".to_string(),
            port: addr.port(),
            path: "/v1/signal".to_string(),
        };
        // `RelayConnection` holds a hyper sender and is not Debug, so match
        // rather than expect_err.
        let err = match RelayConnection::connect(&target, Arc::new(tls)).await {
            Ok(_) => panic!("must refuse a relay whose certificate is not the pinned one"),
            Err(e) => e,
        };
        assert!(err.contains("TLS handshake"), "unexpected error: {err}");
    }
}

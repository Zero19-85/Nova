//! Signaling-relay client, shared by both peers.
//!
//! Hole punching is a chicken-and-egg problem: neither peer can tell the other
//! anything until a path already exists. A relay breaks it — a small,
//! always-reachable service both sides connect **outbound** to, which forwards
//! candidates. It carries no media, only a handful of small messages.
//!
//! Nova and Echo are both *clients* of the relay and speak the same protocol;
//! only the commands they send differ (a host announces and waits, a client
//! looks a host up and offers). So the transport lives here, once.
//!
//! ## Why long-poll rather than WebSocket
//!
//! A WebSocket costs eight extra crates in this tree (`tokio-tungstenite`,
//! `tungstenite`, `sha1`, `data-encoding`, `chacha20`, …). HTTPS long-poll
//! costs zero: `hyper` and `tokio-rustls` are already present. What WebSocket
//! buys — server push and low framing overhead on a long-lived stream — is not
//! what signaling needs: a few messages at session setup, and server push is
//! precisely what a held-open request provides.
//!
//! ## Authentication
//!
//! Mutual TLS. Each peer presents its own certificate ([`identity`]), and the
//! relay is authenticated by **pin** rather than a public CA — one key we
//! operate, rather than every CA on the internet, for a service that brokers
//! connections into people's machines. The trade is that rotating the relay's
//! certificate means shipping a new pin.

use std::sync::Arc;
use std::time::Duration;

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::header::{CONNECTION, CONTENT_TYPE, HOST};
use hyper::{Method, Request as HttpRequest};
use hyper_util::rt::TokioIo;
use serde_json::{Map, Value};
use tokio_rustls::rustls::ClientConfig;
use tokio_rustls::TlsConnector;

use crate::envelope::{decode_line, encode_line, InboundResponse, OutboundRequest};
use crate::identity::Identity;

/// Protocol version peers announce to the relay.
pub const RELAY_PROTOCOL_VERSION: u32 = 1;

/// Longest response body accepted. Signaling messages are a few hundred bytes;
/// anything approaching this is a relay malfunctioning or hostile.
const MAX_RESPONSE_BYTES: usize = 256 * 1024;

/// Reconnect backoff bounds. A relay outage must degrade to a slow retry, not
/// a hot loop against a service that is already struggling.
const BACKOFF_BASE: Duration = Duration::from_secs(1);
const BACKOFF_CAP: Duration = Duration::from_secs(60);

/// Ceiling on a single long-poll. Middleboxes commonly cut idle HTTP requests
/// at 60 s, so waiting longer fails in a way that looks like an error.
pub const MAX_POLL_SECS: u32 = 55;

/// Where and how to reach the relay, resolved once so a malformed URL fails
/// loudly at startup instead of on every retry.
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
            // The client certificate IS the peer's identity; sending it over
            // anything but TLS would hand that identity to the network.
            return Err(format!(
                "relay url must be https (got {}) — signaling carries the peer's identity certificate",
                parsed.scheme()
            ));
        }
        let host = parsed
            .host_str()
            .ok_or_else(|| "relay url has no host".to_string())?
            .to_string();
        Ok(RelayTarget {
            port: parsed.port().unwrap_or(443),
            path: match parsed.path() {
                "" => "/".to_string(),
                p => p.to_string(),
            },
            host,
        })
    }

    pub fn authority(&self) -> String {
        if self.port == 443 {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

/// Reconnect delay for attempt `n` (0-based): 1 s doubling to a 60 s ceiling.
///
/// Deterministic and separately testable; callers add jitter so a fleet
/// recovering from a relay outage does not stampede in lockstep.
pub fn backoff_delay(attempt: u32) -> Duration {
    let secs = BACKOFF_BASE
        .as_secs()
        .saturating_mul(1u64.checked_shl(attempt.min(16)).unwrap_or(u64::MAX));
    Duration::from_secs(secs.min(BACKOFF_CAP.as_secs()))
}

/// Backoff plus up-to-25% jitter.
pub fn backoff_with_jitter(attempt: u32) -> Duration {
    let base = backoff_delay(attempt);
    let span = base.as_millis() as u64 / 4 + 1;
    base + Duration::from_millis(rand::random::<u64>() % span)
}

/// A live connection to the relay: one TLS session carrying sequential
/// request/response pairs.
///
/// Sequential is a real constraint, not an implementation detail: hyper's
/// HTTP/1.1 sender cannot start a request while a response is in flight, so a
/// caller holding a long-poll open must not try to send on the same connection
/// until it completes.
pub struct RelayConnection {
    sender: hyper::client::conn::http1::SendRequest<Full<Bytes>>,
    target: RelayTarget,
    next_id: u64,
}

impl RelayConnection {
    pub async fn connect(target: &RelayTarget, tls: Arc<ClientConfig>) -> Result<Self, String> {
        let stream = tokio::net::TcpStream::connect((target.host.as_str(), target.port))
            .await
            .map_err(|e| format!("connect {}:{}: {e}", target.host, target.port))?;
        let _ = stream.set_nodelay(true);

        let server_name = rustls_pki_types::ServerName::try_from(target.host.clone())
            .map_err(|e| format!("invalid server name {}: {e}", target.host))?;
        let tls_stream = TlsConnector::from(tls)
            .connect(server_name, stream)
            .await
            .map_err(|e| format!("relay TLS handshake: {e}"))?;

        let (sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tls_stream))
            .await
            .map_err(|e| format!("http handshake: {e}"))?;
        // The connection future drives the socket; nothing progresses without it.
        tokio::spawn(async move {
            if let Err(e) = conn.await {
                println!("📡 Relay connection ended ({e})");
            }
        });

        Ok(RelayConnection { sender, target: target.clone(), next_id: 1 })
    }

    /// One request/response round trip.
    pub async fn call(&mut self, command: &str, params: Map<String, Value>) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        let body = encode_line(&OutboundRequest::call(id, command, params))?;

        let req = HttpRequest::builder()
            .method(Method::POST)
            .uri(self.target.path.clone())
            .header(HOST, self.target.authority())
            .header(CONTENT_TYPE, "application/x-ndjson")
            // Keep-alive: announce and every subsequent poll reuse one TLS
            // session, so a poll cycle costs no handshake.
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

        let decoded: InboundResponse = decode_line(&collected)?;
        if decoded.id.is_some() && decoded.id != Some(id) {
            // Sequential HTTP means responses cannot legitimately arrive out
            // of order; a mismatch means the relay is confused about what it
            // is answering, and acting on it is worse than reconnecting.
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

/// Serialize candidates for the relay.
pub fn candidates_json(candidates: &[crate::stun::WanCandidate]) -> Vec<Value> {
    candidates
        .iter()
        .map(|c| serde_json::json!({ "addr": c.mapped.to_string(), "via": c.via.to_string() }))
        .collect()
}

/// Parse the candidate list out of a relay payload, skipping entries that do
/// not parse rather than failing the whole exchange — a peer advertising one
/// malformed address should still be reachable on its others.
pub fn parse_candidates(value: Option<&Value>) -> Vec<std::net::SocketAddr> {
    value
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|c| c.get("addr")?.as_str()?.parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Build the params every peer sends when identifying itself.
pub fn identity_params(identity: &Identity) -> Map<String, Value> {
    let mut params = Map::new();
    params.insert("protocol_version".into(), serde_json::json!(RELAY_PROTOCOL_VERSION));
    params.insert("fingerprint".into(), serde_json::json!(identity.fingerprint));
    params
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

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

        // Plaintext would put the peer's identity certificate on the wire.
        assert!(RelayTarget::parse("http://relay.example.com/v1").is_err());
        assert!(RelayTarget::parse("not a url").is_err());
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
        // Jitter must stay within the documented quarter-step.
        for n in 0..8 {
            let j = backoff_with_jitter(n);
            assert!(j >= backoff_delay(n));
            assert!(j <= backoff_delay(n) + backoff_delay(n) / 4 + Duration::from_millis(1));
        }
    }

    #[test]
    fn candidate_lists_survive_a_malformed_entry() {
        let payload = serde_json::json!({
            "candidates": [
                { "addr": "203.0.113.5:47998" },
                { "addr": "not-an-address" },
                { "via": "1.1.1.1:3478" },
                { "addr": "198.51.100.9:41000" }
            ]
        });
        let got = parse_candidates(payload.get("candidates"));
        assert_eq!(
            got,
            vec![
                "203.0.113.5:47998".parse::<SocketAddr>().unwrap(),
                "198.51.100.9:41000".parse::<SocketAddr>().unwrap(),
            ],
            "one bad entry must not hide the good ones"
        );
        assert!(parse_candidates(None).is_empty());
        assert!(parse_candidates(Some(&serde_json::json!("nonsense"))).is_empty());
    }
}

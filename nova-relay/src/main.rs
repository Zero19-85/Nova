//! Minimal signaling relay — **development infrastructure, not production**.
//!
//! It exists because neither Nova nor Echo can be tested without one: hole
//! punching requires a third party both peers can reach outbound, and until
//! this existed both sides of the handshake were unverifiable code. Running it
//! on a LAN box is enough to exercise the whole path end to end.
//!
//! ## What it does
//!
//! Holds an in-memory directory of hosts, keyed by certificate fingerprint:
//!
//! - `announce` — a host publishes its fingerprint and WAN candidates.
//! - `update`   — a host republishes candidates after its address changed.
//! - `poll`     — a host long-polls for events (currently: connection offers).
//! - `lookup`   — a client asks for a host's candidates.
//! - `offer`    — a client submits its own candidates; the host receives them
//!                on its next `poll`, and both then punch.
//!
//! Every peer authenticates with mutual TLS, and the fingerprint in the
//! request body is checked against the certificate that was actually presented
//! — otherwise any authenticated peer could impersonate any other by simply
//! typing a different fingerprint.
//!
//! ## What it deliberately does not do
//!
//! No persistence (restart forgets everything), no enrolment (any peer with a
//! certificate may announce), no rate limiting, no clustering, no media
//! relaying for the symmetric-NAT case. Each of those is real work for a
//! deployed service. Treated as a stand-in, it is exactly enough to prove the
//! protocol; treated as production, it is a directory anyone can write to.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clap::Parser;
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use nova_core::envelope::{decode_line, encode_line, InboundRequest, OutboundResponse};
use nova_core::identity::{peer_fingerprint, server_config_require_client_cert, Identity};
use serde_json::{json, Map, Value};

/// How long a host entry survives without being refreshed. A host that stopped
/// announcing is either offline or has moved; serving its stale candidates to
/// a client only produces a punch that cannot succeed.
///
/// Comfortably longer than a host's poll cycle (30 s) *and* its re-announce
/// cycle (60 s), so only a genuinely absent host is evicted. It was 120 s,
/// which left barely one missed cycle of slack — and eviction used to be
/// permanent, because nothing in the poll path recreated an entry.
const HOST_TTL: Duration = Duration::from_secs(300);

/// Ceiling on a long-poll hold, matching the client's own cap.
const MAX_WAIT_SECS: u64 = 55;

#[derive(Parser, Debug)]
#[command(about = "Minimal Nova/Echo signaling relay (development)")]
struct Args {
    /// Address to listen on.
    #[arg(long, default_value = "0.0.0.0:8443")]
    listen: SocketAddr,
    /// Directory for the relay's own TLS identity (generated on first run).
    #[arg(long, default_value = ".")]
    data_dir: PathBuf,
}

#[derive(Debug, Clone)]
struct HostEntry {
    candidates: Vec<Value>,
    last_seen: Instant,
    /// Offers waiting to be delivered on the host's next poll.
    pending: Vec<Value>,
}

type Directory = Arc<Mutex<HashMap<String, HostEntry>>>;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    let identity = Identity::load_or_create(&args.data_dir, "relay", "nova-relay")
        .map_err(|e| format!("relay identity: {e}"))?;
    println!("🔑 Relay identity: {}", identity.fingerprint);
    println!();
    println!("   Configure Nova (nova.toml):");
    println!("     [echo.signaling]");
    println!("     url               = \"https://<this-host>:{}/v1/signal\"", args.listen.port());
    println!("     relay_cert_sha256 = \"{}\"", identity.fingerprint);
    println!();

    let tls = server_config_require_client_cert(&identity)?;
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls));
    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    println!("📡 Relay listening on {} (mutual TLS)", args.listen);

    let directory: Directory = Arc::new(Mutex::new(HashMap::new()));

    // Expire silent hosts rather than letting the directory grow forever.
    tokio::spawn({
        let directory = directory.clone();
        async move {
            let mut tick = tokio::time::interval(Duration::from_secs(30));
            loop {
                tick.tick().await;
                let mut dir = directory.lock().unwrap();
                let before = dir.len();
                dir.retain(|_, e| e.last_seen.elapsed() < HOST_TTL);
                if dir.len() != before {
                    println!("🧹 Expired {} silent host(s)", before - dir.len());
                }
            }
        }
    });

    loop {
        let (tcp, peer) = listener.accept().await?;
        let acceptor = acceptor.clone();
        let directory = directory.clone();
        tokio::spawn(async move {
            let tls_stream = match acceptor.accept(tcp).await {
                Ok(s) => s,
                Err(e) => {
                    println!("⚠️  {peer} TLS handshake failed: {e}");
                    return;
                }
            };
            // The certificate the peer actually presented — the only identity
            // claim that means anything here.
            let fingerprint = {
                let (_, conn) = tls_stream.get_ref();
                match peer_fingerprint(conn.peer_certificates()) {
                    Some(fp) => fp,
                    None => {
                        println!("⛔ {peer} sent no client certificate");
                        return;
                    }
                }
            };
            println!("🔓 {peer} connected as {}…", &fingerprint[..16]);

            let service = hyper::service::service_fn(move |req: Request<hyper::body::Incoming>| {
                let directory = directory.clone();
                let fingerprint = fingerprint.clone();
                async move { Ok::<_, std::convert::Infallible>(handle(req, directory, fingerprint).await) }
            });
            if let Err(e) = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(tls_stream), service)
                .await
            {
                println!("🔌 {peer} disconnected ({e})");
            }
        });
    }
}

async fn handle(
    req: Request<hyper::body::Incoming>,
    directory: Directory,
    peer_fp: String,
) -> Response<Full<Bytes>> {
    let body = match req.into_body().collect().await {
        Ok(b) => b.to_bytes(),
        Err(e) => return reply(OutboundResponse::err(None, "bad_request", format!("body: {e}"))),
    };
    let request: InboundRequest = match decode_line(&body) {
        Ok(r) => r,
        Err(e) => return reply(OutboundResponse::err(None, "bad_request", e)),
    };
    let id = request.id;

    // Identity check: the fingerprint in the body must be the certificate that
    // was actually presented. Without this, any peer holding any certificate
    // could publish candidates under someone else's name — the relay would
    // become a redirection primitive.
    if let Some(claimed) = request.params.get("fingerprint").and_then(Value::as_str) {
        if claimed != peer_fp {
            println!("⛔ {}… claimed to be {}… — refused", &peer_fp[..16], &claimed[..16.min(claimed.len())]);
            return reply(OutboundResponse::err(
                id,
                "identity_mismatch",
                "the fingerprint in this request is not the certificate you presented",
            ));
        }
    }

    let response = match request.command.as_str() {
        "announce" | "update" => {
            let candidates = request
                .params
                .get("candidates")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let mut dir = directory.lock().unwrap();
            let entry = dir.entry(peer_fp.clone()).or_insert_with(|| HostEntry {
                candidates: Vec::new(),
                last_seen: Instant::now(),
                pending: Vec::new(),
            });
            entry.candidates = candidates;
            entry.last_seen = Instant::now();
            println!(
                "📥 {} from {}… with {} candidate(s)",
                request.command,
                &peer_fp[..16],
                entry.candidates.len()
            );
            OutboundResponse::ok(id, json!({ "host_id": &peer_fp[..16] }))
        }

        "poll" => {
            let wait = request
                .params
                .get("wait_secs")
                .and_then(Value::as_u64)
                .unwrap_or(30)
                .min(MAX_WAIT_SECS);
            let deadline = Instant::now() + Duration::from_secs(wait);
            loop {
                {
                    let mut dir = directory.lock().unwrap();
                    match dir.get_mut(&peer_fp) {
                        Some(entry) => {
                            entry.last_seen = Instant::now();
                            if !entry.pending.is_empty() {
                                let events = std::mem::take(&mut entry.pending);
                                println!("📤 Delivering {} event(s) to {}…", events.len(), &peer_fp[..16]);
                                break OutboundResponse::ok(id, json!({ "events": events }));
                            }
                        }
                        // Polling a directory that has forgotten us. Say so
                        // explicitly instead of returning an empty event list:
                        // a host whose entry expired would otherwise poll
                        // contentedly forever while being invisible to every
                        // lookup, since nothing in the poll path recreates an
                        // entry. Telling it to re-announce is what makes the
                        // directory self-healing.
                        None => {
                            println!("🔁 {}… polled but is not in the directory — asking it to re-announce", &peer_fp[..16]);
                            break OutboundResponse::err(
                                id,
                                "not_announced",
                                "this host is not in the directory — send announce",
                            );
                        }
                    }
                }
                if Instant::now() >= deadline {
                    // Empty result, not an error: a quiet poll is the normal
                    // case and the host simply polls again.
                    break OutboundResponse::ok(id, json!({ "events": [] }));
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }

        "lookup" => {
            let host = request.params.get("host").and_then(Value::as_str).unwrap_or("");
            let dir = directory.lock().unwrap();
            match dir.get(host) {
                Some(entry) => {
                    println!("🔎 {}… looked up {}…", &peer_fp[..16], &host[..16.min(host.len())]);
                    OutboundResponse::ok(id, json!({ "candidates": entry.candidates }))
                }
                None => OutboundResponse::err(
                    id,
                    "host_offline",
                    "no host with that fingerprint has announced recently",
                ),
            }
        }

        "offer" => {
            let host = request.params.get("host").and_then(Value::as_str).unwrap_or("").to_string();
            let candidates = request
                .params
                .get("candidates")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let mut dir = directory.lock().unwrap();
            match dir.get_mut(&host) {
                Some(entry) => {
                    entry.pending.push(json!({
                        "event": "offer",
                        "peer_fingerprint": peer_fp,
                        "candidates": candidates,
                    }));
                    println!("🤝 Offer from {}… queued for {}…", &peer_fp[..16], &host[..16.min(host.len())]);
                    OutboundResponse::ok(id, json!({ "queued": true }))
                }
                None => OutboundResponse::err(id, "host_offline", "that host is not connected"),
            }
        }

        other => OutboundResponse::err(id, "unknown_command", format!("unknown command \"{other}\"")),
    };

    reply(response)
}

fn reply(response: OutboundResponse) -> Response<Full<Bytes>> {
    let body = encode_line(&response)
        .unwrap_or_else(|_| b"{\"ok\":false,\"error\":{\"code\":\"internal\"}}\n".to_vec());
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/x-ndjson")
        .body(Full::new(Bytes::from(body)))
        .expect("static response")
}

/// Unused today; kept so the params helper has a home when the relay grows a
/// second endpoint shape.
#[allow(dead_code)]
fn empty_params() -> Map<String, Value> {
    Map::new()
}

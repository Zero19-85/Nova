//! The connection sequence, as a library.
//!
//! This is the code that was inside the CLI's `main`. It moved here unchanged in
//! behaviour and changed in exactly one respect: it no longer prints. Progress
//! is reported through [`Progress`], which the CLI implements with `println!`
//! and the Android bridge implements by queueing JSON for Kotlin to poll.
//!
//! That single change is what makes the library reusable. `println!` on Android
//! goes to a stdout nobody reads, so a session that reported progress by
//! printing would be a session an app could not narrate — no "punching…", no
//! "host refused", just a spinner and eventually a picture or nothing. Since
//! every one of those messages is a state transition a UI needs, they became
//! events rather than being deleted.
//!
//! ## The order is not interchangeable
//!
//! 1. **Gather** — discover our public address *from the socket that will carry
//!    traffic*. A throwaway discovery socket produces a mapping nothing will
//!    ever reach.
//! 2. **Look up** the host's candidates through the relay.
//! 3. **Offer** ours, so the host learns where to punch toward.
//! 4. **Punch** — simultaneous open.
//! 5. **Then** ask for a session. The host grants sessions only over a path it
//!    has already confirmed (`echo::session`'s `NoPathLatched`), so a client
//!    that asked before punching is refused every time.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use nova_core::identity::{client_config_pinned, parse_fingerprint, Identity};
use nova_core::media_crypto::SessionKeys;
use nova_core::punch;
use nova_core::relay::{
    candidates_json, identity_params, parse_candidates, RelayConnection, RelayTarget,
};
use nova_core::stun::{self, MappingBehavior};
use serde_json::{json, Value};

use crate::control::{self, ControlChannel};
use crate::receiver::{self, FrameSink, ReceiveStats};

/// Something worth telling the user about.
///
/// Deliberately *not* the media path: frames go to a [`FrameSink`], events go
/// here. The split matches the bridge Kotlin sees — `nativePollEvent` for
/// control, `nativeFillBuffer` for media — because they have genuinely
/// different rates and lifetimes. Mixing a 120 Hz stream into a channel a UI
/// thread polls would make the UI the bottleneck for video.
#[derive(Debug, Clone)]
pub enum Event {
    Identity { fingerprint: String },
    SocketBound { local: SocketAddr },
    PublicAddress { mapped: SocketAddr, via: SocketAddr },
    Mapping { behavior: MappingBehavior },
    RelayConnected { authority: String },
    HostCandidates { addrs: Vec<SocketAddr> },
    Offered,
    Punching { interval: Duration, timeout: Duration },
    PathOpen { peer: SocketAddr, rounds: u32, proof: String, local: SocketAddr },
    /// The punch failed. `endpoint_dependent` distinguishes "this network
    /// cannot do P2P" from "something is misconfigured" — a difference worth
    /// surfacing, because only one of them is worth retrying.
    PunchFailed { endpoint_dependent: bool, error: String },
    ControlOpening { peer: SocketAddr, lan: Option<SocketAddr> },
    ControlAuthenticated,
    Hello { server: String, protocol_version: u64, device_name: String },
    Granted { session_id: u64, width: u64, height: u64, fps: u64, codec: String },
    /// The host declined — most often the anti-hijack gate doing its job while
    /// somebody else is streaming. An expected answer, not a failure.
    Refused { reason: String },
    Warning { message: String },
    Ended { stats: ReceiveStats },
}

impl Event {
    /// The event as the JSON the Android bridge hands to Kotlin.
    ///
    /// Every variant carries a `type` discriminator, so the Kotlin side is a
    /// `when` over one field rather than a shape-sniffing parser.
    pub fn to_json(&self) -> Value {
        match self {
            Event::Identity { fingerprint } => json!({"type": "identity", "fingerprint": fingerprint}),
            Event::SocketBound { local } => json!({"type": "socket_bound", "local": local.to_string()}),
            Event::PublicAddress { mapped, via } => {
                json!({"type": "public_address", "mapped": mapped.to_string(), "via": via.to_string()})
            }
            Event::Mapping { behavior } => json!({
                "type": "mapping",
                "behavior": match behavior {
                    MappingBehavior::EndpointIndependent => "endpoint_independent",
                    MappingBehavior::EndpointDependent => "endpoint_dependent",
                    MappingBehavior::Unknown => "unknown",
                },
            }),
            Event::RelayConnected { authority } => json!({"type": "relay_connected", "authority": authority}),
            Event::HostCandidates { addrs } => json!({
                "type": "host_candidates",
                "addrs": addrs.iter().map(|a| a.to_string()).collect::<Vec<_>>(),
            }),
            Event::Offered => json!({"type": "offered"}),
            Event::Punching { timeout, .. } => {
                json!({"type": "punching", "timeout_ms": timeout.as_millis() as u64})
            }
            Event::PathOpen { peer, rounds, proof, local } => json!({
                "type": "path_open",
                "peer": peer.to_string(),
                "rounds": rounds,
                "proof": proof,
                "local": local.to_string(),
            }),
            Event::PunchFailed { endpoint_dependent, error } => json!({
                "type": "punch_failed",
                "endpoint_dependent": endpoint_dependent,
                "error": error,
            }),
            Event::ControlOpening { peer, lan } => json!({
                "type": "control_opening",
                "peer": peer.to_string(),
                "lan": lan.map(|a| a.to_string()),
            }),
            Event::ControlAuthenticated => json!({"type": "control_authenticated"}),
            Event::Hello { server, protocol_version, device_name } => json!({
                "type": "hello",
                "server": server,
                "protocol_version": protocol_version,
                "device_name": device_name,
            }),
            Event::Granted { session_id, width, height, fps, codec } => json!({
                "type": "granted",
                "session_id": session_id,
                "width": width,
                "height": height,
                "fps": fps,
                "codec": codec,
            }),
            Event::Refused { reason } => json!({"type": "refused", "reason": reason}),
            Event::Warning { message } => json!({"type": "warning", "message": message}),
            Event::Ended { stats } => json!({
                "type": "ended",
                "frames_completed": stats.frames_completed,
                "keyframes": stats.keyframes,
                "frames_incomplete": stats.frames_incomplete,
                "frames_failed_auth": stats.frames_failed_auth,
                "frames_recovered_by_fec": stats.frames_recovered_by_fec,
                "frames_dropped_before_keyframe": stats.frames_dropped_before_keyframe,
                "packets_rejected": stats.packets_rejected,
            }),
        }
    }
}

/// Receives session progress. See [`Event`].
pub trait Progress {
    fn event(&mut self, event: Event);
}

/// Discards everything — for tests and for callers that only want the outcome.
impl Progress for () {
    fn event(&mut self, _event: Event) {}
}

/// Where to find the host, and how long to try.
#[derive(Debug, Clone)]
pub struct ConnectOptions {
    pub relay_url: String,
    pub relay_pin: String,
    /// The host's fingerprint. Its relay identity, its pairing certificate, and
    /// the pin for the control channel are all this one value.
    pub host_fingerprint: String,
    pub punch_timeout: Duration,
}

/// What to ask the host for.
#[derive(Debug, Clone)]
pub struct StreamOptions {
    pub res: String,
    pub fps: u32,
    pub codec: String,
    pub bitrate_kbps: u32,
    /// `Some` = debug over the host's LAN TCP control port instead of the
    /// punched tunnel. The host refuses that port from non-private addresses,
    /// so this is not a WAN fallback and must not be presented as one.
    pub control: Option<String>,
}

impl Default for StreamOptions {
    fn default() -> Self {
        Self {
            res: "1080p".into(),
            fps: 60,
            codec: "hevc".into(),
            bitrate_kbps: 20000,
            control: None,
        }
    }
}

/// An open path: the socket holding the NAT pinhole, and the peer it opened
/// toward.
///
/// The socket is carried rather than rebound because **the mapping belongs to
/// this socket**. Anything that binds a fresh one has thrown the punch away.
pub struct OpenPath {
    pub socket: tokio::net::UdpSocket,
    pub peer: SocketAddr,
}

/// Gather, trade candidates through the relay, and punch.
pub async fn open_path(
    identity: &Identity,
    opts: &ConnectOptions,
    progress: &mut impl Progress,
) -> Result<OpenPath, String> {
    progress.event(Event::Identity { fingerprint: identity.fingerprint.clone() });

    // ── 1. Gather ───────────────────────────────────────────────────────────
    let socket = tokio::net::UdpSocket::bind("0.0.0.0:0")
        .await
        .map_err(|e| format!("bind media socket: {e}"))?;
    let local = socket.local_addr().map_err(|e| format!("local address: {e}"))?;
    progress.event(Event::SocketBound { local });

    let servers = resolve_default_stun(progress).await;
    if servers.is_empty() {
        return Err("no STUN server resolved — cannot discover a public address".into());
    }
    let (behavior, mine) = stun::classify_mapping(&socket, &servers).await;
    if mine.is_empty() {
        return Err("no STUN server answered — cannot discover a public address".into());
    }
    for c in &mine {
        progress.event(Event::PublicAddress { mapped: c.mapped, via: c.via });
    }
    progress.event(Event::Mapping { behavior });

    // ── 2/3. Relay: look up the host, offer ourselves ───────────────────────
    let target = RelayTarget::parse(&opts.relay_url).map_err(|e| format!("relay URL: {e}"))?;
    let pin = parse_fingerprint(&opts.relay_pin).map_err(|e| format!("relay pin: {e}"))?;
    let tls = Arc::new(client_config_pinned(identity, pin)?);
    let mut conn = RelayConnection::connect(&target, tls)
        .await
        .map_err(|e| format!("connect to relay: {e}"))?;
    progress.event(Event::RelayConnected { authority: target.authority().to_string() });

    let mut params = identity_params(identity);
    params.insert("host".into(), json!(opts.host_fingerprint));
    let result = conn.call("lookup", params).await.map_err(|e| format!("relay lookup: {e}"))?;
    let host_candidates = parse_candidates(result.get("candidates"));
    if host_candidates.is_empty() {
        return Err(format!(
            "the relay knows no candidates for host {}… — is Nova running with \
             [echo.signaling] configured against this relay?",
            short(&opts.host_fingerprint)
        ));
    }
    progress.event(Event::HostCandidates { addrs: host_candidates.clone() });

    let mut params = identity_params(identity);
    params.insert("host".into(), json!(opts.host_fingerprint));
    params.insert("candidates".into(), Value::Array(candidates_json(&mine)));
    conn.call("offer", params).await.map_err(|e| format!("relay offer: {e}"))?;
    progress.event(Event::Offered);

    // ── 4. Punch ────────────────────────────────────────────────────────────
    // Both sides are now sending. Early packets are expected to be dropped by
    // the far NAT; retrying through that window is the mechanism, not a
    // workaround for one.
    progress.event(Event::Punching {
        interval: punch::PROBE_INTERVAL,
        timeout: opts.punch_timeout,
    });
    let mut io = punch::UdpPunchIo::new(&socket);
    let cfg = punch::PunchConfig { timeout: opts.punch_timeout, ..Default::default() };
    match punch::punch_io(&mut io, &host_candidates, cfg).await {
        Ok(result) => {
            progress.event(Event::PathOpen {
                peer: result.peer,
                rounds: result.rounds,
                proof: format!("{:?}", result.proof),
                local,
            });
            Ok(OpenPath { socket, peer: result.peer })
        }
        Err(e) => {
            let endpoint_dependent = behavior == MappingBehavior::EndpointDependent;
            progress.event(Event::PunchFailed { endpoint_dependent, error: e.to_string() });
            Err(if endpoint_dependent {
                "no path could be opened: this NAT is endpoint-dependent (symmetric), so the \
                 address a STUN server reports is not the one a peer can reach — sessions from \
                 this network need a relay"
                    .into()
            } else {
                format!(
                    "no path could be opened — check that Nova is polling the same relay, that \
                     its fingerprint is {}…, and that its own punch is running",
                    short(&opts.host_fingerprint)
                )
            })
        }
    }
}

/// Ask the host for a session over the punched path, then receive it until
/// `stop` is set.
///
/// `stop` is the caller's, not this function's: the CLI ends on a timer or
/// Ctrl-C, and an app ends when the user backs out or the process is
/// backgrounded. Owning the trigger here would have forced both into the same
/// policy.
pub async fn stream(
    identity: &Identity,
    host_fingerprint: &str,
    path: OpenPath,
    opts: &StreamOptions,
    sink: &mut impl FrameSink,
    progress: &mut impl Progress,
    stop: tokio::sync::watch::Receiver<bool>,
) -> Result<ReceiveStats, String> {
    let pin = parse_fingerprint(host_fingerprint).map_err(|e| format!("host fingerprint: {e}"))?;
    let OpenPath { socket, peer } = path;
    let socket = Arc::new(socket);

    // The socket must have exactly one reader, so demultiplexing starts before
    // anything else — the TLS handshake below needs its datagrams delivered,
    // and they arrive here.
    let (media_tx, media_rx) = tokio::sync::mpsc::unbounded_channel();
    let (control_tx, control_rx) = tokio::sync::mpsc::unbounded_channel();
    let demux_task = tokio::spawn({
        let socket = socket.clone();
        let stop_rx = stop.clone();
        async move {
            let _ = receiver::demultiplex(&socket, peer, media_tx, control_tx, stop_rx).await;
        }
    });

    // Any early return past this point must not leak the demultiplexer, so the
    // body runs in a helper and the abort happens exactly once, below.
    let outcome = stream_inner(
        identity, pin, &socket, peer, opts, sink, progress, stop, media_rx, control_rx,
    )
    .await;
    demux_task.abort();
    outcome
}

#[allow(clippy::too_many_arguments)]
async fn stream_inner(
    identity: &Identity,
    pin: [u8; 32],
    socket: &Arc<tokio::net::UdpSocket>,
    peer: SocketAddr,
    opts: &StreamOptions,
    sink: &mut impl FrameSink,
    progress: &mut impl Progress,
    stop: tokio::sync::watch::Receiver<bool>,
    media_rx: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    control_rx: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
) -> Result<ReceiveStats, String> {
    let lan = match &opts.control {
        Some(addr) => Some(
            tokio::net::lookup_host(addr)
                .await
                .map_err(|e| format!("resolve control address: {e}"))?
                .next()
                .ok_or("could not resolve the control address")?,
        ),
        None => None,
    };
    progress.event(Event::ControlOpening { peer, lan });

    let mut ctl = match lan {
        // Explicit LAN address: TCP to port 48011, for local debugging only.
        Some(addr) => ControlChannel::connect_lan(addr, identity, pin)
            .await
            .map_err(|e| format!("LAN control channel: {e}"))?,
        // The zero-config path: mutual TLS over reliable UDP, on the socket the
        // punch opened. No port forwarding, no trusted relay.
        None => ControlChannel::connect_wan(socket.clone(), peer, control_rx, identity, pin)
            .await
            .map_err(|e| format!("control tunnel: {e}"))?,
    };
    progress.event(Event::ControlAuthenticated);

    let hello = ctl.call("hello", serde_json::Map::new()).await?;
    progress.event(Event::Hello {
        server: control::field_str(&hello, "server").unwrap_or("nova").to_string(),
        protocol_version: control::field_u64(&hello, "protocol_version").unwrap_or(0),
        device_name: control::field_str(&hello, "device_name").unwrap_or("?").to_string(),
    });
    if hello.pointer("/capabilities/sessions") != Some(&Value::Bool(true)) {
        return Err("this host has no session layer — it cannot hand media over yet".into());
    }

    let mut params = serde_json::Map::new();
    params.insert("res".into(), json!(opts.res));
    params.insert("fps".into(), json!(opts.fps));
    params.insert("codec".into(), json!(opts.codec));
    params.insert("bitrate_kbps".into(), json!(opts.bitrate_kbps));

    let grant = match ctl.call("start_session", params).await {
        Ok(g) => g,
        Err(e) => {
            // The anti-hijack refusal is the expected, correct answer while
            // someone else is streaming — report it as an answer, not a fault.
            progress.event(Event::Refused { reason: e.clone() });
            return Err(format!("no session was granted: {e}"));
        }
    };

    let session_id = control::field_u64(&grant, "session_id")?;
    let keys = SessionKeys::from_hex(control::field_str(&grant, "media_key")?)
        .map_err(|e| format!("media key from host: {e}"))?;
    progress.event(Event::Granted {
        session_id,
        width: control::field_u64(&grant, "width").unwrap_or(0),
        height: control::field_u64(&grant, "height").unwrap_or(0),
        fps: control::field_u64(&grant, "fps").unwrap_or(0),
        codec: control::field_str(&grant, "codec").unwrap_or("?").to_string(),
    });

    let stats = receiver::run_receiver(socket, peer, media_rx, Some(keys), sink, stop)
        .await
        .map_err(|e| format!("receive loop: {e}"))?;

    // Always tell the host we are done. A session left open would block the
    // next client — including a Moonlight one, which is exactly the asymmetry
    // the host's gate exists to prevent.
    if let Err(e) = ctl.call("stop_session", serde_json::Map::new()).await {
        // Not fatal: the host also releases the session when the control tunnel
        // closes, precisely so a client that vanishes cannot hold the pipeline.
        progress.event(Event::Warning {
            message: format!("could not stop the session cleanly: {e}"),
        });
    }

    progress.event(Event::Ended { stats });
    Ok(stats)
}

/// Resolve the built-in STUN servers to one IPv4 address each.
pub async fn resolve_default_stun(progress: &mut impl Progress) -> Vec<SocketAddr> {
    let mut out = Vec::new();
    for name in stun::DEFAULT_STUN_SERVERS {
        match tokio::net::lookup_host(name).await {
            Ok(addrs) => out.extend(addrs.filter(SocketAddr::is_ipv4).take(1)),
            Err(e) => progress.event(Event::Warning {
                message: format!("could not resolve {name}: {e}"),
            }),
        }
    }
    out
}

/// First 16 characters of a fingerprint — how Nova abbreviates them in logs.
fn short(fingerprint: &str) -> &str {
    &fingerprint[..16.min(fingerprint.len())]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_event_carries_a_type_discriminator_for_the_kotlin_side() {
        let events = [
            Event::Identity { fingerprint: "ab".into() },
            Event::SocketBound { local: "1.2.3.4:5".parse().unwrap() },
            Event::Mapping { behavior: MappingBehavior::EndpointDependent },
            Event::Offered,
            Event::ControlAuthenticated,
            Event::Refused { reason: "moonlight_active".into() },
            Event::Ended { stats: ReceiveStats::default() },
        ];
        for ev in &events {
            let json = ev.to_json();
            assert!(
                json.get("type").and_then(Value::as_str).is_some(),
                "every event needs a `type` field, this one has {json}"
            );
        }
    }

    #[test]
    fn a_refusal_is_reported_with_its_reason_intact() {
        // The host's refusal text is the whole diagnostic value of the event —
        // "MoonlightActive" and "HeldByAnotherDevice" mean different things to
        // a user, and collapsing them would hide which one happened.
        let ev = Event::Refused { reason: "HeldByAnotherDevice".into() };
        assert_eq!(ev.to_json()["reason"], json!("HeldByAnotherDevice"));
    }

    #[test]
    fn the_gate_tally_survives_into_the_ended_event() {
        let stats = ReceiveStats { frames_dropped_before_keyframe: 3, ..Default::default() };
        assert_eq!(Event::Ended { stats }.to_json()["frames_dropped_before_keyframe"], json!(3));
    }
}

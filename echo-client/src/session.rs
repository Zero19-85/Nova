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

/// Which route the media path actually took.
///
/// **Determined from the peer that was latched, never from which branch of the
/// cascade ran.** Those are not the same question and the difference is the
/// whole reason this is an enum rather than a boolean: the relay is a
/// *signalling* channel, so a session that used the relay to trade candidates
/// and then punched to a private address is carrying media entirely on the
/// local segment — LAN by every measure that affects latency. Reporting it as
/// WAN because the relay was involved would put a cyan badge on the fastest
/// path Echo has, and send anyone debugging latency looking at the internet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// Media flows to a private address: same subnet, no internet involved.
    Lan,
    /// A punched path to a public address, via relay signalling.
    WanPunch,
    /// A punched path to the endpoint the user configured by hand.
    DirectWan,
}

impl Transport {
    /// The wire name. Kotlin branches on this string, so it is API.
    pub fn as_str(self) -> &'static str {
        match self {
            Transport::Lan => "lan",
            Transport::WanPunch => "wan_punch",
            Transport::DirectWan => "direct_wan",
        }
    }
}

/// Whether an address is one only reachable from inside a local network.
///
/// The same test the host applies in `rpc.rs::is_lan_peer`, and it must stay
/// the same: the two ends classifying one path differently is how a badge ends
/// up disagreeing with the host log about what just happened.
fn is_private_addr(addr: &SocketAddr) -> bool {
    match addr.ip() {
        std::net::IpAddr::V4(ip) => ip.is_loopback() || ip.is_private() || ip.is_link_local(),
        std::net::IpAddr::V6(ip) => {
            ip.is_loopback()
                || (ip.segments()[0] & 0xffc0) == 0xfe80
                || (ip.segments()[0] & 0xfe00) == 0xfc00
        }
    }
}

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
    /// Our address on the local network, offered so a peer on the *same*
    /// network can reach us without NAT hairpinning.
    LocalCandidate { addr: SocketAddr },
    Mapping { behavior: MappingBehavior },
    RelayConnected { authority: String },
    HostCandidates { addrs: Vec<SocketAddr> },
    Offered,
    Punching { interval: Duration, timeout: Duration },
    /// Stage 1 of the cascade is dialling the host's LAN control port.
    LanAttempt { endpoint: SocketAddr },
    /// The LAN rendezvous succeeded and the host named where to punch.
    LanRendezvous { offered: SocketAddr, host_candidates: Vec<SocketAddr> },
    /// Stage 1 was given up on. **Not an error** — it is the expected outcome
    /// from anywhere except the host's own network, and the session continues
    /// down the cascade. Reported because "why is this slow when both machines
    /// are in the same room" needs an answer that names the step that failed.
    LanAbandoned { reason: String },
    PathOpen {
        peer: SocketAddr,
        rounds: u32,
        proof: String,
        local: SocketAddr,
        transport: Transport,
    },
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
            Event::LocalCandidate { addr } => {
                json!({"type": "local_candidate", "addr": addr.to_string()})
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
            Event::LanAttempt { endpoint } => {
                json!({"type": "lan_attempt", "endpoint": endpoint.to_string()})
            }
            Event::LanRendezvous { offered, host_candidates } => json!({
                "type": "lan_rendezvous",
                "offered": offered.to_string(),
                "candidates": host_candidates.iter().map(|a| a.to_string()).collect::<Vec<_>>(),
            }),
            Event::LanAbandoned { reason } => {
                json!({"type": "lan_abandoned", "reason": reason})
            }
            Event::PathOpen { peer, rounds, proof, local, transport } => json!({
                "type": "path_open",
                "peer": peer.to_string(),
                "rounds": rounds,
                "proof": proof,
                "local": local.to_string(),
                // The badge reads this. It is the one field on this event a UI
                // cannot derive for itself.
                "transport": transport.as_str(),
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
    /// The host's LAN control endpoint, for stage 1 of the cascade.
    ///
    /// `host:port`, or a bare address for [`DEFAULT_CONTROL_PORT`]. `None`
    /// skips the LAN attempt entirely, which is what a caller that has never
    /// seen this host on a local network should pass — dialling a cached
    /// address from a different network is a guaranteed timeout, and the point
    /// of the cascade is to reach the relay faster, not slower.
    pub lan_endpoint: Option<String>,
    /// A WAN address the user configured by hand, offered as an extra punch
    /// candidate. See [`open_path`] for what this can and cannot do.
    pub wan_endpoint: Option<String>,
    /// Budget for the LAN dial — TCP connect plus the TLS handshake.
    ///
    /// Short on purpose. Every millisecond here is added to the time a cellular
    /// user waits for a picture, and it buys nothing on their network: a
    /// private address from outside its network fails immediately with
    /// "unreachable" or not at all.
    pub lan_timeout: Duration,
}

/// Nova's Echo control port. Matches `nova-server/src/echo/rpc.rs`.
pub const DEFAULT_CONTROL_PORT: u16 = 48011;

/// Budget for the LAN control dial when a caller does not choose one.
pub const DEFAULT_LAN_TIMEOUT: Duration = Duration::from_millis(500);

/// How long to blast on a LAN rendezvous before giving up.
///
/// Deliberately equal to the host's `wan::LAN_PUNCH_TIMEOUT`. The two sides
/// blast at each other and both must stay in it for the same window: a client
/// that gave up earlier would abandon a punch the host was still completing,
/// and one that stayed longer would sit waiting on a host that had already
/// stopped. On a LAN there is no NAT to open, so this converges in one or two
/// 25 ms rounds — anything unconfirmed at 1.5 s is blocked, not slow.
pub const LAN_PUNCH_TIMEOUT: Duration = Duration::from_millis(1500);

/// How often the control round trip is measured.
const RTT_PROBE_INTERVAL: Duration = Duration::from_secs(2);

/// Most recent and best control round-trip time, in milliseconds.
///
/// The one quantity nothing else in the client can see. Every other measurement
/// so far has been one-sided — the host times its own injection, the client
/// times its own pipeline — and neither includes the wire. A pointer that
/// trails the hand by a round trip looks exactly like a pointer delayed by
/// software, and the only way to tell them apart is to measure the wire
/// directly.
///
/// The *best* value matters as much as the last: it is the floor this path can
/// achieve, which is the number that says whether the remaining lag is
/// something to fix or something to route around.
static RTT_LAST_MS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static RTT_BEST_MS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(u32::MAX);

/// `(most recent, best seen)` control round trip in milliseconds; `0` for the
/// best when nothing has been measured yet.
pub fn rtt_stats() -> (u32, u32) {
    use std::sync::atomic::Ordering;
    let best = RTT_BEST_MS.load(Ordering::Relaxed);
    (RTT_LAST_MS.load(Ordering::Relaxed), if best == u32::MAX { 0 } else { best })
}

/// Ceiling on packets drained into one batch.
///
/// Bounds the work per iteration without bounding the *queue*: a larger burst
/// simply becomes two batches, so nothing is ever discarded.
const MAX_INPUT_BATCH: usize = 64;

/// Idle gap after which the last datagrams are repeated.
///
/// Long enough that it never fires during continuous movement (which is already
/// protected by the redundancy in each new datagram), short enough that a
/// trailing key-up is repeated well before a human notices a stuck key.
const TAIL_REPEAT_DELAY: Duration = Duration::from_millis(25);

/// How many times an idle tail is repeated.
///
/// Two extra copies spread over `TAIL_REPEAT_DELAY` each, which survives a
/// burst loss long enough to matter and then goes completely silent — a resting
/// keyboard must not keep a link busy.
const TAIL_REPEATS: usize = 2;

/// The client→host channels a session may carry.
///
/// A struct rather than two positional `Option<Receiver>` parameters, which is
/// what this started as. Two arguments of the *same type*, adjacent, both
/// optional, both meaning "a queue of bytes going to the host" is a swap
/// waiting to happen — and a swapped pair would compile, run, and deliver
/// microphone audio to `SendInput` (where the tag check would refuse it) while
/// keystrokes went to the speaker. Naming them costs one struct and makes that
/// mistake unrepresentable.
///
/// Receivers cannot be cloned, which is the other reason these live here rather
/// than in [`StreamOptions`]: they are one-shot resources, not settings, and a
/// session's input source is not something you would want copied.
#[derive(Default)]
pub struct Uplink {
    /// GameStream input packets, built by the platform layer.
    pub input: Option<tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>>,
    /// Encoded microphone packets — one Opus packet per item.
    pub mic: Option<tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>>,
    /// Where downstream game audio is delivered.
    ///
    /// In `Uplink` despite travelling the other way, because it is the same kind
    /// of thing: a one-shot resource the platform layer supplies, not a setting.
    /// Held as an `Arc` rather than moved, since the platform's audio thread
    /// polls the same handle this session arms.
    ///
    /// `None` = no audio playback (the headless CLI). The host still seals and
    /// sends; the datagrams are dropped at the demultiplexer for the cost of a
    /// channel send that goes nowhere.
    pub audio: Option<std::sync::Arc<crate::audio::AudioPlayout>>,
}

impl Uplink {
    /// Nothing travels toward the host. What the headless CLI uses: it exists
    /// to prove the media path, not to drive the host.
    pub fn none() -> Self {
        Self::default()
    }
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
    /// How this path was reached. Carried so the UI can say which route it got
    /// rather than guessing from what it asked for.
    pub transport: Transport,
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

    // ── Stage 1: the LAN ────────────────────────────────────────────────────
    //
    // Tried first because when it works it is both the fastest path and the
    // cheapest to establish: one TCP round trip and one or two punch rounds,
    // no STUN, no relay, no internet. Tried on THIS socket, because the socket
    // is the path — anything that binds a fresh one has thrown away the very
    // thing being negotiated.
    //
    // Staged rather than raced against the relay, deliberately. Racing would
    // put two concurrent writers on the host's latch cell and leave which one
    // wins to timing; the loser would still be blasting at a host that had
    // moved on. A failed LAN attempt costs `lan_timeout` plus, at worst, a
    // punch window — and on a network where the host is absent, the dial fails
    // immediately rather than timing out, because a private address off its own
    // network is unreachable rather than silent.
    if let Some(endpoint) = &opts.lan_endpoint {
        match open_lan_path(identity, opts, endpoint, &socket, progress).await {
            Ok(peer) => return Ok(OpenPath { socket, peer, transport: Transport::Lan }),
            // Never fatal. Every reason to abandon the LAN is a reason the relay
            // exists, and the relay is the next thing this function does.
            Err(reason) => progress.event(Event::LanAbandoned { reason }),
        }
    }

    // ── Stages 2 and 3 both need the relay ──────────────────────────────────
    //
    // Checked before STUN rather than after, so a LAN-only host — one paired
    // over the local network with no `[echo.signaling]` configured at all —
    // fails with the reason a user can act on instead of a relay-URL parse
    // error thirty lines later. That configuration is now legitimate rather
    // than broken: stage 1 needs no relay, no STUN and no internet, so a host
    // that answers on the LAN streams with nothing else set up.
    if opts.relay_url.trim().is_empty() || opts.relay_pin.trim().is_empty() {
        return Err(match &opts.lan_endpoint {
            Some(endpoint) => format!(
                "{endpoint} did not answer and no relay is configured for this host — either \
                 bring the host onto this network, or set a relay so it can be reached from \
                 anywhere"
            ),
            None => "no relay is configured for this host and no LAN address is known for it — \
                     there is no route to try"
                .into(),
        });
    }

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
    let mut conn = RelayConnection::connect(&target, tls).await.map_err(|e| {
        // A relay on a private address is reachable from the host's own network
        // and nowhere else, so this is the failure every cellular connect
        // produces on a LAN-only deployment — and "connect to relay: connection
        // timed out" sends the reader looking at the phone, the app and the
        // cascade before they look at the URL. Naming it converts the whole
        // investigation into one sentence.
        //
        // Detected from the URL rather than from our own connectivity: we
        // cannot know what this network can reach, but we can know that
        // 10.x/192.168.x/127.x is never routable from outside the network that
        // owns it.
        let private_relay = target
            .host
            .parse::<std::net::IpAddr>()
            .map(|ip| is_private_addr(&SocketAddr::new(ip, 0)))
            .unwrap_or(false);
        if private_relay {
            format!(
                "the relay for this host is {}, a private address — it can only be reached \
                 from the host's own network, so there is no route to it from here. Point \
                 [echo.signaling] at a relay with a public address (or a forwarded port) to \
                 stream from anywhere. ({e})",
                target.host
            )
        } else {
            format!("connect to relay: {e}")
        }
    })?;
    progress.event(Event::RelayConnected { authority: target.authority().to_string() });

    let mut params = identity_params(identity);
    params.insert("host".into(), json!(opts.host_fingerprint));
    let result = conn.call("lookup", params).await.map_err(|e| format!("relay lookup: {e}"))?;
    let mut host_candidates = parse_candidates(result.get("candidates"));
    if host_candidates.is_empty() {
        return Err(format!(
            "the relay knows no candidates for host {}… — is Nova running with \
             [echo.signaling] configured against this relay?",
            short(&opts.host_fingerprint)
        ));
    }
    progress.event(Event::HostCandidates { addrs: host_candidates.clone() });

    // Offer a host candidate alongside the reflexive ones. Without it, a peer
    // on the *same* network as us can only be reached via our shared public
    // address, which needs NAT hairpinning — and "phone on the same Wi-Fi as
    // the PC" is the most common way this client gets used, so that would make
    // the commonest case the one that depends on router behaviour nobody
    // controls. See `stun::local_host_candidate`.
    let mut offered = candidates_json(&mine);
    if let Some(local) = stun::local_host_candidate(local.port()) {
        if !mine.iter().any(|c| c.mapped == local) {
            progress.event(Event::LocalCandidate { addr: local });
            offered.push(json!({ "addr": local.to_string(), "via": "host" }));
        }
    }

    let mut params = identity_params(identity);
    params.insert("host".into(), json!(opts.host_fingerprint));
    params.insert("candidates".into(), Value::Array(offered));
    conn.call("offer", params).await.map_err(|e| format!("relay offer: {e}"))?;
    progress.event(Event::Offered);

    // ── Stage 3: the manually configured WAN endpoint ───────────────────────
    //
    // Added as an EXTRA CANDIDATE to the relay-mediated punch rather than as a
    // standalone dial, and that is a limitation of the host, not a shortcut
    // here. The host latches a peer only in the arm that runs when it has been
    // *told* to punch — by a relay offer or a LAN rendezvous. An unsolicited
    // probe is answered (a cooperative obligation: our reply completes the
    // peer's side) but never latched, and `start_session` refuses with
    // `no_path` when nothing is latched. So a client that dialled a manual
    // endpoint with no signalling would see its punch succeed and the session
    // refused immediately afterwards — the worst shape of failure, one that
    // looks like it worked.
    //
    // Offered here, it is real: the relay offer authorises the host to blast,
    // and if the direct address is the one that answers, media flows straight
    // to it and never touches a relay-discovered candidate. That is worth
    // having on a host behind a port forward whose reflexive candidate is
    // wrong. See the handoff for what a standalone direct dial would need.
    let direct = opts.wan_endpoint.as_deref().and_then(|text| {
        // A bare address is completed with the port the relay says the host's
        // media socket is on. Guessing a port would be worse than useless: the
        // punch would blast at something that is not listening and the failure
        // would read as "the host is unreachable".
        let fallback_port = host_candidates.first().map(SocketAddr::port);
        match parse_endpoint_with(text, fallback_port) {
            Some(addr) => Some(addr),
            None => {
                progress.event(Event::Warning {
                    message: format!(
                        "manual WAN endpoint \"{text}\" has no port and the relay named no \
                         candidate to borrow one from — ignoring it"
                    ),
                });
                None
            }
        }
    });
    if let Some(addr) = direct {
        if !host_candidates.contains(&addr) {
            host_candidates.push(addr);
        }
    }

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
            // Classified from the address that answered, not from the branch
            // that got here. A relay-signalled punch that landed on a private
            // address IS a LAN path — same subnet, no internet — and calling it
            // WAN would mislabel the fastest route Echo has.
            let transport = if is_private_addr(&result.peer) {
                Transport::Lan
            } else if Some(result.peer) == direct {
                Transport::DirectWan
            } else {
                Transport::WanPunch
            };
            progress.event(Event::PathOpen {
                peer: result.peer,
                rounds: result.rounds,
                proof: format!("{:?}", result.proof),
                local,
                transport,
            });
            Ok(OpenPath { socket, peer: result.peer, transport })
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

/// Stage 1: trade candidates over the host's LAN control port and punch.
///
/// Returns the peer the punch latched on, or the reason this route was
/// abandoned. **Every error here is ordinary.** Being on a different network is
/// the common case, not a fault, so nothing in this function is worth failing a
/// session over — the caller reports the reason and moves to the relay.
///
/// ## A rendezvous, not a transport
///
/// The TCP channel exchanges candidates and is then dropped. It carries no
/// control traffic and no session's liveness depends on it, which is
/// load-bearing rather than tidy: the host's detach/sweep model is bound to the
/// punched tunnel — `transport.rs`'s `release_session_of` fires when *that*
/// closes — and this port has no equivalent. A second long-lived control
/// transport would need every one of those invariants re-derived for it.
///
/// Everything downstream is unchanged. This function's only product is a peer
/// address, exactly like the relay path's, so `stream()` cannot tell which
/// route produced it.
async fn open_lan_path(
    identity: &Identity,
    opts: &ConnectOptions,
    endpoint: &str,
    socket: &tokio::net::UdpSocket,
    progress: &mut impl Progress,
) -> Result<SocketAddr, String> {
    let addr = tokio::time::timeout(
        opts.lan_timeout,
        resolve_endpoint(endpoint, DEFAULT_CONTROL_PORT),
    )
    .await
    .map_err(|_| format!("looking up \"{endpoint}\" took longer than {:?}", opts.lan_timeout))?
    .ok_or_else(|| format!("\"{endpoint}\" is not an address this client can dial"))?;
    progress.event(Event::LanAttempt { endpoint: addr });

    let pin = parse_fingerprint(&opts.host_fingerprint)
        .map_err(|e| format!("host fingerprint: {e}"))?;

    // One budget over connect AND handshake. Splitting them would let a host
    // that accepts TCP and then stalls in TLS — a half-open connection through
    // a firewall, a machine mid-suspend — hold the whole cascade open for as
    // long as the handshake felt like taking.
    let mut channel =
        tokio::time::timeout(opts.lan_timeout, ControlChannel::connect_lan(addr, identity, pin))
            .await
            .map_err(|_| format!("{addr} did not answer within {:?}", opts.lan_timeout))??;

    // The address the HOST sees this connection coming from. The host refuses
    // any candidate whose IP does not match it — that check is what stops an
    // authenticated client from pointing the host's blast at a third party —
    // so offering anything else is offering something guaranteed to be refused.
    let source = channel
        .local_addr()
        .ok_or_else(|| "the LAN channel reported no local address".to_string())?;
    let media_port = socket
        .local_addr()
        .map_err(|e| format!("media socket address: {e}"))?
        .port();
    let offered = SocketAddr::new(source.ip(), media_port);

    let mut params = serde_json::Map::new();
    params.insert("candidates".into(), json!([offered.to_string()]));
    let result = channel
        .call("lan_rendezvous", params)
        .await
        .map_err(|e| format!("lan_rendezvous: {e}"))?;

    // Plain strings here, not the relay's `{"addr": …}` objects — a different
    // wire shape for a different protocol, so `relay::parse_candidates` does
    // not apply and silently returns nothing if used.
    let host_candidates: Vec<SocketAddr> = result
        .get("candidates")
        .and_then(Value::as_array)
        .map(|list| list.iter().filter_map(|c| c.as_str()?.parse().ok()).collect())
        .unwrap_or_default();
    if host_candidates.is_empty() {
        return Err("the host accepted the rendezvous but named no candidate".into());
    }
    progress.event(Event::LanRendezvous { offered, host_candidates: host_candidates.clone() });

    // Done with TCP. The host is already blasting — `punch_toward` is
    // fire-and-forget into the gatherer task, and does not depend on this
    // connection staying up.
    drop(channel);

    let mut io = punch::UdpPunchIo::new(socket);
    let cfg = punch::PunchConfig { timeout: LAN_PUNCH_TIMEOUT, ..Default::default() };
    let result = punch::punch_io(&mut io, &host_candidates, cfg)
        .await
        .map_err(|e| format!("LAN punch: {e}"))?;
    progress.event(Event::PathOpen {
        peer: result.peer,
        rounds: result.rounds,
        proof: format!("{:?}", result.proof),
        local: SocketAddr::new(source.ip(), media_port),
        transport: Transport::Lan,
    });
    Ok(result.peer)
}

/// Parse `host:port`, or `host` with a default port, resolving a name if it is
/// one.
///
/// Literals are handled without touching the resolver, which matters on a phone:
/// the common input is an IPv4 literal from the host list, and a DNS round trip
/// for it would be latency spent to learn what was already known.
async fn resolve_endpoint(text: &str, default_port: u16) -> Option<SocketAddr> {
    let text = text.trim();
    if let Some(addr) = parse_endpoint_with(text, Some(default_port)) {
        return Some(addr);
    }
    // A name. `lookup_host` needs a port in the string to answer at all.
    let with_port =
        if text.contains(':') { text.to_string() } else { format!("{text}:{default_port}") };
    tokio::net::lookup_host(with_port).await.ok()?.next()
}

/// Parse an address literal, supplying `fallback_port` when the text has none.
///
/// Returns `None` for a portless address with no fallback — the caller must
/// decide what to do about that, because inventing a port produces a punch at
/// something that is not listening and a failure that reads as "host
/// unreachable".
fn parse_endpoint_with(text: &str, fallback_port: Option<u16>) -> Option<SocketAddr> {
    let text = text.trim();
    if let Ok(addr) = text.parse::<SocketAddr>() {
        return Some(addr);
    }
    // A bare IP. Bracketed IPv6 without a port (`[::1]`) is accepted too, since
    // that is how it is written everywhere else.
    let bare = text.strip_prefix('[').and_then(|t| t.strip_suffix(']')).unwrap_or(text);
    let ip: std::net::IpAddr = bare.parse().ok()?;
    Some(SocketAddr::new(ip, fallback_port?))
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
    uplink: Uplink,
) -> Result<ReceiveStats, String> {
    let pin = parse_fingerprint(host_fingerprint).map_err(|e| format!("host fingerprint: {e}"))?;
    let OpenPath { socket, peer, .. } = path;
    let socket = Arc::new(socket);

    // The socket must have exactly one reader, so demultiplexing starts before
    // anything else — the TLS handshake below needs its datagrams delivered,
    // and they arrive here.
    let (media_tx, media_rx) = tokio::sync::mpsc::unbounded_channel();
    let (control_tx, control_rx) = tokio::sync::mpsc::unbounded_channel();
    let (audio_tx, audio_rx) = tokio::sync::mpsc::unbounded_channel();
    let demux_task = tokio::spawn({
        let socket = socket.clone();
        let stop_rx = stop.clone();
        async move {
            let _ = receiver::demultiplex(&socket, peer, media_tx, control_tx, audio_tx, stop_rx)
                .await;
        }
    });

    // Any early return past this point must not leak the demultiplexer, so the
    // body runs in a helper and the abort happens exactly once, below.
    let outcome = stream_inner(
        identity, pin, &socket, peer, opts, sink, progress, stop, media_rx, control_rx, audio_rx,
        uplink,
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
    audio_rx: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    uplink: Uplink,
) -> Result<ReceiveStats, String> {
    let Uplink { input: input_rx, mic: mic_rx, audio: playout } = uplink;
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

    // Downstream game audio gets its OWN task, off the video path entirely.
    //
    // Not merged into the receive loop, and not for tidiness: that loop is the
    // one carrying every video frame, and a decode-and-schedule job running 50
    // times a second inside it would make audio's cost part of video's latency
    // budget. That is precisely the coupling that made input feel broken for a
    // whole session upstream, and the rule earned there — a real-time path gets
    // its own drain — applies unchanged here.
    //
    // Armed with the session keys only now, after the grant: the buffer cannot
    // open anything before them, and arming earlier would mean holding a
    // half-built session the platform's audio thread could already be polling.
    let audio_task = playout.as_ref().map(|playout| {
        playout.arm(keys.clone());
        let playout = playout.clone();
        let mut audio_rx = audio_rx;
        tokio::spawn(async move {
            // Rate-limited: a foreign key or a corrupted path fails on every
            // packet, 50 times a second. The first line says what is wrong; the
            // next thousand would only make the log useless.
            let mut last_notice: Option<std::time::Instant> = None;
            let mut refused = 0u64;
            while let Some(datagram) = audio_rx.recv().await {
                if let Err(why) = playout.accept(&datagram) {
                    refused += 1;
                    let due = last_notice
                        .is_none_or(|t| t.elapsed() > std::time::Duration::from_secs(10));
                    if due {
                        eprintln!("⚠️  game audio datagram refused ({refused} so far): {why}");
                        last_notice = Some(std::time::Instant::now());
                    }
                }
            }
        })
    });

    // The sink's repair path. Nova's GOP is infinite, so a sink that loses its
    // reference chain recovers only by asking — see `FrameSink::
    // take_keyframe_request`. The channel exists so the request crosses from
    // the receive loop to the control channel without either owning the other.
    let (idr_tx, mut idr_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let ctl = std::sync::Arc::new(tokio::sync::Mutex::new(ctl));
    let idr_task = tokio::spawn({
        let ctl = ctl.clone();
        async move {
            while idr_rx.recv().await.is_some() {
                // Best-effort: a failed request is retried by the next drop,
                // and a lost session is about to end the loop anyway.
                let _ = ctl
                    .lock()
                    .await
                    .call("request_idr", serde_json::Map::new())
                    .await;
            }
        }
    });

    // Measure the wire. `get_status` is the cheapest command the host answers
    // and it touches no session state, so timing it is a clean round trip over
    // the same punched path the input datagrams take.
    let rtt_task = tokio::spawn({
        let ctl = ctl.clone();
        async move {
            loop {
                tokio::time::sleep(RTT_PROBE_INTERVAL).await;
                let began = std::time::Instant::now();
                let ok = ctl.lock().await.call("get_status", serde_json::Map::new()).await.is_ok();
                if !ok {
                    return; // the session is ending; the receive loop reports it
                }
                let ms = began.elapsed().as_millis().min(u32::MAX as u128) as u32;
                RTT_LAST_MS.store(ms, std::sync::atomic::Ordering::Relaxed);
                RTT_BEST_MS.fetch_min(ms, std::sync::atomic::Ordering::Relaxed);

                // Send the client's own view of the session to the host, so it
                // lands in the host log next to the host's measurements. Both
                // halves of every question so far have lived on opposite
                // machines, and correlating them meant asking the user to read
                // numbers off a phone. A notification, so a failed report never
                // delays input or blocks the session.
                let (batch, batch_worst) = crate::input::batch_stats();
                let (peak_rate, peak_samples, capture) = crate::input::ui_state();
                let mut params = serde_json::Map::new();
                params.insert("rtt_ms".into(), ms.into());
                params.insert("rtt_best_ms".into(), rtt_stats().1.into());
                params.insert("input_batch".into(), batch.into());
                params.insert("input_batch_worst".into(), batch_worst.into());
                params.insert("peak_events_per_sec".into(), peak_rate.into());
                params.insert("peak_samples_per_event".into(), peak_samples.into());
                params.insert("capture_held".into(), capture.into());
                // The client's half of the microphone measurement. Without it a
                // silent host is ambiguous between "the encoder produced
                // nothing" and "the path lost everything", and only this side
                // can tell those apart — see `crate::mic`.
                let mic = crate::mic::stats();
                params.insert("mic_packets".into(), mic.packets.into());
                params.insert("mic_bytes".into(), mic.bytes.into());
                params.insert("mic_refused".into(), mic.refused.into());
                params.insert("mic_worst_gap_ms".into(), mic.worst_gap_ms.into());
                let _ = ctl.lock().await.notify("client_stats", params).await;
            }
        }
    });

    // Input goes out on its own unreliable datagrams, straight onto the punched
    // socket — NOT through `ctl`.
    //
    // The control channel is reliable and ordered, and both properties are
    // actively harmful here. Its window of eight unacknowledged messages is a
    // rate ceiling a mouse exceeds, so the surplus queued and drained after the
    // user stopped moving; and ordering means one lost datagram stalls every
    // input behind it for a retransmit timeout. Live 2026-08-16 that read as a
    // pointer that "drags heavily" even with the mouse's polling rate turned
    // down. `nova_core::input_channel` gives up both guarantees and buys the
    // one thing input actually needs — a lost key-up must not strand a key —
    // with redundancy instead of acknowledgement.
    let input_task = input_rx.map(|mut rx| {
        let socket = socket.clone();
        let mut sender = nova_core::input_channel::InputSender::new(keys.clone());
        tokio::spawn(async move {
            // The datagrams most recently sent, kept so the tail of a burst can
            // be repeated. Redundancy only protects a packet while *later*
            // datagrams still carry it, which leaves the last one of a burst
            // unprotected — and the last packet of a burst is exactly the
            // key-up, the button-release, or the release-all sent as the app is
            // backgrounded. Repeating the tail while idle closes that hole for
            // the price of a few bytes on a link that has gone quiet anyway.
            let mut tail: Vec<Vec<u8>> = Vec::new();
            let mut repeats_left = 0usize;

            loop {
                let next = if repeats_left > 0 {
                    match tokio::time::timeout(TAIL_REPEAT_DELAY, rx.recv()).await {
                        Ok(received) => received,
                        Err(_) => {
                            for datagram in &tail {
                                let _ = socket.send_to(datagram, peer).await;
                            }
                            repeats_left -= 1;
                            continue;
                        }
                    }
                } else {
                    rx.recv().await
                };
                let Some(first) = next else { return };
                // No delay before draining: this loop is **self-clocking**.
                //
                // It sends immediately, and whatever arrives while that send is
                // in flight is drained and coalesced into the next one. Under a
                // light load that means one datagram per event with no added
                // latency; under a heavy one the batches grow by themselves and
                // consecutive deltas merge. The rate limit emerges from how fast
                // the loop can actually run rather than from a constant.
                //
                // There *was* a fixed window here, and it was justified when
                // input rode the reliable control channel, whose eight-message
                // send window made a burst fatal. On unreliable datagrams the
                // cost of sending eagerly is about 83 bytes per event against a
                // 20 Mbps video stream, and the cost of waiting is latency on
                // every single movement. A second rate cap on top of Android's
                // delivery rate bought nothing and hid how slow that rate was.
                let mut batch = vec![first];
                while let Ok(next) = rx.try_recv() {
                    batch.push(next);
                    if batch.len() >= MAX_INPUT_BATCH {
                        break;
                    }
                }
                crate::input::record_batch(batch.len());
                let batch = crate::input::coalesce(batch);
                if batch.is_empty() {
                    continue;
                }

                let datagrams = match sender.datagrams(batch) {
                    Ok(d) => d,
                    // Only reachable if the 32-bit sequence space is exhausted,
                    // which would take over a year in one session. Continuing
                    // would mean repeating a GCM nonce, so input stops instead —
                    // the stream itself is unaffected.
                    Err(e) => {
                        eprintln!("⚠️  input channel stopped: {e}");
                        return;
                    }
                };
                for datagram in &datagrams {
                    // Best-effort by design: there is no acknowledgement to wait
                    // for and nothing useful to do about a failed send. A send
                    // error here means the socket is gone, which the receive
                    // loop is about to report properly.
                    if socket.send_to(datagram, peer).await.is_err() {
                        return;
                    }
                }
                tail = datagrams;
                repeats_left = TAIL_REPEATS;
            }
        })
    });

    // The microphone goes out on its own unreliable datagrams, on the same
    // punched socket, sealed under the same session key but a different stream
    // id — so a captured mic datagram cannot be replayed into the input path.
    //
    // Note what this loop deliberately does *not* do, both of which the input
    // task above does:
    //
    // - **No coalescing.** Batching would add the batch's own duration to every
    //   packet in it, and a microphone's entire budget is latency. Input can
    //   coalesce because merging two pointer deltas loses nothing; merging two
    //   20 ms audio frames delays the first one by 20 ms.
    // - **No tail repeat.** The input task repeats its last datagrams because
    //   the final packet of a burst is typically a key-up, and losing it strands
    //   a key held down on the host. Audio has no such packet: the last frame of
    //   a sentence is just the end of the sentence, and repeating it would cost
    //   uplink bandwidth to re-send speech the listener has already heard.
    crate::mic::reset();
    let mic_task = mic_rx.map(|mut rx| {
        let socket = socket.clone();
        let mut sender = nova_core::mic_channel::MicSender::new(keys.clone());
        tokio::spawn(async move {
            while let Some(packet) = rx.recv().await {
                let datagram = match sender.datagram(&packet) {
                    Ok(d) => d,
                    Err(nova_core::mic_channel::MicError::Exhausted) => {
                        // Only reachable after years in one session. Continuing
                        // would repeat a GCM nonce, so the microphone stops
                        // while the stream itself carries on.
                        eprintln!("⚠️  microphone channel stopped: sequence space exhausted");
                        return;
                    }
                    Err(e) => {
                        // A payload the channel will not carry: an encoder bug,
                        // not a network condition. Skip this packet and keep
                        // going — one malformed frame must not end the call.
                        crate::mic::record_refused();
                        eprintln!("⚠️  microphone packet dropped: {e}");
                        continue;
                    }
                };
                // Best-effort, like input: there is no acknowledgement to wait
                // for, and a send error means the socket is gone — which the
                // receive loop is about to report properly.
                if socket.send_to(&datagram, peer).await.is_err() {
                    return;
                }
                crate::mic::record_sent(packet.len());
            }
        })
    });

    let stats = receiver::run_receiver(socket, peer, media_rx, Some(keys), sink, stop, Some(idr_tx))
        .await
        .map_err(|e| format!("receive loop: {e}"))?;

    // Dropped senders end the task; abort covers a request in flight.
    idr_task.abort();
    rtt_task.abort();
    if let Some(t) = input_task {
        t.abort();
    }
    if let Some(t) = mic_task {
        t.abort();
    }
    if let Some(t) = audio_task {
        t.abort();
    }
    // Stop opening datagrams under keys that no longer describe a live session.
    // Safe to skip on the error path above: `arm` replaces the buffer outright,
    // so the next session cannot inherit this one's playout point.
    if let Some(p) = &playout {
        p.disarm();
    }

    // Always tell the host we are done. A session left open would block the
    // next client — including a Moonlight one, which is exactly the asymmetry
    // the host's gate exists to prevent.
    if let Err(e) = ctl.lock().await.call("stop_session", serde_json::Map::new()).await {
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

    // ── The transport cascade ───────────────────────────────────────────────

    #[test]
    fn the_transport_reaches_kotlin_on_the_path_open_event() {
        // The badge is driven by this string. A rename here is a UI change.
        let ev = Event::PathOpen {
            peer: "203.0.113.7:47998".parse().unwrap(),
            rounds: 2,
            proof: "RoundTrip".into(),
            local: "192.168.1.9:51000".parse().unwrap(),
            transport: Transport::DirectWan,
        };
        assert_eq!(ev.to_json()["transport"], json!("direct_wan"));
        assert_eq!(Transport::Lan.as_str(), "lan");
        assert_eq!(Transport::WanPunch.as_str(), "wan_punch");
    }

    #[test]
    fn a_private_peer_is_a_lan_path_however_it_was_signalled() {
        // The classification that matters most, and the one most easily got
        // wrong: the relay is a SIGNALLING channel, so a relay-mediated punch
        // that landed on a private address is carrying media on the local
        // segment. Calling it WAN would mislabel the fastest path Echo has.
        for addr in ["192.168.1.50:47998", "10.0.0.205:47998", "172.16.4.4:1", "127.0.0.1:9"] {
            assert!(is_private_addr(&addr.parse().unwrap()), "{addr} is private");
        }
        for addr in ["203.0.113.7:47998", "8.8.8.8:53", "172.32.0.1:1"] {
            assert!(!is_private_addr(&addr.parse().unwrap()), "{addr} is public");
        }
    }

    #[test]
    fn an_endpoint_may_omit_its_port_only_when_a_fallback_exists() {
        // A bare WAN address borrows the port the relay named for the host.
        // Without one there is nothing to borrow, and inventing a port would
        // blast at something that is not listening — a failure that reads as
        // "the host is unreachable" and sends the search in the wrong
        // direction entirely.
        assert_eq!(
            parse_endpoint_with("10.0.0.205", Some(48011)),
            Some("10.0.0.205:48011".parse().unwrap())
        );
        assert_eq!(
            parse_endpoint_with("10.0.0.205:9999", Some(48011)),
            Some("10.0.0.205:9999".parse().unwrap()),
            "an explicit port always wins over the fallback"
        );
        assert_eq!(parse_endpoint_with("203.0.113.7", None), None);
        assert_eq!(parse_endpoint_with("  10.0.0.205  ", Some(1)), Some("10.0.0.205:1".parse().unwrap()));
        assert_eq!(parse_endpoint_with("not-an-address", Some(1)), None, "a name is not a literal");
    }

    #[test]
    fn ipv6_literals_survive_both_spellings() {
        assert_eq!(parse_endpoint_with("[::1]:48011", None), Some("[::1]:48011".parse().unwrap()));
        assert_eq!(parse_endpoint_with("[::1]", Some(48011)), Some("[::1]:48011".parse().unwrap()));
        assert_eq!(parse_endpoint_with("::1", Some(48011)), Some("[::1]:48011".parse().unwrap()));
    }

    #[tokio::test]
    async fn a_literal_endpoint_resolves_without_touching_the_resolver() {
        // Not a performance nicety: this runs on a phone, where the common
        // input is an IPv4 literal from the host list and a DNS round trip for
        // it would be latency spent to learn what was already known.
        assert_eq!(
            resolve_endpoint("10.0.0.205", 48011).await,
            Some("10.0.0.205:48011".parse().unwrap())
        );
    }
}

/// End whatever session the host is holding for this device, **without starting
/// one**.
///
/// ## Why this exists as its own path
///
/// A session outlives the app that started it. If the app is swiped away, killed
/// or loses the network, it never sends `stop_session`, so the host detaches and
/// holds the virtual display for the grace period — which is the point of
/// detaching, but leaves the operator's monitors rearranged with no client left
/// to ask for them back.
///
/// The first attempt at fixing that drove [`stream`] and stopped it the instant
/// the grant arrived. It worked roughly one press in three, and the log says
/// exactly why: reclaim, start session N+1, then silence — the teardown raced
/// the session it had just created, and when it lost, the host learned nothing
/// and the newly started session simply detached again. Pressing the button
/// repeatedly walked the session id up one at a time (live 2026-08-17, four
/// presses for two successes).
///
/// `stop_session` is an RPC on the control tunnel. It needs an authenticated
/// channel and nothing else: no media socket, no keys, no grant. So this stops
/// after `hello` and asks. One press, one round trip, no session churn, nothing
/// to race.
pub async fn release(
    identity: &Identity,
    host_fingerprint: &str,
    path: OpenPath,
    progress: &mut impl Progress,
) -> Result<(), String> {
    let pin = parse_fingerprint(host_fingerprint).map_err(|e| format!("host fingerprint: {e}"))?;
    let OpenPath { socket, peer, .. } = path;
    let socket = Arc::new(socket);

    // Same rule as `stream`: the socket must have exactly one reader, and the
    // TLS handshake below needs its datagrams delivered. Media and audio are
    // demultiplexed into channels nothing drains — correct here, because a
    // release never asks for a session and the host therefore never sends any.
    let (media_tx, _media_rx) = tokio::sync::mpsc::unbounded_channel();
    let (control_tx, control_rx) = tokio::sync::mpsc::unbounded_channel();
    let (audio_tx, _audio_rx) = tokio::sync::mpsc::unbounded_channel();
    let (_stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let demux_task = tokio::spawn({
        let socket = socket.clone();
        async move {
            let _ = receiver::demultiplex(&socket, peer, media_tx, control_tx, audio_tx, stop_rx)
                .await;
        }
    });

    let outcome = release_inner(identity, pin, &socket, peer, control_rx, progress).await;
    demux_task.abort();
    outcome
}

async fn release_inner(
    identity: &Identity,
    pin: [u8; 32],
    socket: &Arc<tokio::net::UdpSocket>,
    peer: SocketAddr,
    control_rx: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    progress: &mut impl Progress,
) -> Result<(), String> {
    progress.event(Event::ControlOpening { peer, lan: None });
    let mut ctl = ControlChannel::connect_wan(socket.clone(), peer, control_rx, identity, pin)
        .await
        .map_err(|e| format!("control tunnel: {e}"))?;
    progress.event(Event::ControlAuthenticated);

    // `hello` first, for the same reason `stream` does it: it is what proves the
    // host speaks a protocol this client understands, and its answer names the
    // device the host thinks we are — worth having in the log when a release is
    // refused as "not the owner".
    let hello = ctl.call("hello", serde_json::Map::new()).await?;
    progress.event(Event::Hello {
        server: control::field_str(&hello, "server").unwrap_or("nova").to_string(),
        protocol_version: control::field_u64(&hello, "protocol_version").unwrap_or(0),
        device_name: control::field_str(&hello, "device_name").unwrap_or("?").to_string(),
    });

    // The host answers this even when it holds nothing — "ended something that
    // was already ended" is a satisfied request, not an error — so a release
    // sent at the wrong moment is harmless rather than confusing.
    ctl.call("stop_session", serde_json::Map::new())
        .await
        .map(|_| ())
        .map_err(|e| format!("stop_session: {e}"))
}

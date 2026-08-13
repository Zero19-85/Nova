//! Echo — Nova's native client. Headless CLI iteration.
//!
//! No video decoding, no UI, no platform integration: this build exists to
//! prove the **connection path** end to end, which is the part that cannot be
//! developed against a mock. Everything it uses lives in `nova-core`, the
//! crate that will compile for Xbox and Android later, so none of this work is
//! throwaway.
//!
//! ## Identity — a correction worth stating plainly
//!
//! Echo does **not** read `nova_paired.json`. That file is the *host's* trust
//! store: it maps a client certificate fingerprint to a device record, and it
//! contains other devices' public certificates and no private keys at all.
//! Reading it would give Echo nothing it could authenticate with.
//!
//! Instead Echo has its own identity, generated on first run and persisted
//! (`echo.cert.der` / `echo.key.der`), exactly as a Moonlight client does. The
//! host learns to trust it through the ordinary pairing flow — that is what
//! puts Echo's fingerprint into `nova_paired.json`. Until that pairing
//! happens, Echo can talk to the relay (which accepts any certificate) but
//! Nova will refuse it, which is the correct behaviour.
//!
//! ## The connection sequence
//!
//! 1. **Gather** — discover our own public address by STUN, from the socket
//!    that will carry traffic.
//! 2. **Look up** — ask the relay for the host's candidates.
//! 3. **Offer** — publish ours so the host learns where to punch toward.
//! 4. **Punch** — both sides send simultaneously until a path opens.
//!
//! Steps 2 and 3 are one relay round trip each; step 4 is where NATs are
//! actually traversed.
//!
//! `stream` continues past step 4 into the media handoff: it asks the host for
//! a session over the control channel, receives the media keys, and runs the
//! receive loop. There is still no decoder — frames are authenticated,
//! reassembled, and counted — so what this build proves is that media *flows
//! and is genuinely ours*, which is the property a decoder would otherwise
//! hide behind a picture.

mod control;
mod receiver;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand};
use nova_core::identity::{client_config_pinned, parse_fingerprint, Identity};
use nova_core::media_crypto::SessionKeys;
use nova_core::relay::{
    candidates_json, identity_params, parse_candidates, RelayConnection, RelayTarget,
};
use nova_core::stun::{self, MappingBehavior};
use nova_core::punch;
use serde_json::{json, Value};

#[derive(Parser, Debug)]
#[command(name = "echo", about = "Echo — Nova's native client (headless)")]
struct Args {
    /// Directory holding Echo's identity (created on first run).
    #[arg(long, default_value = ".", global = true)]
    data_dir: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Print this client's certificate fingerprint — the value Nova must
    /// trust before it will accept a connection.
    Id,

    /// Discover this machine's public address and NAT behaviour.
    Probe,

    /// Connect to a Nova host through the signaling relay and punch a path.
    Connect {
        /// Relay URL, e.g. https://relay.example.com:8443/v1/signal
        #[arg(long)]
        relay: String,
        /// SHA-256 of the relay's TLS certificate (printed by nova-relay).
        #[arg(long)]
        relay_pin: String,
        /// Fingerprint of the Nova host to reach.
        #[arg(long)]
        host: String,
        /// Seconds to keep punching before giving up.
        #[arg(long, default_value_t = 8)]
        punch_secs: u64,
    },

    /// Punch a path, ask the host for a session, and receive media.
    ///
    /// Everything rides the punched UDP socket: mutual TLS over a reliable
    /// control channel, and the media stream, on one NAT mapping. No port
    /// forwarding and no trusted relay.
    Stream {
        #[arg(long)]
        relay: String,
        #[arg(long)]
        relay_pin: String,
        /// Fingerprint of the Nova host — its relay identity, its pairing
        /// certificate, and the pin for the control channel are all this value.
        #[arg(long)]
        host: String,
        /// Debugging only: use the host's LAN TCP control port (`host:48011`)
        /// instead of the punched tunnel. The host refuses that port from
        /// non-private addresses, so this is not a WAN fallback.
        #[arg(long)]
        control: Option<String>,
        #[arg(long, default_value_t = 8)]
        punch_secs: u64,
        /// Seconds of media to receive before stopping. 0 = until Ctrl-C.
        #[arg(long, default_value_t = 30)]
        seconds: u64,
        /// Requested resolution ("1080p", "1920x1080").
        #[arg(long, default_value = "1080p")]
        res: String,
        #[arg(long, default_value_t = 60)]
        fps: u32,
        #[arg(long, default_value = "hevc")]
        codec: String,
        #[arg(long, default_value_t = 20000)]
        bitrate_kbps: u32,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let identity = Identity::load_or_create(&args.data_dir, "echo", "echo-client")
        .map_err(|e| format!("identity: {e}"))?;

    match args.command {
        Command::Id => {
            println!("{}", identity.fingerprint);
            println!();
            println!("Pair this fingerprint with Nova before connecting — until then the host");
            println!("will refuse Echo, which is the trust model working as intended.");
        }

        Command::Probe => {
            let socket = tokio::net::UdpSocket::bind("0.0.0.0:0").await?;
            println!("🔌 Local socket: {}", socket.local_addr()?);
            let servers = resolve_default_stun().await;
            if servers.is_empty() {
                return Err("no STUN server resolved — check DNS/connectivity".into());
            }
            let (behavior, candidates) = stun::classify_mapping(&socket, &servers).await;
            for c in &candidates {
                println!("🌐 Public address {} (via {})", c.mapped, c.via);
            }
            report_behavior(behavior);
        }

        Command::Connect { relay, relay_pin, host, punch_secs } => {
            open_path(&identity, &relay, &relay_pin, &host, Duration::from_secs(punch_secs)).await?;
            println!();
            println!("   Media would flow here next; `stream` continues past this point.");
        }

        Command::Stream {
            relay,
            relay_pin,
            host,
            control,
            punch_secs,
            seconds,
            res,
            fps,
            codec,
            bitrate_kbps,
        } => {
            let (socket, peer) = open_path(
                &identity,
                &relay,
                &relay_pin,
                &host,
                Duration::from_secs(punch_secs),
            )
            .await?;
            stream(
                &identity,
                &host,
                socket,
                peer,
                StreamOptions { seconds, res, fps, codec, bitrate_kbps, control },
            )
            .await?;
        }
    }
    Ok(())
}

struct StreamOptions {
    seconds: u64,
    res: String,
    fps: u32,
    codec: String,
    bitrate_kbps: u32,
    /// `Some` = debug over the LAN TCP port instead of the punched tunnel.
    control: Option<String>,
}

/// Ask the host to start a session, then receive it.
///
/// The order matters and is not interchangeable: the path is punched **before**
/// the session is requested, because the host will only grant a session over a
/// path it has already confirmed (see `echo::session`'s `NoPathLatched`). A
/// client that asked first and punched afterwards would be refused every time.
async fn stream(
    identity: &Identity,
    host_fingerprint: &str,
    socket: tokio::net::UdpSocket,
    peer: SocketAddr,
    opts: StreamOptions,
) -> Result<(), Box<dyn std::error::Error>> {
    let pin = parse_fingerprint(host_fingerprint)?;
    let socket = Arc::new(socket);

    // The socket must have exactly one reader, so demultiplexing starts before
    // anything else — the TLS handshake below needs its datagrams delivered,
    // and they arrive here.
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let (media_tx, media_rx) = tokio::sync::mpsc::unbounded_channel();
    let (control_tx, control_rx) = tokio::sync::mpsc::unbounded_channel();
    let demux_task = tokio::spawn({
        let socket = socket.clone();
        let stop_rx = stop_rx.clone();
        async move {
            let _ = receiver::demultiplex(&socket, peer, media_tx, control_tx, stop_rx).await;
        }
    });

    println!();
    println!("🎛️  Opening the control tunnel over the punched path to {peer}…");
    let mut ctl = match &opts.control {
        // Explicit LAN address: TCP to port 48011, for local debugging. The
        // host refuses that port from non-private addresses, so this is not a
        // WAN fallback and is not meant to be one.
        Some(addr) => {
            let addr: SocketAddr = tokio::net::lookup_host(addr)
                .await?
                .next()
                .ok_or("could not resolve the control address")?;
            println!("   (using the LAN control port {addr} instead of the tunnel)");
            control::ControlChannel::connect_lan(addr, identity, pin).await?
        }
        // The zero-config path: mutual TLS over reliable UDP, on the socket the
        // punch opened. No port forwarding, no trusted relay.
        None => {
            control::ControlChannel::connect_wan(socket.clone(), peer, control_rx, identity, pin)
                .await?
        }
    };
    println!("🔐 Control tunnel authenticated — mutual TLS over the punched path");

    let hello = ctl.call("hello", serde_json::Map::new()).await?;
    println!(
        "👋 {} says hello — protocol {}, paired as \"{}\"",
        control::field_str(&hello, "server").unwrap_or("nova"),
        control::field_u64(&hello, "protocol_version").unwrap_or(0),
        control::field_str(&hello, "device_name").unwrap_or("?"),
    );
    if hello.pointer("/capabilities/sessions") != Some(&serde_json::Value::Bool(true)) {
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
            // someone is streaming — say so rather than reporting a failure.
            println!("⛔ The host refused: {e}");
            return Err("no session was granted".into());
        }
    };

    let session_id = control::field_u64(&grant, "session_id")?;
    let keys = SessionKeys::from_hex(control::field_str(&grant, "media_key")?)
        .map_err(|e| format!("media key from host: {e}"))?;
    println!(
        "🎬 Session {} granted — {}x{}@{} {} — media keyed and inbound",
        session_id,
        control::field_u64(&grant, "width").unwrap_or(0),
        control::field_u64(&grant, "height").unwrap_or(0),
        control::field_u64(&grant, "fps").unwrap_or(0),
        control::field_str(&grant, "codec").unwrap_or("?"),
    );

    let seconds = opts.seconds;
    tokio::spawn(async move {
        if seconds > 0 {
            tokio::time::sleep(Duration::from_secs(seconds)).await;
        } else {
            let _ = tokio::signal::ctrl_c().await;
        }
        let _ = stop_tx.send(true);
    });

    let mut sink = receiver::LoggingSink::default();
    let stats =
        receiver::run_receiver(&socket, peer, media_rx, Some(keys), &mut sink, stop_rx).await?;

    // Always tell the host we are done. A session left open would block the
    // next client — including a Moonlight client, which is exactly the
    // asymmetry the host's gate is there to prevent.
    if let Err(e) = ctl.call("stop_session", serde_json::Map::new()).await {
        // Not fatal: the host also releases the session when the control
        // tunnel closes, precisely so a client that vanishes cannot leave the
        // pipeline held.
        println!("⚠️  Could not stop the session cleanly: {e}");
    }
    demux_task.abort();

    println!();
    println!("📊 {} frame(s), {} keyframe(s), {} bytes", stats.frames_completed, stats.keyframes, sink.bytes);
    if stats.frames_incomplete > 0 || stats.frames_failed_auth > 0 {
        println!(
            "   {} incomplete (no FEC recovery in this build), {} failed authentication",
            stats.frames_incomplete, stats.frames_failed_auth
        );
    }
    if stats.frames_completed == 0 {
        println!(
            "   Nothing arrived. The path was open, so suspect the host side: check that a \
             Worker is connected and that nova.log shows frames being sent."
        );
    }
    Ok(())
}

/// Gather, trade candidates through the relay, and punch. Returns the socket
/// holding the open pinhole and the peer address it was opened toward.
async fn open_path(
    identity: &Identity,
    relay_url: &str,
    relay_pin: &str,
    host_fingerprint: &str,
    punch_timeout: Duration,
) -> Result<(tokio::net::UdpSocket, SocketAddr), Box<dyn std::error::Error>> {
    println!("🪪 Echo identity: {}", identity.fingerprint);

    // ── 1. Gather ───────────────────────────────────────────────────────────
    // The socket bound here is the one that will carry traffic, so the mapping
    // discovered is the mapping the host will actually reach. Binding a
    // throwaway socket for discovery and a different one for media is the
    // classic way to make a punch that cannot possibly work.
    let socket = tokio::net::UdpSocket::bind("0.0.0.0:0").await?;
    println!("🔌 Media socket bound to {}", socket.local_addr()?);

    let servers = resolve_default_stun().await;
    if servers.is_empty() {
        return Err("no STUN server resolved — cannot discover a public address".into());
    }
    let (behavior, mine) = stun::classify_mapping(&socket, &servers).await;
    if mine.is_empty() {
        return Err("no STUN server answered — cannot discover a public address".into());
    }
    for c in &mine {
        println!("🌐 Our public address {} (via {})", c.mapped, c.via);
    }
    report_behavior(behavior);

    // ── 2/3. Relay: look up the host, offer ourselves ───────────────────────
    let target = RelayTarget::parse(relay_url)?;
    let pin = parse_fingerprint(relay_pin)?;
    let tls = Arc::new(client_config_pinned(identity, pin)?);
    let mut conn = RelayConnection::connect(&target, tls).await?;
    println!("📡 Connected to relay {}", target.authority());

    let mut params = identity_params(identity);
    params.insert("host".into(), json!(host_fingerprint));
    let result = conn.call("lookup", params).await?;
    let host_candidates = parse_candidates(result.get("candidates"));
    if host_candidates.is_empty() {
        return Err(format!(
            "the relay knows no candidates for host {}… — is Nova running with \
             [echo.signaling] configured against this relay?",
            &host_fingerprint[..16.min(host_fingerprint.len())]
        )
        .into());
    }
    println!("🎯 Host candidates: {}", join_addrs(&host_candidates));

    let mut params = identity_params(identity);
    params.insert("host".into(), json!(host_fingerprint));
    params.insert("candidates".into(), Value::Array(candidates_json(&mine)));
    conn.call("offer", params).await?;
    println!("🤝 Offered our candidates — the host learns them on its next poll");

    // ── 4. Punch ────────────────────────────────────────────────────────────
    // Both sides are now sending. Early packets are expected to be dropped by
    // the far NAT; retrying through that window is the mechanism.
    println!("🥊 Blasting every {:?} for up to {:?}…", punch::PROBE_INTERVAL, punch_timeout);
    let mut io = punch::UdpPunchIo::new(&socket);
    let cfg = punch::PunchConfig { timeout: punch_timeout, ..Default::default() };
    match punch::punch_io(&mut io, &host_candidates, cfg).await {
        Ok(result) => {
            println!(
                "✅ Path open to {} after {} round(s) — confirmed by {:?}",
                result.peer, result.rounds, result.proof
            );
            println!("   The NAT pinhole is open on {}.", socket.local_addr()?);
            // The socket is returned rather than dropped: the mapping belongs
            // to *this* socket, so anything that binds a new one has thrown the
            // punch away.
            Ok((socket, result.peer))
        }
        Err(e) => {
            println!("❌ Punch failed: {e}");
            if behavior == MappingBehavior::EndpointDependent {
                println!(
                    "   This NAT is endpoint-dependent, so that is expected rather than a bug: \
                     the address a STUN server reports is not the one a peer would reach. \
                     Sessions from this network need a relay."
                );
            } else {
                println!(
                    "   Check that Nova is polling the same relay, that its fingerprint is \
                     {}…, and that the host's own punch is running.",
                    &host_fingerprint[..16.min(host_fingerprint.len())]
                );
            }
            Err("no path could be opened".into())
        }
    }
}

async fn resolve_default_stun() -> Vec<SocketAddr> {
    let mut out = Vec::new();
    for name in stun::DEFAULT_STUN_SERVERS {
        match tokio::net::lookup_host(name).await {
            Ok(addrs) => out.extend(addrs.filter(SocketAddr::is_ipv4).take(1)),
            Err(e) => println!("⚠️  Could not resolve {name}: {e}"),
        }
    }
    out
}

fn report_behavior(behavior: MappingBehavior) {
    match behavior {
        MappingBehavior::EndpointIndependent => {
            println!("🧭 NAT mapping: endpoint-independent — hole punching viable")
        }
        MappingBehavior::EndpointDependent => println!(
            "🧭 NAT mapping: endpoint-dependent (symmetric) — direct connections will NOT \
             work from this network; sessions need a relay"
        ),
        MappingBehavior::Unknown => println!(
            "🧭 NAT mapping: unknown (fewer than two servers answered) — attempting direct \
             anyway, with relay fallback if it fails"
        ),
    }
}

fn join_addrs(addrs: &[SocketAddr]) -> String {
    addrs.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(", ")
}

//! Echo's headless CLI — argument parsing and console rendering, nothing else.
//!
//! All the behaviour moved to the library (`echo_client::session`) so the
//! Android bridge can share it. What is left here is the part that is genuinely
//! CLI-specific: clap, a [`ConsoleProgress`] that renders
//! [`Event`](echo_client::session::Event)s as the lines this tool has always
//! printed, and the choice of when to stop (a timer or Ctrl-C — an app stops for
//! entirely different reasons, which is why the library does not decide).

use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, Subcommand};
use echo_client::pairing::{self, PairEvent, PairOptions};
use echo_client::receiver::LoggingSink;
use echo_client::session::{
    self, ConnectOptions, Event, Progress, StreamOptions,
};
use nova_core::identity::Identity;
use nova_core::stun::{self, MappingBehavior};

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

    /// Pair with a Nova host on the LAN, so it will trust this client.
    ///
    /// Run this once per host. Echo shows a PIN; type it into Nova's dialog on
    /// the host machine. Pairing must happen over the LAN — it uses Nova's
    /// unauthenticated HTTP port, which is exactly why the handshake carries its
    /// own mutual proof. Streaming afterwards works from anywhere.
    Pair {
        /// The host's LAN address, without a port.
        #[arg(long)]
        host: String,
        /// Use a specific PIN instead of a generated one.
        #[arg(long)]
        pin: Option<String>,
        /// Name to suggest for this device. Nova uses whatever is typed into
        /// its own dialog, so this is only a hint.
        #[arg(long, default_value = "Echo")]
        name: String,
        /// How long to wait for someone to answer Nova's PIN dialog.
        #[arg(long, default_value_t = 180)]
        consent_secs: u64,
        /// Skip the post-pairing HTTPS check that the host really authorises
        /// this certificate.
        #[arg(long)]
        skip_verify: bool,
    },

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
        /// Try this LAN address first: `10.0.0.5` or `10.0.0.5:48011`.
        ///
        /// Stage 1 of the cascade — a candidate exchange over the host's TCP
        /// control port, then a direct punch. Skipped entirely when absent,
        /// because dialling a private address from another network is time
        /// spent to learn nothing.
        #[arg(long)]
        lan: Option<String>,
        /// Offer this WAN endpoint as an extra punch candidate.
        ///
        /// For a host behind a port forward whose reflexive candidate is wrong.
        /// A bare address borrows the port the relay reports for the host.
        #[arg(long)]
        wan: Option<String>,
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
        /// Try this LAN address first. See `connect --lan`.
        #[arg(long)]
        lan: Option<String>,
        /// Offer this WAN endpoint as an extra punch candidate. See `connect --wan`.
        #[arg(long)]
        wan: Option<String>,
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
        /// Nova app to open: 1 Desktop, 2 Steam, 3 Xbox, 4 RetroArch,
        /// 5 Virtual Desktop. Anything but 1 makes the host launch a process.
        #[arg(long, default_value_t = 1)]
        app_id: u32,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    // RSA-2048, not the faster ECDSA: GameStream pairing verifies the client
    // certificate's signature as RSA, and Echo must pair and connect with the
    // same identity — Nova's trust store is keyed by the certificate it saw
    // during pairing. Costs about a second, once, on first run.
    let identity = Identity::load_or_create_rsa2048(&args.data_dir, "echo", "echo-client")
        .map_err(|e| format!("identity: {e}"))?;
    let mut progress = ConsoleProgress;

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
            let servers = session::resolve_default_stun(&mut progress).await;
            if servers.is_empty() {
                return Err("no STUN server resolved — check DNS/connectivity".into());
            }
            let (behavior, candidates) = stun::classify_mapping(&socket, &servers).await;
            for c in &candidates {
                println!("🌐 Public address {} (via {})", c.mapped, c.via);
            }
            report_behavior(behavior);
        }

        Command::Pair { host, pin, name, consent_secs, skip_verify } => {
            let pin = pin.unwrap_or_else(pairing::generate_pin);
            let opts = PairOptions {
                device_name: name,
                consent_timeout: Duration::from_secs(consent_secs),
                ..PairOptions::new(&host, &pin)
            };

            println!("🪪 Echo identity: {}", identity.fingerprint);
            println!("🔗 Pairing with Nova at {host}…");
            println!();

            let paired = pairing::pair(&identity, &opts, &mut render_pair_event).await?;

            if skip_verify {
                println!("⏭️  Skipping the HTTPS authorisation check (--skip-verify).");
            } else {
                println!("🔍 Confirming the host authorises this certificate…");
                match pairing::verify_paired(&identity, &host, &paired.cert_der).await {
                    Ok(()) => println!("✅ Confirmed over HTTPS — the trust store entry is live."),
                    // The handshake did complete, so this is a warning rather
                    // than a failure: it narrows the problem instead of hiding
                    // it behind a "pairing failed" that would send someone back
                    // to re-check the PIN they typed correctly.
                    Err(e) => println!(
                        "⚠️  Pairing completed, but the confirmation failed: {e}\n   \
                         Try `stream` anyway; if it is refused, pair again."
                    ),
                }
            }

            println!();
            println!("🎉 Paired. Nova's fingerprint:");
            println!("   {}", paired.fingerprint);
            println!();
            println!("Stream over the LAN with:");
            println!(
                "   echo-client stream --host {} --control {}:48011 \\\n     --relay <url> --relay-pin <fp>",
                paired.fingerprint, host
            );
        }

        Command::Connect { relay, relay_pin, host, punch_secs, lan, wan } => {
            let opts = ConnectOptions {
                relay_url: relay,
                relay_pin,
                host_fingerprint: host,
                punch_timeout: Duration::from_secs(punch_secs),
                lan_endpoint: lan,
                wan_endpoint: wan,
                lan_timeout: session::DEFAULT_LAN_TIMEOUT,
            };
            session::open_path(&identity, &opts, &mut progress).await?;
            println!();
            println!("   Media would flow here next; `stream` continues past this point.");
        }

        Command::Stream {
            relay,
            relay_pin,
            host,
            control,
            lan,
            wan,
            punch_secs,
            seconds,
            res,
            fps,
            codec,
            bitrate_kbps,
            app_id,
        } => {
            let connect = ConnectOptions {
                relay_url: relay,
                relay_pin,
                host_fingerprint: host.clone(),
                punch_timeout: Duration::from_secs(punch_secs),
                lan_endpoint: lan,
                wan_endpoint: wan,
                lan_timeout: session::DEFAULT_LAN_TIMEOUT,
            };
            let path = session::open_path(&identity, &connect, &mut progress).await?;

            // The CLI's stop policy: a fixed duration, or Ctrl-C when 0.
            let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
            tokio::spawn(async move {
                if seconds > 0 {
                    tokio::time::sleep(Duration::from_secs(seconds)).await;
                } else {
                    let _ = tokio::signal::ctrl_c().await;
                }
                let _ = stop_tx.send(true);
            });

            let opts = StreamOptions { res, fps, codec, bitrate_kbps, app_id, control };
            let mut sink = LoggingSink::default();
            let stats = session::stream(
                &identity,
                &host,
                path,
                &opts,
                &mut sink,
                &mut progress,
                stop_rx,
                // The headless CLI has no window to read input from and no
                // microphone to open; it exists to prove the media path, not to
                // drive the host.
                session::Uplink::none(),
            )
            .await?;

            println!();
            println!(
                "📊 {} frame(s), {} keyframe(s), {} bytes",
                stats.frames_completed, stats.keyframes, sink.bytes
            );
            if stats.frames_incomplete > 0 || stats.frames_failed_auth > 0 {
                println!(
                    "   {} incomplete (beyond FEC repair), {} failed authentication",
                    stats.frames_incomplete, stats.frames_failed_auth
                );
            }
            if stats.frames_recovered_by_fec > 0 {
                println!(
                    "   {} frame(s) rebuilt from parity — loss a viewer would never have seen",
                    stats.frames_recovered_by_fec
                );
            }
            if stats.frames_dropped_before_keyframe > 0 {
                println!(
                    "   {} frame(s) held back waiting for the first keyframe. On a healthy link \
                     this should be 0 — Nova starts a session with an IDR.",
                    stats.frames_dropped_before_keyframe
                );
            }
            if stats.frames_completed == 0 {
                println!(
                    "   Nothing arrived. The path was open, so suspect the host side: check that \
                     a Worker is connected and that nova.log shows frames being sent."
                );
            }
        }
    }
    Ok(())
}

/// Renders pairing progress. The PIN gets a box of its own because it is the
/// one thing on screen the user has to act on.
fn render_pair_event(event: PairEvent) {
    match event {
        PairEvent::AwaitingConsent { pin } => {
            println!("   ┌────────────────┐");
            println!("   │   PIN:  {pin}   │");
            println!("   └────────────────┘");
            println!();
            println!("   Type this PIN into Nova's dialog on the host, then name the device.");
            println!("   Waiting… (Nova's dialog has no timeout; this does)");
        }
        PairEvent::HostCertificate { fingerprint } => {
            println!("📜 Host certificate received: {}…", &fingerprint[..16]);
        }
        PairEvent::ChallengeAccepted => println!("🔑 PIN accepted — challenge exchanged"),
        PairEvent::HostVerified => {
            println!("🛡️  Host verified — its hash matches and it holds the certificate's key")
        }
        PairEvent::Paired { .. } => println!("🤝 Handshake complete"),
    }
}

/// Renders session events as the console output this tool has always produced.
struct ConsoleProgress;

impl Progress for ConsoleProgress {
    fn event(&mut self, event: Event) {
        match event {
            Event::Identity { fingerprint } => println!("🪪 Echo identity: {fingerprint}"),
            Event::SocketBound { local } => println!("🔌 Media socket bound to {local}"),
            Event::PublicAddress { mapped, via } => {
                println!("🌐 Our public address {mapped} (via {via})")
            }
            Event::LocalCandidate { addr } => {
                println!("🏠 Our address on this network {addr} (offered for same-LAN peers)")
            }
            Event::Mapping { behavior } => report_behavior(behavior),
            Event::RelayConnected { authority } => println!("📡 Connected to relay {authority}"),
            Event::HostCandidates { addrs } => println!(
                "🎯 Host candidates: {}",
                addrs.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(", ")
            ),
            Event::Offered => {
                println!("🤝 Offered our candidates — the host learns them on its next poll")
            }
            Event::Punching { interval, timeout } => {
                println!("🥊 Blasting every {interval:?} for up to {timeout:?}…")
            }
            Event::LanAttempt { endpoint } => {
                println!("🏠 Trying the LAN first: {endpoint}…")
            }
            Event::LanRendezvous { offered, host_candidates } => {
                println!(
                    "🤝 LAN rendezvous accepted — offered {offered}, host answers from {}",
                    host_candidates.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(", ")
                )
            }
            Event::LanAbandoned { reason } => {
                println!("↪️  LAN route abandoned ({reason}) — falling through to the relay")
            }
            Event::PathOpen { peer, rounds, proof, local, transport } => {
                println!(
                    "✅ Path open to {peer} after {rounds} round(s) — confirmed by {proof} [{}]",
                    transport.as_str()
                );
                println!("   The NAT pinhole is open on {local}.");
            }
            Event::PunchFailed { endpoint_dependent, error } => {
                println!("❌ Punch failed: {error}");
                if endpoint_dependent {
                    println!(
                        "   This NAT is endpoint-dependent, so that is expected rather than a \
                         bug: the address a STUN server reports is not the one a peer would \
                         reach. Sessions from this network need a relay."
                    );
                }
            }
            Event::ControlOpening { peer, lan } => {
                println!();
                println!("🎛️  Opening the control tunnel over the punched path to {peer}…");
                if let Some(addr) = lan {
                    println!("   (using the LAN control port {addr} instead of the tunnel)");
                }
            }
            Event::ControlAuthenticated => {
                println!("🔐 Control tunnel authenticated — mutual TLS over the punched path")
            }
            Event::Hello { server, protocol_version, device_name } => println!(
                "👋 {server} says hello — protocol {protocol_version}, paired as \"{device_name}\""
            ),
            Event::Granted { session_id, width, height, fps, codec } => println!(
                "🎬 Session {session_id} granted — {width}x{height}@{fps} {codec} — media keyed \
                 and inbound"
            ),
            Event::Refused { reason } => println!("⛔ The host refused: {reason}"),
            Event::Warning { message } => println!("⚠️  {message}"),
            // The CLI prints its own richer summary once `stream` returns, so
            // this would only duplicate it.
            Event::Ended { .. } => {}
        }
    }
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

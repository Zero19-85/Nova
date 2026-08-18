mod app_launcher;
mod audio;
mod capture;
mod config;
mod control;
pub mod debug; // pub so nova-server binary can call init_debug_logger() during --install/--uninstall
/// Everything built for Echo, Nova's own native client: the control/telemetry
/// RPC on port 48011 (`echo::rpc`) and WAN/NAT-traversal primitives
/// (`echo::wan`). Master-side by necessity — see the module docs. Public so a
/// future CLI/diagnostic mode can drive the same surfaces in-process.
pub mod echo;
mod encoder;
mod input;
// Microphone passthrough: the client's Opus → VB-CABLE, rendered in the Master.
mod mic;
// pub so the binary's `--mic-probe` mode can drive it. Stage 0 of the
// microphone work: measured whether a Session 0 service may render audio a
// user-session application can capture — see the module docs.
pub mod mic_probe;
/// Master↔Worker IPC transport (Session-Survival Architecture, Phase 1).
/// Public so the binary's `--worker` mode and `service.rs`'s Master-side
/// spawn/accept logic can both reach it.
pub mod ipc;
mod pairing;
/// Bitrate policy: the per-session budget (resolution cap + audio reservation)
/// and the closed-loop congestion controller that runs underneath it. Both
/// capture loops share the controller; both negotiators share the budget.
mod qos;
mod rtp;
mod rtsp;
mod session_negotiate;
/// Secure-desktop UAC policy toggle — the opt-in complement to the DDA
/// secure-desktop capture backend. Public so the installer/CLI and a future tray
/// item can query and flip `PromptOnSecureDesktop`.
pub mod secure_desktop;
/// Thin SYSTEM launcher service (Phase 15.2c) — spawns the interactive host
/// with a SYSTEM-derived elevated token. Public so the binary's `--service` /
/// `--install-service` / `--uninstall-service` subcommands can reach it.
pub mod service;
mod shutdown;
/// Live session telemetry (resolution / fps / codec / bitrate) published by
/// whichever capture loop is running and read by the tray's Server Stats
/// window. Lock-free atomics — see the module docs for why it is not a channel.
mod stats;
pub mod tray;
mod virtual_display;

use clap::Parser;
// Trait for the capture manager's per-frame surface (width()/height()/origin()/
// device()/try_get_frame()/rebind()) — the concrete backend behind it is the
// manager's business (WGC normally, DDA during secure-desktop interludes).
use capture::DesktopCapture;
use encoder::{Encoder, EncoderConfig};
use qos::QosController;
use socket2;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::core::Result;
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_MOUSE, MOUSEEVENTF_MOVE,
};
use mdns_sd::{ServiceDaemon, ServiceInfo};
use tokio::signal;

/// The UDP port Nova's media socket binds.
///
/// GameStream's video port, and Echo's too — one socket carries RTP, the
/// punch's STUN probes, and Echo's sealed media, which is what lets a NAT
/// mapping discovered by a probe be the mapping the stream actually uses.
///
/// Named rather than spelled at each site because the LAN rendezvous now has to
/// tell a client which port to punch toward, and a third literal is how the
/// three would drift apart.
pub(crate) const ECHO_MEDIA_PORT: u16 = 47998;

/// CLI overrides — all optional; omitted fields fall back to nova.toml values.
#[derive(Parser, Debug)]
#[command(author, version, about = "Nova Server")]
struct Args {
    /// Override nova.toml stream.width
    #[arg(long)] width:   Option<i32>,
    /// Override nova.toml stream.height
    #[arg(long)] height:  Option<i32>,
    /// Override nova.toml stream.bitrate_kbps
    #[arg(long)] bitrate: Option<i32>,
    /// Override nova.toml stream.codec ("h264" | "hevc" | "av1")
    #[arg(long)] codec:   Option<String>,
    /// Override nova.toml stream.fps
    #[arg(long)] fps:     Option<u32>,
    /// Override nova.toml network.fec_percentage (0 = disable)
    #[arg(long)] fec:     Option<u32>,
}

fn get_local_ip() -> String {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").expect("bind UDP for IP discovery");
    socket.connect("8.8.8.8:80").ok();
    socket.local_addr().map(|a| a.ip().to_string()).unwrap_or_else(|_| "127.0.0.1".to_string())
}

/// Recreates the NVENC encoder at `width`x`height` on the capturer's device
/// (the manager guarantees one D3D11 device for the process lifetime).
///
/// **The size is the SESSION's, never the capturer's.** A Moonlight client
/// fixes its decoder's resolution and HDR profile when the session starts, so
/// changing the encoder's geometry mid-session corrupts it permanently. What
/// we can capture, in contrast, changes freely underneath a live session — the
/// logon screen is the physical monitor (a SYSTEM-fallback Worker cannot drive
/// the VDD at all: SetDisplayConfig is denied on the Winlogon desktop), and the
/// signed-in desktop is the VDD. The shim absorbs that difference by scaling
/// the capture into the encoder's surface (aspect-preserving, black bars when
/// the shapes differ) and cross-converting SDR↔HDR as needed — see shim.cpp's
/// `UpdateScaleRect` and the `ps_sdr2hdr_*` / `ps_hdr2sdr_*` shaders.
fn recreate_encoder_at(
    capturer: &capture::DesktopManager,
    enc: &mut Encoder,
    width: i32,
    height: i32,
) -> std::result::Result<(), String> {
    println!("🔁 Recreating NVENC encoder at {width}x{height} (capture is {}x{})",
        capturer.width(), capturer.height());
    // Tear down the old encoder's shim-global NVENC/D3D state BEFORE creating
    // the replacement — otherwise Encoder::new() below overwrites those
    // globals with the new encoder's state, and *enc's old value being dropped
    // (by the assignment further down) would destroy the brand-new encoder
    // instead, leaving g_nvEncoder/g_device null.
    enc.cleanup();
    match Encoder::new(capturer.device(), EncoderConfig {
        width,
        height,
        fps:          enc.config.fps,
        bitrate_kbps: enc.config.bitrate_kbps,
        codec:        enc.config.codec,
        is_hdr:       enc.config.is_hdr,
    }) {
        Ok(new_enc) => {
            *enc = new_enc;
            Ok(())
        }
        Err(e) => {
            eprintln!("❌ Failed to recreate encoder: {e}");
            Err(e)
        }
    }
}

/// Idle/no-session helper: match the encoder to the capturer. Only correct
/// when NO client session is live (nothing has fixed a decoder yet) — during a
/// session use [`recreate_encoder_at`] with the session's negotiated size.
fn recreate_encoder_for_capture(
    capturer: &capture::DesktopManager,
    enc: &mut Encoder,
) -> std::result::Result<(), String> {
    let (w, h) = (capturer.width() as i32, capturer.height() as i32);
    recreate_encoder_at(capturer, enc, w, h)
}

/// Re-targets the capture manager to `target` (GDI device name, or `None` for
/// the physical primary). The manager routes the rebind to whichever backend
/// matches the current input desktop (sessions land on WGC; a live
/// secure-desktop DDA interlude retargets DDA).
///
/// The two size arguments are deliberately independent:
/// * `wait_for_capture_size` — poll until the MONITOR reports this size before
///   binding (a VDD mode change takes a moment to settle). Purely a capture-side
///   hint; passing a size the monitor will never reach just burns its 3 s
///   timeout, so it must be `None` whenever we aren't driving the VDD.
/// * `enc_size` — pin the ENCODER to this (the session's negotiated geometry),
///   regardless of what is actually being captured. `None` means "follow the
///   capture", which is only correct outside a session. See
///   [`recreate_encoder_at`] for why the session's geometry must never move.
///
/// The capture's pixel format follows the SOURCE DISPLAY's actual state
/// (`capture_hdr`), never the encoder's. Asking DXGI for FP16 from a display
/// that is not in Advanced Color yields scRGB with SDR white pinned at 1.0
/// (= 80 nits), which the HDR shader then renders at a third of reference
/// white — and when the FP16 request fails outright, DDA silently falls back
/// to BGRA8, so the two paths alternate and the picture visibly pulses between
/// dim and bright (reported live 2026-08-11 as brightness flashing on mouse
/// movement). An SDR capture in an HDR session is fine and expected now: the
/// shim's sRGB→BT.2020-PQ path maps SDR white to 203 nits, stably.
///
/// Synchronous — WGC session creation and CCD calls block briefly; this is
/// called while holding the `client_info` mutex where `.await` is unsound.
fn rebind_capture_and_encoder(
    capturer: &mut capture::DesktopManager,
    enc: &mut Encoder,
    target: Option<&str>,
    wait_for_capture_size: Option<(u32, u32)>,
    enc_size: Option<(u32, u32)>,
    capture_hdr: Option<bool>,
) -> std::result::Result<(), String> {
    // `None` = "same as the encoder", the pre-scaling assumption. Correct on
    // the monolithic path, where an HDR session always means the VDD really is
    // in Advanced Color.
    let capture_hdr = capture_hdr.unwrap_or(enc.config.is_hdr);
    match capturer.rebind(target, capture_hdr, wait_for_capture_size) {
        Ok(needs_new_encoder) => {
            match enc_size {
                // In a session: hold the negotiated geometry. Recreate only
                // when the encoder isn't already there (a capture change alone
                // no longer forces one — the shim rescales instead).
                Some((w, h)) => {
                    if enc.config.width != w as i32 || enc.config.height != h as i32 {
                        recreate_encoder_at(capturer, enc, w as i32, h as i32)?;
                    }
                }
                None if needs_new_encoder => recreate_encoder_for_capture(capturer, enc)?,
                None => {}
            }
            // Keep input.rs's mouse-mapping rect in sync even when the
            // resolution didn't change — rebind() can move the captured
            // output to a different position in the virtual screen (e.g.
            // the Virtual Desktop output becoming primary at (0,0) while a
            // physical monitor that used to be primary shifts to a non-zero
            // origin).
            let (ox, oy) = capturer.origin();
            input::set_active_capture_rect(ox, oy, capturer.width(), capturer.height());
            Ok(())
        }
        Err(e) => {
            let msg = format!("Capture rebind failed: {:?}", e);
            eprintln!("❌ {msg}");
            Err(msg)
        }
    }
}

// ── Master orchestration (Session-Survival Architecture, Phase 2) ──────────
//
// Runs inside `service.rs`'s `--service` process (Master IS the service, not
// a separate spawned child — see the approved plan's architecture decision
// and its security-exposure tradeoff note). `start_master_network` is called
// once, from within `service_worker`'s `ipc_runtime.block_on(...)`; it
// spawns everything and returns immediately — the spawned tasks/threads keep
// running on the runtime's own worker threads for the service's lifetime.

use ipc::{ControlMsg, WireCodec, WorkerLink};
use tokio::net::windows::named_pipe::NamedPipeServer;
use tokio::sync::mpsc;

/// Handles `service.rs`'s reconcile loop needs: `client_info`/`worker_link`
/// for `spawn_host_with_ipc`'s own use (the latter also handed to
/// `control::start_control_server`), plus the two channels for adopting a
/// freshly-handshaked Worker's pipes into the long-lived supervisor tasks.
#[derive(Clone)]
pub struct MasterHandles {
    pub client_info: Arc<Mutex<Option<rtsp::ClientInfo>>>,
    pub worker_link: WorkerLink,
    control_pipe_tx: mpsc::UnboundedSender<NamedPipeServer>,
    media_pipe_tx: mpsc::UnboundedSender<NamedPipeServer>,
}

impl MasterHandles {
    /// (service.rs side) Hand a freshly-handshaked Worker's pipes to the
    /// control/media supervisor tasks. Call AFTER `spawn_host_with_ipc`'s own
    /// accept+`WorkerReady` handshake succeeds — these channels are for
    /// ONGOING traffic, not the initial handshake.
    pub fn adopt_worker_pipes(&self, control: NamedPipeServer, media: NamedPipeServer) {
        let _ = self.control_pipe_tx.send(control);
        let _ = self.media_pipe_tx.send(media);
    }
}

fn to_wire_codec(c: encoder::Codec) -> WireCodec {
    WireCodec::from(c)
}

fn negotiated_to_configure_start(n: &session_negotiate::NegotiatedParams) -> ipc::ConfigureStart {
    ipc::ConfigureStart {
        width: n.width,
        height: n.height,
        fps: n.fps,
        codec: to_wire_codec(n.codec),
        hdr_confirmed: n.hdr_confirmed,
        bitrate_kbps: n.bitrate_kbps,
        app_id: n.app_id,
        launch_app: n.launch_app,
        device_name: n.device_name.clone(),
        rikey: n.rikey,
        rikeyid: n.rikeyid,
        host_audio: n.host_audio,
        audio_encryption: n.audio_encryption,
        audio_packet_duration_ms: n.audio_packet_duration_ms,
        packet_size: n.packet_size,
        min_fec_packets: n.min_fec_packets,
        // Placeholder — control_supervisor stamps the real value (RtpSender's
        // last-sent index + 1) on every ConfigureStart it puts on the pipe,
        // including this initial one. See ipc::ConfigureStart::start_frame_index.
        start_frame_index: 1,
    }
}

/// Master's network/orchestration bootstrap. Spawns RTSP/control/pairing/
/// mDNS exactly as the monolithic `run()` used to, plus three new long-lived
/// tasks (`session_watcher`, `control_supervisor`, `media_supervisor`) that
/// replace what used to be inline logic in `run()`'s main loop. Must be
/// called from within a tokio runtime.
pub async fn start_master_network() -> MasterHandles {
    let local_ip = get_local_ip();
    println!("🌐 LAN IP: {local_ip}");
    // This process is the SYSTEM Master — app launches must happen in the
    // Worker's user session (see app_launcher::LAUNCH_VIA_WORKER), not here.
    app_launcher::set_launch_via_worker();
    let cfg = Arc::new(config::NovaConfig::load());
    encoder::set_hdr_metadata(cfg.hdr.max_luminance_nits, cfg.hdr.max_cll_nits, cfg.hdr.max_fall_nits);

    let server_id  = "0123456789ABCDEF";
    let server_mac = "00:11:22:33:44:55";
    // See lib.rs::run()'s identical constant for the SCM bitmask rationale —
    // unchanged, just now computed Master-side since /serverinfo (pairing.rs)
    // is Master-side.
    let codec_mode_support: u32 = 0x1301;

    let client_info = Arc::new(Mutex::new(None::<rtsp::ClientInfo>));

    // RTSP server (blocking thread — owns the TCP listener). Unchanged
    // implementation; only WHICH process runs it is new.
    std::thread::spawn({
        let info = client_info.clone();
        move || rtsp::start_rtsp_server(48010, info)
    });

    let (worker_link, control_outbound_rx) = WorkerLink::new();

    // Control stream (ENet/reliable-UDP) on port 47999 — Some(worker_link):
    // the three session-bound call-outs (IDR/congestion/input) become IPC
    // sends now instead of same-process calls.
    std::thread::spawn({
        let info = client_info.clone();
        let link = worker_link.clone();
        move || control::start_control_server(47999, info, Some(link))
    });

    // Pairing HTTP/HTTPS server (tokio task). The tray PIN dialog lives in
    // the Worker (the only session-visible process); the Master relays both
    // directions over the control pipe (closes the old Phase 2 gap that made
    // new-device pairing hang under WORKER_SPLIT_ENABLED):
    //   pairing getservercert → tray_tx → forwarder below →
    //     ControlMsg::OpenPairDialog → Worker tray dialog →
    //     ControlMsg::PinRelay → control_supervisor → global_pin →
    //     pairing's waiting loop proceeds.
    let (tray_tx, tray_rx) = std::sync::mpsc::sync_channel::<tray::TrayCmd>(32);
    let global_pin: Arc<Mutex<(String, String)>> = Arc::new(Mutex::new((String::new(), String::new())));
    std::thread::Builder::new()
        .name("nova-pair-dialog-fwd".into())
        .spawn({
            let link = worker_link.clone();
            move || {
                // Blocking std receiver — pairing sends at human cadence.
                // Recv errors only when the sender side is dropped (process
                // shutdown), so exiting then is correct.
                while let Ok(cmd) = tray_rx.recv() {
                    if matches!(cmd, tray::TrayCmd::OpenPairDialog) {
                        println!("🔑 Master: pairing needs a PIN — asking Worker to open the pair dialog");
                        link.send(ipc::ControlMsg::OpenPairDialog);
                    }
                }
            }
        })
        .expect("spawn nova-pair-dialog-fwd thread");
    tokio::spawn(pairing::start_pairing_server(
        47989,
        local_ip.clone(),
        server_id.to_string(),
        server_mac.to_string(),
        client_info.clone(),
        codec_mode_support,
        Arc::new(tray_tx),
        global_pin.clone(),
    ));

    // mDNS — Sunshine-compatible service record, unchanged.
    let mdns = ServiceDaemon::new().expect("Failed to create mDNS daemon");
    let svc = ServiceInfo::new(
        "_nvstream._tcp.local.",
        "Nova",
        "nova.local.",
        local_ip.as_str(),
        47989,
        &[
            ("txtvers", "1"),
            ("port",     "47989"),
            ("mac",      server_mac),
            ("uniqueid", server_id),
        ][..],
    )
    .unwrap();
    let _ = mdns.register(svc);
    println!("📡 mDNS broadcaster started for Nova (Master)");

    // The Echo record — a second service type on the same daemon, for Echo
    // clients rather than Moonlight ones. Registered on a task because it must
    // wait for the pairing certificate; see `echo::discovery`.
    echo::discovery::spawn(&mdns, &cfg.echo, &local_ip);

    let rtp_sender = Arc::new(Mutex::new(
        rtp::RtpSender::new(ECHO_MEDIA_PORT)
            .expect("Failed to bind RTP socket on 47998"),
    ));

    // Audio's network-send (RTP header + AES-CBC + UDP) — relocated here from
    // the Worker (Phase 3); see audio::AudioTxState's doc comment. Same
    // DSCP-EF/non-blocking setup the Worker used to apply to its own copy of
    // this socket.
    let audio_socket = {
        let raw = socket2::Socket::from(
            std::net::UdpSocket::bind("0.0.0.0:48000").expect("Failed to bind audio socket on 48000"),
        );
        let _ = raw.set_tos(0xB8_u32);
        raw.set_nonblocking(true).expect("set_nonblocking on audio socket");
        std::net::UdpSocket::from(raw)
    };
    let audio_tx = Arc::new(Mutex::new(audio::AudioTxState::new(audio_socket)));

    // SYSTEM input helper (secure-desktop injection — see input.rs's UIPI
    // note): its supervisor owns the helper process/pipe; `helper_ready` is
    // the lock-free flag control_supervisor checks per input packet.
    let (helper_tx, helper_rx) = mpsc::unbounded_channel::<HelperCmd>();
    let helper_ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
    tokio::spawn(input_helper_supervisor(helper_rx, helper_ready.clone()));

    // What the live Worker can physically stream (ControlMsg::
    // WorkerCapabilities). session_watcher negotiates against it so a session
    // is never given a geometry/HDR profile the serving Worker can't hold for
    // the session's whole life — see session_negotiate::negotiate.
    let worker_caps: Arc<Mutex<Option<session_negotiate::WorkerCaps>>> = Arc::new(Mutex::new(None));

    // The Worker's reported display topology (ControlMsg::DisplayInventory),
    // cached for echo_rpc's seat list. Master cannot enumerate displays itself
    // — see that message's doc comment.
    let display_seats: echo::rpc::SeatCache = Arc::new(Mutex::new(Vec::new()));

    // Channel now, supervisor below: it needs `echo_sessions` (for the tray's
    // "End Stream", which must be able to end an Echo session too), and that
    // is built by the WAN block further down. Nothing is lost by waiting —
    // both of its inputs are channels, so a Worker pipe or an outbound message
    // that arrives first simply queues until the task starts.
    let (control_pipe_tx, control_pipe_rx) = mpsc::unbounded_channel();

    // Echo's path layer. Read the split here carefully — it is not the shape it
    // used to be, and the reason is a bug.
    //
    // The gatherer probes through `rtp_sender`'s media socket (so the NAT
    // mapping it discovers is the one the stream will use), keeps that pinhole
    // open, answers a peer's punch probes, and latches the path a punch proves.
    // Only the FIRST of those is about the WAN. The last two are what any Echo
    // session needs, LAN or not: `start_session` refuses with `NoPathLatched`
    // unless something owns the latch cell.
    //
    // This whole block used to sit inside `if relay_configured`, which meant a
    // relay-less install had no latch, no punch inbox, and no session manager —
    // so it could not grant an Echo session at all. That is precisely the
    // install LAN-direct exists to serve. The gatherer and the session manager
    // are therefore unconditional now; only STUN gathering, the keepalive and
    // relay publication stay gated, which is what keeps the original promise
    // that a LAN-only install emits nothing to third-party servers.
    //
    // Started BEFORE the Echo RPC because the session manager needs the
    // gatherer's latched-peer cell: a session may only be granted over a path
    // a punch has actually proven.
    let relay_configured = !cfg.echo.signaling.url.trim().is_empty();
    let wan_candidates: Arc<Mutex<Vec<echo::wan::WanCandidate>>> =
        Arc::new(Mutex::new(Vec::new()));
    let gather = echo::wan::spawn_gatherer(
        rtp_sender.clone(),
        relay_configured.then(|| echo::wan::StunGathering {
            candidates: wan_candidates.clone(),
            servers: echo::wan::DEFAULT_STUN_SERVERS.iter().map(|s| s.to_string()).collect(),
        }),
    );
    let echo_sessions: Option<Arc<echo::session::SessionManager>> =
        Some(Arc::new(echo::session::SessionManager::new(
            Arc::new(echo::session::WorkerMediaPlane::new(
                rtp_sender.clone(),
                worker_link.clone(),
                worker_caps.clone(),
            )),
            client_info.clone(),
            gather.latched_handle(),
            echo::session::SessionPolicy {
                audio_reserve_kbps: cfg.network.audio_reserve_kbps,
                // The same grace period a Moonlight session gets — this is a
                // property of how long the operator wants a display held for
                // an absent client, not of which protocol asked for it.
                detach_grace: Duration::from_secs(cfg.stream.detach_grace() as u64),
            },
        )));
    if relay_configured {
        echo::signaling::spawn(&cfg.echo.signaling, wan_candidates, gather.clone());
    } else {
        println!(
            "📡 Signaling: no relay configured — Echo WAN connections are disabled; \
             LAN-direct sessions still work"
        );
    }
    // The RPC's view of the gatherer is one method wide: "blast at these".
    let echo_prober: Arc<dyn echo::rpc::PathProber> = Arc::new(gather);

    // Spawned after the WAN block because the frame path needs the session
    // manager to seal Echo frames — see `media_supervisor`'s seal call.
    let (media_pipe_tx, media_pipe_rx) = mpsc::unbounded_channel();
    tokio::spawn(media_supervisor(
        media_pipe_rx,
        rtp_sender.clone(),
        audio_tx.clone(),
        echo_sessions.clone(),
        client_info.clone(),
    ));

    // Echo's command surface. Master-side because the Worker owns no sockets
    // and is the process that dies on every sign-out — see echo::rpc's module
    // docs. Display commands leave here as ControlMsg::SetDisplayMode on the
    // same pipe everything else uses.
    //
    // ONE handler, two doors: the TCP listener below (LAN convenience) and the
    // WAN tunnel over the punched path. Sharing the handler is what guarantees
    // a command — and the anti-hijack gate in front of it — cannot behave
    // differently depending on how a client arrived.
    let echo_handler = echo::rpc::build_handler(
        Arc::new(echo::rpc::WorkerOrchestrator::new(
            worker_link.clone(),
            worker_caps.clone(),
            display_seats.clone(),
        )),
        client_info.clone(),
        echo_sessions.clone(),
        echo_prober,
        ECHO_MEDIA_PORT,
    );
    echo::rpc::spawn(&cfg.echo, echo_handler.clone());

    tokio::spawn(control_supervisor(
        control_pipe_rx,
        control_outbound_rx,
        rtp_sender.clone(),
        global_pin,
        helper_tx,
        helper_ready,
        worker_caps.clone(),
        client_info.clone(),
        display_seats,
        echo_sessions.clone(),
    ));

    // WAN control: mutual TLS over reliable UDP, on the socket the punch
    // opened. Only when there is a session layer at all — with no relay
    // configured there is no punched path for a tunnel to ride.
    if let Some(sessions) = echo_sessions.clone() {
        let (echo_tx, echo_rx) = mpsc::unbounded_channel();
        rtp_sender.lock().unwrap().set_echo_inbox(echo_tx);
        echo::transport::spawn(echo_rx, rtp_sender.clone(), echo_handler, sessions.clone());
        spawn_mic_passthrough(&cfg.audio.endpoint_override, rtp_sender.clone(), sessions);
    }

    tokio::spawn(session_watcher(client_info.clone(), worker_link.clone(), cfg, rtp_sender.clone(), audio_tx, worker_caps, echo_sessions));

    println!("🎬 Master network stack ready — RTSP:48010 control:47999 pairing:47989/47984 RTP:47998");

    MasterHandles { client_info, worker_link, control_pipe_tx, media_pipe_tx }
}

/// Start the microphone renderer and the task that feeds it.
///
/// Runs in the Master because that is where the socket is and — measured, not
/// assumed — because a Session 0 LocalSystem process can render into VB-CABLE
/// and have a user-session application capture it at full amplitude. See
/// `mic_probe`. The Master is also the process that never restarts, so the
/// microphone survives the Worker respawns a sign-out causes.
///
/// Optional in every direction: no VB-CABLE, no renderer, no inbox, and mic
/// datagrams are dropped in `rtp.rs` for the cost of a byte comparison.
fn spawn_mic_passthrough(
    endpoint_override: &str,
    rtp_sender: Arc<Mutex<rtp::RtpSender>>,
    sessions: Arc<echo::session::SessionManager>,
) {
    let sink = match mic::start(Some(endpoint_override)) {
        Ok(sink) => sink,
        Err(why) => {
            // Not an error: a host without a virtual cable simply has no
            // microphone passthrough, and everything else works unchanged.
            println!("🎤 {why}");
            return;
        }
    };

    let (mic_tx, mut mic_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    rtp_sender.lock().unwrap().set_mic_inbox(mic_tx);

    tokio::spawn(async move {
        // Rate-limited, like the input path's: a spray of forged datagrams must
        // not turn this into a log-flooding amplifier, and a real failure is a
        // steady condition rather than an event.
        let mut last_notice: Option<std::time::Instant> = None;
        let mut report = tokio::time::interval(Duration::from_secs(10));
        report.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                received = mic_rx.recv() => {
                    let Some(datagram) = received else { break };
                    match sessions.open_sealed_mic(&datagram) {
                        // The session mutex is already released — `open_sealed_mic`
                        // returns the packet rather than rendering, so nothing is
                        // held across the handoff to the render thread. That same
                        // mutex is taken by `seal_video` for every video frame.
                        Ok(Some(packet)) => sink.submit(packet),
                        // Authentic but not new: a duplicate or a late arrival.
                        Ok(None) => {}
                        Err(why) => {
                            if last_notice.map_or(true, |t: std::time::Instant| {
                                t.elapsed() > Duration::from_secs(10)
                            }) {
                                println!("🚫 Echo microphone datagram refused: {why}");
                                last_notice = Some(std::time::Instant::now());
                            }
                        }
                    }
                }
                _ = report.tick() => {
                    // Both halves in one line, because they answer different
                    // questions and are useless apart: the channel counters say
                    // what the *network* delivered, the render counters say what
                    // the *renderer* did with it. Silence with healthy network
                    // counters is a renderer problem; the reverse is a path
                    // problem. Silent when nothing is arriving.
                    let r = mic::stats();
                    if let Some((s, highest)) = sessions.mic_stats() {
                        if s.applied > 0 && !r.running {
                            // Audio is arriving and being authenticated, but the
                            // renderer is not running — so it died, and every
                            // packet since is going nowhere. Said explicitly
                            // because the alternative evidence is silence, which
                            // is also what "nobody is talking" looks like.
                            println!(
                                "🎤 Echo mic: {} packets arriving but the renderer is STOPPED — \
                                 audio is being discarded",
                                s.applied
                            );
                        } else if s.applied > 0 {
                            println!(
                                "🎤 Echo mic: {} applied, {} lost, {} late, {} reordered, {} refused \
                                 | rendered {}, concealed {}, underran {}, paused {}, dropped {}, \
                                 depth {} (worst {})",
                                s.applied, s.lost(highest), s.late, s.reordered, s.rejected,
                                r.rendered, r.concealed, r.underran, r.paused, r.dropped_late,
                                r.depth, r.worst_depth,
                            );
                        }
                    }
                }
            }
        }
    });
}

/// Is any client session live right now — Moonlight or Echo?
///
/// The Master's single answer to "is anyone actually watching?", which several
/// keepalive-shaped behaviours need and none of them had. Both halves matter:
/// `ClientInfo` is empty during an Echo session and `echo_holds_media` is false
/// during a Moonlight one, so either alone reports the other's live stream as
/// idle.
///
/// Deliberately reads `echo_holds_media`'s lock-free flag rather than the
/// session mutex, and treats a DETACHED Echo session as not live — which it is:
/// its encoder is stopped and its client is not listening.
fn session_is_live(
    client_info: &Arc<Mutex<Option<rtsp::ClientInfo>>>,
    echo_sessions: Option<&Arc<echo::session::SessionManager>>,
) -> bool {
    let moonlight = client_info
        .lock()
        .map(|g| g.as_ref().is_some_and(|c| c.streaming_active))
        .unwrap_or(false);
    moonlight || echo_sessions.is_some_and(|s| s.echo_holds_media())
}

/// Whether a detached session has outlived its grace period.
///
/// Split out of `session_watcher` so the boundary is testable without waiting
/// ten real minutes, and takes the elapsed time rather than the `Instant` for
/// the same reason.
///
/// `grace_secs == 0` means "hold indefinitely" and must never expire — a
/// configured zero is an operator saying they will end sessions themselves, not
/// a request to tear down immediately. Getting that backwards would turn the
/// opt-out into the most aggressive possible setting.
fn detach_grace_expired(grace_secs: u32, elapsed: Duration) -> bool {
    grace_secs > 0 && elapsed >= Duration::from_secs(grace_secs as u64)
}

/// Polls `ClientInfo` for the RTSP PLAY transition (the same trigger the
/// monolithic `run()`'s main loop used to react to in-process) and sends the
/// negotiated `ConfigureStart` to whichever Worker is currently connected.
///
/// Known Phase 2 simplifications (see the approved plan's Phase 2 scope):
/// does NOT do the pre-activation latency-hiding pass (starting the VDD/
/// encoder during the `/launch`→PLAY gap) — every session pays the full VDD-
/// activation latency at PLAY time for now. Also does not yet detect
/// "session ended, tear down THIS worker in place and wait for the next
/// session" (Phase 4 scope) — a Worker in this Phase 2 cut runs exactly one
/// `ConfigureStart` per process lifetime.
async fn session_watcher(
    client_info: Arc<Mutex<Option<rtsp::ClientInfo>>>,
    worker_link: WorkerLink,
    cfg: Arc<config::NovaConfig>,
    rtp_sender: Arc<Mutex<rtp::RtpSender>>,
    audio_tx: Arc<Mutex<audio::AudioTxState>>,
    worker_caps: Arc<Mutex<Option<session_negotiate::WorkerCaps>>>,
    // The other claimant on the one capture pipeline. `echo::session`'s gate
    // refuses to start an Echo session while Moonlight is streaming; this is
    // the same rule pointed the other way, and without it the protection is
    // one-directional — a Moonlight PLAY would reconfigure the Worker and
    // repoint the encoder out from under a live Echo client.
    echo_sessions: Option<Arc<echo::session::SessionManager>>,
) {
    let mut poll = tokio::time::interval(Duration::from_millis(50));
    let mut configured_generation: Option<u64> = None;
    // The generation we last saw with streaming_active=true — used to detect
    // the active→inactive edge (disconnect / RTSP TEARDOWN / cancel) exactly
    // once per session, even when a fresh /launch has already overwritten
    // ClientInfo with a NEW generation (also streaming_active=false, pre-
    // PLAY) before this poll ever observed the old generation's own
    // disconnect directly — the Worker still needs telling about the OLD
    // one either way.
    let mut active_generation: Option<u64> = None;
    // A session we already sent a plain (cancelled=false) Deactivate for,
    // because /cancel hadn't landed yet at the moment. Real client behavior
    // (confirmed live 2026-07-30): Moonlight's "Quit App" closes the ENet
    // control connection BEFORE the /cancel HTTPS request completes its own
    // fresh TLS handshake — the 50 ms poll below sees streaming_active go
    // false and fires a plain suspend well before /cancel ever arrives, so
    // by the time cancelled=true actually lands there was nothing left to
    // catch it: the monitor stayed on the VDD forever after every app-quit.
    // Fix: remember the generation we suspended, and if THAT generation's
    // cancelled flag flips true afterward (before any new session starts),
    // send a second Deactivate to upgrade the suspend into a full teardown.
    //
    // The `Instant` alongside it is the DETACH GRACE CLOCK. A suspend is not a
    // teardown: encoding and transmission stop at once, but the virtual display
    // and everything running on it are held so a reconnect resumes instantly.
    // Held forever, that is a ghost monitor nobody asked for, so the clock
    // upgrades an unclaimed suspend into a full teardown after
    // `[stream] detach_grace_secs`.
    //
    // It lives in the MASTER, and that placement is the whole point. The Worker
    // has had `mark_suspended()`/`suspended_idle_secs()` since Phase 13, but
    // only the monolithic `run()` ever polled them — `run_worker` never did, so
    // in the deployed split the timer has been dead and a vanished client left
    // the virtual display up indefinitely. Moving it here also makes it correct
    // rather than merely alive: the Worker dies on every sign-out, taking any
    // Worker-side clock with it, and a sign-out is exactly when a detached
    // session must keep counting rather than start over.
    let mut suspended_generation: Option<(u64, Instant)> = None;
    // The session we last logged an Echo-deferral for, so a Moonlight client
    // waiting behind an Echo session produces one line rather than twenty a
    // second.
    let mut echo_blocked_generation: Option<u64> = None;
    loop {
        poll.tick().await;
        let Some(client) = client_info.lock().unwrap().clone() else { continue };

        if !client.streaming_active {
            if let Some(ended_gen) = active_generation.take() {
                // Nothing previously told the Worker a session ended short
                // of the whole process dying — confirmed live (2026-07-20):
                // closing the app left the VDD primary and the monitor
                // dark. rtp_sender.reset() mirrors the monolithic path's
                // identical call on disconnect (frame_index must restart at
                // 1 for the next session — see rtp.rs's Phase 13 note).
                let cancelled = client.cancelled;
                println!("🛑 Master: session {ended_gen} ended (cancelled={cancelled}) — sending Deactivate to worker");
                rtp_sender.lock().unwrap().reset();
                worker_link.send(ControlMsg::Deactivate { cancelled });
                if cancelled {
                    if let Ok(mut guard) = client_info.lock() {
                        if let Some(info) = guard.as_mut() {
                            info.cancelled = false;
                        }
                    }
                } else {
                    let grace = cfg.stream.detach_grace();
                    if grace > 0 {
                        println!(
                            "⏸️  Master: session {ended_gen} DETACHED — encoder and transmission \
                             stopped, display held for {grace}s in case it comes back"
                        );
                    } else {
                        println!(
                            "⏸️  Master: session {ended_gen} DETACHED — display held indefinitely \
                             ([stream] detach_grace_secs = 0)"
                        );
                    }
                    suspended_generation = Some((ended_gen, Instant::now()));
                }
            } else if let Some((gen, detached_at)) = suspended_generation {
                if client.session_generation == gen && client.cancelled {
                    println!("🛑 Master: session {gen}'s /cancel arrived after the disconnect that suspended it — upgrading to full teardown");
                    worker_link.send(ControlMsg::Deactivate { cancelled: true });
                    suspended_generation = None;
                    if let Ok(mut guard) = client_info.lock() {
                        if let Some(info) = guard.as_mut() {
                            info.cancelled = false;
                        }
                    }
                } else if detach_grace_expired(cfg.stream.detach_grace(), detached_at.elapsed()) {
                    // Nobody came back. Upgrade the suspend into the teardown
                    // the client never asked for, which also clears
                    // `last_configure` in control_supervisor (only a CANCELLED
                    // Deactivate does — Phase 16.1's invariant) so no Worker
                    // respawn can resurrect a session that has now expired.
                    println!(
                        "🕐 Master: detached session {gen} was not reclaimed within {}s — \
                         tearing down and restoring the display",
                        cfg.stream.detach_grace()
                    );
                    worker_link.send(ControlMsg::Deactivate { cancelled: true });
                    suspended_generation = None;
                }
            }
            continue;
        }

        suspended_generation = None; // a new active session supersedes any pending upgrade
        active_generation = Some(client.session_generation);
        if configured_generation == Some(client.session_generation) {
            continue; // already sent ConfigureStart for this session/generation
        }
        // An Echo client holds the pipeline. Configuring the Worker now would
        // re-point the encoder and (via RtpSender) the media target, blacking
        // out a stream someone is watching — exactly what echo::session's gate
        // refuses in the opposite direction. Logged once per generation
        // (`configured_generation` is left unset, so this re-evaluates: if the
        // Echo session ends while the Moonlight client is still trying, its
        // session starts normally on the next poll).
        if echo_sessions.as_ref().is_some_and(|m| m.echo_holds_media()) {
            if echo_blocked_generation != Some(client.session_generation) {
                echo_blocked_generation = Some(client.session_generation);
                println!(
                    "⛔ Master: session {} wants to stream, but an Echo session holds the \
                     pipeline — deferring (it will start when Echo disconnects)",
                    client.session_generation
                );
            }
            continue;
        }
        let caps = *worker_caps.lock().unwrap();
        let negotiated = session_negotiate::negotiate(&client, &cfg.stream, &cfg.network, caps);
        println!(
            "🚀 Master: PLAY latched — negotiated {}x{}@{}fps {}{} (session {}) — sending ConfigureStart",
            negotiated.width, negotiated.height, negotiated.fps, negotiated.codec.as_str(),
            if negotiated.hdr_confirmed { "/HDR10" } else { "" }, client.session_generation,
        );
        // packetSize/FEC shard config is pure ClientInfo + nova.toml data —
        // unlike fps/codec (which the Worker might not apply exactly as
        // requested), nothing here depends on the Worker's actual outcome,
        // so this can be configured immediately rather than waiting for
        // WorkerConfigured. See the monolithic run()'s identical
        // rtp_sender.configure(...) call for why 512/1024/2 are the fallbacks.
        {
            let pkt_size = if negotiated.packet_size >= 512 { negotiated.packet_size as usize } else { 1024 };
            let min_fec = if negotiated.min_fec_packets > 0 { negotiated.min_fec_packets as usize } else { 2 };
            rtp_sender.lock().unwrap().configure(pkt_size, cfg.network.fec_percentage as usize, min_fec);
            audio_tx.lock().unwrap().reconfigure(
                negotiated.rikey, negotiated.rikeyid,
                negotiated.audio_encryption, negotiated.audio_packet_duration_ms,
            );
        }
        worker_link.send(ControlMsg::ConfigureStart(negotiated_to_configure_start(&negotiated)));
        configured_generation = Some(client.session_generation);
        // Consume the launch intent: any later re-send for this session (e.g.
        // a Worker respawn) must not start a second copy of the app.
        if negotiated.launch_app {
            if let Ok(mut guard) = client_info.lock() {
                if let Some(info) = guard.as_mut() {
                    info.pending_app_launch = false;
                }
            }
        }
    }
}

/// Dedicated per-pipe reader: owns the read half EXCLUSIVELY for the pipe's
/// whole lifetime and does nothing but loop `recv_control`, forwarding each
/// decoded result through `tx`. This exists ONLY to keep `recv_control`'s
/// `read_exact`-based framing off of `tokio::select!` — see
/// `control_supervisor`'s doc comment for why that combination corrupts the
/// stream. `mpsc::Sender::send` never awaits mid-message, so this loop can
/// only ever be cancelled between whole messages (by `abort()`, when the
/// supervisor adopts a replacement pipe), never mid-`read_exact`.
async fn control_reader_loop(
    mut read_half: tokio::io::ReadHalf<NamedPipeServer>,
    tx: mpsc::UnboundedSender<std::io::Result<ControlMsg>>,
) {
    loop {
        let msg = ipc::recv_control(&mut read_half).await;
        let is_err = msg.is_err();
        if tx.send(msg).is_err() {
            return; // supervisor moved on to a different pipe
        }
        if is_err {
            return;
        }
    }
}

/// Long-lived task: adopts each Worker's control pipe in turn (via
/// `pipe_rx`, fed by `MasterHandles::adopt_worker_pipes`) and is the SOLE
/// owner of it — both reading `WorkerReady`/`WorkerConfigured`/`WorkerError`/
/// `PinRelay` replies and writing whatever `WorkerLink::send` queues via
/// `outbound_rx`. `WorkerConfigured` is where `RtpSender` learns the codec/
/// fps the Worker ACTUALLY applied (not just what was requested) — see the
/// approved plan's note on why that handoff has to be two-way.
///
/// The actual pipe read happens in a dedicated `control_reader_loop` task,
/// never inline in this function's `select!`. Confirmed live (2026-07-20):
/// racing `recv_control`'s `read_exact`-based framing directly inside
/// `tokio::select!` against `pipe_rx`/`outbound_rx` corrupts the stream —
/// `select!` drops whichever branch didn't win, and `read_exact` is NOT
/// cancellation-safe: if it had already consumed the length prefix (or part
/// of the payload) from the pipe when dropped, those bytes are gone forever,
/// and the next read starts misaligned, decoding garbage from the middle of
/// a previous frame as a fresh tag byte. `mpsc::Receiver::recv` (what this
/// function races instead) IS cancellation-safe, so this split is required,
/// not just tidier.
async fn control_supervisor(
    mut pipe_rx: mpsc::UnboundedReceiver<NamedPipeServer>,
    mut outbound_rx: mpsc::UnboundedReceiver<ControlMsg>,
    rtp_sender: Arc<Mutex<rtp::RtpSender>>,
    global_pin: Arc<Mutex<(String, String)>>,
    helper_tx: mpsc::UnboundedSender<HelperCmd>,
    helper_ready: Arc<std::sync::atomic::AtomicBool>,
    worker_caps: Arc<Mutex<Option<session_negotiate::WorkerCaps>>>,
    // Needed for ControlMsg::EndSession (tray "End Stream"): the session state
    // the Worker is asking to end lives here, in Master, and ending it means
    // marking it cancelled so the ORDINARY teardown path runs.
    client_info: Arc<Mutex<Option<rtsp::ClientInfo>>>,
    // Cache of the Worker's ControlMsg::DisplayInventory, read by echo_rpc.
    display_seats: echo::rpc::SeatCache,
    // Also needed for EndSession: an Echo client's session lives here and NOT
    // in `client_info`, so a tray "End Stream" that consulted only the latter
    // ended nothing while a phone was streaming.
    echo_sessions: Option<Arc<echo::session::SessionManager>>,
) {
    // Mirrors the Worker's view of the input desktop (ControlMsg::
    // SecureDesktopChanged). While true AND a helper is live, input routes to
    // the SYSTEM helper instead of the Worker.
    let mut secure_desktop = false;
    let mut write_half: Option<tokio::io::WriteHalf<NamedPipeServer>> = None;
    let mut reader_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut incoming_rx: Option<mpsc::UnboundedReceiver<std::io::Result<ControlMsg>>> = None;
    // The last ConfigureStart session_watcher asked for, replayed to every
    // NEWLY connected Worker pipe — not just whichever pipe happened to be
    // live the instant it was sent. Without this, a ConfigureStart sent
    // while the previous Worker's pipe had just died and the next hadn't
    // finished its handshake yet (a real race: SYSTEM-fallback→interactive
    // upgrade, or any respawn racing a client's PLAY) was silently dropped —
    // session_watcher only ever sends once per session_generation, so the
    // Worker that eventually connected was never told what to stream.
    let mut last_configure: Option<ipc::ConfigureStart> = None;
    loop {
        tokio::select! {
            maybe_pipe = pipe_rx.recv() => {
                match maybe_pipe {
                    Some(pipe) => {
                        println!("🔗 Master: worker control pipe connected");
                        if let Some(h) = reader_task.take() { h.abort(); }
                        let (read_half, mut w) = tokio::io::split(pipe);
                        if let Some(mut cs) = last_configure.clone() {
                            // Mid-session Worker adoption: the replacement must
                            // continue the client's frame timeline, not restart
                            // at 1 — the client discards indices behind its
                            // expected next frame FOREVER (permanent black after
                            // a sign-out/upgrade respawn, confirmed live
                            // 2026-08-10). Also never re-launch the app: it is
                            // (at most) already running from the original send.
                            cs.start_frame_index =
                                rtp_sender.lock().unwrap().last_sent_index().wrapping_add(1).max(1);
                            cs.launch_app = false;
                            println!("🔁 Master: replaying ConfigureStart to newly-connected worker \
                                (resuming at wire frame {})", cs.start_frame_index);
                            if let Err(e) = ipc::send_control(&mut w, &ControlMsg::ConfigureStart(cs)).await {
                                println!("⚠️  Master: ConfigureStart replay failed ({e})");
                            }
                        }
                        write_half = Some(w);
                        let (tx, rx) = mpsc::unbounded_channel();
                        reader_task = Some(tokio::spawn(control_reader_loop(read_half, tx)));
                        incoming_rx = Some(rx);
                    }
                    None => return, // Master shutting down
                }
            }
            incoming = async {
                match incoming_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match incoming {
                    Some(Ok(ControlMsg::WorkerReady)) => println!("✅ Master: worker ready"),
                    Some(Ok(ControlMsg::WorkerConfigured(wc))) => {
                        let released = {
                            let mut rtp = rtp_sender.lock().unwrap();
                            rtp.set_fps(wc.fps.max(1));
                            rtp.set_codec(wc.codec == WireCodec::Hevc, wc.codec == WireCodec::Av1);
                            rtp.release_config_hold()
                        };
                        println!("📐 Master: worker configured {}x{}@{}fps codec={:?}{}",
                            wc.width, wc.height, wc.fps, wc.codec, if wc.is_hdr { "/HDR10" } else { "" });
                        if released {
                            // The IDR is mandatory, not tidiness. The GOP is
                            // infinite, so the keyframe emitted at
                            // reconfiguration may already have been dropped by
                            // the hold, and the next one only comes on request —
                            // without this the client waits forever.
                            println!("▶️  Master: configuration confirmed — resuming frames with a fresh IDR");
                            if let Some(w) = write_half.as_mut() {
                                if let Err(e) = ipc::send_control(w, &ControlMsg::RequestIdr).await {
                                    println!("⚠️  Master: IDR request to worker failed ({e})");
                                    write_half = None;
                                }
                            }
                        }
                    }
                    Some(Ok(ControlMsg::WorkerError(e))) => println!("⚠️  Master: worker reported error: {e}"),
                    Some(Ok(ControlMsg::WorkerCapabilities { vdd_capable, native_width, native_height })) => {
                        println!("🧩 Master: worker capabilities — vdd={vdd_capable} native={native_width}x{native_height}");
                        *worker_caps.lock().unwrap() = Some(session_negotiate::WorkerCaps {
                            vdd_capable, native_width, native_height,
                        });
                    }
                    Some(Ok(ControlMsg::DisplayInventory(entries))) => {
                        // Replaces wholesale rather than merging: this is the
                        // Worker's complete current view, and a display that
                        // vanished from it is a display that no longer exists.
                        let seats: Vec<echo::rpc::DisplaySeat> =
                            entries.iter().map(echo::rpc::DisplaySeat::from_entry).collect();
                        println!("🖥️  Master: worker reported {} display(s):", seats.len());
                        for s in &seats {
                            println!("   • {} \"{}\" {}x{}@{}Hz{}{}{}",
                                s.id, s.label, s.width, s.height, s.refresh_hz,
                                if s.is_primary { " primary" } else { "" },
                                if s.virtual_display { " virtual" } else { "" },
                                if s.hdr_active { " HDR-on" } else if s.hdr_capable { " HDR-capable" } else { "" });
                        }
                        *display_seats.lock().unwrap() = seats;
                    }
                    Some(Ok(msg @ ControlMsg::CaptureRect { .. })) => {
                        // Cached by the helper supervisor and replayed to each
                        // helper on connect — see ControlMsg::CaptureRect.
                        let _ = helper_tx.send(HelperCmd::Rect(msg));
                    }
                    Some(Ok(ControlMsg::SecureDesktopChanged { secure })) => {
                        // Only the Worker can see this (Session 0 can't) — it
                        // drives the SYSTEM input helper's whole lifecycle.
                        if secure != secure_desktop {
                            secure_desktop = secure;
                            println!("🔐 Master: input desktop is now {} — {} SYSTEM input helper",
                                if secure { "SECURE (Winlogon)" } else { "interactive" },
                                if secure { "starting" } else { "stopping" });
                            let _ = helper_tx.send(if secure { HelperCmd::Start } else { HelperCmd::Stop });
                        }
                    }
                    Some(Ok(ControlMsg::EndSession)) => {
                        // Tray "End Stream": stop the stream AND put the
                        // desktop back on the physical monitor, in one press.
                        //
                        // This was briefly two presses (stop, then release).
                        // Rejected after using it: "End Stream" reads as one
                        // intention, and stopping a stream while leaving the
                        // monitor dark is a state nobody asked to be in. The
                        // two-stage machinery still exists for the case that
                        // genuinely produces it — a client that disconnects
                        // without saying goodbye leaves the display suspended,
                        // and the tray item relabels itself to "Release
                        // Display" so that state is reachable — but the
                        // deliberate press does the whole job.
                        //
                        // Both kinds of session are ended here, and that is the
                        // original fix: an Echo client's session lives in
                        // `SessionManager`, not in `ClientInfo`, so consulting
                        // only the latter meant "End Stream" during an Echo
                        // stream logged "nothing to end" and the stream kept
                        // running (live log, 2026-08-16).
                        let echo_ended = echo_sessions
                            .as_ref()
                            .is_some_and(|s| {
                                s.force_end(
                                    "tray \"End Stream\"",
                                    echo::session::EndMode::TearDown,
                                )
                            });

                        // Moonlight: hand the session to session_watcher's
                        // active→inactive edge, which resets the RTP timeline
                        // and sends the Deactivate. `cancelled` is that
                        // message's display decision — true tears the VDD down
                        // and restores the monitor.
                        let moonlight_ended = {
                            let mut guard = client_info.lock().unwrap();
                            match guard.as_mut() {
                                Some(info) if info.streaming_active => {
                                    info.streaming_active = false;
                                    info.cancelled = true;
                                    Some(info.session_generation)
                                }
                                _ => None,
                            }
                        };
                        if let Some(gen) = moonlight_ended {
                            println!("🛑 Master: tray \"End Stream\" — ending Moonlight session \
                                {gen} and restoring the physical display");
                            // Ordered AFTER the flags above: the Disconnect
                            // event this produces must never be able to read
                            // as a client leaving a session still marked live.
                            control::request_peer_kick();
                        }

                        // The session is over by the operator's decision, so no
                        // future Worker may inherit it. The Moonlight path's
                        // `cancelled = true` also clears this via the outbound
                        // arm, but an Echo-only session never travels that way,
                        // so the clear is explicit here for both. Phase 16.1's
                        // invariant, kept.
                        if echo_ended || moonlight_ended.is_some() {
                            last_configure = None;
                        } else {
                            println!("ℹ️  Master: tray \"End Stream\" with no active session — \
                                nothing to end");
                        }
                    }
                    Some(Ok(ControlMsg::ClearPaired)) => {
                        // The trust store this revokes is Master-side state; see
                        // pairing::clear_all_paired for why the Worker deleting
                        // the JSON itself would revoke nothing.
                        println!("🗑️  Master: tray \"Clear Paired Devices\" — wiping the trust store");
                        pairing::clear_all_paired();
                    }
                    Some(Ok(ControlMsg::PinRelay { pin, device })) => {
                        // Same handshake point the monolithic host's tray uses:
                        // pairing's getservercert loop polls global_pin every
                        // 200 ms and consumes (clears) it once non-empty.
                        println!("🔑 Master: PIN relay from worker for device \"{device}\" — handing to pairing");
                        *global_pin.lock().unwrap() = (pin, device);
                    }
                    Some(Ok(other)) => println!("⚠️  Master: unexpected control message from worker: {other:?}"),
                    Some(Err(e)) => {
                        println!("🔌 Master: worker control pipe closed/errored ({e}) — waiting for next worker");
                        write_half = None;
                        incoming_rx = None;
                    }
                    None => {
                        // reader task ended (pipe closed, or superseded by a
                        // freshly-adopted pipe that already replaced it above).
                        write_half = None;
                        incoming_rx = None;
                    }
                }
            }
            outgoing = outbound_rx.recv() => {
                match outgoing {
                    Some(msg) => {
                        // Secure-desktop input detour: the Worker's SendInput
                        // is silently swallowed by UIPI at the credential
                        // provider (its primary token is the interactive
                        // user), so while the secure desktop is up and the
                        // SYSTEM helper is live, injection goes THERE instead.
                        // Everything else still goes to the Worker, and if no
                        // helper is available we fall through to the Worker
                        // exactly as before — never worse than the old path.
                        // Gamepad packets deliberately stay with the Worker —
                        // see input::is_gamepad_packet.
                        let mut msg = if secure_desktop
                            && helper_ready.load(std::sync::atomic::Ordering::Acquire)
                        {
                            match msg {
                                ControlMsg::InjectInput(payload)
                                    if !input::is_gamepad_packet(&payload) =>
                                {
                                    let _ = helper_tx.send(HelperCmd::Inject(payload));
                                    continue;
                                }
                                other => other,
                            }
                        } else {
                            msg
                        };
                        // Stamp the wire-timeline start on every outbound
                        // ConfigureStart: 1 for a fresh session (RtpSender was
                        // reset ⇒ last_sent_index == 0), or last+1 when a
                        // session is being re-configured with frames already
                        // on the wire — see ConfigureStart::start_frame_index.
                        if let ControlMsg::ConfigureStart(cs) = &mut msg {
                            cs.start_frame_index =
                                rtp_sender.lock().unwrap().last_sent_index().wrapping_add(1).max(1);
                        }
                        // Recorded BEFORE the send attempt so a dropped/failed
                        // send (no pipe connected yet, or the pipe just died)
                        // still gets replayed to whichever Worker connects
                        // next. Stop and a CANCELLED Deactivate (explicit
                        // app-quit) mean the session is genuinely over — an
                        // unrelated future Worker shouldn't inherit it.
                        //
                        // A non-cancelled Deactivate (bare disconnect/
                        // suspend) must NOT clear it, though — confirmed
                        // live (2026-08-06): a sign-out's Deactivate
                        // (cancelled=false, matching vd.mark_suspended()'s
                        // "stay ready for a fast /resume" semantics) cleared
                        // this, so the interactive Worker that came up after
                        // sign-in got nothing replayed and sat unconfigured
                        // forever — exactly the "video never resumes after
                        // the session swap" symptom. The client's session is
                        // still logically alive in this case; only an
                        // explicit quit should make a future Worker forget.
                        match &msg {
                            ControlMsg::ConfigureStart(cs) => last_configure = Some(cs.clone()),
                            ControlMsg::Stop => last_configure = None,
                            ControlMsg::Deactivate { cancelled: true } => last_configure = None,
                            _ => {}
                        }
                        if let Some(w) = write_half.as_mut() {
                            if let Err(e) = ipc::send_control(w, &msg).await {
                                println!("⚠️  Master: send to worker failed ({e}) — dropping");
                                write_half = None;
                            }
                        }
                        // else: no worker connected right now — ConfigureStart
                        // was remembered above and replayed once one connects.
                    }
                    None => return, // every WorkerLink clone dropped — Master shutting down
                }
            }
        }
    }
}

/// Dedicated per-pipe reader — the media-pipe counterpart of
/// `control_reader_loop`; see its doc comment for why `recv_media`'s
/// `read_exact`-based framing must never be raced inside `tokio::select!`
/// directly (it isn't cancellation-safe, and here the sibling branch is
/// `learn_ticker.tick()` — easily often enough to land mid-frame on a
/// multi-packet HEVC IDR and desync the stream permanently, and now that the
/// ticker runs at 2 ms rather than 500 ms, essentially guaranteed to).
async fn media_reader_loop(
    mut pipe: NamedPipeServer,
    tx: mpsc::UnboundedSender<std::io::Result<ipc::MediaMsg>>,
) {
    loop {
        let msg = ipc::recv_media(&mut pipe).await;
        let is_err = msg.is_err();
        if tx.send(msg).is_err() {
            return; // supervisor moved on to a different pipe
        }
        if is_err {
            return;
        }
    }
}

/// Long-lived task: adopts each Worker's media pipe in turn and forwards
/// video frames to `rtp_sender` — but only once the client's video target is
/// LEARNED and only starting from a real IDR, mirroring the exact
/// `video_learned`/`first_idr_sent` gate the monolithic path ran in-process.
/// That gating relocates here (not the Worker) because `RtpSender` is the
/// thing that actually knows whether the target is learned, and `RtpSender`
/// lives in Master post-split — see the approved plan.
///
/// Also owns `try_learn_target()`'s polling, on its own timer, independent
/// of whether a Worker is even connected right now — a Worker-down gap is
/// exactly when the receive buffer most needs draining (per the approved
/// plan's explicit warning about coupling this to frame arrival).
///
/// The actual pipe read happens in a dedicated `media_reader_loop` task —
/// see `control_supervisor`'s doc comment for why racing `read_exact`-based
/// framing directly inside this function's `select!` corrupted every stream
/// (confirmed live 2026-07-20: "unknown MediaMsg tag 0" on literally the
/// first frame of the first session, every time).
async fn media_supervisor(
    mut pipe_rx: mpsc::UnboundedReceiver<NamedPipeServer>,
    rtp_sender: Arc<Mutex<rtp::RtpSender>>,
    audio_tx: Arc<Mutex<audio::AudioTxState>>,
    // Always present now that a relay-less install can still hold an Echo
    // session (see `start_master_network`). Consulted per frame via a
    // lock-free flag, so a Moonlight session pays one atomic load whether or
    // not an Echo client exists — see `SessionManager::seal_video`.
    echo_sessions: Option<Arc<echo::session::SessionManager>>,
    // Read once a second by the keepalive tick, to answer "is anyone still
    // watching?" — see `session_is_live`. Not on the frame path.
    client_info: Arc<Mutex<Option<rtsp::ClientInfo>>>,
) {
    // 2 ms, not the 500 ms this ran at for Moonlight's sake.
    //
    // This ticker is the ONLY thing that drains the media socket in the split
    // architecture, and `try_learn_target` is where Echo's demux hook lives —
    // so it is also the only thing that hands Echo's control and **input**
    // datagrams to the rest of the host. At 500 ms every input packet waited up
    // to half a second in the receive buffer and then arrived with thirty of
    // its neighbours, which the injector applied in a few milliseconds. That is
    // precisely the reported symptom: a pointer that lags behind the hand and
    // moves in hops. It measured as a ~500 ms control round trip that did not
    // change when the client moved from cellular to the LAN — the tell that it
    // was never the network (live 2026-08-16).
    //
    // 500 ms was correct for its original job: GameStream clients ping every
    // 500 ms and only the most recent one matters. It became wrong the moment
    // this drain also carried real-time input. The cost of 2 ms is one
    // non-blocking `recv_from` returning WouldBlock, ~500 times a second.
    let mut learn_ticker = tokio::time::interval(Duration::from_millis(2));
    // Phase 4 — the actual "survive a Worker respawn" mechanism. Master's own
    // RTSP/control connection to the client never dies on a Worker respawn
    // (sign-out, crash, session change) — the gap that WAS killing the
    // stream is Moonlight's OWN client-side video-frame watchdog: several
    // real seconds of VDD/WGC/DDA bring-up with zero frames arriving reads
    // to it as "connection lost" and it disconnects, even though nothing on
    // Master's side ever actually dropped. Re-sending the last known-good
    // IDR once a second while no Worker is connected keeps frames arriving
    // often enough that the watchdog never trips — a brief freeze on the
    // client's screen instead of a full disconnect. Deliberately NOT tied
    // to video_learned/first_idr_forwarded's reset-on-reconnect below: this
    // needs to keep resending the OLD session's last frame right through
    // that reset, until the NEW worker's own first real frame supersedes it.
    // Cached last IDR (index + bytes) for the keepalive retransmit below.
    let mut last_idr: Option<(u32, Vec<u8>)> = None;
    // Confirmed live (2026-08-06): gating the resend on "no pipe connected"
    // was NOT enough — most of a respawn's dead-air window is spent with the
    // pipe already reconnected but the new Worker still mid-VDD/WGC/DDA
    // bring-up (multiple real seconds), during which this ticker's old gate
    // never fired at all. Track wall-clock time since the last REAL frame
    // instead — that covers both "no pipe" and "pipe up, nothing flowing
    // yet" as the same condition, which is what actually matters to
    // Moonlight's watchdog.
    let mut last_frame_at = Instant::now();
    let mut keepalive_ticker = tokio::time::interval(Duration::from_secs(1));
    let mut reader_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut frame_rx: Option<mpsc::UnboundedReceiver<std::io::Result<ipc::MediaMsg>>> = None;
    let mut video_learned = false;
    let mut first_idr_forwarded = false;
    // Throttle for the Echo audio send-failure notice. A dead path fails on
    // every packet, and at 50 packets a second an unthrottled log line is an
    // amplifier rather than a diagnostic.
    let mut audio_send_failed_at: Option<Instant> = None;
    loop {
        tokio::select! {
            maybe_pipe = pipe_rx.recv() => {
                match maybe_pipe {
                    Some(pipe) => {
                        println!("🔗 Master: worker media pipe connected");
                        if let Some(h) = reader_task.take() { h.abort(); }
                        let (tx, rx) = mpsc::unbounded_channel();
                        reader_task = Some(tokio::spawn(media_reader_loop(pipe, tx)));
                        frame_rx = Some(rx);
                        // Seed from whether a target is ALREADY known rather than
                        // clearing to false: on a mid-session Worker swap the
                        // client never went away, so its address never changes
                        // again and the learn ticker's change-detector would
                        // never re-open this gate — the freeze this whole
                        // handoff exists to avoid. See RtpSender::has_target.
                        video_learned = rtp_sender.lock().unwrap().has_target();
                        // Still require a fresh IDR to open the new Worker's
                        // stream: its encoder is brand new, so its P-frames
                        // reference nothing the client holds.
                        first_idr_forwarded = false;
                    }
                    None => return,
                }
            }
            frame = async {
                match frame_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match frame {
                    Some(Ok(ipc::MediaMsg::VideoFrame { frame_index, frame_type, data })) => {
                        if !video_learned {
                            continue;
                        }
                        let is_idr = frame_type == 2;
                        if !first_idr_forwarded && !is_idr {
                            continue; // don't open the stream with a P-frame
                        }
                        // Encrypt for an Echo session, pass through for
                        // Moonlight. This is the ONLY correct place for the
                        // seal: after the Worker's encoder, before RtpSender
                        // shards the frame — so Reed-Solomon parity covers
                        // ciphertext and the client can repair lost shards
                        // without the key, then authenticate once after
                        // reassembly. Sealing per shard instead would cost a
                        // 16-byte tag on every packet and hand an attacker a
                        // per-packet oracle.
                        //
                        // `frame_index` is the AES-GCM counter, which is why
                        // the wire index has to be the same number the encoder
                        // stamped: a mismatch is an authentication failure on
                        // the client, not a picture glitch.
                        let sealed = echo_sessions
                            .as_ref()
                            .and_then(|s| s.seal_video(frame_index, frame_type, &data));
                        let payload: &[u8] = sealed.as_deref().unwrap_or(&data);

                        // The Worker chose frame_index (== the NVENC timestamp);
                        // Master puts it straight on the wire so the index the
                        // client references matches what invalidation targets.
                        let sent = rtp_sender.lock().unwrap().send_frame(frame_index, payload, frame_type);
                        if sent {
                            first_idr_forwarded = true;
                            last_frame_at = Instant::now();
                            if is_idr {
                                // Cache the PLAINTEXT frame, not the sealed
                                // one: the keepalive retransmit below re-sends
                                // it later, and re-sealing it then produces the
                                // correct nonce for whatever index it goes out
                                // under. Caching ciphertext would replay a
                                // frame under a nonce the client has already
                                // seen, which the tag check would (correctly)
                                // reject.
                                last_idr = Some((frame_index, data));
                            }
                        }
                        // else: send-thread queue saturated (rare/transient) —
                        // the client's own loss-triggered IDR request recovers;
                        // no direct encoder handle to re-request from here
                        // without another IPC round trip, deliberately not
                        // built in this pass.
                    }
                    Some(Ok(ipc::MediaMsg::AudioFrame { data })) => {
                        // No video_learned-style gate needed — AudioTxState's
                        // own send_frame() is already a no-op until IT has
                        // learned a target (via the client's audio pings on
                        // port 48000, independent of the video target).
                        audio_tx.lock().unwrap().send_frame(&data);

                        // Fork, don't switch. The Worker captures the ghost sink
                        // and encodes Opus once; both wires carry those same
                        // bytes. Moonlight's path above is untouched, and an
                        // Echo client gets the packet sealed on its punched path
                        // because it listens on nothing else — port 48000 is a
                        // socket it never opened.
                        //
                        // Unlike video, this is NOT sealed before RtpSender:
                        // audio is one packet per frame with no sharding and no
                        // FEC, so there is nothing for parity to cover and no
                        // reassembly to authenticate after. It goes out whole.
                        if let Some(sessions) = echo_sessions.as_ref() {
                            // Seal under the lock, send outside it — the guard
                            // is dropped by the time send_raw runs, so a slow
                            // socket can never stall a video frame's seal.
                            if let Some((datagram, peer)) = sessions.seal_audio(&data) {
                                if let Err(e) =
                                    rtp_sender.lock().unwrap().send_raw(&datagram, peer)
                                {
                                    // Rate-limited: a broken path fails on every
                                    // packet 50 times a second, and the video
                                    // stream's own telemetry will already be
                                    // saying the same thing far more loudly.
                                    if audio_send_failed_at
                                        .is_none_or(|t: Instant| t.elapsed() > Duration::from_secs(10))
                                    {
                                        println!("🔇 Echo audio send to {peer} failed: {e}");
                                        audio_send_failed_at = Some(Instant::now());
                                    }
                                }
                            }
                        }
                    }
                    Some(Err(e)) => {
                        println!("🔌 Master: worker media pipe closed/errored ({e}) — waiting for next worker");
                        frame_rx = None;
                    }
                    None => {
                        // reader task ended (pipe closed, or superseded by a
                        // freshly-adopted pipe that already replaced it above).
                        frame_rx = None;
                    }
                }
            }
            _ = learn_ticker.tick() => {
                // Drain pings (also keeps a stale backlog from latching the next
                // session onto a dead port — see try_learn_target), then gate on
                // whether a target is KNOWN, not on whether it just changed.
                let learned_now = {
                    let mut tx = rtp_sender.lock().unwrap();
                    tx.try_learn_target();
                    tx.has_target()
                };
                if learned_now && !video_learned {
                    video_learned = true;
                    println!("🎯 Master: learned client video target");
                }
            }
            _ = keepalive_ticker.tick() => {
                // Fires whenever a real frame hasn't landed in over a
                // second, regardless of whether a pipe happens to be
                // connected right now — see last_frame_at's doc comment. An
                // alive, actively-streaming Worker already has its own
                // static-desktop IDR keepalive (run_worker's loop) well
                // under this threshold, so this never fights with it.
                // A session must still exist. The retransmit exists to carry a
                // LIVE client across a Worker respawn's dead air — it is not a
                // reason to keep transmitting after the session is over.
                //
                // Nothing checked that, and the two facts it depends on are both
                // sticky: `video_learned` is only ever set true, and `last_idr`
                // holds the frame forever. So after any session ended, this went
                // on re-sending that one cached IDR — once a second, for as long
                // as Nova ran. It kept finding a target because the departed
                // client's app carries on pinging the media socket, which
                // `try_learn_target` dutifully re-learns. Observed live
                // (2026-08-17): `📦 frame 1 … frame_type=2` at 1-2 fps,
                // indefinitely, minutes after the operator had force-ended the
                // session from the tray.
                //
                // Clearing the cache rather than merely skipping the send: the
                // next session must not be able to inherit the previous one's
                // last frame, whose reference timeline means nothing to it.
                if !session_is_live(&client_info, echo_sessions.as_ref()) {
                    if last_idr.is_some() || video_learned {
                        video_learned = false;
                        first_idr_forwarded = false;
                        last_idr = None;
                    }
                } else if video_learned && last_frame_at.elapsed() >= Duration::from_secs(1) {
                    if let Some((idx, data)) = &last_idr {
                        // Retransmit the cached IDR under its ORIGINAL index — a
                        // genuine retransmit, not a new frame; the client keeps
                        // the same reference timeline.
                        //
                        // Re-sealed rather than cached sealed: the nonce is
                        // derived from the index, and this goes out under the
                        // same index, so re-sealing reproduces a byte-identical
                        // datagram the client accepts as the retransmit it is.
                        let sealed = echo_sessions
                            .as_ref()
                            .and_then(|s| s.seal_video(*idx, 2, data));
                        let payload: &[u8] = sealed.as_deref().unwrap_or(data);
                        let _ = rtp_sender.lock().unwrap().send_frame(*idx, payload, 2);
                    }
                }
            }
        }
    }
}

/// Applies a Master-sent `ConfigureStart`: activates the VDD, switches
/// codec/HDR if needed, rebinds capture+encoder, and reports back what was
/// ACTUALLY achieved (`RtpSender` needs the real values, not just what was
/// requested — see the approved plan). Mirrors the monolithic `run()`'s
/// PLAY-handling block (VDD activate / codec switch / HDR enable /
/// resolution re-snap) — same operations, same order, just reading from
/// `ConfigureStart` instead of `ClientInfo`.
///
/// Simplified relative to `run()`: no `client.activated` pre-activation fast
/// path, since Phase 2 doesn't pre-activate during the `/launch`→PLAY gap
/// (known scope simplification — see `session_watcher`'s doc comment). Every
/// `ConfigureStart` goes through the full activation sequence.
/// True when the input desktop is the secure/Winlogon (or screensaver) desktop,
/// where `SetDisplayConfig` (the VDD/CCD topology changes) is denied with
/// ERROR_ACCESS_DENIED (error 5). Activating the VDD there half-succeeds — the
/// resolution force fails but capture/encoder proceed at the requested size
/// against a VDD that never actually resized, producing a green/garbled half
/// frame (confirmed live 2026-08-09: a client connected at the sign-in screen,
/// the session started on Winlogon, and app-5 activation error-5'd before the
/// user finished logging in). VDD activation must wait for the interactive
/// (Default) desktop.
fn desktop_is_secure() -> bool {
    use capture::desktop_switch::InputDesktop;
    matches!(
        capture::desktop_switch::current_input_desktop(),
        InputDesktop::Secure | InputDesktop::ScreenSaver
    )
}

/// Align the shim's SDR→HDR conversion with the level Windows itself uses.
///
/// The display that matters is the one being ENCODED FOR — the VDD, when it is
/// active in Advanced Color. Windows composites SDR content into that display's
/// HDR surface at its "SDR content brightness", so an FP16 capture arrives
/// already scaled that way. Using the same number when we convert an SDR
/// capture makes the two paths land at identical luminance, which is what stops
/// the brightness jumping when the capture path switches under a UAC prompt
/// (WGC/FP16 → DDA/BGRA8 and back).
///
/// Deliberately NOT the *source* display's level when no VDD is active: a
/// display that isn't in Advanced Color reports the 80-nit default, which would
/// render the logon screen dimmer than reference. With nothing to match, the
/// shim's BT.2408 default (203 nits) is the right answer, so leave it alone.
fn refresh_sdr_white_level(vd: &virtual_display::VirtualDisplay) {
    let Some(device) = vd.active_device_name() else { return };
    if let Some(level) = virtual_display::VirtualDisplay::query_sdr_white_level(device) {
        println!("🔆 SDR white level for {device}: {:.0} nits — matching the shim's \
            SDR→HDR conversion to it", level * 80.0);
        encoder::set_sdr_white_level(level);
    }
}

fn apply_configure_start(
    cs: &ipc::ConfigureStart,
    vd: &mut virtual_display::VirtualDisplay,
    capturer: &mut capture::DesktopManager,
    enc: &mut Encoder,
    cfg: &config::NovaConfig,
) -> std::result::Result<ipc::WorkerConfigured, String> {
    let codec = match cs.codec {
        ipc::WireCodec::H264 => encoder::Codec::H264,
        ipc::WireCodec::Hevc => encoder::Codec::Hevc,
        ipc::WireCodec::Av1  => encoder::Codec::Av1,
    };
    if codec != enc.config.codec {
        println!("🎥 Codec selected by Master: {} — switching encoder", codec.as_str());
        enc.config.codec = codec;
        enc.config.is_hdr = false; // reset; re-armed below if HDR is also confirmed
        enc.cleanup();
        *enc = Encoder::new(capturer.device(), EncoderConfig {
            width: cs.width as i32,
            height: cs.height as i32,
            fps: enc.config.fps,
            bitrate_kbps: enc.config.bitrate_kbps,
            codec,
            is_hdr: false,
        }).map_err(|e| format!("Failed to recreate NVENC for codec change: {e}"))?;
        let (ox, oy) = capturer.origin();
        input::set_active_capture_rect(ox, oy, capturer.width(), capturer.height());
    }
    enc.config.fps = cs.fps as i32;
    enc.config.bitrate_kbps = cs.bitrate_kbps as i32;

    // Publish the session's CBR target so congestion control can work at all.
    // `encoder::signal_congestion_reduction` (called on the control thread when
    // Master relays a PT_LOSS_STATS report) computes its reduction from
    // STREAM_BITRATE_KBPS and does NOTHING while that reads 0 — which is
    // exactly what happened for the whole Master/Worker era: the Worker never
    // set it, so every congestion signal was silently discarded and the
    // encoder held full bitrate straight through a saturated link. Together
    // with the reduce/ramp block in the Worker's frame loop, this is what
    // makes dynamic QoS live in the split deployment.
    //
    // `enc.config.bitrate_kbps` stays the negotiated CEILING (reconfigure
    // never rewrites it), so the frame loop uses it as the ramp-back target
    // while this atomic tracks what is CURRENTLY applied.
    encoder::set_stream_bitrate_kbps(cs.bitrate_kbps as i32);

    let system_fallback = service::is_system_fallback();
    // Defer VDD activation while the SECURE desktop is up: SetDisplayConfig is
    // denied there (error 5) for every identity, and half-activating garbles
    // the frame — see desktop_is_secure(). The capture loop re-runs this
    // ConfigureStart the moment the desktop returns to Default (post-login), so
    // the VDD comes up cleanly then instead.
    //
    // The DESKTOP is the gate, not the Worker's identity. The old code also
    // skipped the VDD whenever this was the SYSTEM-fallback Worker, on the
    // strength of a 2026-08-06 observation of error 5 — but that observation
    // was made pre-login, i.e. on the secure desktop, so it was really this
    // same restriction. Excluding SYSTEM outright turned out to be load-bearing
    // in the worst way: when the signed-in user is a STANDARD account the
    // elevated Worker can never start (ERROR_ELEVATION_REQUIRED — live
    // 2026-08-11), so the SYSTEM Worker is the ONLY Worker that session will
    // ever have, and refusing to let it drive the VDD left those users with a
    // dead screen. If the CCD calls really are denied here, the existing Err
    // arm below logs it and streams the physical display instead — no worse
    // than skipping, and now it can actually succeed.
    let defer_for_secure = desktop_is_secure();
    if app_launcher::uses_virtual_display(cs.app_id, cfg.stream.headless_for_all_apps)
        && !defer_for_secure
    {
        // HOT RESUME: a client that dropped and came back inside the detach
        // grace window is looking at a display that is already exactly right.
        // Rebuilding it would cost seconds of visible desktop rearrangement to
        // arrive where we started — see `resume_suspended`, which verifies the
        // committed mode out of CCD rather than trusting what we last asked for,
        // and refuses (falling through to the full path) on any doubt.
        //
        // Only the display is skipped. The encoder is still rebound below, which
        // it must be: the returning client may have negotiated a different
        // codec, HDR mode or bitrate, and its decoder needs a fresh IDR either
        // way.
        let resumed = match vd.resume_suspended(cs.width, cs.height, cs.fps) {
            virtual_display::Resume::Reused => true,
            virtual_display::Resume::Mismatch(why) => {
                if vd.active_device_name().is_some() {
                    println!("🖥️  Cannot reclaim the existing virtual display ({why}) — full activation");
                }
                false
            }
        };

        if resumed {
            // Still armed even though nothing is flipping. The detach already
            // ran the claim-once restore (via the audio manager's
            // stop_and_release), so the current default endpoint is the real
            // speaker again and this captures the right target — see audio.rs.
            // Cheap and idempotent; skipping it would leave the invariant
            // "armed before a session touches audio" true only by luck.
            audio::arm_endpoint_restore();
            // The label follows whoever holds the display now. A detached
            // session can be reclaimed by a DIFFERENT paired device, and this is
            // a registry property write rather than a topology change — the
            // expensive part of activation is the devnode cycle and the GDI
            // wait, neither of which happens here.
            if !cs.device_name.is_empty() {
                if let Err(e) = vd.rename_devnode(&cs.device_name) {
                    println!("⚠️  Monitor rename: {e}");
                }
            }
            // The display is already up and already ours, so the capture side is
            // all that needs re-pointing. Same call the Ok arm below makes.
            rebind_capture_and_encoder(capturer, enc, vd.active_device_name(),
                Some((cs.width, cs.height)), Some((cs.width, cs.height)), Some(false))?;
        } else {
            println!("🖥️  Activating virtual display for upcoming session ({}x{}@{}fps{})",
                cs.width, cs.height, cs.fps, if cs.hdr_confirmed { " HDR10" } else { "" });
            // Capture the restore target BEFORE the VDD flip — see audio.rs's
            // arm_endpoint_restore doc comment for why this ordering matters.
            audio::arm_endpoint_restore();
            match vd.activate_for_stream(cs.width, cs.height, cs.fps) {
                Ok(()) => {
                    if !cs.device_name.is_empty() {
                        if let Err(e) = vd.rename_devnode(&cs.device_name) {
                            println!("⚠️  Monitor rename: {e}");
                        }
                    }
                    // The VDD is up but not yet in Advanced Color — that flip (and
                    // the FP16 rebind that follows it) happens in the HDR block below.
                    rebind_capture_and_encoder(capturer, enc, vd.active_device_name(),
                        Some((cs.width, cs.height)), Some((cs.width, cs.height)), Some(false))?;
                }
                Err(e) => {
                    println!("⚠️  Virtual display activation failed: {e} — streaming from the physical display");
                }
            }
        }
    } else {
        if app_launcher::uses_virtual_display(cs.app_id, cfg.stream.headless_for_all_apps) {
            if defer_for_secure {
                println!("🖥️  VDD activation deferred — secure desktop up; will activate on return \
                    to the interactive desktop (post-login). Streaming the physical display, \
                    scaled to the session's {}x{}.", cs.width, cs.height);
            } else if system_fallback {
                println!("🖥️  No VDD on this host (pre-login SYSTEM fallback) — streaming the \
                    physical display, scaled to the session's {}x{}.", cs.width, cs.height);
            }
        }
        // No wait-for-monitor hint (nothing is driving the VDD to that size),
        // but the ENCODER still holds the session's geometry — the shim scales
        // the physical capture up into it. This is what makes the logon screen
        // a full-frame 4K stream instead of a small image in the corner.
        // Physical display, which is not in Advanced Color — capture SDR and let
        // the shim convert. See rebind_capture_and_encoder's capture_hdr note.
        rebind_capture_and_encoder(capturer, enc, None, None,
            Some((cs.width, cs.height)), Some(false))?;
    }

    // Launch the session's app HERE — in the Worker (user session), AFTER the
    // VDD activation above. In true-headless the virtual monitor is now the
    // primary (and only) active display, so the app's window can only open on
    // the virtual desktop space; each launcher then self-foregrounds (Steam
    // Big Picture, RetroArch -f fullscreen, Xbox app via Win+F11). Master's
    // pairing handler deliberately skipped its own launch_app call for this
    // (see app_launcher::LAUNCH_VIA_WORKER) — launching from the SYSTEM
    // Master would land the app in the service's session, invisible to the
    // stream and blind to the VDD topology.
    if cs.launch_app {
        println!("🚀 Worker: launching app {} onto the active display", cs.app_id);
        app_launcher::launch_app(cs.app_id);
    }

    // HDR10 gate. The ENCODER's HDR-ness is fixed by the session (the client
    // built a Main10/PQ decoder), so it follows `cs.hdr_confirmed` alone —
    // never the capture. Putting the VDD into FP16/Advanced Color is a separate,
    // best-effort quality step: when it isn't possible (no VDD on this host, or
    // the secure desktop is denying CCD calls) the capture stays SDR BGRA8 and
    // the shim's sRGB→BT.2020-PQ path converts it, so the stream is still valid
    // HDR10 the client can decode without ever renegotiating.
    // Advanced Color is only reachable when the VDD is actually up (same CCD
    // restriction as the activation above — desktop-gated, not identity-gated).
    let vdd_hdr_possible = !defer_for_secure
        && vd.active_device_name().is_some()
        && (cfg.stream.enable_hdr || vd.is_advanced_color_supported());
    if cs.hdr_confirmed && enc.config.codec == encoder::Codec::Hevc && !enc.config.is_hdr {
        if vdd_hdr_possible {
            println!("🎨 HEVC Main10/HDR10 encoder active (VDD switching to FP16 mode)");
            let _ = vd.set_active_display_hdr(true);
        } else {
            println!("🎨 HEVC Main10/HDR10 encoder active — capture stays SDR for now \
                (no VDD/Advanced Color here); the shim converts sRGB → BT.2020 PQ");
        }
        enc.config.is_hdr = true;
        enc.cleanup();
        *enc = Encoder::new(capturer.device(), EncoderConfig {
            width: cs.width as i32,
            height: cs.height as i32,
            fps: enc.config.fps,
            bitrate_kbps: enc.config.bitrate_kbps,
            codec: enc.config.codec,
            is_hdr: true,
        }).map_err(|e| format!("Failed to recreate NVENC for HDR: {e}"))?;
        // Only wait on the monitor when we actually drove it. `capture_hdr`
        // tracks whether Advanced Color is genuinely on: FP16 frames only exist
        // when it is, and asking for them otherwise is what made the picture
        // pulse between dim and bright.
        let wait = if vdd_hdr_possible { Some((cs.width, cs.height)) } else { None };
        let target = if vdd_hdr_possible { vd.active_device_name() } else { None };
        rebind_capture_and_encoder(capturer, enc, target, wait,
            Some((cs.width, cs.height)), Some(vdd_hdr_possible))?;
    } else if !cs.hdr_confirmed && enc.config.is_hdr {
        // Client did not confirm HDR (or Master re-sent a config without it).
        println!("⚠️  Reverting to SDR (HDR not confirmed for this session)");
        let _ = vd.set_active_display_hdr(false);
        enc.config.is_hdr = false;
        let wait = if vdd_hdr_possible { Some((cs.width, cs.height)) } else { None };
        let target = if vdd_hdr_possible { vd.active_device_name() } else { None };
        rebind_capture_and_encoder(capturer, enc, target, wait,
            Some((cs.width, cs.height)), Some(false))?;
    }

    // Resolution guard — if wait_for_display_resolution timed out inside
    // activate_for_stream, re-snap and rebind once more, exactly as run()'s
    // monolithic path does. Also skipped under SYSTEM-fallback: the mismatch
    // here is EXPECTED (streaming the physical primary's own resolution,
    // never having attempted the VDD at all) rather than a stuck VDD force,
    // and re_snap_resolution is itself another VDD/CCD call that would just
    // fail the same way as the activation attempt above.
    //
    // Looped up to 3x rather than once — confirmed live (2026-08-06): a VDD
    // freshly re-enabled after a devnode cycle sometimes needs longer than
    // one 3s wait_for_display_resolution cycle (activate_for_stream's) PLUS
    // one re-snap's own 3s cycle to actually commit the requested mode; a
    // THIRD, fully independent activation attempt (a later reconnect)
    // succeeded cleanly on the same box where two straight attempts had
    // both still read back the stale resolution. Each iteration exits
    // immediately once the size matches (wait_for_display_resolution's own
    // polling), so this costs nothing extra in the common case — it only
    // extends the worst-case wait for a slow-to-settle VDD instead of
    // giving up and streaming the wrong resolution.
    // Also skipped while deferred: forcing the VDD to cs.width×height is exactly
    // the CCD call that's denied on the secure desktop.
    //
    // Note this now compares the CAPTURE, not the encoder — the encoder is
    // always already at the session's size. A capture that doesn't match is no
    // longer a correctness problem (the shim scales it), just a sharpness one,
    // so this is a quality retry rather than the mismatch guard it used to be.
    if !defer_for_secure && vd.active_device_name().is_some() {
        for attempt in 1..=3 {
            if capturer.width() == cs.width && capturer.height() == cs.height {
                break;
            }
            println!("📐 Resolution re-snap (attempt {attempt}/3): capture={}x{} target={}x{}@{}fps — retrying VDD force",
                capturer.width(), capturer.height(), cs.width, cs.height, cs.fps);
            vd.re_snap_resolution(cs.width, cs.height, cs.fps);
            rebind_capture_and_encoder(capturer, enc, vd.active_device_name(),
                Some((cs.width, cs.height)), Some((cs.width, cs.height)),
                Some(vdd_hdr_possible && enc.config.is_hdr))?;
        }
    }

    // Advanced Color (if any) is settled by now, so the level we read is the
    // one Windows will actually composite SDR content at for this session.
    refresh_sdr_white_level(vd);

    Ok(ipc::WorkerConfigured {
        width: enc.config.width as u32,
        height: enc.config.height as u32,
        fps: enc.config.fps as u32,
        codec: to_wire_codec(enc.config.codec),
        is_hdr: enc.config.is_hdr,
    })
}

/// Enumerate the Worker's live display topology and report it to Master.
///
/// Called at Worker startup and after anything that can change the topology
/// (a session's VDD activation, a teardown). Only the Worker can produce this
/// — see `ControlMsg::DisplayInventory` — and it is the foundation of Echo's
/// targetable seat list.
///
/// Best-effort by design: a failed enumeration reports nothing rather than
/// erroring a session path, and Master keeps its previous view. Sending an
/// empty list would be worse than staying quiet, since "no displays" is
/// indistinguishable from a genuinely headless box.
fn publish_display_inventory(
    reply_tx: &mpsc::UnboundedSender<ipc::ControlMsg>,
    vd: &virtual_display::VirtualDisplay,
) {
    let displays = vd.enumerate_displays();
    if displays.is_empty() {
        return;
    }
    let entries: Vec<ipc::DisplayEntry> = displays
        .iter()
        .map(|d| ipc::DisplayEntry {
            device_name: d.device_name.clone(),
            label: d.label.clone(),
            width: d.width,
            height: d.height,
            refresh_hz: d.refresh_hz,
            is_primary: d.is_primary,
            is_virtual: d.is_virtual,
            hdr_active: d.hdr_active,
            hdr_capable: d.hdr_capable,
        })
        .collect();
    println!("🖥️  Worker: reporting {} display(s) to Master", entries.len());
    let _ = reply_tx.send(ipc::ControlMsg::DisplayInventory(entries));
}

/// Applies a Master-sent `Deactivate`: mirrors the monolithic `run()`'s
/// disconnect-teardown block (stop audio, stop virtual input, scorched-earth
/// the encoder, then either fully deactivate the VDD — `cancelled` — or just
/// suspend it at its current resolution for a fast /resume reconnect).
/// Always rebuilds a fresh SDR encoder afterward so `enc` is never left in a
/// torn-down state between sessions — same reasoning as
/// `recreate_encoder_for_capture`.
///
/// This exists because nothing previously told the Worker a session ended
/// short of the whole process dying (confirmed live 2026-07-20: closing the
/// app left the VDD primary and the physical monitor dark) — Master owns
/// `ClientInfo`/`streaming_active` now, and `session_watcher` is the one
/// place that can see the active→inactive edge; see its doc comment.
fn deactivate_worker(
    cancelled: bool,
    vd: &mut virtual_display::VirtualDisplay,
    capturer: &mut capture::DesktopManager,
    enc: &mut Encoder,
    audio_manager: &mut audio::AudioCaptureManager,
) {
    audio_manager.stop_and_release();
    input::stop_session();
    // Session over: park the congestion-control target so a late
    // PT_LOSS_STATS relay can't compute a reduction against a dead session
    // (mirrors the monolithic path's identical reset on disconnect).
    encoder::set_stream_bitrate_kbps(0);
    // Tray goes back to its idle icon/tooltip. Done here rather than at the
    // call sites so no future teardown path can leave the tray advertising a
    // session that has already been torn down.
    stats::session_ended();

    let was_hdr = enc.config.is_hdr;
    enc.config.is_hdr = false;
    enc.cleanup();
    if was_hdr {
        if let Err(e) = vd.set_active_display_hdr(false) {
            println!("⚠️  Advanced Color disable on disconnect: {e}");
        }
    }

    if cancelled {
        println!("🛑 /cancel — tearing down virtual display, restoring host topology");
        if let Err(e) = vd.deactivate_after_stream() {
            println!("⚠️  Virtual display deactivation failed: {e}");
        }
        let _ = capturer.rebind(None, false, None);
    } else {
        println!("⏸️  Client disconnected — encoder torn down; VDD active for /resume reconnect");
        vd.mark_suspended();
        let _ = capturer.rebind(vd.active_device_name(), false, None);
    }

    match Encoder::new(capturer.device(), EncoderConfig {
        width: capturer.width() as i32,
        height: capturer.height() as i32,
        fps: enc.config.fps,
        bitrate_kbps: enc.config.bitrate_kbps,
        codec: enc.config.codec,
        is_hdr: false,
    }) {
        Ok(new_enc) => *enc = new_enc,
        Err(e) => println!("❌ Failed to rebuild encoder after disconnect: {e}"),
    }
    let (ox, oy) = capturer.origin();
    input::set_active_capture_rect(ox, oy, capturer.width(), capturer.height());

    // Tell the tray whether anything is still held. Read from the virtual
    // display itself rather than from `cancelled`, so a teardown that failed
    // partway leaves the menu offering to finish the job instead of claiming
    // the desktop is already back.
    stats::set_teardown_pending(vd.active_device_name().is_some());
}

/// Advance the frame-pacing deadline after a slot has been served, dropping
/// missed slots instead of repaying them.
///
/// **The bug this exists to prevent** (live-confirmed 2026-08-06, 4K120 HDR):
/// the loops previously did `next_frame_time += frame_interval` unconditionally.
/// When anything blocked the loop past its deadline — VDD/CCD activation, an
/// encoder recreate, a WGC↔DDA swap, host disk/GPU contention — the deadline
/// stayed in the past, so the loop stopped sleeping entirely and emitted frames
/// flat out until every missed slot had been repaid. One stalled second (64 fps)
/// produced a TWELVE-second burst peaking at **179 fps on a 120 fps session**.
/// Because NVENC's CBR budgets per frame at the configured fps, the wire rate
/// scaled with it: 148–160 Mbps against a 90 Mbps negotiation, which floods the
/// client's decode queue (freeze, then a catch-up jump) and saturates the link,
/// whose loss causes the next stall. Self-sustaining.
///
/// Behaviour: normally returns `deadline + interval`, preserving exact cadence
/// (and absorbing sub-frame slips, which self-correct within one frame). Only
/// when that would still be in the past — a real stall — does it re-base on
/// `now`, discarding the accumulated debt so it can never build up across
/// repeated stalls.
fn advance_frame_deadline(deadline: Instant, interval: Duration, now: Instant) -> Instant {
    let next = deadline + interval;
    if next < now {
        now + interval
    } else {
        next
    }
}

/// Rate-limited "the desktop has gone static" diagnostic, shared by both
/// capture loops.
///
/// The guard this replaces (`streak == 1 || streak % 300 == 0`) *looked* like
/// "once per episode, then every ~5 s", but `timeout_streak` resets on every
/// delivered frame — so a 60 Hz source polled at 120 fps tripped `streak == 1`
/// on every other slot. Live 2026-08-06: **55,203 lines / 2.3 MB in one
/// session**, i.e. a blocking `WriteFile` ~60×/s on the TIME_CRITICAL capture
/// thread. That is exactly the hot-path cost Phase 15.4 removed from the
/// `[ENC]` line, and here it was itself causing the missed frame deadlines
/// behind the pacing catch-up bursts — so the throttle is a correctness fix,
/// not just log hygiene.
fn log_static_desktop(
    backend: capture::BackendKind,
    streak: u32,
    last_logged: &mut Option<Instant>,
) {
    /// Only a genuinely motionless screen is worth a line: ~0.25 s at 120 fps.
    /// Normal alternating hit/miss slots never get near this.
    const MIN_STREAK: u32 = 30;
    const INTERVAL: Duration = Duration::from_secs(5);

    if streak < MIN_STREAK {
        return;
    }
    let due = match last_logged {
        Some(t) => t.elapsed() >= INTERVAL,
        None => true,
    };
    if due {
        *last_logged = Some(Instant::now());
        println!("⏳ {backend:?}: static desktop (streak {streak})");
    }
}

/// Commands the dedicated `nova-worker-control` thread (see `run_worker`)
/// hands to the main capture/encode loop. Everything else the control pipe
/// can carry (`InjectInput`/`RequestIdr`/`CongestionReduce`) is applied
/// directly on that thread instead — `InjectInput` specifically MUST be,
/// since `input.rs`'s `sync_desktop_for_input` is thread-affine (see its doc
/// comment) and that dedicated thread is this process's equivalent of the
/// monolithic path's single long-lived ENet control thread.
enum WorkerCommand {
    Configure(ipc::ConfigureStart),
    Deactivate { cancelled: bool },
    Stop,
}

/// `--system-input-helper` entry point: the SYSTEM-primary-token injector that
/// makes remote input reach the Winlogon/PIN screen.
///
/// Deliberately tiny — no VDD, no capture, no encoder, no audio, no tray, no
/// singleton mutex, no display-restore hooks. It connects to Master's input
/// pipe, attaches its (single, window-free) thread to whatever desktop has
/// input focus, and injects what Master forwards. Master spawns it when the
/// Worker reports a secure-desktop transition and kills it when the desktop
/// reverts, so a SYSTEM process capable of synthesising input exists only for
/// the seconds it is actually needed.
///
/// See `input.rs`'s UIPI note for why this process has to exist at all, and
/// `service::spawn_input_helper` for how its SYSTEM primary token is built.
///
/// **Must run on a current-thread runtime** (see `bin/nova-server.rs`): the
/// desktop attachment is thread-affine thread-local state, so the recv→inject
/// loop has to stay on one OS thread for its whole life. It is driven inline
/// here rather than via `tokio::spawn` for exactly that reason.
pub async fn run_input_helper() -> Result<()> {
    println!("=== Nova SYSTEM input helper (secure-desktop injection) ===");

    // Follow the input desktop unconditionally — this process has no
    // desktop-switch monitor and only exists during secure interludes.
    input::set_always_follow_input_desktop();

    let mut pipe = ipc::connect_to_master_input().await.map_err(|e| {
        println!("❌ Input helper could not connect to Master's input pipe: {e}");
        windows::core::Error::from(windows::Win32::Foundation::E_FAIL)
    })?;
    println!("🔌 Input helper connected to Master");

    // Attach up front so the very first keystroke lands, instead of paying a
    // rejected-injection round trip to discover the desktop.
    input::attach_to_input_desktop();

    let mut injected = 0u64;
    loop {
        match ipc::recv_control(&mut pipe).await {
            Ok(ipc::ControlMsg::InjectInput(bytes)) => {
                // Defence in depth: Master already keeps gamepad packets on
                // the Worker (input::is_gamepad_packet). If one ever reaches
                // here, drop it rather than letting this process open its own
                // ViGEm client and materialise a second virtual controller.
                if input::is_gamepad_packet(&bytes) {
                    continue;
                }
                input::handle_input_packet(&bytes);
                injected += 1;
                if injected == 1 {
                    println!("⌨️  Input helper: first packet injected on the secure desktop");
                }
            }
            Ok(ipc::ControlMsg::CaptureRect { origin_x, origin_y, width, height }) => {
                // Without this the absolute-mouse mapping has no rect to work
                // with and drops every move — the "cursor frozen mid-screen at
                // the UAC prompt" bug. Master sends it before opening the
                // injection path, and again on any change.
                println!("🖱️  Input helper: capture rect {width}x{height} at ({origin_x},{origin_y})");
                input::set_active_capture_rect(origin_x, origin_y, width, height);
            }
            Ok(ipc::ControlMsg::Stop) => {
                println!("🛑 Input helper: Master requested stop ({injected} packets injected)");
                return Ok(());
            }
            Ok(other) => println!("⚠️  Input helper: unexpected message {other:?}"),
            Err(e) => {
                // Master dropped the pipe — the secure desktop ended, or
                // Master is shutting down. Either way this process is done.
                println!("🔌 Input helper: pipe closed ({e}) — exiting ({injected} packets injected)");
                return Ok(());
            }
        }
    }
}

/// Commands for [`input_helper_supervisor`].
enum HelperCmd {
    /// Secure desktop is up — create the pipe and spawn the SYSTEM helper.
    Start,
    /// Secure desktop ended (or Master is done) — stop and kill the helper.
    Stop,
    /// Forward one Moonlight INPUT_DATA payload to the helper.
    Inject(Vec<u8>),
    /// The Worker's active capture rect. Cached and (re)sent to every helper
    /// on connect — the helper needs it to map absolute mouse positions (see
    /// `ipc::ControlMsg::CaptureRect`).
    Rect(ControlMsg),
}

/// Owns the SYSTEM input helper's whole lifecycle: its pipe, its process, and
/// the `ready` flag `control_supervisor` consults when deciding where to route
/// injection. One task so the process handle and the pipe can never disagree
/// about whether a helper is live.
///
/// Failure is always survivable: if the spawn or the connect fails, `ready`
/// stays false and `control_supervisor` keeps routing input to the Worker —
/// i.e. exactly the pre-helper behaviour (attached-but-swallowed at the PIN
/// screen), never worse.
async fn input_helper_supervisor(
    mut rx: mpsc::UnboundedReceiver<HelperCmd>,
    ready: Arc<std::sync::atomic::AtomicBool>,
) {
    use std::sync::atomic::Ordering;

    let mut helper: Option<service::InputHelper> = None;
    let mut pipe: Option<NamedPipeServer> = None;
    // Last capture rect the Worker reported. Replayed to each helper right
    // after it connects, BEFORE any injection — without it the helper maps
    // absolute mouse positions onto a 0×0 rect and drops them all.
    let mut last_rect: Option<ControlMsg> = None;

    while let Some(cmd) = rx.recv().await {
        match cmd {
            HelperCmd::Rect(msg) => {
                last_rect = Some(msg.clone());
                // A rect change mid-interlude (backend swap, resolution
                // change) must reach the live helper immediately.
                if let Some(p) = pipe.as_mut() {
                    if let Err(e) = ipc::send_control(p, &msg).await {
                        println!("⚠️  Input helper: capture-rect send failed ({e})");
                    }
                }
            }
            HelperCmd::Start => {
                if helper.is_some() {
                    continue; // already running for this interlude
                }
                let server = match ipc::create_input_pipe() {
                    Ok(s) => s,
                    Err(e) => {
                        println!("⚠️  Input helper: could not create the input pipe ({e}) — \
                            falling back to Worker-side injection");
                        continue;
                    }
                };
                match service::spawn_input_helper() {
                    Ok(h) => match ipc::accept_input_helper(&server).await {
                        Ok(()) => {
                            let mut server = server;
                            // Rect FIRST, before `ready` opens the injection
                            // path — otherwise the first packets race it and
                            // get dropped by the helper's 0×0 rect guard.
                            if let Some(rect) = last_rect.as_ref() {
                                if let Err(e) = ipc::send_control(&mut server, rect).await {
                                    println!("⚠️  Input helper: initial capture-rect send failed ({e})");
                                }
                            } else {
                                println!("⚠️  Input helper: no capture rect known yet — \
                                    absolute mouse moves will be dropped until the Worker reports one");
                            }
                            println!("🔐 Input helper: SYSTEM injector live in the console session");
                            helper = Some(h);
                            pipe = Some(server);
                            ready.store(true, Ordering::Release);
                        }
                        Err(e) => {
                            println!("⚠️  Input helper did not connect ({e}) — killing it, \
                                falling back to Worker-side injection");
                            h.terminate();
                        }
                    },
                    Err(e) => println!("⚠️  Input helper spawn failed ({e}) — \
                        falling back to Worker-side injection"),
                }
            }
            HelperCmd::Stop => {
                ready.store(false, Ordering::Release);
                if let Some(mut p) = pipe.take() {
                    // Best-effort graceful exit; the terminate below is the
                    // backstop. Dropping the pipe alone would also end it.
                    let _ = ipc::send_control(&mut p, &ControlMsg::Stop).await;
                }
                if let Some(h) = helper.take() {
                    h.terminate();
                    println!("🔐 Input helper: stopped (interactive desktop restored)");
                }
            }
            HelperCmd::Inject(bytes) => {
                let Some(p) = pipe.as_mut() else { continue };
                if let Err(e) = ipc::send_control(p, &ControlMsg::InjectInput(bytes)).await {
                    println!("⚠️  Input helper: send failed ({e}) — dropping it, \
                        reverting to Worker-side injection");
                    ready.store(false, Ordering::Release);
                    pipe = None;
                    if let Some(h) = helper.take() {
                        h.terminate();
                    }
                }
            }
        }
    }
}

/// Worker entry point (Session-Survival Architecture, Phase 2 — see the
/// approved plan, `transient-snuggling-cosmos.md`). Everything session-bound
/// that used to live in `run()`'s single process: VDD, capture, NVENC,
/// WASAPI/Opus audio (unchanged — audio's own IPC extraction is Phase 3,
/// audio stays fully in-Worker on its own UDP socket for now), and input
/// injection. Driven entirely by IPC from Master instead of reading
/// `ClientInfo`/owning the RTSP/control/pairing/mDNS servers.
///
/// Known Phase 2 scope simplifications (not oversights — see the approved
/// plan and `session_watcher`'s doc comment): no pre-activation latency
/// hiding, and one `ConfigureStart` per Worker process lifetime (no in-place
/// session-end-then-wait-for-next-session — a new session after this one
/// ends gets a freshly-spawned Worker instead, exactly like today's
/// crash/session-change respawn path already does). Full in-place session
/// lifecycle (suspend/resume without a respawn) is Phase 4 scope.
pub async fn run_worker() -> Result<()> {
    // ── Process-wide setup — identical to run()'s equivalent block; the
    // Worker is still the process actually doing the TIME_CRITICAL capture/
    // encode work, so it needs the same scheduling/timer treatment. ─────────
    unsafe {
        use windows::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
        };
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
    }
    unsafe {
        use windows::Win32::System::Threading::{
            GetCurrentProcess, SetProcessInformation, PROCESS_INFORMATION_CLASS,
        };
        #[repr(C)]
        struct ProcessPowerThrottlingState { version: u32, control_mask: u32, state_mask: u32 }
        let mut pt = ProcessPowerThrottlingState { version: 1, control_mask: 0x1, state_mask: 0 };
        let _ = SetProcessInformation(
            GetCurrentProcess(), PROCESS_INFORMATION_CLASS(4),
            std::ptr::addr_of_mut!(pt).cast(), std::mem::size_of::<ProcessPowerThrottlingState>() as u32,
        );
    }
    unsafe {
        use windows::Win32::Media::timeBeginPeriod;
        let _ = timeBeginPeriod(1);
    }
    debug::init_debug_logger();
    println!("=== Nova Worker (Session-Survival Architecture, Phase 2) ===");

    let _host_singleton = match service::acquire_host_singleton() {
        Ok(guard) => guard,
        Err(msg) => {
            println!("🚫 {msg}");
            return Ok(());
        }
    };
    {
        let wide = debug::log_path_wide();
        encoder::init_shim_log(wide.as_ptr());
    }
    debug::log_shim_dll_path();

    if service::is_system_fallback() {
        println!("🛡️  Running as SYSTEM (pre-login fallback) — VDD lifecycle + HDR10 control \
            available; WGC unavailable until a user signs in (DDA covers the logon screen)");
    } else if unsafe { windows::Win32::UI::Shell::IsUserAnAdmin() }.as_bool() {
        println!("🛡️  Elevated token confirmed — VDD lifecycle + HDR10 control available");
    } else {
        println!("❌ NOT ELEVATED — virtual display activation and HDR10 switching WILL fail.");
    }

    // System tray: the Worker is the session-visible process now (Master runs
    // headless in Session 0 as the service), so this is the only place left
    // that CAN show a tray icon — without it the user has no on-screen sign
    // Nova is running at all except Task Manager. Spawned early, same as the
    // monolithic run()'s "before anything else" placement, so it's visible
    // through the VDD/WGC bring-up below rather than only after it succeeds.
    // The Master relays pairing's dialog request here as
    // ControlMsg::OpenPairDialog (handled in the control thread below), and a
    // watcher task forwards whatever the dialog writes into tray_global_pin
    // back to the Master as ControlMsg::PinRelay — the Worker is the only
    // session-visible process, so its tray is the only place the PIN dialog
    // can exist under the split.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let shutdown_tx = Arc::new(shutdown_tx);
    let (tray_tx, tray_rx) = std::sync::mpsc::sync_channel::<tray::TrayCmd>(32);
    // Tray menu actions that only the Master can carry out (End Stream, Clear
    // Paired Devices) — forwarded over the control pipe below. Bounded and
    // small: these arrive at human cadence, and a full queue would mean the
    // forwarder is wedged, which is worth a log line rather than unbounded
    // growth.
    let (tray_action_tx, tray_action_rx) = std::sync::mpsc::sync_channel::<tray::TrayAction>(8);
    let tray_global_pin: Arc<Mutex<(String, String)>> = Arc::new(Mutex::new((String::new(), String::new())));
    tray::spawn(tray_rx, shutdown_tx.clone(), tray_global_pin.clone(), tray_action_tx);

    input::check_vigem_driver_at_startup();
    audio::recover_stuck_sink();
    let _desktop_switch_monitor = capture::desktop_switch::DesktopSwitchMonitor::spawn();

    // Connect to Master's IPC pipes BEFORE any VDD/capture/encoder work —
    // nothing useful to do without a Master to stream to.
    let (mut control_pipe, mut media_pipe) = ipc::connect_to_master().await.map_err(|e| {
        println!("❌ Worker could not connect to Master's IPC pipes: {e}");
        windows::core::Error::from(windows::Win32::Foundation::E_FAIL)
    })?;
    println!("🔌 Connected to Master's IPC pipes");
    ipc::send_control(&mut control_pipe, &ipc::ControlMsg::WorkerReady).await.map_err(|e| {
        println!("❌ Worker could not send WorkerReady: {e}");
        windows::core::Error::from(windows::Win32::Foundation::E_FAIL)
    })?;
    println!("✅ Sent WorkerReady");

    let cfg = config::NovaConfig::load();
    encoder::set_hdr_metadata(cfg.hdr.max_luminance_nits, cfg.hdr.max_cll_nits, cfg.hdr.max_fall_nits);
    if !cfg.audio.endpoint_override.is_empty() {
        audio::set_sink_override(&cfg.audio.endpoint_override);
        audio::recover_stuck_sink();
    }
    let width  = cfg.stream.width;
    let height = cfg.stream.height;
    let fps    = cfg.stream.fps;
    let startup_codec = encoder::Codec::from_str(&cfg.stream.codec);

    let mut vd = virtual_display::VirtualDisplay::new();
    let virtual_device_name = match vd.ensure_enabled_at_boot(width as u32, height as u32, fps) {
        Ok(name) => name,
        Err(e) => {
            println!("❌ VDD BOOT PREFLIGHT FAILED: {e}");
            vd.log_vdd_diagnostics();
            None
        }
    };
    // Pre-login there may be NO usable capture backend: WGC needs a real
    // user session (0x80070424 at the logon screen), and the DDA fallback
    // needs SYSTEM identity or the service's --system-token (a task/manual
    // launch has neither). Exiting here put the Worker in a crash-respawn
    // loop against the service's backoff (confirmed live 2026-08-06: the
    // Worker died ~1 s after every pre-login spawn, and the 4→60 s backoff
    // then also DELAYED post-login recovery). Retry in place instead: the
    // Worker stays alive — IPC pipes connected, tray retry running — and
    // binds a backend within ~3 s of one becoming available. A service stop
    // still works: the stop_host TerminateProcess grace backstop covers a
    // Worker parked in this loop.
    let mut capturer = {
        let mut attempt = 0u32;
        loop {
            match capture::DesktopManager::new_wgc(virtual_device_name.as_deref()) {
                Ok(c) => break c,
                Err(e) => {
                    attempt += 1;
                    if attempt == 1 || attempt % 20 == 0 {
                        println!(
                            "⏳ Desktop capture unavailable (attempt {attempt}): {e:?} — \
                             likely no user session yet (logon screen). Retrying every 3 s until sign-in."
                        );
                    }
                    tokio::time::sleep(Duration::from_secs(3)).await;
                }
            }
        }
    };
    {
        let (ox, oy) = capturer.origin();
        input::set_active_capture_rect(ox, oy, capturer.width(), capturer.height());
    }
    let mut enc = Encoder::new(
        capturer.device(),
        EncoderConfig {
            width: capturer.width() as i32,
            height: capturer.height() as i32,
            fps: fps as i32,
            bitrate_kbps: cfg.stream.bitrate_kbps,
            codec: startup_codec,
            is_hdr: false,
        },
    ).map_err(|e| {
        println!("❌ Failed to initialize NVENC encoder: {e}");
        windows::core::Error::from(windows::Win32::Foundation::E_FAIL)
    })?;

    let mut audio_manager = audio::AudioCaptureManager::new();
    // Audio's network-send (RTP header + AES-CBC + UDP) is Master's job now
    // (see audio::AudioTxState) — the Worker just needs to get raw
    // Opus-encoded bytes out over the media pipe. `audio_frame_rx` is
    // drained once per main-loop iteration, well under the 5-20ms cadence
    // audio frames arrive at.
    let (audio_frame_tx, audio_frame_rx) = std::sync::mpsc::channel::<Vec<u8>>();

    shutdown::install_console_hook();
    shutdown::spawn_session_monitor();

    // Dedicated control-reader thread — see WorkerCommand's doc comment for
    // why InjectInput/RequestIdr/CongestionReduce are applied HERE, not
    // forwarded to the main loop: this thread is this process's one
    // long-lived OS thread, the precondition input.rs's secure-desktop
    // attach needs. `reply_rx` carries the main loop's OUTBOUND replies
    // (currently just WorkerConfigured) back through this same thread/pipe —
    // the pipe itself isn't split, so this thread stays the sole owner of
    // both directions, avoiding any cross-thread pipe access.
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<WorkerCommand>();
    let (reply_tx, mut reply_rx) = mpsc::unbounded_channel::<ipc::ControlMsg>();

    // Secure-desktop reporter: Master needs to know when the input desktop
    // crosses the Winlogon boundary so it can start/stop the SYSTEM input
    // helper (see input.rs's UIPI note — the Worker's own SendInput is
    // silently swallowed at the credential provider). Only THIS process can
    // observe it: `desktop_switch` reads the console session's input desktop,
    // and Master runs in Session 0. Polled on the monitor's own 250 ms
    // cadence rather than wired into the capture loop, so it keeps reporting
    // even when no client is connected and the loop is idle.
    // The same task also reports the active capture rect, which Master relays
    // to the SYSTEM input helper: that process has its own copy of the rect
    // static and never runs a capture loop, so without this it stays 0×0 and
    // `inject_mouse_move_abs` drops every absolute move (live 2026-08-06: the
    // cursor froze mid-screen at every UAC prompt). Reported from here rather
    // than from each `set_active_capture_rect` call site so a future call site
    // cannot forget to report.
    tokio::spawn({
        let reply_tx = reply_tx.clone();
        async move {
            use capture::desktop_switch::{current_input_desktop, InputDesktop};
            let mut last_secure: Option<bool> = None;
            let mut last_rect: Option<(i32, i32, u32, u32)> = None;
            loop {
                tokio::time::sleep(Duration::from_millis(250)).await;

                let rect = input::current_capture_rect();
                // (_, _, 0, 0) means capture hasn't bound yet — nothing useful
                // to send, and the helper's guard would reject it anyway.
                if rect.2 > 0 && rect.3 > 0 && last_rect != Some(rect) {
                    last_rect = Some(rect);
                    let (origin_x, origin_y, width, height) = rect;
                    if reply_tx
                        .send(ipc::ControlMsg::CaptureRect { origin_x, origin_y, width, height })
                        .is_err()
                    {
                        return; // control thread gone — process is shutting down
                    }
                }

                let secure = matches!(
                    current_input_desktop(),
                    InputDesktop::Secure | InputDesktop::ScreenSaver
                );
                if last_secure == Some(secure) {
                    continue;
                }
                last_secure = Some(secure);
                if reply_tx
                    .send(ipc::ControlMsg::SecureDesktopChanged { secure })
                    .is_err()
                {
                    return; // control thread gone — process is shutting down
                }
            }
        }
    });

    // PIN forwarder: the tray pair dialog (opened via OpenPairDialog from the
    // Master, or manually from the tray menu) writes (pin, device) into
    // tray_global_pin — poll it and relay to the Master, whose
    // control_supervisor hands it to pairing's own global_pin. 200 ms matches
    // pairing's poll cadence on the consuming side; the claim-and-clear here
    // mirrors pairing's, so one entry is relayed exactly once.
    tokio::spawn({
        let pin_slot = tray_global_pin.clone();
        let reply_tx = reply_tx.clone();
        async move {
            loop {
                tokio::time::sleep(Duration::from_millis(200)).await;
                let taken = {
                    let mut p = pin_slot.lock().unwrap();
                    if p.0.is_empty() { None } else { Some(std::mem::take(&mut *p)) }
                };
                if let Some((pin, device)) = taken {
                    println!("🔑 Worker: PIN entered for \"{device}\" — relaying to Master");
                    if reply_tx.send(ipc::ControlMsg::PinRelay { pin, device }).is_err() {
                        return; // control thread gone — process is shutting down
                    }
                }
            }
        }
    });

    // Tray actions. "End Stream" and "Clear Paired Devices" act on state this
    // process does not own (the client session; pairing's trust store) — see
    // ControlMsg::EndSession / ControlMsg::ClearPaired for why doing either
    // locally would be wrong rather than merely inconvenient.
    //
    // "Release Display" is the opposite: the virtual display belongs to THIS
    // process, and by the time it can be pressed there is no session left for
    // the Master to have an opinion about. It goes straight onto the Worker's
    // own command channel as the same Deactivate the Master would have sent,
    // so the teardown runs through one code path (`deactivate_worker`) rather
    // than a second one written for the tray.
    //
    // A dedicated OS thread, not a tokio task: the tray hands work over a
    // blocking `std::sync::mpsc` receiver, and blocking a runtime worker on it
    // would park one of the Worker's executor threads forever. Same shape as
    // Master's `nova-pair-dialog-fwd`.
    std::thread::Builder::new()
        .name("nova-tray-action-fwd".into())
        .spawn({
            let reply_tx = reply_tx.clone();
            let cmd_tx = cmd_tx.clone();
            move || {
                // recv() only errors once the tray thread is gone (Quit /
                // process teardown), so exiting then is correct.
                while let Ok(action) = tray_action_rx.recv() {
                    if matches!(action, tray::TrayAction::ReleaseDisplay) {
                        println!("🖥️  Worker: tray \"Release Display\" — tearing down the \
                            virtual display and restoring the physical monitor");
                        if cmd_tx.send(WorkerCommand::Deactivate { cancelled: true }).is_err() {
                            return; // main loop gone — process is shutting down
                        }
                        continue;
                    }
                    let msg = match action {
                        tray::TrayAction::EndStream => ipc::ControlMsg::EndSession,
                        tray::TrayAction::ClearPairedDevices => ipc::ControlMsg::ClearPaired,
                        tray::TrayAction::ReleaseDisplay => unreachable!("handled above"),
                    };
                    println!("📨 Worker: relaying tray action {action:?} to Master");
                    if reply_tx.send(msg).is_err() {
                        return; // control thread gone — process is shutting down
                    }
                }
            }
        })
        .expect("spawn nova-tray-action-fwd thread");

    // Tray "Quit Nova" → the same WorkerCommand::Stop path as every other
    // graceful-stop trigger (below). tray::spawn only knows how to signal a
    // watch<bool>, so this task's only job is translating that into a send
    // on cmd_tx; it exits itself the moment that happens.
    //
    // Under the service deployment the service respawns a Worker the moment
    // this one exits — by design, so a crash recovers on its own — which
    // means a bare WorkerCommand::Stop here just looked like Nova instantly
    // relaunching itself after Quit. Same fix as the monolithic run()'s
    // identical shutdown_rx arm: tell the service to stop FIRST (before this
    // Worker tears down), so its reconcile loop won't respawn. No-op when
    // not launched by the service; harmless if the shutdown ORIGINATED from
    // a service stop (service is already STOP_PENDING, the extra `sc stop`
    // errors and is ignored).
    //
    // Every step here logs. This path used to be completely silent — a Quit
    // that failed anywhere between the tray click and the Worker's teardown
    // left no trace at all in nova.log, so "I clicked Quit and nothing
    // happened" was unanswerable from the logs. Three lines, once per process
    // lifetime, on a path that ends the process.
    tokio::spawn({
        let mut shutdown_rx = shutdown_rx.clone();
        let cmd_tx = cmd_tx.clone();
        async move {
            if shutdown_rx.changed().await.is_ok() && *shutdown_rx.borrow() {
                println!("🛑 Quit requested — asking the service to stop before tearing down");
                let verdict = service::request_service_stop();
                match &verdict {
                    service::ServiceStopRequest::Accepted =>
                        println!("🛑 Service stop accepted — proceeding with Worker teardown"),
                    service::ServiceStopRequest::NotServiceManaged =>
                        println!("🛑 Not service-managed — proceeding with Worker teardown"),
                    // The one case where exiting is WRONG: the service is alive
                    // and keeping a Worker in the console session is its job, so
                    // quitting here just hands it a vacancy to fill. Staying up
                    // and saying so beats a Quit that silently reincarnates.
                    service::ServiceStopRequest::Refused(why) => println!(
                        "❌ Quit ignored: NovaService refused to stop ({why}). This Worker is \
                         NOT exiting — the service would respawn it within seconds and Quit \
                         would look like it did nothing. Stop the service directly \
                         (`sc stop NovaService` from an elevated prompt) to shut Nova down."
                    ),
                }
                if verdict.safe_to_exit() {
                    let _ = cmd_tx.send(WorkerCommand::Stop);
                }
            }
        }
    });

    // Service-initiated graceful stop (session change, SYSTEM-fallback
    // upgrade, or a real service stop — see service.rs's stop_host): without
    // this, `Global\NovaHostShutdown` has no effect on a Worker at all, and
    // every one of those paths would silently skip straight to the 6s-grace-
    // then-TerminateProcess backstop, losing the VDD/audio teardown below —
    // exactly the class of bug the rest of this codebase has fought hard to
    // close. Reuses the same WorkerCommand::Stop the main loop already
    // watches; the shutdown watcher's callback runs on its own background
    // thread (spawn_host_shutdown_watcher's doc comment), so sending into an
    // unbounded channel from there is safe.
    service::spawn_host_shutdown_watcher({
        let cmd_tx = cmd_tx.clone();
        move || {
            let _ = cmd_tx.send(WorkerCommand::Stop);
        }
    });

    let rt_handle = tokio::runtime::Handle::current();
    std::thread::Builder::new()
        .name("nova-worker-control".into())
        .spawn(move || {
            rt_handle.block_on(async move {
                // Same fix as Master's control_supervisor/media_supervisor
                // (see their doc comments): `recv_control`'s `read_exact`-
                // based framing must never be raced inside `tokio::select!`
                // against anything else, since a cancelled `read_exact` can
                // silently drop already-consumed bytes and desync the
                // stream. A dedicated reader task owns the read half
                // exclusively; this loop only races the (cancellation-safe)
                // channel it feeds against the outbound reply channel.
                let (read_half, mut write_half) = tokio::io::split(control_pipe);
                let (in_tx, mut in_rx) = mpsc::unbounded_channel::<std::io::Result<ipc::ControlMsg>>();
                tokio::spawn(async move {
                    let mut read_half = read_half;
                    loop {
                        let msg = ipc::recv_control(&mut read_half).await;
                        let is_err = msg.is_err();
                        if in_tx.send(msg).is_err() || is_err {
                            return;
                        }
                    }
                });
                // Input-injection accounting — see the InjectInput arm below.
                let mut inject_window = std::time::Instant::now();
                let mut inject_count: u32 = 0;
                let mut inject_cost = Duration::ZERO;
                let (mut inject_rel, mut inject_abs, mut inject_other) = (0u32, 0u32, 0u32);
                loop {
                    tokio::select! {
                        incoming = in_rx.recv() => {
                            match incoming {
                                Some(Ok(ipc::ControlMsg::InjectInput(bytes))) => {
                                    // Timed because this is the last unmeasured
                                    // segment of the input path, and the only
                                    // one doing real Win32 work per packet on a
                                    // single thread fed by an unbounded queue.
                                    // If injection costs more than the interval
                                    // between packets, the queue grows without
                                    // bound and the host's cursor keeps moving
                                    // after the user's hand has stopped — which
                                    // is the reported symptom, and which no
                                    // amount of transport work can fix.
                                    let began = std::time::Instant::now();
                                    input::handle_input_packet(&bytes);
                                    inject_cost += began.elapsed();
                                    inject_count += 1;
                                    // Which KIND of mouse packet is arriving
                                    // decides everything about how the pointer
                                    // behaves, and the host is the only place
                                    // that sees the truth. ABS is one exact
                                    // MOUSEEVENTF_ABSOLUTE — it maps the
                                    // client's own cursor onto the capture rect,
                                    // so it cannot drift or lag behind. REL is
                                    // MOUSEEVENTF_MOVE, which accumulates and
                                    // goes through Windows' acceleration curve.
                                    // The client only sends ABS when Android
                                    // refused pointer capture, so this line also
                                    // reports capture state from the far end,
                                    // without trusting the client to say so.
                                    match bytes.get(4..8).map(|m| {
                                        u32::from_le_bytes(m.try_into().expect("4 bytes"))
                                    }) {
                                        Some(0x05) => inject_abs += 1,
                                        Some(0x07) => inject_rel += 1,
                                        _ => inject_other += 1,
                                    }
                                    if inject_window.elapsed() >= Duration::from_secs(1) {
                                        println!(
                                            "⌨️  Worker inject/s: {inject_count} packets \
                                             ({inject_rel} rel, {inject_abs} abs, {inject_other} other), \
                                             {:.1}ms total, {:.0}µs avg",
                                            inject_cost.as_secs_f64() * 1000.0,
                                            inject_cost.as_micros() as f64
                                                / inject_count.max(1) as f64,
                                        );
                                        inject_window = std::time::Instant::now();
                                        inject_count = 0;
                                        inject_cost = Duration::ZERO;
                                        inject_rel = 0;
                                        inject_abs = 0;
                                        inject_other = 0;
                                    }
                                }
                                Some(Ok(ipc::ControlMsg::RequestIdr)) => encoder::request_idr_global(),
                                Some(Ok(ipc::ControlMsg::CongestionReduce)) => encoder::signal_congestion_reduction(),
                                Some(Ok(ipc::ControlMsg::InvalidateRefFrames { first, last })) => {
                                    // RFI recovery: invalidate the lost range so the
                                    // next P-frame recovers; fall back to an IDR if
                                    // NVENC can't honour it (range too large, etc).
                                    if !encoder::invalidate_ref_frames(first as u64, last as u64) {
                                        encoder::request_idr_global();
                                    }
                                }
                                Some(Ok(ipc::ControlMsg::ConfigureStart(cs))) => {
                                    if cmd_tx.send(WorkerCommand::Configure(cs)).is_err() {
                                        break; // main loop gone
                                    }
                                }
                                Some(Ok(ipc::ControlMsg::Deactivate { cancelled })) => {
                                    if cmd_tx.send(WorkerCommand::Deactivate { cancelled }).is_err() {
                                        break; // main loop gone
                                    }
                                }
                                Some(Ok(ipc::ControlMsg::SetDisplayMode {
                                    display_id, width, height, refresh_hz, hdr,
                                })) => {
                                    // Wire is live; applying it is the next step
                                    // (graceful capture/encoder rebuild + the
                                    // client-side format-change handshake). Logged
                                    // rather than silently swallowed so an Echo
                                    // client's command is visible end-to-end in
                                    // nova.log before any of it can misbehave.
                                    println!(
                                        "🎛️  Worker: Echo set_display received for \"{display_id}\" \
                                         → {width}x{height}@{refresh_hz}Hz hdr={hdr} — not applied \
                                         yet (hot format change is not implemented)"
                                    );
                                }
                                Some(Ok(ipc::ControlMsg::OpenPairDialog)) => {
                                    println!("🔑 Worker: Master requests the pair dialog — opening");
                                    // try_send: a full queue means dialogs are already
                                    // pending; dropping the extra request is harmless
                                    // (pairing re-sends on every getservercert retry).
                                    let _ = tray_tx.try_send(tray::TrayCmd::OpenPairDialog);
                                }
                                Some(Ok(ipc::ControlMsg::Stop)) => {
                                    let _ = cmd_tx.send(WorkerCommand::Stop);
                                    break;
                                }
                                Some(Ok(other)) => println!("⚠️  Worker: unexpected control message from Master: {other:?}"),
                                Some(Err(e)) => {
                                    println!("🔌 Worker: Master control pipe closed/errored ({e}) — stopping");
                                    let _ = cmd_tx.send(WorkerCommand::Stop);
                                    break;
                                }
                                None => {
                                    let _ = cmd_tx.send(WorkerCommand::Stop);
                                    break;
                                }
                            }
                        }
                        outgoing = reply_rx.recv() => {
                            match outgoing {
                                Some(msg) => {
                                    if let Err(e) = ipc::send_control(&mut write_half, &msg).await {
                                        println!("⚠️  Worker: reply to Master failed ({e}) — stopping");
                                        let _ = cmd_tx.send(WorkerCommand::Stop);
                                        break;
                                    }
                                }
                                None => break, // main loop gone
                            }
                        }
                    }
                }
            });
        })
        .expect("spawn nova-worker-control thread");

    let mut out_buffer = vec![0u8; 8 * 1024 * 1024];
    let mut client_connected = false;
    let mut first_idr_sent = false;
    let mut frames_encoded = 0u64;
    // Wire frame index (== NVENC inputTimeStamp), 1-based like Moonlight expects.
    // Owned here so the encoder and the wire agree; reset to 1 per session.
    let mut wire_index: u32 = 1;
    // Deferred VDD activation: when a ConfigureStart arrives while the secure
    // desktop is up (client connected at the sign-in screen), the VDD/CCD
    // topology change is denied (error 5) and half-activating garbles the frame
    // — see desktop_is_secure(). We stash the ConfigureStart and re-run it the
    // moment the desktop returns to Default (post-login), so the VDD comes up
    // cleanly then instead of the user having to restart the app.
    let mut pending_configure: Option<ipc::ConfigureStart> = None;
    let mut vdd_activation_pending = false;
    let mut send_queue_drops = 0u64;
    let mut audio_send_drops = 0u64;
    let mut timeout_streak = 0u32;
    let mut jiggle_toggle = false;
    // Throttle for the static-desktop diagnostic (see log_static_desktop).
    let mut last_static_log: Option<Instant> = None;
    // Dynamic-bitrate controller (see QosController): remembers the rate that
    // failed and holds recovery at 90% of it instead of climbing back to the
    // ceiling, which is what produced the 12-second freeze sawtooth.
    let mut qos = QosController::new();
    let mut enc_rate_bytes = 0u64;
    // Frames in the current 1 s window — the only per-frame cost the tray's
    // telemetry adds anywhere (see stats.rs).
    let mut enc_rate_frames = 0u32;
    let mut enc_rate_tick = Instant::now();
    let startup_frame_interval = Duration::from_secs_f64(1.0 / fps.max(1) as f64);
    let mut frame_interval = startup_frame_interval;
    let mut next_frame_time = Instant::now();

    println!("▶️  Worker capture loop running");

    // Tell Master what this Worker can physically stream, now that the capture
    // backend is up and its native size is known. Master negotiates every new
    // session against this so a session is never handed a geometry/HDR profile
    // its Worker can't hold for the session's whole life — see
    // session_negotiate::negotiate and ControlMsg::WorkerCapabilities.
    {
        let vdd_capable = !service::is_system_fallback();
        let _ = reply_tx.send(ipc::ControlMsg::WorkerCapabilities {
            vdd_capable,
            native_width: capturer.width(),
            native_height: capturer.height(),
        });
        println!("🧩 Worker capabilities: vdd={vdd_capable} native={}x{}",
            capturer.width(), capturer.height());
    }

    // The display topology as it stands before any session — this is what
    // makes Echo's `list_displays` a real, targetable list rather than the
    // single inferred entry `WorkerCapabilities` can support.
    publish_display_inventory(&reply_tx, &vd);

    'outer: loop {
        // Frame pacing, same shape as the monolithic path's loop, racing the
        // command channel instead of ClientInfo/tray/service-shutdown signals
        // (this process has none of those — Master owns the client session,
        // shutdown.rs's console/WM_ENDSESSION hooks cover THIS process's own
        // involuntary death, handled below via the normal loop exit).
        let now = Instant::now();
        if now < next_frame_time {
            let wait = next_frame_time - now;
            let sleep_for = if client_connected { wait.saturating_sub(Duration::from_millis(1)) } else { wait };
            tokio::select! {
                _ = tokio::time::sleep(sleep_for) => {}
                cmd = cmd_rx.recv() => {
                    match cmd {
                        Some(WorkerCommand::Stop) | None => break 'outer,
                        Some(WorkerCommand::Configure(cs)) => {
                            match apply_configure_start(&cs, &mut vd, &mut capturer, &mut enc, &cfg) {
                                Ok(applied) => {
                                    audio_manager.start_for_stream(
                                        audio_frame_tx.clone(),
                                        cs.audio_packet_duration_ms, cs.host_audio,
                                    );
                                    input::start_session();
                                    // Fresh session ⇒ clear the QoS link memory:
                                    // it is per-link-episode and a reconnect
                                    // deserves a clean slate (see reset()).
                                    qos.reset();
                                    client_connected = true;
                                    first_idr_sent = false;
                                    // 1 for a fresh session; a mid-session adoption
                                    // (Worker respawn) continues the client's frame
                                    // timeline — see ConfigureStart::start_frame_index.
                                    wire_index = cs.start_frame_index.max(1);
                                    // Remember the config so a deferred VDD
                                    // activation (secure desktop up) can run on
                                    // return to the interactive desktop.
                                    pending_configure = Some(cs.clone());
                                    vdd_activation_pending = app_launcher::uses_virtual_display(
                                            cs.app_id, cfg.stream.headless_for_all_apps)
                                        && desktop_is_secure();
                                    frame_interval = Duration::from_secs_f64(1.0 / applied.fps.max(1) as f64);
                                    next_frame_time = Instant::now();
                                    // "Thaw" keyframe: this Worker may be ADOPTING a session that is
                                    // already live on the client (sign-in/sign-out handoff). Its encoder
                                    // is brand new, so nothing it emits references frames the client
                                    // still holds -- force an IDR now rather than waiting for the
                                    // first-frame check to notice, so the decoder resumes on the very
                                    // next frame instead of showing a frozen image for one more slot.
                                    enc.request_idr();
                                    // Publish what the tray's Server Stats window
                                    // reports. Taken from the ENCODER, not the
                                    // request: apply_configure_start may land on a
                                    // different geometry than was asked for.
                                    stats::session_started(
                                        applied.width, applied.height, applied.fps,
                                        enc.config.codec, enc.config.is_hdr,
                                        enc.config.bitrate_kbps.max(0) as u32,
                                    );
                                    let _ = reply_tx.send(ipc::ControlMsg::WorkerConfigured(applied));
                                    // VDD activation can change the topology —
                                    // refresh Master's Echo seat list.
                                    publish_display_inventory(&reply_tx, &vd);
                                }
                                Err(e) => println!("❌ apply_configure_start failed: {e}"),
                            }
                        }
                        Some(WorkerCommand::Deactivate { cancelled }) => {
                            deactivate_worker(cancelled, &mut vd, &mut capturer, &mut enc, &mut audio_manager);
                            // A VDD teardown changes the topology too.
                            publish_display_inventory(&reply_tx, &vd);
                            client_connected = false;
                            first_idr_sent = false;
                            send_queue_drops = 0;
                            pending_configure = None;
                            vdd_activation_pending = false;
                            frame_interval = startup_frame_interval;
                            next_frame_time = Instant::now();
                        }
                    }
                }
            }
            // Precise dispatch: spin out the sub-millisecond remainder the
            // sleep above deliberately left. Even at 1 ms timer resolution the
            // OS sleep wakes ±1 ms — at 120 fps (8.33 ms budget) that jitter
            // samples the capturer a frame early/late, visible as micro-stutter.
            if client_connected {
                while Instant::now() < next_frame_time {
                    std::hint::spin_loop();
                }
            }
        }
        // Never a bare `+=` — missed slots are dropped, not repaid. See
        // advance_frame_deadline for the 179-fps catch-up-burst bug.
        next_frame_time = advance_frame_deadline(next_frame_time, frame_interval, Instant::now());

        // Drain any command that arrived exactly on a frame boundary (the
        // sleep above already consumed it via select! in the common case;
        // this covers the next_frame_time <= now fast path where we never
        // entered the sleep branch at all).
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                WorkerCommand::Stop => break 'outer,
                WorkerCommand::Configure(cs) => {
                    match apply_configure_start(&cs, &mut vd, &mut capturer, &mut enc, &cfg) {
                        Ok(applied) => {
                            audio_manager.start_for_stream(
                                audio_frame_tx.clone(),
                                cs.audio_packet_duration_ms, cs.host_audio,
                            );
                            input::start_session();
                            // Fresh session ⇒ clear the QoS link memory (see reset()).
                            qos.reset();
                            client_connected = true;
                            first_idr_sent = false;
                            // See the select!-arm twin above: fresh session ⇒ 1,
                            // mid-session adoption ⇒ continue the timeline.
                            wire_index = cs.start_frame_index.max(1);
                            pending_configure = Some(cs.clone());
                            vdd_activation_pending = app_launcher::uses_virtual_display(
                                    cs.app_id, cfg.stream.headless_for_all_apps)
                                && desktop_is_secure();
                            frame_interval = Duration::from_secs_f64(1.0 / applied.fps.max(1) as f64);
                            next_frame_time = Instant::now();
                            // "Thaw" keyframe: this Worker may be ADOPTING a session that is
                            // already live on the client (sign-in/sign-out handoff). Its encoder
                            // is brand new, so nothing it emits references frames the client
                            // still holds -- force an IDR now rather than waiting for the
                            // first-frame check to notice, so the decoder resumes on the very
                            // next frame instead of showing a frozen image for one more slot.
                            enc.request_idr();
                            // See the select!-arm twin above.
                            stats::session_started(
                                applied.width, applied.height, applied.fps,
                                enc.config.codec, enc.config.is_hdr,
                                enc.config.bitrate_kbps.max(0) as u32,
                            );
                            let _ = reply_tx.send(ipc::ControlMsg::WorkerConfigured(applied));
                            // See the select!-arm twin above.
                            publish_display_inventory(&reply_tx, &vd);
                        }
                        Err(e) => println!("❌ apply_configure_start failed: {e}"),
                    }
                }
                WorkerCommand::Deactivate { cancelled } => {
                    deactivate_worker(cancelled, &mut vd, &mut capturer, &mut enc, &mut audio_manager);
                    // See the select!-arm twin above.
                    publish_display_inventory(&reply_tx, &vd);
                    client_connected = false;
                    first_idr_sent = false;
                    send_queue_drops = 0;
                    pending_configure = None;
                    vdd_activation_pending = false;
                    frame_interval = startup_frame_interval;
                    next_frame_time = Instant::now();
                }
            }
        }

        // ── Dynamic bitrate (QoS) ────────────────────────────────────────────
        // Master relays the client's PT_LOSS_STATS as ControlMsg::
        // CongestionReduce; the control thread turns that into a pending
        // reduction, and THIS is where it gets applied to NVENC. Without this
        // call the whole chain was a no-op in the split deployment — see
        // qos_tick's doc comment. `enc.config.bitrate_kbps` is the negotiated
        // ceiling (reconfigure never rewrites it).
        if client_connected {
            qos.tick(
                enc.config.bitrate_kbps.max(0) as u32,
                enc.config.fps.max(1) as u32,
            );
        }

        // ── 1 Hz telemetry tick ──────────────────────────────────────────────
        // Deliberately in the loop body rather than inside the encode branch it
        // used to live in: a capture that stalls mid-session stops producing
        // frames entirely, and from inside the encode branch the tick would
        // stop firing too — freezing the tray at the last healthy sample at
        // exactly the moment someone opens the stats window to find out why the
        // stream looks stuck. Out here a stall correctly reads as 0.0 fps.
        //
        // The log line keeps its old "only when frames were encoded" behaviour
        // so an idle Worker doesn't write "0 Kbps" to nova.log every second.
        {
            let elapsed = enc_rate_tick.elapsed();
            if elapsed >= Duration::from_secs(1) {
                if enc_rate_frames > 0 {
                    println!("🎞  Encoder output: {} Kbps", (enc_rate_bytes * 8) / 1000);
                }
                if client_connected {
                    stats::sample(
                        enc_rate_frames,
                        enc_rate_bytes,
                        elapsed.as_millis() as u64,
                        encoder::get_stream_bitrate_kbps().max(0) as u32,
                    );
                }
                enc_rate_bytes = 0;
                enc_rate_frames = 0;
                enc_rate_tick = Instant::now();
            }
        }

        if client_connected {
            if let Some(_resized) = capturer.maybe_swap_backend() {
                // A backend swap can change the capture's size and format, but
                // NOT the encoder's — the client's decoder is fixed for the
                // session, and the shim rescales/reconverts the new capture into
                // the same output surface. Just force a keyframe so the client
                // decodes cleanly from the first post-swap frame.
                enc.request_idr();
                let (ox, oy) = capturer.origin();
                input::set_active_capture_rect(ox, oy, capturer.width(), capturer.height());
            }
        } else if capturer.backend_kind() == capture::BackendKind::Dda {
            if capturer.maybe_swap_backend().is_some() {
                let (ox, oy) = capturer.origin();
                input::set_active_capture_rect(ox, oy, capturer.width(), capturer.height());
            }
        }

        // Deferred VDD activation: a ConfigureStart that arrived while the
        // secure desktop was up skipped the VDD/CCD topology change (it would
        // have been denied and garbled the frame — see desktop_is_secure).
        // Now that the interactive desktop is back (user logged in), re-run it
        // so the VDD comes up cleanly, WITHOUT re-launching the app.
        if client_connected && vdd_activation_pending && !desktop_is_secure() {
            if let Some(mut cs) = pending_configure.clone() {
                cs.launch_app = false; // already launched at the original configure
                println!("🖥️  Interactive desktop restored — running deferred VDD activation");
                match apply_configure_start(&cs, &mut vd, &mut capturer, &mut enc, &cfg) {
                    Ok(applied) => {
                        vdd_activation_pending = false;
                        let (ox, oy) = capturer.origin();
                        input::set_active_capture_rect(ox, oy, capturer.width(), capturer.height());
                        enc.request_idr();
                        // The VDD coming up can change the capture geometry, so
                        // the stats window must be re-stamped, not left showing
                        // the pre-activation size.
                        stats::session_started(
                            applied.width, applied.height, applied.fps,
                            enc.config.codec, enc.config.is_hdr,
                            enc.config.bitrate_kbps.max(0) as u32,
                        );
                        let _ = reply_tx.send(ipc::ControlMsg::WorkerConfigured(applied));
                        // The deferred activation just brought the VDD up.
                        publish_display_inventory(&reply_tx, &vd);
                    }
                    Err(e) => println!("❌ deferred VDD activation failed: {e}"),
                }
            } else {
                vdd_activation_pending = false;
            }
        }

        let mut texture_to_encode: Option<ID3D11Texture2D> = None;
        match capturer.try_get_frame() {
            Some(texture) => {
                timeout_streak = 0;
                // Only when someone is watching. The duplicate-frame path below
                // has always been gated this way; the REAL-frame path was not,
                // so any desktop movement with no client connected was captured,
                // colour-converted, encoded by NVENC and then thrown away at the
                // send gate. Measured live on an idle host (2026-08-17):
                // ~15 Mbps of encoder output, 60 fps, with no session at all —
                // the exact opposite of the "0% Video Encode while idle"
                // signature Phase 11 was built for, hiding in the one branch
                // that never got the check.
                if client_connected {
                    texture_to_encode = Some(texture);
                }
            }
            None => {
                timeout_streak += 1;
                if client_connected {
                    log_static_desktop(capturer.backend_kind(), timeout_streak, &mut last_static_log);
                }
                if !capturer.has_frame() && timeout_streak % 25 == 0 {
                    let (dx, dy): (i32, i32) = if jiggle_toggle { (1, 1) } else { (-1, -1) };
                    jiggle_toggle = !jiggle_toggle;
                    unsafe {
                        let mut input: INPUT = std::mem::zeroed();
                        input.r#type = INPUT_MOUSE;
                        input.Anonymous.mi.dx = dx;
                        input.Anonymous.mi.dy = dy;
                        input.Anonymous.mi.dwFlags = MOUSEEVENTF_MOVE;
                        SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
                    }
                }
                if !capturer.has_frame() {
                    continue;
                }
                // Strict frame pacing: no desktop damage this slot (WGC/DDA
                // WAIT_TIMEOUT) ⇒ re-submit the last captured surface as a
                // duplicate P-frame so the client receives an uninterrupted
                // constant-fps bitstream. Without this a static screen starves
                // the decoder and CBR degrades the image until motion resumes;
                // the duplicates also let rate control spend the idle bitrate
                // refining the static picture to full sharpness. Gated on an
                // active session — idle with no client keeps NVENC at 0%.
                if client_connected {
                    texture_to_encode = capturer.cached_texture().cloned();
                }
            }
        }

        if let Some(texture) = texture_to_encode {
            // The index this frame is encoded AND sent under (== NVENC
            // inputTimeStamp, see encode_frame). Advances once per encoded frame
            // so NVENC's reference timeline and the client's wire indices stay
            // in bijection; a frame dropped after encode simply leaves a gap.
            let this_index = wire_index;
            let packet_size = enc.encode_frame(&texture, &mut out_buffer, this_index as u64);
            if packet_size > 0 {
                wire_index = wire_index.wrapping_add(1);
                if wire_index == 0 { wire_index = 1; } // Moonlight discards frame 0
                frames_encoded += 1;
                if frames_encoded == 1 {
                    println!("🎬 First encoded frame: {} bytes", packet_size);
                }
                // Accumulate only — the 1 Hz report itself now runs from the
                // loop body (see the telemetry tick above), so a stalled
                // capture still reports.
                enc_rate_bytes += packet_size as u64;
                enc_rate_frames += 1;
                if client_connected {
                    let data = &out_buffer[..packet_size as usize];
                    let is_hevc_enc = enc.config.codec == encoder::Codec::Hevc;
                    let is_av1_enc = enc.config.codec == encoder::Codec::Av1;
                    let is_idr = rtp::detect_frame_type(data, is_hevc_enc, is_av1_enc) == 2;
                    if !first_idr_sent && !is_idr {
                        enc.request_idr();
                    } else {
                        // 2 = IDR, 5 = RFI recovery (reference re-pointed after an
                        // invalidation — client needs this to decode it), 1 = P.
                        let frame_type = if is_idr {
                            2u8
                        } else if encoder::RFI_ENABLED && encoder::last_frame_was_rfi_recovery() {
                            5u8
                        } else {
                            1u8
                        };
                        match ipc::send_media(&mut media_pipe, &ipc::MediaMsg::VideoFrame {
                            frame_index: this_index, frame_type, data: data.to_vec(),
                        }).await {
                            Ok(()) => {
                                first_idr_sent = true;
                                if frames_encoded <= 10 || is_idr {
                                    println!("[ENC] frame={} size={} bytes ({})", frames_encoded, packet_size, if is_idr { "IDR" } else { "P" });
                                }
                            }
                            Err(e) => {
                                send_queue_drops += 1;
                                enc.request_idr();
                                if send_queue_drops == 1 || send_queue_drops % 120 == 0 {
                                    println!("⚠️  Media pipe send failed ({e}) — frame dropped ({} total), IDR re-requested", send_queue_drops);
                                }
                            }
                        }
                    }
                }
            }
        }

        // Forward captured audio out over the media pipe every iteration —
        // NOT gated on client_connected/texture_to_encode: audio frames
        // arrive on their own 5-20ms cadence from the audio thread and must
        // never wait on video's pacing. Best-effort, same pattern as video:
        // a failed send just drops that one frame (a lost 5-20ms of audio),
        // not fatal.
        while let Ok(opus_bytes) = audio_frame_rx.try_recv() {
            if let Err(e) = ipc::send_media(&mut media_pipe, &ipc::MediaMsg::AudioFrame { data: opus_bytes }).await {
                audio_send_drops += 1;
                if audio_send_drops == 1 || audio_send_drops % 240 == 0 {
                    println!("⚠️  Media pipe audio send failed ({e}) — frame dropped ({audio_send_drops} total)");
                }
            }
        }
    }

    println!("🔊 Restoring host audio output before exit...");
    audio_manager.stop_and_release();
    enc.cleanup();
    if let Err(e) = vd.deactivate_after_stream() {
        println!("⚠️  VDD shutdown teardown: {e}");
    }
    println!("✅ Worker capture loop done — {frames_encoded} frames encoded");
    Ok(())
}

pub async fn run() -> Result<()> {
    // Elevate this thread (capture / encode / RTP-send path) to TIME_CRITICAL
    // priority so Windows scheduler doesn't preempt it for background tasks.
    // This is the same OS thread that drives the frame loop; since tokio's work-
    // stealing runtime may migrate async tasks, this grants the initial worker
    // thread the elevated priority — adequate for the mostly-synchronous hot path.
    unsafe {
        use windows::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
        };
        // TIME_CRITICAL priority is the primary scheduling mechanism:
        // prevents preemption by normal user-mode threads and gives
        // the OS scheduler a strong hint to keep us on a performance core.
        // (SetIdealProcessor is omitted — it's advisory only and not
        // available through windows-rs's thunk layer without extra glue.)
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
    }

    // ── Disable Windows Efficiency Mode throttling ────────────────────────────
    // On Windows 10 1709+ (and Windows 11), the OS can park streaming threads
    // on efficiency (low-power) cores or reduce CPU clock under "background
    // power throttling". SetProcessInformation(ProcessPowerThrottling) with
    // StateMask=0 (disable) guarantees foreground/HighQoS scheduling for the
    // entire nova-server process, matching a "Games" process category.
    unsafe {
        use windows::Win32::System::Threading::{
            GetCurrentProcess, SetProcessInformation,
            PROCESS_INFORMATION_CLASS,
        };
        #[repr(C)]
        struct ProcessPowerThrottlingState {
            version:      u32, // = 1  (PROCESS_POWER_THROTTLING_CURRENT_VERSION)
            control_mask: u32, // = 0x1 (PROCESS_POWER_THROTTLING_EXECUTION_SPEED)
            state_mask:   u32, // = 0   disable throttling → HighPerformance
        }
        let mut pt = ProcessPowerThrottlingState {
            version: 1, control_mask: 0x1, state_mask: 0,
        };
        // ProcessPowerThrottling = 4 in PROCESS_INFORMATION_CLASS
        let _ = SetProcessInformation(
            GetCurrentProcess(),
            PROCESS_INFORMATION_CLASS(4),
            std::ptr::addr_of_mut!(pt).cast(),
            std::mem::size_of::<ProcessPowerThrottlingState>() as u32,
        );
    }
    println!("⚡ Process power throttling disabled (foreground performance mode)");

    // ── 1 ms system timer resolution ──────────────────────────────────────────
    // Windows' default timer tick is ~15.6 ms; every sleep-based wait in the
    // process (tokio's frame-pacing sleep in the capture loop below included)
    // rounds up to it. At 120 fps the frame budget is 8.33 ms — coarser than
    // the timer — so without this the pacing sleep alone injects multi-ms
    // rhythmic dispatch jitter. Sunshine/Apollo do the same. Windows undoes
    // the request automatically at process exit.
    unsafe {
        use windows::Win32::Media::timeBeginPeriod;
        let _ = timeBeginPeriod(1);
    }
    println!("⏱️  System timer resolution → 1 ms (timeBeginPeriod)");

    // ── File logging: must be first so all subsequent println! go to nova.log ─
    debug::init_debug_logger();

    // ── Single-instance guard (Phase 15.5) ────────────────────────────────────
    // The scheduled-task fallback, a manual launch, and the NovaService
    // deployment must never BOTH run a host (double VDD devnode cycling,
    // dueling WGC sessions, port conflicts). Claim the machine-wide mutex
    // BEFORE touching any system state; if another host holds it, exit
    // cleanly and let the existing instance keep serving. The guard must stay
    // named-alive for the whole of run().
    let _host_singleton = match service::acquire_host_singleton() {
        Ok(guard) => guard,
        Err(msg) => {
            println!("🚫 {msg}");
            return Ok(());
        }
    };

    // Tell the C++ shim where to write its own log output.  The shim opens the
    // file independently (CRT file descriptors don't follow SetStdHandle) and
    // also _dup2's the CRT stdout/stderr so any stray printf() lands there too.
    {
        let wide = debug::log_path_wide();
        encoder::init_shim_log(wide.as_ptr());
    }

    // Log which nova_shim.dll is actually on disk / in the search path.
    // "half-green / half-smeared" service output means a stale DLL or Session 0
    // D3D11 failure — this line makes the root cause visible immediately.
    debug::log_shim_dll_path();

    // ── Privilege preflight ───────────────────────────────────────────────────
    // Nova needs an elevated token for the VDD lifecycle (SetupAPI
    // DICS_ENABLE/DISABLE on Root\MttVDD) and HDR10 Advanced Color switching.
    // The embedded manifest requests requireAdministrator, so an unelevated
    // start should be impossible — but a stale unmanifested build, or a
    // launcher that strips elevation (an Inno Setup postinstall [Run] entry
    // without runascurrentuser executes as the ORIGINAL unelevated user),
    // otherwise fails silently: no virtual monitor, no HDR, black stream.
    // Make that failure loud in the log AND on screen.
    if service::is_system_fallback() {
        // Spawned as SYSTEM-in-session itself (service.rs's pre-login
        // fallback — no user token existed yet to duplicate). SYSTEM has
        // strictly more privilege than a merely-elevated user token but is
        // not "an admin" by group membership, so IsUserAnAdmin() below would
        // false-alarm here. VDD/HDR control both work fine under SYSTEM
        // (SetupAPI + CCD don't require an interactive user); only WGC does
        // not, and that already degrades gracefully to DDA.
        println!("🛡️  Running as SYSTEM (pre-login fallback) — VDD lifecycle + HDR10 control \
            available; WGC unavailable until a user signs in (DDA covers the logon screen)");
    } else if unsafe { windows::Win32::UI::Shell::IsUserAnAdmin() }.as_bool() {
        println!("🛡️  Elevated token confirmed — VDD lifecycle + HDR10 control available");
    } else {
        println!("❌ NOT ELEVATED — virtual display activation and HDR10 switching WILL fail. \
            Start Nova as administrator (the NovaServerBoot task and the installer's \
            'Launch Nova now' step both do this automatically).");
        // Warn on-screen from a background thread so an unattended start
        // (pairing/serverinfo still work unelevated) isn't blocked forever.
        std::thread::spawn(|| unsafe {
            use windows::core::w;
            use windows::Win32::Foundation::HWND;
            use windows::Win32::UI::WindowsAndMessaging::{
                MessageBoxW, MB_ICONWARNING, MB_OK, MB_SETFOREGROUND, MB_TOPMOST,
            };
            MessageBoxW(
                HWND(std::ptr::null_mut()),
                w!("Nova is running without administrator privileges.\n\nThe virtual display (and HDR10) cannot be activated without elevation, so streams will show a black screen.\n\nClose Nova and start it as administrator — or reinstall, so the NovaServerBoot task launches it elevated at every logon."),
                w!("Nova — Administrator Required"),
                MB_OK | MB_ICONWARNING | MB_TOPMOST | MB_SETFOREGROUND,
            );
        });
    }

    // ── ViGEmBus preflight ────────────────────────────────────────────────────
    // Detects a missing virtual Xbox 360 controller driver and offers a
    // one-click download+install. Background thread — never blocks startup;
    // video/audio/mouse/keyboard don't depend on it.
    input::check_vigem_driver_at_startup();

    // If a previous run was killed/closed without restoring the default audio
    // device, fix that up before anything else (host would otherwise stay
    // silent with no client connected).
    audio::recover_stuck_sink();

    // Desktop-switch detection (Phase 15.1b — observe + log only). Runs for
    // the whole process lifetime; logs interactive↔secure desktop transitions
    // (UAC prompts, logon screen) so live sessions confirm detection before
    // Phase 2 wires the WGC→DDA backend swap to it. The handle must stay
    // named-alive: `let _ =` would drop (and stop) it immediately.
    let _desktop_switch_monitor = capture::desktop_switch::DesktopSwitchMonitor::spawn();

    // System tray: spawn before anything else so pairing PIN notifications
    // are visible from the moment the server is ready.
    // The watch channel is the graceful-shutdown bridge: the tray's "Quit"
    // menu item sends `true`; the capture-loop select! below breaks on it.
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    let shutdown_tx = Arc::new(shutdown_tx);
    let (tray_tx, tray_rx) = std::sync::mpsc::sync_channel::<tray::TrayCmd>(32);
    // global_pin is the handshake point between the tray PIN dialog and the
    // pairing async task: the tray writes the 4-digit string here and the
    // pairing poll loop reads + clears it.
    let global_pin: Arc<Mutex<(String, String)>> = Arc::new(Mutex::new((String::new(), String::new())));
    // Tray menu actions (End Stream / Clear Paired Devices). Monolithically
    // every subsystem they touch is in THIS process, so the handler task below
    // acts on `client_info` and the pairing store directly — no IPC leg, but
    // the same observable behaviour as the split path.
    let (tray_action_tx, tray_action_rx) = std::sync::mpsc::sync_channel::<tray::TrayAction>(8);
    tray::spawn(tray_rx, shutdown_tx.clone(), global_pin.clone(), tray_action_tx);
    // Service-initiated graceful stop (Phase 15.5): when the SCM stops
    // NovaService (installer upgrade, manual `sc stop`, OS shutdown), the
    // service signals a named event instead of immediately terminating us.
    // The watcher funnels that signal into the same shutdown channel as the
    // tray "Quit", so the full graceful teardown (display topology + audio
    // endpoint restore) runs before the service's TerminateProcess backstop.
    service::spawn_host_shutdown_watcher({
        let tx = shutdown_tx.clone();
        move || {
            let _ = tx.send(true);
        }
    });
    let tray_tx = Arc::new(tray_tx);
    // Load nova.toml first; CLI args override individual fields.
    let cfg  = config::NovaConfig::load();
    // Push HDR luminance parameters to the shim immediately after config load,
    // before the first Encoder::new() call that invokes BuildHdrMetadata().
    encoder::set_hdr_metadata(
        cfg.hdr.max_luminance_nits,
        cfg.hdr.max_cll_nits,
        cfg.hdr.max_fall_nits,
    );
    // Designate the streaming sink from nova.toml (no-op when empty). Must be
    // set before any sink resolution — recover_stuck_sink already ran above
    // with the built-in list only, so re-run it here: with an override, a
    // stuck default from a previous unclean exit might only now be
    // recognisable as "the sink".
    if !cfg.audio.endpoint_override.is_empty() {
        audio::set_sink_override(&cfg.audio.endpoint_override);
        audio::recover_stuck_sink();
    }
    // Parse from a FILTERED arg list: the service launches the host with
    // `--system-token <n>`, `--system-fallback`, and/or `--skip-vdd-cycle`
    // (handled in bin/main before run()), which clap does not know about.
    // Strip them so clap doesn't abort.
    let filtered_args = {
        let mut out: Vec<std::ffi::OsString> = Vec::new();
        let mut it = std::env::args_os();
        while let Some(a) = it.next() {
            if a == "--system-token" {
                let _ = it.next(); // skip its value
            } else if a == "--system-fallback" || a == "--skip-vdd-cycle" {
                // bare flags, no value to skip
            } else {
                out.push(a);
            }
        }
        out
    };
    let args = Args::parse_from(filtered_args);
    let width   = args.width  .unwrap_or(cfg.stream.width);
    let height  = args.height .unwrap_or(cfg.stream.height);
    let bitrate = args.bitrate.unwrap_or(cfg.stream.bitrate_kbps);
    let codec   = args.codec  .unwrap_or_else(|| cfg.stream.codec.clone());
    let fps     = args.fps    .unwrap_or(cfg.stream.fps);
    let fec     = args.fec    .unwrap_or(cfg.network.fec_percentage);
    let local_ip = get_local_ip();
    println!("=== Nova Server ===\n🌐 LAN IP: {}\n", local_ip);
    debug::debug_log(&format!("Nova started — {}x{} {} {} Kbps {} fps",
        width, height, codec, bitrate, fps));

    let server_id  = "0123456789ABCDEF";
    let server_mac = "00:11:22:33:44:55";

    // moonlight-common-c Limelight.h SCM bits: H264=0x1, HEVC(Main8)=0x100,
    // HEVC_MAIN10=0x200 → 0x301 = 769.
    //
    // The old value 259 (0x103) was built on a wrong map (0x100 believed to be
    // Main10): it advertised H264 + H264_HIGH8_444(0x2, unsupported) + HEVC
    // Main8, and NO Main10 bit. moonlight-common-c only sets
    // dynamicRangeMode:1 in ANNOUNCE when (client wants HDR) ∧ (server SCM has
    // 0x200) — so every client, Xbox included, silently declined HDR
    // (confirmed live 2026-07-06: /launch hdrMode=1 + clientSupportHevc:1 but
    // ANNOUNCE dynamicRangeMode:0 against SCM=259).
    // Old-protocol clients (Xbox Moonlight 1.18.0, corever=1) read
    // sprop-parameter-sets=AAAAAU in DESCRIBE for HEVC capability and the
    // fps cap handles graceful degradation for H264 fallback scenarios.
    // SCM bits (moonlight-common-c Limelight.h VIDEO_FORMAT_*): H264=0x1,
    // HEVC Main8=0x100, HEVC Main10=0x200, AV1 Main8=0x1000. 0x1301 advertises
    // H264 + HEVC(Main8/Main10) + AV1(Main8). AV1 uses the same GameStream
    // packetization as H264/HEVC; only rtp::detect_frame_type is codec-specific
    // (it parses OBUs for AV1). AV1 Main10/HDR (0x2000) is not advertised yet —
    // the shim's AV1 path is 8-bit and Codec::from_video_format only maps 0x1000.
    let codec_mode_support: u32 = 0x1301;
    let startup_codec = encoder::Codec::from_str(&codec);
    println!("🎥 ServerCodecModeSupport={codec_mode_support} (H264+HEVC+AV1); startup encoder: {}", startup_codec.as_str());
    if cfg.stream.enable_hdr {
        println!("✨ nova.toml: enable_hdr=true — HDR10 will activate for HEVC sessions regardless of VDD capability query");
    }

    let startup_frame_interval = Duration::from_secs_f64(1.0 / fps.max(1) as f64);
    let mut frame_interval = startup_frame_interval;

    // Owns the virtual-display lifecycle for the whole process. Audio endpoint
    // state is NOT this object's concern (Phase 15.1): crate::audio is the
    // single owner — audio::arm_endpoint_restore() is called below before each
    // activate_for_stream, and the AudioCaptureManager restores on stop.
    //
    // Enable Root\MttVDD ONCE, here, at boot, and leave it enabled for the
    // server's entire lifetime. The old code disabled/re-enabled the devnode
    // inside activate_for_stream on every session start, which raced the
    // IDD's transient 800x600 default mode against the client's requested
    // resolution. Bringing it up once at boot means the devnode has long
    // since settled at the configured mode by the time any client connects.
    let mut vd = virtual_display::VirtualDisplay::new();
    let virtual_device_name = match vd.ensure_enabled_at_boot(width as u32, height as u32, fps) {
        Ok(name) => name,
        Err(e) => {
            println!("❌ VDD BOOT PREFLIGHT FAILED: {e}");
            println!("   Virtual-display sessions will mirror the physical desktop until this is fixed.");
            vd.log_vdd_diagnostics();
            None
        }
    };

    // DesktopManager owns the capture backend for the whole process: WGC
    // normally, DDA while the secure desktop is up (maybe_swap_backend in the
    // frame loop below). One D3D11 device for the process lifetime, shared
    // with NVENC across every backend swap.
    let mut capturer = capture::DesktopManager::new_wgc(virtual_device_name.as_deref())
        .map_err(|e| {
            println!("❌ Failed to start desktop capture (WGC and DDA fallback): {e:?}");
            e
        })?;
    {
        let (ox, oy) = capturer.origin();
        input::set_active_capture_rect(ox, oy, capturer.width(), capturer.height());
    }

    // The WGC frame pool captures at the monitor's native resolution, which may
    // not match nova.toml's width/height target. The encoder and D3D11 video
    // processor must be sized to the ACTUAL captured texture — a mismatch leaves
    // the VP blitting into a differently-sized output, producing black/garbage in
    // the bottom portion of every encoded frame.
    if capturer.width() as i32 != width || capturer.height() as i32 != height {
        println!("⚠️  Monitor native resolution ({}x{}) differs from nova.toml target ({}x{}) — using native resolution for capture/encoder pipeline.",
            capturer.width(), capturer.height(), width, height);
    }

    let mut enc = Encoder::new(
        capturer.device(),
        EncoderConfig {
            width:        capturer.width() as i32,
            height:       capturer.height() as i32,
            fps:          fps as i32,
            bitrate_kbps: bitrate,
            codec:        startup_codec,
            is_hdr:       false, // upgraded per-session when client negotiates HEVC Main10/HDR
        },
    )
    .map_err(|e| {
        println!("❌ Failed to initialize NVENC encoder: {e}");
        // Encoder::new returns String errors; convert to a windows::core::Error
        // so run() can propagate via ? (run() returns windows::core::Result<()>).
        windows::core::Error::from(windows::Win32::Foundation::E_FAIL)
    })?;

    let client_info = Arc::new(Mutex::new(None::<rtsp::ClientInfo>));

    // RTSP server (blocking thread — owns the TCP listener)
    std::thread::spawn({
        let info = client_info.clone();
        move || rtsp::start_rtsp_server(48010, info)
    });

    // Tray actions — the monolithic twin of the Worker's `nova-tray-action-fwd`
    // thread. "End Stream" marks the session cancelled and disconnects the
    // client; the capture loop's own `!streaming_active` branch below then runs
    // the ordinary /cancel teardown (VDD released, physical monitor restored,
    // audio endpoint returned), leaving every listener bound. A dedicated OS
    // thread because the tray's receiver blocks.
    std::thread::Builder::new()
        .name("nova-tray-action".into())
        .spawn({
            let info = client_info.clone();
            move || {
                while let Ok(action) = tray_action_rx.recv() {
                    match action {
                        // Stop the stream AND restore the physical display, in
                        // one press — `cancelled = true` is what makes the
                        // capture loop's disconnect handling tear the VDD down
                        // rather than suspend it. Mirrors control_supervisor's
                        // EndSession arm; see it for why this is one press.
                        tray::TrayAction::EndStream => {
                            let ended = {
                                let mut guard = info.lock().unwrap_or_else(|e| e.into_inner());
                                match guard.as_mut() {
                                    Some(c) if c.streaming_active => {
                                        c.streaming_active = false;
                                        c.cancelled = true;
                                        Some(c.session_generation)
                                    }
                                    _ => None,
                                }
                            };
                            match ended {
                                Some(gen) => {
                                    println!("🛑 Tray: ending session {gen} and restoring the \
                                        physical display");
                                    // After the flags, never before — see
                                    // control::request_peer_kick.
                                    control::request_peer_kick();
                                }
                                None => println!("ℹ️  Tray: \"End Stream\" with no active session — nothing to end"),
                            }
                        }
                        // Press two: release the display. Marking the (already
                        // ended) session cancelled is what the monolithic
                        // capture loop's idle block watches for — it runs the
                        // same `deactivate_after_stream` + rebind the /cancel
                        // path does, so this adds no second teardown path.
                        tray::TrayAction::ReleaseDisplay => {
                            println!("🖥️  Tray: \"Release Display\" — tearing down the virtual \
                                display and restoring the physical monitor");
                            let mut guard = info.lock().unwrap_or_else(|e| e.into_inner());
                            match guard.as_mut() {
                                Some(c) => {
                                    c.streaming_active = false;
                                    c.cancelled = true;
                                }
                                // No ClientInfo at all (nothing has ever
                                // connected) means no VDD was ever activated
                                // for a session, so there is nothing held.
                                None => println!("ℹ️  Tray: no session state — nothing to release"),
                            }
                        }
                        tray::TrayAction::ClearPairedDevices => {
                            pairing::clear_all_paired();
                        }
                    }
                }
            }
        })
        .expect("spawn nova-tray-action thread");

    // Control stream (ENet/reliable-UDP) on port 47999. `None`: the
    // monolithic path has no Worker to talk to — IDR/congestion/input calls
    // straight into encoder/input in-process, unchanged from before the
    // Session-Survival Architecture existed.
    std::thread::spawn({
        let info = client_info.clone();
        move || control::start_control_server(47999, info, None)
    });

    // Pairing HTTP/HTTPS server (tokio task)
    tokio::spawn(crate::pairing::start_pairing_server(
        47989,
        local_ip.clone(),
        server_id.to_string(),
        server_mac.to_string(),
        client_info.clone(),
        codec_mode_support,
        tray_tx.clone(),
        global_pin.clone(),
    ));

    // mDNS — Sunshine-compatible service record
    let mdns = ServiceDaemon::new().expect("Failed to create mDNS daemon");
    let svc = ServiceInfo::new(
        "_nvstream._tcp.local.",
        "Nova",
        "nova.local.",
        local_ip.as_str(),
        47989,
        &[
            ("txtvers", "1"),
            ("port",     "47989"),
            ("mac",      server_mac),
            ("uniqueid", server_id),
        ][..],
    )
    .unwrap();
    let _ = mdns.register(svc);
    println!("📡 mDNS broadcaster started for Nova");

    // The Echo record — a second service type on the same daemon, for Echo
    // clients rather than Moonlight ones. Registered on a task because it must
    // wait for the pairing certificate; see `echo::discovery`.
    echo::discovery::spawn(&mdns, &cfg.echo, &local_ip);

    // Bind to the GameStream video port (47998) so RTP packets arrive from the
    // port advertised in the RTSP SETUP response — Moonlight validates the source port.
    let mut rtp_sender = crate::rtp::RtpSender::new(ECHO_MEDIA_PORT)
        .expect("Failed to bind RTP socket on 47998");

    // Audio port (48000) — the audio session's send thread learns the client's
    // address from its pings and sends Opus RTP back on this socket.
    let audio_socket = {
        let raw = socket2::Socket::from(
            std::net::UdpSocket::bind("0.0.0.0:48000")
                .expect("Failed to bind audio socket on 48000"),
        );
        // DSCP EF (0xB8) — same low-latency tag as the video socket.
        let _ = raw.set_tos(0xB8_u32);
        raw.set_nonblocking(true).expect("set_nonblocking on audio socket");
        std::net::UdpSocket::from(raw)
    };
    // Sole owner of the streaming audio lifecycle (sink swap, WASAPI capture,
    // endpoint restore). start_for_stream/stop_and_release are driven by the
    // session state machine below; the manager serializes sessions internally
    // so a /resume can never overlap a zombie session's audio teardown.
    let mut audio_manager = audio::AudioCaptureManager::new();

    let mut out_buffer       = vec![0u8; 8 * 1024 * 1024];
    let mut client_connected = false;
    let mut video_learned    = false;
    // The FIRST frame sent to the client each session must be an IDR. A leading
    // P-frame is fatal for AV1 (its P-frames carry no sequence header, so the
    // decoder can't initialize and rejects the stream — Moonlight kicks back to
    // the app list). NVENC's forced IDR can land on a warm-up frame that is
    // encoded before the client's video address is learned and thus never sent,
    // so we gate: drop leading P-frames and keep re-requesting an IDR until a
    // real keyframe is the first thing the client receives. No-op for H264/HEVC
    // (their first frame is already an IDR). Reset per session.
    let mut first_idr_sent   = false;
    let mut next_frame_time  = Instant::now();
    let mut frames_encoded   = 0u64;
    // Wire frame index (== NVENC inputTimeStamp), 1-based; reset per session.
    let mut wire_index: u32  = 1;
    // Frames refused by the RTP send thread's bounded queue (saturated link).
    // Each refusal triggers an IDR re-request; count is per session, for the
    // rate-limited diagnostic log.
    let mut send_queue_drops = 0u64;
    // Congestion control: the session's negotiated bitrate ceiling (written at
    // session start, reset on disconnect) plus the AIMD-with-memory controller
    // that decides what to do about loss — see QosController.
    let mut congestion_stable_kbps: u32 = 0;
    let mut qos = QosController::new();
    // Per-second encoder output rate — catches rate-control regressions
    // locally (works without any client connected), e.g. CBR overshooting
    // what the link/client can take.
    let mut enc_rate_bytes   = 0u64;
    // Frames in the current 1 s window (see run_worker's twin).
    let mut enc_rate_frames  = 0u32;
    let mut enc_rate_tick    = Instant::now();
    // Consecutive WGC iterations with no new frame (desktop unchanged).
    let mut timeout_streak = 0u32;
    // Throttle for the static-desktop diagnostic (see log_static_desktop).
    let mut last_static_log: Option<Instant> = None;
    // Stateful tick-tock for the damage-generator jiggle — alternates the
    // cursor between +1 and -1 each fire so it actually rests at a new
    // position for ~50 ms, guaranteeing DWM composites a fresh frame.
    let mut jiggle_toggle = false;

    // `signal::ctrl_c()` only ever fires for CTRL_C_EVENT. Closing the console
    // window, logging off, or a shutdown sends CTRL_CLOSE/LOGOFF/SHUTDOWN
    // instead — without these handlers the process is torn down without
    // running Rust destructors, so AudioCaptureManager's Drop (which restores
    // the default audio device away from the virtual sink) never runs and the
    // host is left silent. recover_stuck_sink() at startup is the last-resort
    // backstop for paths even these handlers can't catch (e.g. taskkill /F).
    let mut ctrl_close = signal::windows::ctrl_close().expect("register ctrl_close handler");
    let mut ctrl_shutdown = signal::windows::ctrl_shutdown().expect("register ctrl_shutdown handler");
    let mut ctrl_logoff = signal::windows::ctrl_logoff().expect("register ctrl_logoff handler");

    // Emergency display recovery for process-death paths (must come AFTER the
    // tokio watchers above — console handlers run in LIFO order, and the
    // synchronous CCD restore has to happen before tokio's handler parks the
    // notification thread). The session-monitor window covers WM_ENDSESSION,
    // which Windows can deliver (and then terminate us) without ever running
    // the console handler chain because the tray thread owns windows.
    shutdown::install_console_hook();
    shutdown::spawn_session_monitor();

    println!("▶️  Capture loop running — press Ctrl+C to stop");

    loop {
        // Frame pacing: sleep until the next frame slot, but also watch for shutdown signals.
        // While a client is streaming, hand the LAST ~1 ms to a spin-wait below:
        // even at 1 ms timer resolution the OS sleep wakes ±1 ms, which at
        // 120 fps (8.33 ms budget) is enough dispatch jitter to sample WGC a
        // frame late/early — visible as micro-stutter on motion. Idle (no
        // client) keeps the plain sleep: nobody is watching, don't burn CPU.
        let now = Instant::now();
        if now < next_frame_time {
            let wait = next_frame_time - now;
            let sleep_for = if client_connected {
                wait.saturating_sub(Duration::from_millis(1))
            } else {
                wait
            };
            tokio::select! {
                _ = tokio::time::sleep(sleep_for) => {}
                _ = signal::ctrl_c() => {
                    println!("\n🛑 Ctrl+C — shutting down ({} frames encoded)", frames_encoded);
                    break;
                }
                _ = ctrl_close.recv() => {
                    println!("\n🛑 Console closed — shutting down ({} frames encoded)", frames_encoded);
                    break;
                }
                _ = ctrl_shutdown.recv() => {
                    println!("\n🛑 System shutdown — shutting down ({} frames encoded)", frames_encoded);
                    break;
                }
                _ = ctrl_logoff.recv() => {
                    println!("\n🛑 User logoff — shutting down ({} frames encoded)", frames_encoded);
                    break;
                }
                _ = shutdown_rx.changed() => {
                    println!("\n🛑 Quit requested (tray or service stop) — shutting down ({} frames encoded)", frames_encoded);
                    // Under the service deployment, the host is respawned on exit
                    // by design — so a user "Quit" must also stop the service, or
                    // it just relaunches. Request the stop now (before teardown)
                    // so the service's worker won't respawn us; the service then
                    // grace-waits for this graceful teardown to finish. No-op when
                    // not launched by the service, and harmless when the shutdown
                    // ORIGINATED from a service stop (already STOP_PENDING, which
                    // the verdict reports as accepted).
                    //
                    // A REFUSED stop is the one case where breaking out is
                    // wrong: the service is alive and its job is to keep a host
                    // running, so exiting is a relaunch rather than a shutdown.
                    // Say so and keep streaming — same reasoning as the Worker
                    // path in run_worker.
                    match crate::service::request_service_stop() {
                        crate::service::ServiceStopRequest::Refused(why) => {
                            println!(
                                "❌ Quit ignored: NovaService refused to stop ({why}). Not \
                                 exiting — the service would respawn this host within seconds. \
                                 Stop the service directly (`sc stop NovaService` from an \
                                 elevated prompt) to shut Nova down."
                            );
                            // Fall through into the normal frame path — the
                            // session keeps running. `changed()` has consumed
                            // this edge, so the arm stays quiet until the next
                            // Quit rather than re-firing every iteration.
                        }
                        verdict => {
                            println!("🛑 Service stop: {verdict:?} — proceeding with teardown");
                            break;
                        }
                    }
                }
            }
            // Precise dispatch: spin out the sub-millisecond remainder.
            if client_connected {
                while Instant::now() < next_frame_time {
                    std::hint::spin_loop();
                }
            }
        }
        // Never a bare `+=` — missed slots are dropped, not repaid. See
        // advance_frame_deadline for the 179-fps catch-up-burst bug.
        next_frame_time = advance_frame_deadline(next_frame_time, frame_interval, Instant::now());

        // Pre-activate the virtual display as soon as /launch or /resume has
        // recorded a target mode — well before RTSP PLAY/control-connect.
        // The devcon/CCD switch in activate_for_stream is slow enough that
        // doing it after the control stream connects can stall long enough
        // for Moonlight to drop the connection before the first frame goes
        // out. Doing it here, during the handshake gap, gives it that time
        // without blocking the latency-critical path.
        if !client_connected {
            // Handle /cancel that arrived after the client already disconnected
            // (e.g. user backed out, VDD was suspended, then clicked "Quit App"),
            // and auto-teardown of a VDD left suspended (client disconnected
            // WITHOUT /cancel — the common "just quit Moonlight" flow) past
            // [stream] detach_grace_secs. Neither path's normal handler runs
            // while idle, so we do the full teardown here.
            //
            // This is the MONOLITHIC loop's copy of the grace clock. The split
            // deployment's clock lives in Master's `session_watcher` instead,
            // deliberately: this one dies with the process on every sign-out,
            // which is precisely a moment the grace period must survive.
            if vd.active_device_name().is_some() {
                let was_cancelled = client_info.lock()
                    .map(|g| g.as_ref().is_some_and(|c| c.cancelled))
                    .unwrap_or(false);
                let idle_timeout_secs = cfg.stream.detach_grace() as u64;
                let idle_timed_out = idle_timeout_secs > 0
                    && vd.suspended_idle_secs().is_some_and(|s| s >= idle_timeout_secs);
                if was_cancelled || idle_timed_out {
                    if was_cancelled {
                        println!("🛑 /cancel while suspended — tearing down virtual display");
                        debug::debug_log("Deferred /cancel: VDD teardown");
                    } else {
                        println!("🕐 Virtual display idle {idle_timeout_secs}s with no reconnect — tearing down (ghost-monitor guard)");
                        debug::debug_log("Idle timeout: VDD teardown");
                    }
                    if let Err(e) = vd.deactivate_after_stream() {
                        println!("⚠️  Virtual display deactivation: {e}");
                    }
                    enc.config.is_hdr = false;
                    frame_interval  = startup_frame_interval;
                    next_frame_time = Instant::now();
                    // Clear cancelled flag BEFORE the rebind attempt so this
                    // block cannot re-fire on the next loop iteration regardless
                    // of whether the rebind succeeds.
                    if let Ok(mut guard) = client_info.lock() {
                        if let Some(info) = guard.as_mut() {
                            info.cancelled = false;
                        }
                    }
                    // Rebind to the physical primary. If the display state is
                    // still settling after a topology-restore failure (error 87),
                    // this may fail with E_INVALIDARG. Do NOT break the loop —
                    // the server stays alive and the capturer recovers via WGC's
                    // internal ACCESS_LOST handling or the next
                    // activate_for_stream rebind when a new client connects.
                    // The display is released (or failed to release) — either
                    // way the tray's item now reflects what is actually held.
                    stats::set_teardown_pending(vd.active_device_name().is_some());
                    if let Err(e) = rebind_capture_and_encoder(&mut capturer, &mut enc, None, None, None, None) {
                        eprintln!("⚠️  Capture rebind after deferred cancel failed ({e}) — staying in idle loop");
                    }
                }
            }

            if let Ok(mut guard) = client_info.lock() {
                let pending = guard.as_ref()
                    .filter(|c| c.app_id != 0 && !c.activated && !c.streaming_active)
                    .map(|c| (c.app_id, c.width, c.height, c.fps, c.video_format, c.device_name.clone()));
                if let Some((app_id, width, height, fps, video_format, session_device_name)) = pending {
                    // Read HDR flag while we still hold the lock.
                    let hdr_req = guard.as_ref().map(|c| c.hdr_requested).unwrap_or(false);

                    // Derive codec from /launch videoFormat BEFORE rebind so the
                    // encoder is recreated at the right codec (H264/HEVC/AV1) for
                    // this session, not the CLI startup default.
                    // NOTE: do NOT force HEVC here even when hdrMode=1 — the ANNOUNCE
                    // SDP (dynamic_range_mode) hasn't arrived yet and is the authoritative
                    // gate. Forcing HEVC at pre-activation produces an H264 client (e.g.
                    // Xbox Moonlight 1.18.0) receiving an HEVC stream it can't decode.
                    let negotiated_codec = encoder::Codec::from_video_format(video_format);
                    if negotiated_codec != enc.config.codec {
                        println!("🎥 Codec selected by client: {} (videoFormat={:#x}) — switching encoder",
                            negotiated_codec.as_str(), video_format);
                        enc.config.codec  = negotiated_codec;
                        enc.config.is_hdr = false; // reset; re-armed below if HDR is also requested
                    }
                    enc.config.fps = fps as i32;
                    let vdd_ok = if app_launcher::uses_virtual_display(app_id, cfg.stream.headless_for_all_apps) {
                        // Capture the restore target BEFORE the VDD flip: once the
                        // virtual display is primary, Windows may auto-switch the
                        // default endpoint to its HDMI audio device — arming after
                        // that would restore to the wrong endpoint at session end.
                        audio::arm_endpoint_restore();
                        // HOT RESUME, mirroring apply_configure_start's — a client
                        // returning inside the detach grace window is looking at a
                        // display that is already right, and rebuilding it costs
                        // seconds of visible rearrangement to arrive where we
                        // started. Everything after this point (rename, rebind, the
                        // HDR flip) runs identically either way; only the
                        // activation itself is skipped.
                        let activation = match vd.resume_suspended(width, height, fps) {
                            virtual_display::Resume::Reused => Ok(()),
                            virtual_display::Resume::Mismatch(why) => {
                                if vd.active_device_name().is_some() {
                                    println!("🖥️  Cannot reclaim the existing virtual display ({why}) — full activation");
                                }
                                println!("🖥️  Pre-activating virtual display for upcoming session ({width}x{height}@{fps}fps{})",
                                    if hdr_req { " HDR10" } else { "" });
                                vd.activate_for_stream(width, height, fps)
                            }
                        };
                        match activation {
                            Ok(()) => {
                                // Rename the virtual monitor so Display Settings and Device
                                // Manager show the client device name (e.g. "Xbox") instead
                                // of the driver's generic "VDD by MTT" label.
                                if !session_device_name.is_empty() {
                                    match vd.rename_devnode(&session_device_name) {
                                        Ok(()) => println!("🏷️  Virtual monitor renamed to \"{}\"", session_device_name),
                                        Err(e) => println!("⚠️  Monitor rename: {e}"),
                                    }
                                }
                                if rebind_capture_and_encoder(&mut capturer, &mut enc, vd.active_device_name(),
                                    Some((width, height)), Some((width, height)), None).is_err() {
                                    break;
                                }
                                // Enable Advanced Color (HDR/scRGB) during the /launch→PLAY gap
                                // so the ACCESS_LOST storm from the color-space switch settles
                                // before any frames need to be sent. By connect-time the VDD is
                                // already stable in FP16 mode — calling set_active_display_hdr
                                // again there is a no-op (no second storm).
                                if hdr_req {
                                    // enable_hdr=true in nova.toml lets the user force HDR
                                    // even when is_advanced_color_supported() is slow to
                                    // reflect HDRPlus=true after a devnode cycle.
                                    let hdr_ok = cfg.stream.enable_hdr || vd.is_advanced_color_supported();
                                    if hdr_ok {
                                        // Force a full SDR→HDR cycle rather than a guarded enable.
                                        // On devnode re-enable (HDRPlus=true in EDID) Windows may
                                        // auto-enable Advanced Color, so the idempotent
                                        // set_active_display_hdr(true) would see "already enabled"
                                        // and skip — leaving stale MDCV/MaxCLL SEI from the
                                        // previous session and causing washed-out colours on reconnect.
                                        if let Err(e) = vd.force_hdr_reconnect_cycle() {
                                            println!("⚠️  Advanced Color pre-activation failed: {e}");
                                        } else {
                                            println!("⏳ Waiting for VDD to settle in HDR/FP16 mode...");
                                            std::thread::sleep(Duration::from_secs(2));
                                            // Recreate the WGC frame pool in R16G16B16A16Float now
                                            // that the VDD surface is in Advanced Color (FP16 scRGB)
                                            // mode. enc.config.is_hdr is still false here (codec not
                                            // confirmed until ANNOUNCE/PLAY), so we cannot use
                                            // rebind_capture_and_encoder — it would pass is_hdr=false
                                            // and create a BGRA8 pool that WGC would silently tone-map
                                            // to SDR, feeding wrong data to the NVENC HDR pipeline.
                                            match capturer.rebind(vd.active_device_name(), true, Some((width, height))) {
                                                Ok(_) => {
                                                    let (ox, oy) = capturer.origin();
                                                    input::set_active_capture_rect(ox, oy, capturer.width(), capturer.height());
                                                    println!("✅ WGC frame pool recreated in FP16 — VDD in HDR/Advanced Color mode");
                                                }
                                                Err(e) => eprintln!("⚠️  WGC FP16 rebind failed: {e} — HDR frames may be tone-mapped to SDR"),
                                            }
                                            println!("✅ VDD in FP16 HDR mode — encoder pipeline ready for HEVC Main10");
                                        }
                                    } else {
                                        println!("⚠️  MttVDD does not support Advanced Color (HDR) — \
                                            streaming HEVC SDR. Set enable_hdr=true in nova.toml or \
                                            enable HDRPlus in vdd_settings.xml for true HDR10 output.");
                                    }
                                }
                                true
                            }
                            Err(e) => {
                                println!("⚠️  Virtual display activation failed: {e} — streaming from the physical display");
                                false
                            }
                        }
                    } else {
                        // universal VDD: this branch is unreachable
                        true
                    };
                    // Only mark activated=true on success; a failure leaves it
                    // false so the connect-time fallback can retry activate_for_stream.
                    if vdd_ok {
                        if let Some(info) = guard.as_mut() {
                            info.activated = true;
                        }
                    }
                }
            }
        }

        // Latch Moonlight client info the moment RTSP PLAY arrives. Clone
        // and drop the lock immediately — the setup below (NVENC reconfigure,
        // WASAPI audio pipeline, ViGEm probe) can take real wall-clock time,
        // and holding the client_info mutex across it would block the
        // control thread's handle_event (PT_ENCRYPTED/PERIODIC_PING/Disconnect
        // all lock client_info), starving ENet's host.service() poll loop and
        // making the client think the connection is dead.
        if !client_connected {
            let client = client_info.lock().ok()
                .and_then(|g| g.as_ref().filter(|c| c.streaming_active).cloned());
            if let Some(client) = client {
                {
                        println!("🎮 Moonlight connected: {} ({}x{}@{}fps)",
                            client.ip, client.width, client.height, client.fps);
                        debug::debug_log(&format!("Client connected {}", client.ip));

                        // Log the codec that was negotiated vs what the encoder delivers.
                        let vf_name = if client.video_format & 0x100 != 0 { "HEVC Main10" }
                            else if client.video_format & 0x002 != 0 { "HEVC Main" }
                            else { "H264" };
                        let enc_name = enc.config.codec.as_str();
                        let hdr_sfx  = if client.hdr_requested { " [HDR requested]" } else { "" };
                        println!("🔑 Codec negotiation: client={}{} (videoFormat={:#x})  encoder={}{}",
                            vf_name, hdr_sfx, client.video_format, enc_name,
                            if enc.config.is_hdr { "/HDR10" } else { "" });

                        // Derive codec from /launch videoFormat. Old-protocol clients
                        // (Xbox Moonlight ≤ 1.18.0) never set videoFormat — the field
                        // arrives as 0 in that case. For those clients, use
                        // bitStreamFormat from the RTSP ANNOUNCE SDP instead: it is
                        // set by moonlight-common-c based on (client caps ∩ server
                        // ServerCodecModeSupport) and is the authoritative codec for
                        // the wire stream regardless of protocol version.
                        let negotiated_codec = {
                            let raw = if client.video_format != 0 {
                                encoder::Codec::from_video_format(client.video_format)
                            } else {
                                match client.bit_stream_format {
                                    1 => encoder::Codec::Hevc,
                                    2 => encoder::Codec::Av1,
                                    _ => encoder::Codec::H264,
                                }
                            };
                            // HDR10 requires HEVC Main10. Override to HEVC ONLY when
                            // dynamic_range_mode == 1 (client confirmed HDR in its ANNOUNCE)
                            // or enable_hdr=true in nova.toml (operator override).
                            // DO NOT use hdr_requested alone — it reflects what the USER asked
                            // for but not what the client can actually decode. Clients that
                            // cannot do HDR (e.g. Xbox Moonlight 1.18.0) send dynamicRangeMode:0
                            // in their ANNOUNCE; forcing HEVC on them produces a guaranteed
                            // 10-second watchdog timeout since they have no HEVC decoder.
                            let client_confirmed_hdr = client.dynamic_range_mode == 1
                                || cfg.stream.enable_hdr;
                            if client_confirmed_hdr && raw == encoder::Codec::H264 {
                                println!("🎨 ANNOUNCE confirmed HDR (dynamicRangeMode={}) — \
                                    overriding H.264 → HEVC Main10 \
                                    (videoFormat={:#x} bitStreamFormat={})",
                                    client.dynamic_range_mode, client.video_format,
                                    client.bit_stream_format);
                                encoder::Codec::Hevc
                            } else {
                                raw
                            }
                        };
                        let bsf_name = match client.bit_stream_format { 1=>"HEVC", 2=>"AV1", _=>"H264" };
                        println!("🎥 Codec: {} (videoFormat={:#x}  bitStreamFormat={}/{})",
                            negotiated_codec.as_str(), client.video_format,
                            client.bit_stream_format, bsf_name);

                        // H264 Level 5.2 fps cap — applied after codec determination so we
                        // know whether we're actually in H264. Xbox Moonlight 1.18.0
                        // (corever=1) hardwires H264 and cannot negotiate HEVC from the
                        // server side; at 4K or 1440p@120fps that exceeds H264 Level 5.2
                        // (983,040 MB/s). Cap fps to what Level 5.2 allows (4K→30fps,
                        // 1440p→60fps, 1080p→120fps) so the stream works instead of
                        // crashing the Xbox hardware H264 decoder.
                        let session_fps: u32 = {
                            let mb_per_frame = ((client.width + 15) / 16) as u64
                                * ((client.height + 15) / 16) as u64;
                            let mb_per_sec = mb_per_frame * client.fps as u64;
                            if negotiated_codec == encoder::Codec::H264 && mb_per_sec > 983_040 {
                                let safe = (983_040u64 / mb_per_frame).max(1) as u32;
                                println!("⚠️  H264 Level 5.2 cap: {}x{}@{}fps = {} MB/s > 983,040. \
                                    Reducing to {}fps so Xbox H264 decoder won't crash. \
                                    (HEVC needed for higher fps — client corever=1 cannot negotiate it.)",
                                    client.width, client.height, client.fps, mb_per_sec, safe);
                                enc.config.fps = safe as i32;
                                safe
                            } else {
                                client.fps
                            }
                        };

                        if negotiated_codec != enc.config.codec {
                            enc.config.codec  = negotiated_codec;
                            enc.config.is_hdr = false;
                            // rebind_capture_and_encoder only recreates NVENC when the
                            // capture RESOLUTION changes. A pure codec switch (same VDD,
                            // same mode) returns needs_new_encoder=false — the H264
                            // encoder would keep running. Force recreation here directly.
                            enc.cleanup();
                            match encoder::Encoder::new(capturer.device(), encoder::EncoderConfig {
                                width:        capturer.width() as i32,
                                height:       capturer.height() as i32,
                                fps:          enc.config.fps,
                                bitrate_kbps: enc.config.bitrate_kbps,
                                codec:        negotiated_codec,
                                is_hdr:       false,
                            }) {
                                Ok(new_enc) => enc = new_enc,
                                Err(e) => {
                                    eprintln!("❌ Failed to recreate NVENC for codec change: {e}");
                                    break;
                                }
                            }
                            let (ox, oy) = capturer.origin();
                            input::set_active_capture_rect(ox, oy, capturer.width(), capturer.height());
                        }

                        // HDR10 pipeline activation gate:
                        //   - dynamic_range_mode == 1: client ANNOUNCE confirmed HDR. This is
                        //     the authoritative source. Xbox Moonlight 1.18.0 sends 0 here
                        //     (no HEVC/HDR10 support) — it must receive H264/SDR.
                        //   - cfg.stream.enable_hdr: operator override in nova.toml bypasses
                        //     the client negotiation (useful when the EDID query is slow).
                        //   - hdr_requested alone is NOT sufficient: it reflects the user's
                        //     intent but not the client's decoder capability.
                        let client_confirmed_hdr = client.dynamic_range_mode == 1
                            || cfg.stream.enable_hdr;

                        // Revert: if pre-activation enabled FP16 on the VDD but the client
                        // declined HDR in ANNOUNCE (dynamicRangeMode=0), we must switch back
                        // to BGRA8/SDR now. The H.264 SDR encoder's shim uses BGRA8 as the
                        // capture source format; feeding it FP16 frames causes CopyResource
                        // format mismatches that produce garbage or zero-byte output.
                        if client.hdr_requested && !client_confirmed_hdr && vd.active_device_name().is_some() {
                            println!("⚠️  Client declined HDR (ANNOUNCE dynamicRangeMode=0) — \
                                reverting VDD to SDR/BGRA8 (H.264 cannot process FP16 frames)");
                            let _ = vd.set_active_display_hdr(false);
                            if let Err(e) = rebind_capture_and_encoder(&mut capturer, &mut enc,
                                vd.active_device_name(), Some((client.width, client.height)), Some((client.width, client.height)), None) {
                                eprintln!("⚠️  SDR rebind after HDR revert: {e}");
                            }
                        }

                        let hdr_ok = cfg.stream.enable_hdr || vd.is_advanced_color_supported();
                        if client_confirmed_hdr && enc.config.codec == encoder::Codec::Hevc && !enc.config.is_hdr
                            && hdr_ok
                        {
                            // Advanced Color was enabled in pre-activation (during the
                            // /launch→PLAY gap). Calling set_active_display_hdr(true) again
                            // when it is already on is a no-op — no ACCESS_LOST storm.
                            // If pre-activation somehow didn't run, this enables it now.
                            let _ = vd.set_active_display_hdr(true);
                            // Recreate NVENC as HEVC Main10/P010.
                            println!("🎨 HEVC Main10/HDR10 encoder active (hdrMode=1, VDD in FP16 mode)");
                            enc.config.is_hdr = true;
                            enc.cleanup();
                            match encoder::Encoder::new(capturer.device(), encoder::EncoderConfig {
                                // Session geometry, not the capture's — see recreate_encoder_at.
                                width:        client.width as i32,
                                height:       client.height as i32,
                                fps:          enc.config.fps,
                                bitrate_kbps: enc.config.bitrate_kbps,
                                codec:        enc.config.codec,
                                is_hdr:       true,
                            }) {
                                Ok(new_enc) => enc = new_enc,
                                Err(e) => {
                                    eprintln!("❌ Failed to recreate NVENC for HDR: {e}");
                                    break;
                                }
                            }
                            // Rebind so the new P010 NVENC input textures are wired to the
                            // FP16→P010 VP output. Advanced Color is already on so no
                            // ACCESS_LOST expected — this is a clean re-DuplicateOutput.
                            if rebind_capture_and_encoder(&mut capturer, &mut enc,
                                vd.active_device_name(), Some((client.width, client.height)), Some((client.width, client.height)), None).is_err() {
                                break;
                            }
                        }

                        // Resolution / FPS / HDR summary — the single most
                        // useful line for diagnosing stream failures.
                        // NOTE: print the LIVE codec, not enc_name — enc_name was
                        // captured before the ANNOUNCE-driven codec switch above and
                        // showed "h264" for sessions that were actually HEVC.
                        // Parity with apply_configure_start (see its call site).
                        refresh_sdr_white_level(&vd);
                        println!("📐 Encoder: {}x{}@{}fps {}{}  |  Client requested: {}x{}@{}fps{}",
                            enc.config.width, enc.config.height, enc.config.fps,
                            enc.config.codec.as_str(),
                            if enc.config.is_hdr { "/HDR10" } else { "" },
                            client.width, client.height, client.fps,
                            if client.hdr_requested { " HDR" } else { "" });

                        // Normally already done by the pre-activation pass
                        // above during the /launch -> PLAY gap. Fall back to
                        // doing it here if that somehow hasn't run yet (e.g.
                        // PLAY arrived before the first idle-loop tick).
                        // audio::arm_endpoint_restore() must run before the
                        // VDD flip AND before start_for_stream below changes
                        // the default device (single-owner endpoint state).
                        if client.activated {
                            // VDD topology is already up from pre-activation.
                            // Force WGC + NVENC recreation to match the session's
                            // negotiated format (codec/HDR may have changed since
                            // pre-activation ran, and the "already active" path
                            // previously skipped this entirely).
                            println!("🖥️  Virtual display already active — forcing WGC+NVENC recreation \
                                ({}x{} {})", client.width, client.height,
                                if enc.config.is_hdr { "FP16/HDR10" } else { "BGRA8/SDR" });
                            if rebind_capture_and_encoder(&mut capturer, &mut enc,
                                vd.active_device_name(), Some((client.width, client.height)), Some((client.width, client.height)), None).is_err() {
                                break;
                            }
                        } else if app_launcher::uses_virtual_display(client.app_id, cfg.stream.headless_for_all_apps) {
                            // Pre-activation didn't run (PLAY arrived before the first idle-loop tick).
                            // Activate the VDD now, then rename the virtual monitor.
                            audio::arm_endpoint_restore();
                            match vd.activate_for_stream(client.width, client.height, client.fps) {
                                Ok(()) => {
                                    if !client.device_name.is_empty() {
                                        match vd.rename_devnode(&client.device_name) {
                                            Ok(()) => println!("🏷️  Virtual monitor renamed to \"{}\"", client.device_name),
                                            Err(e) => println!("⚠️  Monitor rename: {e}"),
                                        }
                                    }
                                    if rebind_capture_and_encoder(&mut capturer, &mut enc, vd.active_device_name(), Some((client.width, client.height)), Some((client.width, client.height)), None).is_err() {
                                        break;
                                    }
                                }
                                Err(e) => println!("⚠️  Virtual display activation failed: {e} — stream may have wrong resolution"),
                            }
                            // Mirror the pre-activation pass: mark activated so the idle-loop
                            // doesn't attempt a second activate once streaming starts.
                            if let Ok(mut guard) = client_info.lock() {
                                if let Some(info) = guard.as_mut() {
                                    info.activated = true;
                                }
                            }
                        } else {
                            // headless_for_all_apps=false and non-VD app: capture stays on
                            // the physical primary display, scaled by the shim into the
                            // session's negotiated geometry.
                            if rebind_capture_and_encoder(&mut capturer, &mut enc, None, None,
                                Some((client.width, client.height)), None).is_err() {
                                break;
                            }
                        }

                        // Resolution guard — runs regardless of activated path.
                        // If wait_for_display_resolution timed out during pre-activation
                        // (common for 4K@120fps modes that take >3 s to settle), the VDD
                        // may have landed at 1080p instead of 4K. Give it one more
                        // re-snap and rebind attempt now, while the client is waiting.
                        // Compares the CAPTURE now — the encoder is always already at the
                        // session's geometry, and a mismatched capture is a sharpness
                        // issue (the shim scales it), not a correctness one.
                        if capturer.width() != client.width || capturer.height() != client.height {
                            println!("📐 Resolution re-snap: capture={}x{}  client={}x{}@{}fps — retrying VDD force",
                                capturer.width(), capturer.height(), client.width, client.height, client.fps);
                            vd.re_snap_resolution(client.width, client.height, client.fps);
                            if rebind_capture_and_encoder(&mut capturer, &mut enc, vd.active_device_name(), Some((client.width, client.height)), Some((client.width, client.height)), None).is_err() {
                                break;
                            }
                        }

                        rtp_sender.set_fps(session_fps.max(1));
                        rtp_sender.set_codec(
                            enc.config.codec == encoder::Codec::Hevc,
                            enc.config.codec == encoder::Codec::Av1,
                        );
                        let negotiated_interval = Duration::from_secs_f64(1.0 / session_fps.max(1) as f64);
                        if negotiated_interval != frame_interval {
                            frame_interval = negotiated_interval;
                            next_frame_time = Instant::now(); // rebase pacing — prevents burst if interval shrank
                            println!("⏱️  Frame interval → {:.2}ms ({} fps{})",
                                frame_interval.as_secs_f64() * 1000.0, session_fps,
                                if session_fps != client.fps {
                                    format!(" [capped from {}fps for H264 Level 5.2]", client.fps)
                                } else {
                                    " (client-negotiated)".to_string()
                                });
                        }
                        // Shard size MUST match the client's negotiated
                        // packetSize (1392 LAN / 1024 remote) or its FEC
                        // reconstruction runs over the wrong block size.
                        let pkt_size = if client.packet_size >= 512 {
                            client.packet_size as usize
                        } else {
                            1024
                        };
                        let min_fec = if client.min_fec_packets > 0 {
                            client.min_fec_packets as usize
                        } else {
                            2
                        };
                        println!("📡 Client negotiated packetSize={} (announced: {}), fps={}, fec.minRequired={}",
                            pkt_size, client.packet_size, client.fps, min_fec);
                        // We encode at the monitor's NATIVE resolution, but the
                        // client chose its bitrate for the mode it requested. If
                        // native is larger, every bit is stretched over more
                        // pixels — shows up as uniform shimmer/soft blocking.
                        let native_px = capturer.width() as u64 * capturer.height() as u64;
                        let client_px = client.width as u64 * client.height as u64;
                        if client_px > 0 && native_px > client_px {
                            println!("⚠️  Encoding {}x{} (native) but client requested {}x{} — bitrate is stretched {:.1}x thinner per pixel. Raise Moonlight's bitrate or match resolutions.",
                                capturer.width(), capturer.height(), client.width, client.height,
                                native_px as f64 / client_px as f64);
                        }
                        rtp_sender.configure(pkt_size, fec as usize, min_fec);

                        // Retarget CBR to the client's negotiated bitrate.
                        // Without this the encoder streams at the CLI default
                        // (15 Mbps) regardless of what the client asked for —
                        // under CBR that's a constant overshoot that makes
                        // Moonlight warn "lower your bitrate" and disconnect.
                        if client.bitrate_kbps > 0 {
                            println!("📊 Retargeting encoder to client bitrate: {} Kbps @ {} fps",
                                client.bitrate_kbps, session_fps);
                            encoder::reconfigure_bitrate(client.bitrate_kbps, session_fps);
                            encoder::set_stream_bitrate_kbps(client.bitrate_kbps as i32);
                            congestion_stable_kbps = client.bitrate_kbps;
                            wire_index = 1; // fresh session ⇒ client expects frame 1
                            // Fresh controller per session: the previous
                            // session's remembered failure point says nothing
                            // about this one's link budget (QosController::tick
                            // also self-heals via the ceiling comparison, but
                            // resetting here keeps the clocks honest too).
                            qos = QosController::new();
                            // Mirror negotiated values into enc.config so any
                            // mid-session rebind (resolution/device change) inherits
                            // the session fps (may be capped below client.fps for H264)
                            // and bitrate, not the CLI default.
                            enc.config.bitrate_kbps = client.bitrate_kbps as i32;
                            enc.config.fps          = session_fps.max(1) as i32;
                        } else {
                            println!("⚠️  Client did not announce a bitrate — keeping nova.toml default {} Kbps", bitrate);
                        }

                        // Start the audio pipeline (WASAPI → Opus → RTP 48000).
                        let pkt_dur = if client.audio_packet_duration > 0 {
                            client.audio_packet_duration
                        } else {
                            5
                        };
                        // audio::AudioCaptureManager only speaks "raw Opus
                        // bytes over a channel" now (the split architecture
                        // moved RTP/AES/UDP-send to Master's AudioTxState —
                        // see audio.rs's module doc). The monolithic path has
                        // no IPC boundary to cross, so relay locally: a
                        // dedicated thread owns a fresh AudioTxState on the
                        // SAME long-lived audio_socket and forwards every
                        // frame from this session's channel to it. Dropping
                        // the old session's Sender (via stop_and_release's
                        // join, just above) ends the old relay thread
                        // automatically — no explicit stop signal needed.
                        let (audio_frame_tx, audio_frame_rx) = std::sync::mpsc::channel::<Vec<u8>>();
                        let mut audio_tx_state = audio::AudioTxState::new(
                            audio_socket.try_clone().expect("clone audio socket")
                        );
                        audio_tx_state.reconfigure(client.rikey, client.rikeyid, client.audio_encryption, pkt_dur);
                        std::thread::spawn(move || {
                            while let Ok(opus_bytes) = audio_frame_rx.recv() {
                                audio_tx_state.send_frame(&opus_bytes);
                            }
                        });
                        audio_manager.start_for_stream(
                            audio_frame_tx,
                            pkt_dur,
                            // localAudioPlayMode: false = client-only (route
                            // audio through a virtual sink, host speakers stay
                            // silent), true = also play on the host speakers.
                            client.host_audio,
                        );
                        // Plug in the virtual Xbox 360 controller(s) for
                        // split-seat gamepad passthrough (input.rs).
                        input::start_session();
                        client_connected = true;
                        // Publish the session to the tray's Server Stats window
                        // (monolithic twin of the Worker's ConfigureStart arm).
                        // Read from the ENCODER and the CAPTURER, not from the
                        // client's request: HDR/codec renegotiation and any
                        // resolution re-snap have already landed by here.
                        stats::session_started(
                            capturer.width(),
                            capturer.height(),
                            enc.config.fps.max(1) as u32,
                            enc.config.codec,
                            enc.config.is_hdr,
                            enc.config.bitrate_kbps.max(0) as u32,
                        );
                        // Tell the service a client is active — see
                        // service::set_client_connected's doc comment. Defers
                        // a SYSTEM-fallback→interactive upgrade so entering
                        // the Windows PIN over Moonlight doesn't get cut off.
                        service::set_client_connected(true);
                }
            }
        } else {
            // RTSP TEARDOWN or control-stream drop sets streaming_active=false.
            // Check whether /cancel was also signalled to determine the path:
            //   • cancelled=true  → full VDD teardown (user clicked "Quit App")
            //   • cancelled=false → suspend (user backed out; /resume reconnects)
            let (still_active, was_cancelled) = client_info.lock()
                .map(|g| g.as_ref()
                    .map(|c| (c.streaming_active, c.cancelled))
                    .unwrap_or((false, false)))
                .unwrap_or((false, false));
            if !still_active {
                // Always: stop stream outputs and virtual input devices.
                // stop_and_release also restores the pre-stream default audio
                // endpoint (claim-once) — it must run BEFORE the VDD teardown
                // below so the restore happens while the endpoint topology is
                // still the in-stream one.
                rtp_sender.reset();
                audio_manager.stop_and_release();
                input::stop_session();
                // Tray back to idle — mirrors deactivate_worker's identical
                // call on the split path.
                stats::session_ended();
                frame_interval  = startup_frame_interval;
                next_frame_time = Instant::now();
                client_connected    = false;
                service::set_client_connected(false);
                video_learned       = false;
                first_idr_sent      = false; // next session must open with an IDR
                send_queue_drops    = 0;
                congestion_stable_kbps = 0;
                encoder::set_stream_bitrate_kbps(0);

                // ── Scorched-earth encoder teardown ──────────────────────────
                // Always destroy the full C++ NVENC/D3D11/VP/RTV pipeline on
                // every disconnect so the next /launch always re-initialises
                // from a clean slate.  Without this, stale g_isHdr / RTV / VP
                // state (and the carried-over enc.config.is_hdr=true) causes
                // the HDR init block's `!enc.config.is_hdr` guard to be false
                // on reconnect — the encoder is never recreated, and subtle
                // NVENC/D3D state from the previous session leaks through.
                let was_hdr = enc.config.is_hdr;
                enc.config.is_hdr = false;
                enc.cleanup(); // releases g_nvEncoder, g_device, VP, RTVs in shim.cpp

                // Disable Windows Advanced Color so the VDD drops back to BGRA8
                // while idle — the next /launch pre-activation re-enables it.
                if was_hdr {
                    if let Err(e) = vd.set_active_display_hdr(false) {
                        println!("⚠️  Advanced Color disable on disconnect: {e}");
                    }
                }

                if was_cancelled {
                    println!("🛑 /cancel — tearing down virtual display, restoring host topology");
                    debug::debug_log("Session cancelled — full VDD teardown");
                    if let Err(e) = vd.deactivate_after_stream() {
                        println!("⚠️  Virtual display deactivation failed: {e}");
                    }
                    let _ = capturer.rebind(None, false, None);
                    if let Ok(mut guard) = client_info.lock() {
                        if let Some(info) = guard.as_mut() {
                            info.cancelled = false;
                        }
                    }
                } else {
                    // Suspend — VDD stays at the current resolution for fast reconnect.
                    // Advanced Color is now off (above) so WGC provides BGRA8 frames
                    // while idle.  The next /launch pre-activation re-enables HDR when
                    // the client negotiates HEVC Main10.
                    println!("⏸️  Client disconnected — encoder torn down; VDD active for /launch reconnect");
                    debug::debug_log("Session suspended — VDD active, encoder torn down");
                    vd.mark_suspended(); // starts the idle_teardown_secs clock (see config.rs)
                    let _ = capturer.rebind(vd.active_device_name(), false, None);
                }

                // Force-create a new SDR encoder so enc is never in a null-handle
                // state between sessions.  rebind_capture_and_encoder only recreates
                // NVENC when the capture RESOLUTION changes, so an explicit rebuild
                // is needed here regardless of whether the resolution changed.
                match encoder::Encoder::new(capturer.device(), encoder::EncoderConfig {
                    width:        capturer.width()  as i32,
                    height:       capturer.height() as i32,
                    fps:          enc.config.fps,
                    bitrate_kbps: enc.config.bitrate_kbps,
                    codec:        enc.config.codec,
                    is_hdr:       false,
                }) {
                    Ok(new_enc) => enc = new_enc,
                    Err(e)      => eprintln!("❌ Failed to rebuild encoder after disconnect: {e}"),
                }
                let (ox, oy) = capturer.origin();
                input::set_active_capture_rect(ox, oy, capturer.width(), capturer.height());
                // Whether the tray's second press has anything to release —
                // same rule as the Worker path's `deactivate_worker`: read the
                // display, not the intent.
                stats::set_teardown_pending(vd.active_device_name().is_some());
            }
        }

        // Learn (and keep refreshed) the client's real video UDP address from
        // its "ping" packets — the source port is ephemeral, wasn't known at
        // SETUP time, and CHANGES on reconnect. This must run every iteration,
        // not just until first learn: it drains the ping backlog (a stale
        // buffered ping from the old session would otherwise become the next
        // session's target → black screen) and follows mid-stream port changes.
        if client_connected {
            if let Some(addr) = rtp_sender.try_learn_target() {
                println!("🎥 Learned client video address: {}", addr);
                debug::debug_log(&format!("Video target {}", addr));
                video_learned = true;
                // Force a fresh IDR (with inline SPS/PPS) on the very next encoded
                // frame — the first one we'll actually transmit — so the client's
                // decoder can initialize immediately.
                enc.request_idr();
                println!("🎯 Force-IDR requested for first transmitted frame");
            }
        }

        // ── Dynamic bitrate (QoS) ────────────────────────────────────────────
        // Same control loop the Worker runs — see QosController. Kept as one shared
        // function so the two capture loops can never drift apart again (they
        // already had, which is how dynamic bitrate ended up dead in the split
        // deployment while working here).
        if client_connected {
            qos.tick(congestion_stable_kbps, enc.config.fps.max(1) as u32);
        }

        // ── 1 Hz telemetry tick ──────────────────────────────────────────────
        // Twin of run_worker's identical block — see it for why this lives in
        // the loop body rather than inside the encode branch (a stalled capture
        // must read as 0.0 fps in the tray, not freeze at the last good sample).
        {
            let elapsed = enc_rate_tick.elapsed();
            if elapsed >= Duration::from_secs(1) {
                if enc_rate_frames > 0 {
                    println!("🎞  Encoder output: {} Kbps", (enc_rate_bytes * 8) / 1000);
                }
                if client_connected {
                    stats::sample(
                        enc_rate_frames,
                        enc_rate_bytes,
                        elapsed.as_millis() as u64,
                        encoder::get_stream_bitrate_kbps().max(0) as u32,
                    );
                }
                enc_rate_bytes = 0;
                enc_rate_frames = 0;
                enc_rate_tick = Instant::now();
            }
        }

        // ── Secure-desktop backend swap (Phase 15.2) ─────────────────────────
        // Keep the capture backend matched to the input desktop: WGC normally,
        // DDA while a UAC prompt / logon screen holds the secure desktop.
        // Steady state this is two atomic loads. Only while streaming — an
        // idle host has nobody watching, and WGC recovers by itself.
        if client_connected {
            if let Some(resized) = capturer.maybe_swap_backend() {
                if resized {
                    // Swap landed on a different-sized output (e.g. headless VDD
                    // session falling back to the physical primary) — the encoder
                    // must match the new capture dimensions.
                    if recreate_encoder_for_capture(&capturer, &mut enc).is_err() {
                        break;
                    }
                } else {
                    // Same size, same device — new backend session needs a fresh
                    // IDR so the client can decode from the first swapped frame.
                    enc.request_idr();
                }
                let (ox, oy) = capturer.origin();
                input::set_active_capture_rect(ox, oy, capturer.width(), capturer.height());
            }
        } else if capturer.backend_kind() == capture::BackendKind::Dda {
            // Idle heal only: a host that booted pre-login starts on DDA (see
            // DesktopManager::new_wgc). Once the user logs in, hand the
            // interactive desktop back to WGC even with no client connected —
            // an idle DDA backend would otherwise hold the SYSTEM-impersonating
            // capture thread and the output's single duplication slot
            // indefinitely. WGC→DDA swaps stay gated on client_connected so an
            // unwatched UAC prompt doesn't churn backends.
            if capturer.maybe_swap_backend().is_some() {
                let (ox, oy) = capturer.origin();
                input::set_active_capture_rect(ox, oy, capturer.width(), capturer.height());
            }
        }

        // Texture to feed the encoder this iteration — either a freshly captured
        // WGC frame, or (when the desktop is unchanged) a re-submission of the
        // last cached frame to keep the stream alive on a static desktop.
        let mut texture_to_encode: Option<ID3D11Texture2D> = None;

        // WGC cursor note: `IsCursorCaptureEnabled(true)` is set on the
        // session so WGC composites the system cursor directly into the captured
        // texture in the display's native colour space (FP16 in HDR mode).
        // The shim cursor-compositing pipeline is idle — no update_cursor_*
        // calls are made here to avoid double-compositing.
        match capturer.try_get_frame() {
            Some(texture) => {
                // texture is our stable D3D11_USAGE_DEFAULT cached copy —
                // the WGC pool frame was already flushed and released inside
                // try_get_frame before this returns. Safe to encode from.
                timeout_streak = 0;
                // Only when someone is watching — see the Worker loop's twin
                // for the measurement. The duplicate-frame path below was
                // always gated; this one was not, so an idle host encoded every
                // desktop change at full bitrate and discarded it at the send
                // gate.
                if client_connected {
                    texture_to_encode = Some(texture);
                }
            }
            None => {
                timeout_streak += 1;
                // Rate-limited (see log_static_desktop) — and suppressed
                // entirely when not streaming: nobody cares about idle
                // capture state.
                if client_connected {
                    log_static_desktop(capturer.backend_kind(), timeout_streak, &mut last_static_log);
                }

                // ── Damage generator (tick-tock jiggle) ──────────────────────
                // An empty VDD produces no DWM damage, so WGC never fires.
                // Every ~50 ms we send a stateful ±1-px relative mouse move via
                // SendInput. The cursor rests in the new position until the next
                // fire, guaranteeing a real dirty rect. Windows coalesces +1/-1
                // in the same tick, but with the toggle they land in separate
                // loop iterations ~50 ms apart — impossible to coalesce.
                // Stops as soon as has_frame() is true (real frames flowing).
                if !capturer.has_frame() && timeout_streak % 25 == 0 {
                    let (dx, dy): (i32, i32) = if jiggle_toggle { (1, 1) } else { (-1, -1) };
                    jiggle_toggle = !jiggle_toggle;
                    unsafe {
                        let mut input: INPUT = std::mem::zeroed();
                        input.r#type = INPUT_MOUSE;
                        input.Anonymous.mi.dx = dx;
                        input.Anonymous.mi.dy = dy;
                        input.Anonymous.mi.dwFlags = MOUSEEVENTF_MOVE;
                        SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
                    }
                }

                // ── Encoder gate ──────────────────────────────────────────────
                // No NVENC calls until WGC has delivered its first real frame.
                if !capturer.has_frame() {
                    continue;
                }

                // Strict frame pacing: no desktop damage this slot ⇒ re-submit
                // the last captured surface as a duplicate P-frame so the
                // client receives an uninterrupted constant-fps bitstream.
                // Without this a static desktop starves the decoder and CBR
                // degrades the image until motion resumes; the duplicates also
                // let rate control spend the idle bitrate refining the static
                // picture to full sharpness. Gated on an active session — idle
                // with no client keeps NVENC hardware-idle (0% Video Encode,
                // the Phase 11 signature).
                if client_connected && video_learned {
                    texture_to_encode = capturer.cached_texture().cloned();
                }
            }
            // No ACCESS_LOST arm: WGC absorbs display mode transitions
            // (including the FP16 Advanced Color switch) internally.
        }

        if let Some(texture) = texture_to_encode {
            // No periodic forced IDR here: FEC handles packet loss, and
            // Moonlight requests IDRs via the control stream when it can't
            // recover. The encoder runs an infinite GOP (Sunshine-style) —
            // IDRs happen only on demand.
            // Index this frame is encoded AND sent under (== NVENC inputTimeStamp).
            let this_index = wire_index;
            let packet_size = enc.encode_frame(&texture, &mut out_buffer, this_index as u64);

            if packet_size == 0 {
                println!("⚠️  encode_frame returned 0 bytes ({}x{})", capturer.width(), capturer.height());
            }

            if packet_size > 0 {
                wire_index = wire_index.wrapping_add(1);
                if wire_index == 0 { wire_index = 1; } // Moonlight discards frame 0
                frames_encoded += 1;
                if frames_encoded == 1 {
                    println!("🎬 First encoded frame: {} bytes", packet_size);
                    debug::debug_log(&format!("First frame {} bytes", packet_size));
                }

                // Accumulate only — the 1 Hz report runs from the loop body
                // (see the telemetry tick above).
                enc_rate_bytes += packet_size as u64;
                enc_rate_frames += 1;

                if video_learned {
                    let data = &out_buffer[..packet_size as usize];
                    let is_hevc_enc = enc.config.codec == encoder::Codec::Hevc;
                    let is_av1_enc = enc.config.codec == encoder::Codec::Av1;
                    let is_idr = rtp::detect_frame_type(data, is_hevc_enc, is_av1_enc) == 2;
                    // 2 = IDR, 5 = RFI recovery, 1 = P (see the Worker path).
                    let frame_type = if is_idr {
                        2u8
                    } else if encoder::RFI_ENABLED && encoder::last_frame_was_rfi_recovery() {
                        5u8
                    } else {
                        1u8
                    };
                    if !first_idr_sent && !is_idr {
                        // Don't open the stream with a P-frame — re-request an IDR
                        // and drop this frame until the first keyframe is ready.
                        enc.request_idr();
                        if frames_encoded < 20 {
                            println!("[ENC] frame={} ({} bytes) dropped — waiting for first IDR", frames_encoded, packet_size);
                        }
                    } else if rtp_sender.send_frame(this_index, data, frame_type) {
                        // Frame queued to the nova-rtp-send thread — packetize/
                        // FEC/pacing/sendto all happen off the capture thread.
                        first_idr_sent = true;
                        // Per-frame logging is itself a hot-path cost (one
                        // blocking WriteFile to nova.log per frame at up to
                        // 120 Hz) — log only session-start frames and IDRs.
                        if frames_encoded <= 10 || is_idr {
                            println!("[ENC] frame={} size={} bytes ({})", frames_encoded, packet_size, if is_idr { "IDR" } else { "P" });
                        }
                    } else {
                        // Send thread ≥3 frames behind (saturated link) — the
                        // frame was refused. A silently dropped frame breaks
                        // the P-frame reference chain, so recover with an IDR.
                        send_queue_drops += 1;
                        enc.request_idr();
                        if send_queue_drops == 1 || send_queue_drops % 120 == 0 {
                            println!("⚠️  RTP send queue full — frame dropped ({} total this session), IDR re-requested", send_queue_drops);
                        }
                    }
                }
            }
        }
    }

    // Explicit stop (rather than relying on drop at function exit) so the
    // restore-default-audio-device log line is visible before we report done.
    println!("🔊 Restoring host audio output before exit...");
    audio_manager.stop_and_release();

    // Release the NVENC/D3D pipeline before tearing down the VDD. The encoder
    // holds D3D texture references on the VDD adapter; releasing them first
    // avoids a dangling-reference when SetDisplayConfig removes the virtual
    // output from the device tree. enc.cleanup() is idempotent (no-ops when
    // the session was already torn down by the normal disconnect path).
    enc.cleanup();

    // Restore the physical display topology if a virtual desktop session was
    // active when the shutdown signal arrived (Ctrl+C, console close, OS logoff,
    // OS shutdown). deactivate_after_stream() is a no-op when vd.active is false
    // so it is always safe to call here. VirtualDisplay::drop() is the safety
    // net for panics; this explicit call gives us the correct enc→vd teardown
    // order and visible log output.
    if let Err(e) = vd.deactivate_after_stream() {
        println!("⚠️  VDD shutdown teardown: {e}");
    }

    println!("✅ Capture loop done — {} frames encoded", frames_encoded);
    // `enc` drops here → CleanupEncoder is idempotent after enc.cleanup() above.
    // `vd` drops here → VirtualDisplay::drop() is a no-op because active=false.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Locks the frame-pacing contract that regressed live on 2026-08-06: a
    /// stalled capture loop must DROP the slots it missed, never repay them.
    /// Repaying them made the loop emit up to 179 fps on a 120 fps session for
    /// twelve seconds after a single stalled second, at ~1.6x the negotiated
    /// bitrate. Instants are only ever built by ADDING to a base — never by
    /// subtracting from `now`, which underflows QPC-backed Instants on a
    /// freshly booted host (the panic fixed in Phase 15.3).
    #[test]
    fn frame_deadline_drops_missed_slots_instead_of_bursting() {
        let base = Instant::now();
        let interval = Duration::from_micros(8333); // 120 fps

        // Steady state: the slot was served on time, so cadence is exact.
        let on_time = advance_frame_deadline(base + interval, interval, base + interval);
        assert_eq!(on_time, base + interval + interval);

        // Sub-frame slip: `deadline + interval` is still ahead of now, so the
        // cadence is preserved and the slip self-corrects within one frame
        // rather than being treated as debt.
        let slipped = advance_frame_deadline(base, interval, base + Duration::from_micros(2000));
        assert_eq!(slipped, base + interval);

        // Real stall (half a second — a VDD activation or DDA swap): the debt
        // is discarded and the next slot is one interval after NOW, so the
        // loop sleeps again immediately instead of running flat out.
        let now = base + Duration::from_millis(500);
        let after_stall = advance_frame_deadline(base, interval, now);
        assert_eq!(after_stall, now + interval);
        assert!(after_stall > now, "must be in the future or the loop won't sleep");

        // Repeated stalls must not accumulate: feeding the result back through
        // another stall still lands one interval past that stall's `now`.
        let now2 = after_stall + Duration::from_millis(300);
        assert_eq!(advance_frame_deadline(after_stall, interval, now2), now2 + interval);
    }

    /// `session_is_live` must see BOTH kinds of client, and must not count a
    /// detached Echo session as live.
    ///
    /// It gates the Master's cached-IDR keepalive. Wrong in the "always live"
    /// direction, that keepalive re-transmits one stale frame every second
    /// forever — which is exactly what it did, because nothing consulted a
    /// session at all (observed live 2026-08-17: `📦 frame 1` at 1-2 fps, minutes
    /// after the operator force-ended the session). Wrong the other way it stops
    /// covering a Worker respawn, and a live client's watchdog trips.
    #[test]
    fn session_liveness_sees_both_client_kinds() {
        let idle: Arc<Mutex<Option<rtsp::ClientInfo>>> = Arc::new(Mutex::new(None));
        assert!(!session_is_live(&idle, None), "nothing connected");

        // A ClientInfo exists from /launch onward, but only PLAY makes it live.
        let pending = Arc::new(Mutex::new(Some(rtsp::ClientInfo {
            streaming_active: false,
            ..Default::default()
        })));
        assert!(!session_is_live(&pending, None), "pre-PLAY is not streaming");

        let streaming = Arc::new(Mutex::new(Some(rtsp::ClientInfo {
            streaming_active: true,
            ..Default::default()
        })));
        assert!(session_is_live(&streaming, None), "a Moonlight PLAY is live");

        // The Echo half is `echo_holds_media()`, which is covered where it can
        // be built without a socket or a Worker:
        // `echo::session::tests::a_silent_client_detaches_and_keeps_the_display`
        // asserts it goes false on detach, which is what makes a detached
        // session read as not-live here.
    }

    /// The detach grace clock's boundary conditions. `0` is the one that would
    /// hurt if it were wrong: it means "hold the session indefinitely", and
    /// treating it as "expire immediately" would make the opt-out the most
    /// aggressive setting available — every disconnect instantly rearranging
    /// the operator's monitors.
    #[test]
    fn detach_grace_zero_means_never_not_now() {
        assert!(!detach_grace_expired(0, Duration::from_secs(0)));
        assert!(!detach_grace_expired(0, Duration::from_secs(86_400)));

        // The configured boundary is inclusive, and nothing before it fires.
        assert!(!detach_grace_expired(600, Duration::from_secs(599)));
        assert!(detach_grace_expired(600, Duration::from_secs(600)));
        assert!(detach_grace_expired(600, Duration::from_secs(601)));

        // A short grace still behaves, including at the extremes.
        assert!(!detach_grace_expired(1, Duration::from_millis(999)));
        assert!(detach_grace_expired(1, Duration::from_secs(1)));
        assert!(detach_grace_expired(u32::MAX, Duration::MAX));
    }

    /// The static-desktop diagnostic must stay off the hot path. The guard it
    /// replaced fired on every other frame slot (55,203 blocking writes in one
    /// live session) because `timeout_streak` resets on every delivered frame.
    #[test]
    fn static_desktop_log_is_throttled() {
        let mut last: Option<Instant> = None;

        // Alternating hit/miss slots (a 60 Hz source polled at 120 fps) never
        // reach the streak threshold, so they must never log at all.
        for _ in 0..10_000 {
            log_static_desktop(capture::BackendKind::Wgc, 1, &mut last);
        }
        assert!(last.is_none(), "brief misses must not log");

        // A genuinely static screen logs once, then is time-throttled.
        log_static_desktop(capture::BackendKind::Wgc, 300, &mut last);
        let first = last.expect("a real static episode should log");
        for streak in 301..1_000 {
            log_static_desktop(capture::BackendKind::Wgc, streak, &mut last);
        }
        assert_eq!(last, Some(first), "must not re-log inside the throttle window");
    }
}

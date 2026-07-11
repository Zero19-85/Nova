mod app_launcher;
mod audio;
mod capture;
mod config;
mod control;
pub mod debug; // pub so nova-server binary can call init_debug_logger() during --install/--uninstall
mod encoder;
mod input;
mod pairing;
mod rtp;
mod rtsp;
/// Secure-desktop UAC policy toggle — the opt-in complement to the DDA
/// secure-desktop capture backend. Public so the installer/CLI and a future tray
/// item can query and flip `PromptOnSecureDesktop`.
pub mod secure_desktop;
/// Thin SYSTEM launcher service (Phase 15.2c) — spawns the interactive host
/// with a SYSTEM-derived elevated token. Public so the binary's `--service` /
/// `--install-service` / `--uninstall-service` subcommands can reach it.
pub mod service;
mod shutdown;
pub mod tray;
mod virtual_display;

use clap::Parser;
// Trait for the capture manager's per-frame surface (width()/height()/origin()/
// device()/try_get_frame()/rebind()) — the concrete backend behind it is the
// manager's business (WGC normally, DDA during secure-desktop interludes).
use capture::DesktopCapture;
use encoder::{Encoder, EncoderConfig};
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

/// Recreates the NVENC encoder to match the capturer's current dimensions
/// (same device — the manager guarantees one D3D11 device for the process
/// lifetime). Shared by the session-rebind path and the WGC↔DDA backend-swap
/// path.
fn recreate_encoder_for_capture(
    capturer: &capture::DesktopManager,
    enc: &mut Encoder,
) -> std::result::Result<(), String> {
    println!("🔁 Capture resolution/device changed — recreating NVENC encoder ({}x{})",
        capturer.width(), capturer.height());
    // Tear down the old encoder's shim-global NVENC/D3D state BEFORE creating
    // the replacement — otherwise Encoder::new() below overwrites those
    // globals with the new encoder's state, and *enc's old value being dropped
    // (by the assignment further down) would destroy the brand-new encoder
    // instead, leaving g_nvEncoder/g_device null.
    enc.cleanup();
    match Encoder::new(capturer.device(), EncoderConfig {
        width:        capturer.width() as i32,
        height:       capturer.height() as i32,
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
            eprintln!("❌ Failed to recreate encoder after capture change: {e}");
            Err(e)
        }
    }
}

/// Re-targets the capture manager to `target` (GDI device name, or `None` for
/// the physical primary), recreating the encoder when the resolution changes.
/// `is_hdr` sets the frame format: FP16 for HDR, BGRA8 for SDR. The manager
/// routes the rebind to whichever backend matches the current input desktop
/// (sessions land on WGC; a live secure-desktop DDA interlude retargets DDA).
///
/// Synchronous — WGC session creation and CCD calls block briefly; this is
/// called while holding the `client_info` mutex where `.await` is unsound.
fn rebind_capture_and_encoder(
    capturer: &mut capture::DesktopManager,
    enc: &mut Encoder,
    target: Option<&str>,
    expected_size: Option<(u32, u32)>,
) -> std::result::Result<(), String> {
    match capturer.rebind(target, enc.config.is_hdr, expected_size) {
        Ok(needs_new_encoder) => {
            if needs_new_encoder {
                recreate_encoder_for_capture(capturer, enc)?;
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

    // ── File logging: must be first so all subsequent println! go to nova.log ─
    debug::init_debug_logger();

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
    if unsafe { windows::Win32::UI::Shell::IsUserAnAdmin() }.as_bool() {
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
    let (tray_tx, tray_rx) = std::sync::mpsc::sync_channel::<tray::TrayCmd>(32);
    // global_pin is the handshake point between the tray PIN dialog and the
    // pairing async task: the tray writes the 4-digit string here and the
    // pairing poll loop reads + clears it.
    let global_pin: Arc<Mutex<(String, String)>> = Arc::new(Mutex::new((String::new(), String::new())));
    tray::spawn(tray_rx, Arc::new(shutdown_tx), global_pin.clone());
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
    // Parse from a FILTERED arg list: the service launches the host with
    // `--system-token <n>` (handled in bin/main before run()), which clap does
    // not know about. Strip that flag and its value so clap doesn't abort.
    let filtered_args = {
        let mut out: Vec<std::ffi::OsString> = Vec::new();
        let mut it = std::env::args_os();
        while let Some(a) = it.next() {
            if a == "--system-token" {
                let _ = it.next(); // skip its value
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

    // When the desktop is static, NVENC stays idle except for a P-frame keep-alive
    // pulse per this interval to satisfy Moonlight's watchdog. We do NOT force an
    // IDR here — IDRs cause a full macroblock refresh which appears as a visible
    // sharpness pop / shimmer on flat text at 1 s cadence. A P-frame (cached
    // texture, no keyframe flag) keeps the stream alive without any visible artifact.
    // 5 s is well inside Moonlight's watchdog timeout (~30 s) and matches
    // Apollo/Sunshine's idle encoding cadence on a static desktop.
    const IDR_KEEPALIVE_INTERVAL: Duration = Duration::from_millis(5000);
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
            println!("❌ Failed to start WGC capture — no usable display found: {e:?}");
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

    // Control stream (ENet/reliable-UDP) on port 47999.
    std::thread::spawn({
        let info = client_info.clone();
        move || control::start_control_server(47999, info)
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

    // Bind to the GameStream video port (47998) so RTP packets arrive from the
    // port advertised in the RTSP SETUP response — Moonlight validates the source port.
    let mut rtp_sender = crate::rtp::RtpSender::new(47998)
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
    let mut next_frame_time  = Instant::now();
    let mut frames_encoded   = 0u64;
    // Congestion control: the session's negotiated bitrate ceiling and bookkeeping
    // for the reduce→ramp-back cycle. Written at session start, reset on disconnect.
    let mut congestion_stable_kbps: u32 = 0;
    let mut congestion_last_event = Instant::now() - Duration::from_secs(30);
    // Per-second encoder output rate — catches rate-control regressions
    // locally (works without any client connected), e.g. CBR overshooting
    // what the link/client can take.
    let mut enc_rate_bytes   = 0u64;
    let mut enc_rate_tick    = Instant::now();
    // Consecutive WGC iterations with no new frame (desktop unchanged).
    let mut timeout_streak = 0u32;
    // Stateful tick-tock for the damage-generator jiggle — alternates the
    // cursor between +1 and -1 each fire so it actually rests at a new
    // position for ~50 ms, guaranteeing DWM composites a fresh frame.
    let mut jiggle_toggle = false;
    // Wall-clock time the last frame (real or re-submitted-from-cache) was
    // handed to the encoder — used to pace duplicate frames on WAIT_TIMEOUT.
    let mut last_frame_sent = Instant::now();

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
        let now = Instant::now();
        if now < next_frame_time {
            let wait = next_frame_time - now;
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
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
                    println!("\n🛑 Tray exit — shutting down ({} frames encoded)", frames_encoded);
                    // Under the service deployment, the host is respawned on exit
                    // by design — so a user "Quit" must also stop the service, or
                    // it just relaunches. Request the stop now (before teardown)
                    // so the service's worker won't respawn us; the service then
                    // grace-waits for this graceful teardown to finish. No-op when
                    // not launched by the service.
                    crate::service::request_service_stop();
                    break;
                }
            }
        }
        next_frame_time += frame_interval;

        // Pre-activate the virtual display as soon as /launch or /resume has
        // recorded a target mode — well before RTSP PLAY/control-connect.
        // The devcon/CCD switch in activate_for_stream is slow enough that
        // doing it after the control stream connects can stall long enough
        // for Moonlight to drop the connection before the first frame goes
        // out. Doing it here, during the handshake gap, gives it that time
        // without blocking the latency-critical path.
        if !client_connected {
            // Handle /cancel that arrived after the client already disconnected
            // (e.g. user backed out, VDD was suspended, then clicked "Quit App").
            // The normal disconnect path never ran for this cancel, so we do the
            // full teardown here while the session is idle.
            if vd.active_device_name().is_some() {
                let was_cancelled = client_info.lock()
                    .map(|g| g.as_ref().is_some_and(|c| c.cancelled))
                    .unwrap_or(false);
                if was_cancelled {
                    println!("🛑 /cancel while suspended — tearing down virtual display");
                    debug::debug_log("Deferred /cancel: VDD teardown");
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
                    if let Err(e) = rebind_capture_and_encoder(&mut capturer, &mut enc, None, None) {
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
                        println!("🖥️  Pre-activating virtual display for upcoming session ({width}x{height}@{fps}fps{})",
                            if hdr_req { " HDR10" } else { "" });
                        // Capture the restore target BEFORE the VDD flip: once the
                        // virtual display is primary, Windows may auto-switch the
                        // default endpoint to its HDMI audio device — arming after
                        // that would restore to the wrong endpoint at session end.
                        audio::arm_endpoint_restore();
                        match vd.activate_for_stream(width, height, fps) {
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
                                if rebind_capture_and_encoder(&mut capturer, &mut enc, vd.active_device_name(), Some((width, height))).is_err() {
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
                                vd.active_device_name(), Some((client.width, client.height))) {
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
                                width:        capturer.width() as i32,
                                height:       capturer.height() as i32,
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
                                vd.active_device_name(), Some((client.width, client.height))).is_err() {
                                break;
                            }
                        }

                        // Resolution / FPS / HDR summary — the single most
                        // useful line for diagnosing stream failures.
                        // NOTE: print the LIVE codec, not enc_name — enc_name was
                        // captured before the ANNOUNCE-driven codec switch above and
                        // showed "h264" for sessions that were actually HEVC.
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
                                vd.active_device_name(), Some((client.width, client.height))).is_err() {
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
                                    if rebind_capture_and_encoder(&mut capturer, &mut enc, vd.active_device_name(), Some((client.width, client.height))).is_err() {
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
                            // the physical primary display. Just rebind to whatever is current.
                            if rebind_capture_and_encoder(&mut capturer, &mut enc, None, None).is_err() {
                                break;
                            }
                        }

                        // Resolution guard — runs regardless of activated path.
                        // If wait_for_display_resolution timed out during pre-activation
                        // (common for 4K@120fps modes that take >3 s to settle), the VDD
                        // may have landed at 1080p instead of 4K. Give it one more
                        // re-snap and rebind attempt now, while the client is waiting.
                        if enc.config.width as u32 != client.width || enc.config.height as u32 != client.height {
                            println!("📐 Resolution re-snap: encoder={}x{}  client={}x{}@{}fps — retrying VDD force",
                                enc.config.width, enc.config.height, client.width, client.height, client.fps);
                            vd.re_snap_resolution(client.width, client.height, client.fps);
                            if rebind_capture_and_encoder(&mut capturer, &mut enc, vd.active_device_name(), Some((client.width, client.height))).is_err() {
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
                            congestion_last_event  = Instant::now() - Duration::from_secs(30);
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
                        audio_manager.start_for_stream(
                            audio_socket.try_clone().expect("clone audio socket"),
                            client.rikey,
                            client.rikeyid,
                            client.audio_encryption,
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
                frame_interval  = startup_frame_interval;
                next_frame_time = Instant::now();
                client_connected    = false;
                video_learned       = false;
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

        // ── Congestion control ────────────────────────────────────────────────
        // PT_LOSS_STATS from the control thread atomically sets a pending
        // reduced bitrate (via signal_congestion_reduction). We apply it here
        // (on the main thread) with a 2-second cooldown to prevent thrashing,
        // then ramp back up by 10% every 5 seconds once the link stabilises.
        if client_connected && congestion_stable_kbps > 0 {
            if let Some(reduced) = encoder::take_congestion_bitrate() {
                if congestion_last_event.elapsed() >= Duration::from_secs(2) {
                    let fps = enc.config.fps as u32;
                    encoder::reconfigure_bitrate(reduced, fps);
                    encoder::set_stream_bitrate_kbps(reduced as i32);
                    congestion_last_event = Instant::now();
                    println!("📉 Congestion: bitrate → {} Kbps ({}% of {} Kbps ceiling)",
                        reduced,
                        reduced * 100 / congestion_stable_kbps,
                        congestion_stable_kbps);
                }
            } else {
                let cur = encoder::get_stream_bitrate_kbps() as u32;
                if cur > 0 && cur < congestion_stable_kbps
                    && congestion_last_event.elapsed() >= Duration::from_secs(5)
                {
                    let ramped = (cur + cur / 10).min(congestion_stable_kbps);
                    let fps = enc.config.fps as u32;
                    encoder::reconfigure_bitrate(ramped, fps);
                    encoder::set_stream_bitrate_kbps(ramped as i32);
                    congestion_last_event = Instant::now();
                    println!("📈 Congestion: ramped bitrate → {} Kbps (+10%)", ramped);
                }
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
                texture_to_encode = Some(texture);
            }
            None => {
                timeout_streak += 1;
                // Suppress per-frame log spam: first hit and then every ~5 s at 60 fps.
                // Suppress entirely when not streaming (nobody cares about idle WGC state).
                if client_connected && (timeout_streak == 1 || timeout_streak % 300 == 0) {
                    println!("⏳ WGC: static desktop (streak {})", timeout_streak);
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

                // Static-frame gate: when the desktop is unchanged, NVENC stays
                // hardware-idle → 0% Video Encode utilization, matching
                // Apollo/Sunshine's idle signature.
                //
                // P-frame keep-alive: after IDR_KEEPALIVE_INTERVAL of silence,
                // submit the cached frame as a standard P-frame to satisfy
                // Moonlight's watchdog. No IDR forced here — forcing a keyframe
                // every second produces a visible sharpness pop ("shimmer") on
                // flat text. P-frames are visually transparent on a static desktop.
                if client_connected && video_learned
                    && last_frame_sent.elapsed() >= IDR_KEEPALIVE_INTERVAL
                {
                    texture_to_encode = capturer.cached_texture().cloned();
                }
                // else: static desktop with active stream — NVENC stays idle.
            }
            // No ACCESS_LOST arm: WGC absorbs display mode transitions
            // (including the FP16 Advanced Color switch) internally.
        }

        if let Some(texture) = texture_to_encode {
            // No periodic forced IDR here: FEC handles packet loss, and
            // Moonlight requests IDRs via the control stream when it can't
            // recover. The encoder runs an infinite GOP (Sunshine-style) —
            // IDRs happen only on demand.
            let packet_size = enc.encode_frame(&texture, &mut out_buffer);

            if packet_size == 0 {
                println!("⚠️  encode_frame returned 0 bytes ({}x{})", capturer.width(), capturer.height());
            }

            if packet_size > 0 {
                last_frame_sent = Instant::now();
                frames_encoded += 1;
                if frames_encoded == 1 {
                    println!("🎬 First encoded frame: {} bytes", packet_size);
                    debug::debug_log(&format!("First frame {} bytes", packet_size));
                }

                enc_rate_bytes += packet_size as u64;
                if enc_rate_tick.elapsed() >= Duration::from_secs(1) {
                    println!("🎞  Encoder output: {} Kbps", (enc_rate_bytes * 8) / 1000);
                    enc_rate_bytes = 0;
                    enc_rate_tick  = Instant::now();
                }

                if video_learned {
                    let data = &out_buffer[..packet_size as usize];
                    let is_hevc_enc = enc.config.codec == encoder::Codec::Hevc;
                    let is_av1_enc = enc.config.codec == encoder::Codec::Av1;
                    let kind = if rtp::detect_frame_type(data, is_hevc_enc, is_av1_enc) == 2 { "IDR" } else { "P" };
                    println!("[ENC] frame={} size={} bytes ({})", frames_encoded, packet_size, kind);
                    rtp_sender.send_frame(data);
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

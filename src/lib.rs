mod app_launcher;
mod audio;
mod capture;
mod control;
mod debug;
mod encoder;
mod input;
mod pairing;
mod rtp;
mod rtsp;
pub mod tray;
mod virtual_display;

use clap::Parser;
use encoder::{Encoder, EncoderConfig};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::core::Result;
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_MOUSE, MOUSEEVENTF_MOVE,
};
use mdns_sd::{ServiceDaemon, ServiceInfo};
use tokio::signal;

#[derive(Parser, Debug)]
#[command(author, version, about = "Nova Server")]
struct Args {
    #[arg(long, default_value_t = 1920)]
    width: i32,
    #[arg(long, default_value_t = 1080)]
    height: i32,
    #[arg(long, default_value_t = 15000)]
    bitrate: i32,
    #[arg(long, default_value = "h264")]
    codec: String,
    #[arg(long, default_value_t = 60)]
    fps: u32,
    /// FEC parity percentage (0 disables FEC — A/B test knob for RS
    /// matrix-compatibility debugging). Default 20, matching Sunshine.
    #[arg(long, default_value_t = 20)]
    fec: u32,
}

fn get_local_ip() -> String {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").expect("bind UDP for IP discovery");
    socket.connect("8.8.8.8:80").ok();
    socket.local_addr().map(|a| a.ip().to_string()).unwrap_or_else(|_| "127.0.0.1".to_string())
}

/// Re-targets the WGC capturer to `target` (GDI device name, or `None` for
/// the physical primary), recreating the encoder when the resolution changes.
/// `is_hdr` sets the frame-pool pixel format: `R16G16B16A16Float` for HDR,
/// `B8G8R8A8UIntNormalized` for SDR.
///
/// Synchronous — WGC session creation and CCD calls block briefly; this is
/// called while holding the `client_info` mutex where `.await` is unsound.
fn rebind_capture_and_encoder(
    capturer: &mut capture::WgcCapturer,
    enc: &mut Encoder,
    target: Option<&str>,
    expected_size: Option<(u32, u32)>,
) -> std::result::Result<(), String> {
    match capturer.rebind(target, enc.config.is_hdr, expected_size) {
        Ok(needs_new_encoder) => {
            if needs_new_encoder {
                println!("🔁 Capture resolution/device changed — recreating NVENC encoder ({}x{})", capturer.width, capturer.height);
                // Tear down the old encoder's shim-global NVENC/D3D state
                // BEFORE creating the replacement — otherwise Encoder::new()
                // below overwrites those globals with the new encoder's
                // state, and *enc's old value being dropped (by the
                // assignment further down) would destroy the brand-new
                // encoder instead, leaving g_nvEncoder/g_device null.
                enc.cleanup();
                match Encoder::new(&capturer.device, EncoderConfig {
                    width:        capturer.width as i32,
                    height:       capturer.height as i32,
                    fps:          enc.config.fps,
                    bitrate_kbps: enc.config.bitrate_kbps,
                    codec:        enc.config.codec,
                    is_hdr:       enc.config.is_hdr,
                }) {
                    Ok(new_enc) => *enc = new_enc,
                    Err(e) => {
                        eprintln!("❌ Failed to recreate encoder after capture rebind: {e}");
                        return Err(e);
                    }
                }
            }
            // Keep input.rs's mouse-mapping rect in sync even when the
            // resolution didn't change — rebind() can move the captured
            // output to a different position in the virtual screen (e.g.
            // the Virtual Desktop output becoming primary at (0,0) while a
            // physical monitor that used to be primary shifts to a non-zero
            // origin).
            input::set_active_capture_rect(capturer.origin_x, capturer.origin_y, capturer.width, capturer.height);
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
    debug::init_debug_logger();
    // If a previous run was killed/closed without restoring the default audio
    // device, fix that up before anything else (host would otherwise stay
    // silent with no client connected).
    audio::recover_stuck_sink();

    // System tray: spawn before anything else so pairing PIN notifications
    // are visible from the moment the server is ready.
    // The watch channel is the graceful-shutdown bridge: the tray's "Quit"
    // menu item sends `true`; the capture-loop select! below breaks on it.
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    let (tray_tx, tray_rx) = std::sync::mpsc::sync_channel::<tray::TrayCmd>(32);
    // global_pin is the handshake point between the tray PIN dialog and the
    // pairing async task: the tray writes the 4-digit string here and the
    // pairing poll loop reads + clears it.
    let global_pin: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    tray::spawn(tray_rx, Arc::new(shutdown_tx), global_pin.clone());
    let tray_tx = Arc::new(tray_tx);
    let args = Args::parse();
    let local_ip = get_local_ip();
    println!("=== Nova Server ===\n🌐 LAN IP: {}\n", local_ip);
    debug::debug_log(&format!("Nova started — {}x{} {} {} Kbps {} fps",
        args.width, args.height, args.codec, args.bitrate, args.fps));

    let server_id  = "0123456789ABCDEF";
    let server_mac = "00:11:22:33:44:55";

    // H264 (1) + HEVC/Main8 (2) + HEVC/Main10 (256) = 259.
    // Bit 0x100 (256) = SCM_HEVC_MAIN10: signals to moonlight-common-c that
    // the server can deliver 10-bit HDR10. Without this bit, clients set
    // dynamicRangeMode:0 in ANNOUNCE even when the user enabled HDR.
    // Old-protocol clients (Xbox Moonlight 1.18.0, corever=1) read
    // sprop-parameter-sets=AAAAAU in DESCRIBE for HEVC capability and the
    // fps cap handles graceful degradation for H264 fallback scenarios.
    let codec_mode_support: u32 = 259;
    let startup_codec = encoder::Codec::from_str(&args.codec);
    println!("🎥 ServerCodecModeSupport={codec_mode_support} (H264+HEVC); startup encoder: {}", startup_codec.as_str());

    let startup_frame_interval = Duration::from_secs_f64(1.0 / args.fps.max(1) as f64);
    let mut frame_interval = startup_frame_interval;

    // Owns the virtual-display lifecycle for the whole process.
    // activate_for_stream/deactivate_after_stream cache/restore the host's
    // audio endpoint, so there's no separate audio bookkeeping elsewhere.
    //
    // Enable Root\MttVDD ONCE, here, at boot, and leave it enabled for the
    // server's entire lifetime. The old code disabled/re-enabled the devnode
    // inside activate_for_stream on every session start, which raced the
    // IDD's transient 800x600 default mode against the client's requested
    // resolution. Bringing it up once at boot means the devnode has long
    // since settled at the configured mode by the time any client connects.
    let mut vd = virtual_display::VirtualDisplay::new();
    let virtual_device_name = match vd.ensure_enabled_at_boot(args.width as u32, args.height as u32, args.fps) {
        Ok(name) => name,
        Err(e) => {
            println!("⚠️  Failed to enable Root\\MttVDD at boot: {e} — Virtual Desktop sessions will be unavailable");
            None
        }
    };

    let mut capturer = capture::WgcCapturer::new_excluding(virtual_device_name.as_deref()).expect("Failed to start WGC capture");
    input::set_active_capture_rect(capturer.origin_x, capturer.origin_y, capturer.width, capturer.height);

    // The DXGI duplication captures at the monitor's native resolution, which
    // may not match --width/--height (CLI defaults 1920x1080). The encoder and
    // the D3D11 video processor's NV12 conversion surface must be sized to the
    // ACTUAL captured texture — a mismatch leaves the video processor blitting
    // into a differently-sized output, which can leave the bottom portion of
    // the NV12 surface (and therefore the encoded frame) black/garbage.
    if capturer.width as i32 != args.width || capturer.height as i32 != args.height {
        println!("⚠️  Monitor native resolution ({}x{}) differs from --width/--height ({}x{}) — using native resolution for capture/encoder pipeline.",
            capturer.width, capturer.height, args.width, args.height);
    }

    let mut enc = Encoder::new(
        &capturer.device,
        EncoderConfig {
            width:        capturer.width as i32,
            height:       capturer.height as i32,
            fps:          args.fps as i32,
            bitrate_kbps: args.bitrate,
            codec:        startup_codec,
            is_hdr:       false, // upgraded per-session when client negotiates HEVC Main10/HDR
        },
    )
    .expect("Failed to initialize NVENC encoder");

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

    // Audio port (48000) — the AudioStreamer thread learns the client's
    // address from its pings and sends Opus RTP back on this socket.
    let audio_socket = std::net::UdpSocket::bind("0.0.0.0:48000")
        .expect("Failed to bind audio socket on 48000");
    audio_socket.set_nonblocking(true).expect("set_nonblocking on audio socket");
    let mut audio_streamer: Option<audio::AudioStreamer> = None;

    let mut out_buffer       = vec![0u8; 8 * 1024 * 1024];
    let mut client_connected = false;
    let mut video_learned    = false;
    let mut next_frame_time  = Instant::now();
    let mut frames_encoded   = 0u64;
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
    // running Rust destructors, so AudioStreamer's Drop (which restores the
    // default audio device away from the virtual sink) never runs and the
    // host is left silent. recover_stuck_sink() at startup is the last-resort
    // backstop for paths even these handlers can't catch (e.g. taskkill /F).
    let mut ctrl_close = signal::windows::ctrl_close().expect("register ctrl_close handler");
    let mut ctrl_shutdown = signal::windows::ctrl_shutdown().expect("register ctrl_shutdown handler");
    let mut ctrl_logoff = signal::windows::ctrl_logoff().expect("register ctrl_logoff handler");

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
                    if rebind_capture_and_encoder(&mut capturer, &mut enc, None, None).is_err() {
                        break;
                    }
                    frame_interval  = startup_frame_interval;
                    next_frame_time = Instant::now();
                    if let Ok(mut guard) = client_info.lock() {
                        if let Some(info) = guard.as_mut() {
                            info.cancelled = false;
                        }
                    }
                }
            }

            if let Ok(mut guard) = client_info.lock() {
                let pending = guard.as_ref()
                    .filter(|c| c.app_id != 0 && !c.activated && !c.streaming_active)
                    .map(|c| (c.app_id, c.width, c.height, c.fps, c.video_format));
                if let Some((app_id, width, height, fps, video_format)) = pending {
                    // Read HDR flag while we still hold the lock.
                    let hdr_req = guard.as_ref().map(|c| c.hdr_requested).unwrap_or(false);

                    // Derive codec from /launch videoFormat BEFORE rebind so the
                    // encoder is recreated at the right codec (H264/HEVC/AV1) for
                    // this session, not the CLI startup default.
                    let negotiated_codec = encoder::Codec::from_video_format(video_format);
                    if negotiated_codec != enc.config.codec {
                        println!("🎥 Codec selected by client: {} (videoFormat={:#x}) — switching encoder",
                            negotiated_codec.as_str(), video_format);
                        enc.config.codec  = negotiated_codec;
                        enc.config.is_hdr = false; // reset; re-armed below if HDR is also requested
                    }
                    enc.config.fps = fps as i32;
                    let vdd_ok = if app_launcher::uses_virtual_display(app_id) {
                        println!("🖥️  Pre-activating virtual display for upcoming session ({width}x{height}@{fps}fps{})",
                            if hdr_req { " HDR10" } else { "" });
                        match vd.activate_for_stream(width, height, fps) {
                            Ok(()) => {
                                if rebind_capture_and_encoder(&mut capturer, &mut enc, vd.active_device_name(), Some((width, height))).is_err() {
                                    break;
                                }
                                // Enable Advanced Color (HDR/scRGB) during the /launch→PLAY gap
                                // so the ACCESS_LOST storm from the color-space switch settles
                                // before any frames need to be sent. By connect-time the VDD is
                                // already stable in FP16 mode — calling set_active_display_hdr
                                // again there is a no-op (no second storm).
                                if hdr_req {
                                    // Check if MttVDD actually supports Advanced Color before
                                    // trying to enable it. On MttVDD 25.7.23 (and many other
                                    // IddCx VDD versions) Advanced Color is NOT supported —
                                    // calling SET_ADVANCED_COLOR_STATE returns success but
                                    // triggers an endless ACCESS_LOST storm without achieving
                                    // FP16 mode, causing the client to time out.
                                    if vd.is_advanced_color_supported() {
                                        if let Err(e) = vd.set_active_display_hdr(true) {
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
                                                    input::set_active_capture_rect(capturer.origin_x, capturer.origin_y, capturer.width, capturer.height);
                                                    println!("✅ WGC frame pool recreated in FP16 — VDD in HDR/Advanced Color mode");
                                                }
                                                Err(e) => eprintln!("⚠️  WGC FP16 rebind failed: {e} — HDR frames may be tone-mapped to SDR"),
                                            }
                                            println!("✅ VDD in FP16 HDR mode — encoder pipeline ready for HEVC Main10");
                                        }
                                    } else {
                                        println!("⚠️  MttVDD does not support Advanced Color (HDR) — \
                                            streaming HEVC SDR. The VDD driver must expose HDR modes \
                                            for true HDR10 output. Check vdd_settings.xml for HDR config.");
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
                        let negotiated_codec = if client.video_format != 0 {
                            encoder::Codec::from_video_format(client.video_format)
                        } else {
                            match client.bit_stream_format {
                                1 => encoder::Codec::Hevc,
                                2 => encoder::Codec::Av1,
                                _ => encoder::Codec::H264,
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
                            match encoder::Encoder::new(&capturer.device, encoder::EncoderConfig {
                                width:        capturer.width as i32,
                                height:       capturer.height as i32,
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
                            input::set_active_capture_rect(capturer.origin_x, capturer.origin_y, capturer.width, capturer.height);
                        }

                        // HDR10 pipeline: gate on /launch hdrMode=1 (client.hdr_requested).
                        // Xbox Moonlight 1.18.0 (corever=1) always sends dynamicRangeMode=0
                        // in the ANNOUNCE regardless of user HDR setting, so dynamicRangeMode
                        // cannot be used as the gate for old clients.
                        //
                        // Two-step activation:
                        //   1. Enable Windows Advanced Color on the VDD via
                        //      DisplayConfigSetDeviceInfo(SET_ADVANCED_COLOR_STATE) so DXGI
                        //      switches its frame buffer to R16G16B16A16_FLOAT (linear scRGB).
                        //      This triggers DXGI_ERROR_ACCESS_LOST; the capture loop's
                        //      ACCESS_LOST rebind re-creates the duplication handle in HDR mode.
                        //   2. Recreate NVENC as HEVC Main10 with P010 buffer format so the
                        //      shim's FP16→P010 VP path has a matching output surface.
                        if client.hdr_requested && enc.config.codec == encoder::Codec::Hevc && !enc.config.is_hdr
                            && vd.is_advanced_color_supported()
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
                            match encoder::Encoder::new(&capturer.device, encoder::EncoderConfig {
                                width:        capturer.width as i32,
                                height:       capturer.height as i32,
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
                        println!("📐 Encoder: {}x{}@{}fps {}{}  |  Client requested: {}x{}@{}fps{}",
                            enc.config.width, enc.config.height, enc.config.fps,
                            enc_name,
                            if enc.config.is_hdr { "/HDR10" } else { "" },
                            client.width, client.height, client.fps,
                            if client.hdr_requested { " HDR" } else { "" });

                        // Normally already done by the pre-activation pass
                        // above during the /launch -> PLAY gap. Fall back to
                        // doing it here if that somehow hasn't run yet (e.g.
                        // PLAY arrived before the first idle-loop tick).
                        // activate_for_stream caches the host's current audio
                        // endpoint first — must happen before AudioStreamer
                        // below changes the default device.
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
                        } else {
                            // Universal VDD: activate for every app.
                            match vd.activate_for_stream(client.width, client.height, client.fps) {
                                Ok(()) => {
                                    if rebind_capture_and_encoder(&mut capturer, &mut enc, vd.active_device_name(), Some((client.width, client.height))).is_err() {
                                        break;
                                    }
                                }
                                Err(e) => println!("⚠️  Virtual display activation failed: {e} — stream may have wrong resolution"),
                            }
                            // Mirror the pre-activation pass: mark this session
                            // activated so that once the control stream
                            // disconnects, the idle-loop pre-activation check
                            // (app_id != 0 && !activated && !streaming_active)
                            // doesn't see a stale "not yet activated" session
                            // and re-run this block with no client connected.
                            if let Ok(mut guard) = client_info.lock() {
                                if let Some(info) = guard.as_mut() {
                                    info.activated = true;
                                }
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
                        let native_px = capturer.width as u64 * capturer.height as u64;
                        let client_px = client.width as u64 * client.height as u64;
                        if client_px > 0 && native_px > client_px {
                            println!("⚠️  Encoding {}x{} (native) but client requested {}x{} — bitrate is stretched {:.1}x thinner per pixel. Raise Moonlight's bitrate or match resolutions.",
                                capturer.width, capturer.height, client.width, client.height,
                                native_px as f64 / client_px as f64);
                        }
                        rtp_sender.configure(pkt_size, args.fec as usize, min_fec);

                        // Retarget CBR to the client's negotiated bitrate.
                        // Without this the encoder streams at the CLI default
                        // (15 Mbps) regardless of what the client asked for —
                        // under CBR that's a constant overshoot that makes
                        // Moonlight warn "lower your bitrate" and disconnect.
                        if client.bitrate_kbps > 0 {
                            println!("📊 Retargeting encoder to client bitrate: {} Kbps @ {} fps",
                                client.bitrate_kbps, session_fps);
                            encoder::reconfigure_bitrate(client.bitrate_kbps, session_fps);
                            // Mirror negotiated values into enc.config so any
                            // mid-session rebind (resolution/device change) inherits
                            // the session fps (may be capped below client.fps for H264)
                            // and bitrate, not the CLI default.
                            enc.config.bitrate_kbps = client.bitrate_kbps as i32;
                            enc.config.fps          = session_fps.max(1) as i32;
                        } else {
                            println!("⚠️  Client did not announce a bitrate — keeping CLI default {} Kbps", args.bitrate);
                        }

                        // Start the audio pipeline (WASAPI → Opus → RTP 48000).
                        let pkt_dur = if client.audio_packet_duration > 0 {
                            client.audio_packet_duration
                        } else {
                            5
                        };
                        audio_streamer = Some(audio::AudioStreamer::start(
                            audio_socket.try_clone().expect("clone audio socket"),
                            client.rikey,
                            client.rikeyid,
                            client.audio_encryption,
                            pkt_dur,
                            // localAudioPlayMode: false = client-only (route
                            // audio through a virtual sink, host speakers stay
                            // silent), true = also play on the host speakers.
                            client.host_audio,
                        ));
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
                rtp_sender.reset();
                if let Some(streamer) = audio_streamer.take() {
                    streamer.stop();
                }
                input::stop_session();
                frame_interval  = startup_frame_interval;
                next_frame_time = Instant::now();
                client_connected = false;
                video_learned    = false;

                if was_cancelled {
                    // Full teardown — restore the host's display topology and
                    // rebind the encoder to the physical primary monitor.
                    println!("🛑 /cancel — tearing down virtual display, restoring host topology");
                    debug::debug_log("Session cancelled — full VDD teardown");
                    if let Err(e) = vd.deactivate_after_stream() {
                        println!("⚠️  Virtual display deactivation failed: {e}");
                    }
                    // Rebuild encoder as SDR (NV12) — the VP still expects FP16
                    // input if is_hdr=true, but the physical display provides BGRA8.
                    enc.config.is_hdr = false;
                    if rebind_capture_and_encoder(&mut capturer, &mut enc, None, None).is_err() {
                        break;
                    }
                    // Clear the flag so a later natural disconnect from the next
                    // session isn't mistakenly treated as a cancel.
                    if let Ok(mut guard) = client_info.lock() {
                        if let Some(info) = guard.as_mut() {
                            info.cancelled = false;
                        }
                    }
                } else {
                    // Suspend — VDD stays active at the current resolution and
                    // HDR mode. enc.config.is_hdr and the WGC session are
                    // preserved so /resume can reconnect without any topology
                    // change or Advanced Color flicker.
                    println!("⏸️  Client disconnected — session suspended \
                        (VDD active; client can /resume to reconnect)");
                    debug::debug_log("Session suspended — VDD active");
                }
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
                if timeout_streak <= 3 || timeout_streak % 10 == 0 {
                    println!("⏳ WGC: no new frame on {}x{} — streak {timeout_streak}",
                        capturer.width, capturer.height);
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

                tokio::time::sleep(Duration::from_millis(2)).await;

                // ── Encoder gate ──────────────────────────────────────────────
                // No NVENC calls until WGC has delivered its first real frame.
                // Before that point cached_texture() is None (cleared on rebind)
                // and passing a null/stale pointer to encode_frame returns 0
                // bytes. Once has_frame() flips true, cached re-submission is
                // safe and maintains the 120-fps stream on a static desktop.
                if !capturer.has_frame() {
                    continue;
                }

                if last_frame_sent.elapsed() >= frame_interval {
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
            let packet_size = enc.encode_frame(&texture, &mut out_buffer);

            if packet_size == 0 {
                println!("⚠️  encode_frame returned 0 bytes ({}x{})", capturer.width, capturer.height);
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
                    let kind = if rtp::detect_frame_type(data) == 2 { "IDR" } else { "P" };
                    println!("[ENC] frame={} size={} bytes ({})", frames_encoded, packet_size, kind);
                    rtp_sender.send_frame(data);
                }
            }
        }
    }

    // Explicit stop (rather than relying on drop at function exit) so the
    // restore-default-audio-device log line is visible before we report done.
    if let Some(streamer) = audio_streamer.take() {
        println!("🔊 Restoring host audio output before exit...");
        streamer.stop();
    }

    println!("✅ Capture loop done — {} frames encoded", frames_encoded);
    // `enc` drops here → CleanupEncoder flushes + closes test.h264
    Ok(())
}

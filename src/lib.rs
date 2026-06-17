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
mod virtual_display;

use clap::Parser;
use encoder::{Encoder, EncoderConfig};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::core::Result;
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
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

/// Re-binds `capturer` to `target` (a GDI device name, or `None` for the
/// default/physical output), recreating `enc` if the resolution or adapter
/// changed. Used both for `DXGI_ERROR_ACCESS_LOST`/`INVALIDCALL` recovery and
/// for following the capture target onto/off of the virtual display as
/// streams start and stop.
///
/// `expected_size`, when `Some((width, height))`, is forwarded to
/// `capturer.rebind` to guard against `GetDesc()` transiently reporting the
/// virtual display's 800x600 failsafe mode right after a CCD topology change
/// — see `capture::DesktopCapturer::rebind`'s doc comment.
///
/// A failed `rebind` is treated as transient (DXGI needs a moment to settle
/// after a topology change) and retried on the next call. A failed `Encoder`
/// recreation is not recoverable — the caller should `break` the capture loop.
///
/// Synchronous (not `async`) — `capturer.rebind` and `VirtualDisplay`'s CCD
/// calls already block via `std::thread::sleep`, and this is called from
/// inside the `client_info` mutex on connect/disconnect, where holding a
/// `std::sync::MutexGuard` across an `.await` would be unsound.
fn rebind_capture_and_encoder(
    capturer: &mut capture::DesktopCapturer,
    enc: &mut Encoder,
    target: Option<&str>,
    expected_size: Option<(u32, u32)>,
) -> std::result::Result<(), String> {
    match capturer.rebind(target, expected_size) {
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
            eprintln!("⚠️  Capture rebind failed: {:?} — retrying", e);
            std::thread::sleep(Duration::from_millis(100));
            Ok(())
        }
    }
}

pub async fn run() -> Result<()> {
    debug::init_debug_logger();
    // If a previous run was killed/closed without restoring the default audio
    // device, fix that up before anything else (host would otherwise stay
    // silent with no client connected).
    audio::recover_stuck_sink();
    let args = Args::parse();
    let local_ip = get_local_ip();
    println!("=== Nova Server ===\n🌐 LAN IP: {}\n", local_ip);
    debug::debug_log(&format!("Nova started — {}x{} {} {} Kbps {} fps",
        args.width, args.height, args.codec, args.bitrate, args.fps));

    let server_id  = "0123456789ABCDEF";
    let server_mac = "00:11:22:33:44:55";

    // Compute ServerCodecModeSupport from the CLI codec so that clients always
    // select the codec the encoder is actually running.  Advertising HEVC when
    // the encoder is H264 causes clients to pick HEVC and send the wrong
    // decoder format — result: black screen on strict clients (Xbox UWP).
    let codec_mode_support = encoder::Codec::from_str(&args.codec).mode_bit();
    println!("🎥 Encoder codec: {} (ServerCodecModeSupport={})", args.codec, codec_mode_support);

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

    let mut capturer = capture::DesktopCapturer::new_excluding(virtual_device_name.as_deref()).expect("Failed to start DXGI capture");
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
            codec:        encoder::Codec::from_str(&args.codec),
            is_hdr:       false, // upgraded to true at RTSP session time when client negotiates HDR
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
    // Consecutive ACCESS_LOST/display-change rebinds with no successful
    // frame in between — used to back off instead of spinning the capture
    // loop at full frame rate while a display topology change settles.
    let mut access_lost_streak = 0u32;
    // Consecutive AcquireNextFrame WAIT_TIMEOUTs (desktop reported no change).
    // Diagnostic only — helps tell "duplication never produces a frame on
    // this output" apart from a silent get_texture()/encode failure.
    let mut timeout_streak = 0u32;
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
            if let Ok(mut guard) = client_info.lock() {
                let pending = guard.as_ref()
                    .filter(|c| c.app_id != 0 && !c.activated && !c.streaming_active)
                    .map(|c| (c.app_id, c.width, c.height, c.fps));
                if let Some((app_id, width, height, fps)) = pending {
                    // Read HDR flag while we still hold the lock.
                    let hdr_req = guard.as_ref().map(|c| c.hdr_requested).unwrap_or(false);

                    // Propagate client-negotiated fps (and HDR mode if HEVC) into
                    // enc.config BEFORE rebind so the new encoder is initialised
                    // at the right fps/profile rather than the CLI startup default.
                    enc.config.fps = fps as i32;
                    if hdr_req && enc.config.codec == encoder::Codec::Hevc && !enc.config.is_hdr {
                        println!("🎨 HDR requested with HEVC — enabling Main10/HDR10 in pre-activated encoder");
                        enc.config.is_hdr = true;
                    }

                    let vdd_ok = if app_launcher::uses_virtual_display(app_id) {
                        println!("🖥️  Pre-activating virtual display for upcoming session ({width}x{height}@{fps}fps{})",
                            if enc.config.is_hdr { " HDR10" } else { "" });
                        match vd.activate_for_stream(width, height, fps) {
                            Ok(()) => {
                                if rebind_capture_and_encoder(&mut capturer, &mut enc, vd.active_device_name(), Some((width, height))).is_err() {
                                    break;
                                }
                                true
                            }
                            Err(e) => {
                                println!("⚠️  Virtual display activation failed: {e} — streaming from the physical display");
                                false
                            }
                        }
                    } else {
                        println!("🖥️  App {app_id} targets the physical display — virtual display not used");
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
                        // Belt-and-suspenders mismatch guard: ServerCodecModeSupport
                        // should prevent this, but warn loudly if it slips through.
                        let client_base = client.video_format & 0x00FF;
                        if client.video_format != 0 && client_base != codec_mode_support {
                            println!("⚠️  CODEC MISMATCH at stream start: client expects {} \
                                but encoder is {}. Stream will likely fail. \
                                Restart with --codec {}.",
                                vf_name, enc_name,
                                if client_base == 2 { "hevc" } else { "h264" });
                        }

                        // Wire HDR if the client requested it and the encoder
                        // hasn't already been armed by the pre-activation pass
                        // (covers App 1 / physical-display sessions and any race
                        // where PLAY arrived before the pre-activation tick).
                        // Requires the display to be in HDR mode so DXGI provides
                        // R16G16B16A16_FLOAT frames; otherwise the VP's scRGB→P010
                        // path receives BGRA8 and colours will be wrong — log it.
                        if client.hdr_requested && enc.config.codec == encoder::Codec::Hevc && !enc.config.is_hdr {
                            println!("🎨 HDR requested — switching encoder to HEVC Main10/HDR10 \
                                (display must be in HDR mode for correct colors)");
                            enc.config.is_hdr = true;
                            if rebind_capture_and_encoder(&mut capturer, &mut enc, vd.active_device_name(), None).is_err() {
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
                        if enc.config.width as u32 != client.width || enc.config.height as u32 != client.height {
                            println!("⚠️  RESOLUTION MISMATCH: SPS will declare {}x{} but client \
                                expects {}x{}. Use App 5 (Virtual Desktop) for dynamic resolution — \
                                it drives the VDD to exactly the client-requested size.",
                                enc.config.width, enc.config.height,
                                client.width, client.height);
                        }

                        // Normally already done by the pre-activation pass
                        // above during the /launch -> PLAY gap. Fall back to
                        // doing it here if that somehow hasn't run yet (e.g.
                        // PLAY arrived before the first idle-loop tick).
                        // activate_for_stream caches the host's current audio
                        // endpoint first — must happen before AudioStreamer
                        // below changes the default device.
                        if client.activated {
                            println!("🖥️  Virtual display already active for this session");
                        } else {
                            if app_launcher::uses_virtual_display(client.app_id) {
                                match vd.activate_for_stream(client.width, client.height, client.fps) {
                                    Ok(()) => {
                                        if rebind_capture_and_encoder(&mut capturer, &mut enc, vd.active_device_name(), Some((client.width, client.height))).is_err() {
                                            break;
                                        }
                                    }
                                    Err(e) => println!("⚠️  Virtual display activation failed: {e} — streaming from the physical display"),
                                }
                            } else {
                                println!("🖥️  App {} streaming from the physical display — virtual display not used", client.app_id);
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

                        rtp_sender.set_fps(client.fps.max(1));
                        let negotiated_interval = Duration::from_secs_f64(1.0 / client.fps.max(1) as f64);
                        if negotiated_interval != frame_interval {
                            frame_interval = negotiated_interval;
                            next_frame_time = Instant::now(); // rebase pacing — prevents burst if interval shrank
                            println!("⏱️  Frame interval → {:.2}ms ({} fps, client-negotiated)",
                                frame_interval.as_secs_f64() * 1000.0, client.fps);
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
                                client.bitrate_kbps, client.fps);
                            encoder::reconfigure_bitrate(client.bitrate_kbps, client.fps);
                            // Mirror negotiated values into enc.config so any
                            // mid-session rebind (resolution/device change) inherits
                            // the client-negotiated fps and bitrate, not the CLI default.
                            enc.config.bitrate_kbps = client.bitrate_kbps as i32;
                            enc.config.fps          = client.fps.max(1) as i32;
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
            // /cancel or TEARDOWN flips streaming_active back to false —
            // reset to idle so the next PLAY starts a clean session.
            let still_active = client_info.lock()
                .map(|g| g.as_ref().is_some_and(|c| c.streaming_active))
                .unwrap_or(false);
            if !still_active {
                println!("⏹️  Stream ended — resetting to idle");
                debug::debug_log("Stream ended, resetting to idle");
                rtp_sender.reset();
                if let Some(streamer) = audio_streamer.take() {
                    streamer.stop();
                }
                // Unplug the virtual controller(s) now that the session is over.
                input::stop_session();
                // Tear down the virtual display, restore the original
                // primary/topology, and force the default audio endpoint
                // back to the cached host speaker.
                if let Err(e) = vd.deactivate_after_stream() {
                    println!("⚠️  Virtual display deactivation failed: {e}");
                }
                // Follow capture back onto the restored physical display.
                if rebind_capture_and_encoder(&mut capturer, &mut enc, None, None).is_err() {
                    break;
                }
                // Restore idle capture pacing — don't spin at 120fps between sessions.
                frame_interval  = startup_frame_interval;
                next_frame_time = Instant::now();
                client_connected = false;
                video_learned    = false;
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

        // Texture to feed the encoder this iteration, if any — either a
        // freshly captured frame, or (on WAIT_TIMEOUT) a re-submission of
        // the last captured frame so a static desktop doesn't leave the
        // stream black forever.
        let mut texture_to_encode: Option<ID3D11Texture2D> = None;

        match capturer.acquire_frame((frame_interval.as_millis() as u32).max(1)) {
            Ok((resource, frame_info)) => {
                access_lost_streak = 0;
                timeout_streak = 0;
                // PointerPosition is only valid when LastMouseUpdateTime != 0
                // (DXGI docs) — on frames without a mouse update it can be
                // stale/zeroed. Updating the shim from a zeroed struct made
                // the cursor flicker to (0,0)/invisible and back on alternate
                // frames while the mouse moved, leaving P-frame remnants
                // along the path (visible as "ghost cursors" until the next
                // IDR). Only push position when DXGI actually reports a move.
                if frame_info.LastMouseUpdateTime != 0 {
                    encoder::update_cursor_position(
                        frame_info.PointerPosition.Position.x,
                        frame_info.PointerPosition.Position.y,
                        frame_info.PointerPosition.Visible.as_bool(),
                    );
                }
                if frame_info.PointerShapeBufferSize > 0 {
                    if let Ok((shape_data, shape_info)) = capturer.get_pointer_shape(frame_info.PointerShapeBufferSize) {
                        encoder::update_cursor_shape(
                            &shape_data,
                            shape_info.Type,
                            shape_info.Width,
                            shape_info.Height,
                            shape_info.Pitch,
                        );
                    }
                }

                match capturer.get_texture(&resource) {
                    Err(e) => println!("⚠️  capturer.get_texture failed: {:?}", e),
                    Ok(texture) => {
                        capturer.cache_frame(&texture).ok();
                        texture_to_encode = Some(texture);
                    }
                }
                capturer.release_frame().ok();
            }
            // DXGI_ERROR_WAIT_TIMEOUT — desktop unchanged this interval, skip frame.
            Err(e) if e.code().0 == 0x887A0027_u32 as i32 => {
                timeout_streak += 1;
                if timeout_streak <= 3 || timeout_streak % 10 == 0 {
                    println!("⏳ AcquireNextFrame WAIT_TIMEOUT (no desktop change) on {}x{} — streak {timeout_streak}", capturer.width, capturer.height);
                }
                // The desktop hasn't changed since the last duplication
                // frame — which, on a freshly-activated virtual display with
                // nothing painting to it, can be true forever. Re-submit the
                // last captured frame at roughly the target frame rate so
                // the stream keeps flowing instead of going black.
                if last_frame_sent.elapsed() >= frame_interval {
                    texture_to_encode = capturer.cached_texture().cloned();
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            // DXGI_ERROR_ACCESS_LOST / DXGI_ERROR_INVALIDCALL — any
            // SetDisplayConfig-class topology/mode change invalidates the
            // existing duplication interface (ACCESS_LOST), and the same
            // class of change can also surface as INVALIDCALL on the next
            // AcquireNextFrame. Re-duplicate instead of tearing down the
            // whole process. `vd.active_device_name()` follows the capture
            // target onto the virtual display while a stream is active, or
            // back to the physical default once it isn't.
            Err(e) if e.code().0 == 0x887A0026_u32 as i32 || e.code().0 == 0x887A0001_u32 as i32 => {
                access_lost_streak += 1;
                // Log every occurrence at first, then taper off — a display
                // topology change that hasn't settled yet can otherwise spam
                // tens of thousands of identical lines per minute.
                if access_lost_streak <= 5 || access_lost_streak % 50 == 0 {
                    if e.code().0 == 0x887A0001_u32 as i32 {
                        println!("🔄 Transient DXGI state validation shift detected. Initiating internal handle recovery... (attempt {access_lost_streak})");
                    } else {
                        println!("⚠️  DXGI duplication lost (display change) — rebinding capture (attempt {access_lost_streak})");
                    }
                }
                if rebind_capture_and_encoder(&mut capturer, &mut enc, vd.active_device_name(), vd.active_resolution()).is_err() {
                    break;
                }
                // Back off instead of spinning at full frame rate once the
                // immediate retries (rebind() already retries internally)
                // haven't recovered — gives the display topology time to
                // settle after a CCD/devnode change.
                if access_lost_streak > 5 {
                    let backoff_ms = 100u64 * access_lost_streak.min(20) as u64;
                    tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                }
            }
            Err(e) => {
                eprintln!("❌ Capture error: {:?}", e);
                debug::debug_log(&format!("Capture error: {:?}", e));
                break;
            }
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

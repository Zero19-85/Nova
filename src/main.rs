mod audio;
mod capture;
mod control;
mod debug;
mod encoder;
mod pairing;
mod rtp;
mod rtsp;

use clap::Parser;
use encoder::{Encoder, EncoderConfig};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::core::Result;
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

#[tokio::main]
async fn main() -> Result<()> {
    debug::init_debug_logger();
    let args = Args::parse();
    let local_ip = get_local_ip();
    println!("=== Nova Server ===\n🌐 LAN IP: {}\n", local_ip);
    debug::debug_log(&format!("Nova started — {}x{} {} {} Kbps {} fps",
        args.width, args.height, args.codec, args.bitrate, args.fps));

    let server_id  = "0123456789ABCDEF";
    let server_mac = "00:11:22:33:44:55";

    let frame_interval = Duration::from_secs_f64(1.0 / args.fps as f64);

    let capturer = capture::DesktopCapturer::new().expect("Failed to start DXGI capture");

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

    let enc = Encoder::new(
        &capturer.device,
        EncoderConfig {
            width:        capturer.width as i32,
            height:       capturer.height as i32,
            fps:          args.fps as i32,
            bitrate_kbps: args.bitrate,
            codec:        encoder::Codec::from_str(&args.codec),
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

    println!("▶️  Capture loop running — press Ctrl+C to stop");

    loop {
        // Frame pacing: sleep until the next frame slot, but also watch for Ctrl+C.
        let now = Instant::now();
        if now < next_frame_time {
            let wait = next_frame_time - now;
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = signal::ctrl_c() => {
                    println!("\n🛑 Ctrl+C — shutting down ({} frames encoded)", frames_encoded);
                    break;
                }
            }
        }
        next_frame_time += frame_interval;

        // Latch Moonlight client info the moment RTSP PLAY arrives.
        if !client_connected {
            if let Ok(guard) = client_info.lock() {
                if let Some(client) = guard.as_ref() {
                    if client.streaming_active {
                        println!("🎮 Moonlight connected: {} ({}x{}@{}fps)",
                            client.ip, client.width, client.height, client.fps);
                        debug::debug_log(&format!("Client connected {}", client.ip));
                        rtp_sender.set_fps(client.fps.max(1));
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
                        client_connected = true;
                    }
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

        match capturer.acquire_frame() {
            Ok((resource, _)) => {
                if let Ok(texture) = capturer.get_texture(&resource) {
                    // No periodic forced IDR here: FEC handles packet loss,
                    // Moonlight requests IDRs via the control stream when it
                    // can't recover, and NVENC's idrPeriod (2s GOP) is the
                    // final backstop.
                    let packet_size = enc.encode_frame(&texture, &mut out_buffer);

                    if packet_size > 0 {
                        frames_encoded += 1;
                        if frames_encoded == 1 {
                            println!("🎬 First encoded frame: {} bytes", packet_size);
                            debug::debug_log(&format!("First frame {} bytes", packet_size));
                        }

                        if video_learned {
                            let data = &out_buffer[..packet_size as usize];
                            let kind = if rtp::detect_frame_type(data) == 2 { "IDR" } else { "P" };
                            println!("[ENC] frame={} size={} bytes ({})", frames_encoded, packet_size, kind);
                            rtp_sender.send_frame(data);
                        }
                    }
                }
                capturer.release_frame().ok();
            }
            // DXGI_ERROR_WAIT_TIMEOUT — desktop unchanged this interval, skip frame.
            Err(e) if e.code().0 == 0x887A0027_u32 as i32 => {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            Err(e) => {
                eprintln!("❌ Capture error: {:?}", e);
                debug::debug_log(&format!("Capture error: {:?}", e));
                break;
            }
        }
    }

    println!("✅ Capture loop done — {} frames encoded", frames_encoded);
    // `enc` drops here → CleanupEncoder flushes + closes test.h264
    Ok(())
}


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

    let enc = Encoder::new(
        &capturer.device,
        EncoderConfig {
            width:        args.width,
            height:       args.height,
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

    // Audio port (48000) — bind so the client's address-learning "ping" packets
    // land somewhere instead of an ICMP port-unreachable. Opus encoding and
    // audio RTP packetization land in a later phase.
    let audio_probe = std::net::UdpSocket::bind("0.0.0.0:48000")
        .expect("Failed to bind audio probe socket on 48000");
    audio_probe.set_nonblocking(true).expect("set_nonblocking on audio probe");

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
                        client_connected = true;
                        // Force a fresh IDR (with inline SPS/PPS) so the client's
                        // decoder can initialize on the very first frame it sees.
                        enc.request_idr();
                    }
                }
            }
        }

        // Learn the client's real video UDP address from its first "ping" packet
        // (its source port is ephemeral and wasn't known at SETUP time).
        if client_connected && !video_learned {
            if let Some(addr) = rtp_sender.try_learn_target() {
                println!("🎥 Learned client video address: {}", addr);
                debug::debug_log(&format!("Video target {}", addr));
                video_learned = true;
            }
        }

        // Drain audio "ping" packets — no audio RTP yet.
        let mut audio_buf = [0u8; 64];
        let _ = audio_probe.recv_from(&mut audio_buf);

        match capturer.acquire_frame() {
            Ok((resource, _)) => {
                if let Ok(texture) = capturer.get_texture(&resource) {
                    let packet_size = enc.encode_frame(&texture, &mut out_buffer);

                    if packet_size > 0 {
                        frames_encoded += 1;
                        if frames_encoded == 1 {
                            println!("🎬 First encoded frame: {} bytes", packet_size);
                            debug::debug_log(&format!("First frame {} bytes", packet_size));
                        }

                        if video_learned {
                            let data = &out_buffer[..packet_size as usize];
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


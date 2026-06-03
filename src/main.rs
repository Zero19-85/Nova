mod capture;
mod rtsp;
mod rtp;
mod pairing;

use clap::Parser;
use windows::core::Result;
use std::ffi::{c_void, CString};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use mdns_sd::{ServiceDaemon, ServiceInfo};

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
    #[arg(long, default_value_t = 300)]
    seconds: u32,
}

extern "C" {
    fn OpenNvEncSession(d3d11_device: *mut c_void, out_encoder: *mut *mut c_void) -> i32;
    fn InitEncoder(encoder: *mut c_void, width: i32, height: i32, codec: *const std::ffi::c_char) -> i32;
    fn InitColorConversion(device: *mut c_void, width: i32, height: i32) -> i32;
    fn EncodeFrame(encoder: *mut c_void, d3d11_texture: *mut c_void, width: i32, height: i32, out_buffer: *mut u8, max_size: i32) -> i32;
    fn CleanupEncoder(encoder: *mut c_void) -> i32;
}

fn get_local_ip() -> String {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").expect("Failed to bind UDP for IP discovery");
    socket.connect("8.8.8.8:80").ok();
    socket.local_addr().map(|addr| addr.ip().to_string()).unwrap_or_else(|_| "127.0.0.1".to_string())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let local_ip = get_local_ip();
    println!("=== Nova Server ===\n🌐 LAN IP: {}\n", local_ip);

 // 🌟 NOVA'S PERMANENT IDENTITY
    // We adopt the exact ID Moonlight cached to bypass the security lock-out!
    let server_id = "0123456789ABCDEF"; 
    let server_mac = "00:11:22:33:44:55";
    
    let frame_interval = Duration::from_secs_f64(1.0 / args.fps as f64);
    let total_frames = (args.fps as f64 * args.seconds as f64) as u32;

    let capturer = capture::DesktopCapturer::new().expect("Failed to start capture");

    unsafe {
        let d3d_device_ptr: *mut c_void = std::mem::transmute(capturer.device.clone());
        let mut h_encoder: *mut c_void = std::ptr::null_mut();
        
        OpenNvEncSession(d3d_device_ptr, &mut h_encoder);
        let codec_cstr = CString::new(args.codec.as_str()).unwrap();
        InitEncoder(h_encoder, args.width, args.height, codec_cstr.as_ptr());
        InitColorConversion(d3d_device_ptr, args.width, args.height);

        let client_info = Arc::new(Mutex::new(None));
        std::thread::spawn({
            let info = client_info.clone();
            move || rtsp::start_rtsp_server(48010, info)
        });

        // Pass the identical IP, ID, and MAC to the Pairing Server
        tokio::spawn(crate::pairing::start_pairing_server(
            47989, 
            local_ip.clone(), 
            server_id.to_string(), 
            server_mac.to_string()
        ));

        // mDNS Discovery Registration (Sunshine exact match)
        let mdns = ServiceDaemon::new().expect("Failed to create mDNS daemon");
        let info = ServiceInfo::new(
            "_nvstream._tcp.local.",
            "Nova",
            "nova.local.",
            local_ip.as_str(),
            47989,
            &[
                ("txtvers", "1"), 
                ("port", "47989"), 
                ("mac", server_mac), 
                ("uniqueid", server_id)
            ][..],
        ).unwrap();
        mdns.register(info).ok();

        let mut rtp_sender = crate::rtp::RtpSender::new(50000, "127.0.0.1", 50002)
            .expect("Failed to create RTP sender");

        let mut next_frame_time = Instant::now();
        let mut out_buffer = vec![0u8; 8 * 1024 * 1024];
        let mut client_connected = false;

        for _ in 0..total_frames {
            let now = Instant::now();
            if now < next_frame_time { tokio::time::sleep(next_frame_time - now).await; }
            next_frame_time += frame_interval;

            if let Ok(guard) = client_info.lock() {
                if let Some(client) = guard.as_ref() {
                    if !client_connected {
                        println!("🎮 Moonlight client connected: {}:{}", client.ip, client.rtp_port);
                        let _ = rtp_sender.update_target_if_changed(&client.ip, client.rtp_port);
                        client_connected = true;
                    }
                }
            }

            match capturer.acquire_frame() {
                Ok((resource, _)) => {
                    if let Ok(texture) = capturer.get_texture(&resource) {
                        let tex_ptr = std::mem::transmute_copy(&texture);
                        EncodeFrame(h_encoder, tex_ptr, args.width, args.height, out_buffer.as_mut_ptr(), out_buffer.len() as i32);
                    }
                    capturer.release_frame().ok();
                }
                Err(e) if e.code().0 == 0x887A0027_u32 as i32 => {
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
                Err(e) => { eprintln!("Capture error: {:?}", e); break; }
            }
        }
        CleanupEncoder(h_encoder);
    }
    Ok(())
}
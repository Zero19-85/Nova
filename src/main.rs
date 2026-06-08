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

        // Start RTSP server in background thread
        std::thread::spawn({
            let info = client_info.clone();
            move || rtsp::start_rtsp_server(48010, info)
        });

        // Start pairing HTTP/HTTPS server (tokio task)
        tokio::spawn(crate::pairing::start_pairing_server(
            47989,
            local_ip.clone(),
            server_id.to_string(),
            server_mac.to_string()
        ));

        // mDNS Discovery (Sunshine-compatible)
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
        let _ = mdns.register(info);

        println!("📡 mDNS broadcaster started for Nova");

        // RTP Sender setup
        let mut rtp_sender = crate::rtp::RtpSender::new(50000, "127.0.0.1", 50002)
            .expect("Failed to create RTP sender");

        let mut next_frame_time = Instant::now();
        let mut out_buffer = vec![0u8; 8 * 1024 * 1024];
        let mut client_connected = false;

        for _ in 0..total_frames {
            let now = Instant::now();
            if now < next_frame_time {
                tokio::time::sleep(next_frame_time - now).await;
            }
            next_frame_time += frame_interval;

            // Poll the shared ClientInfo that rtsp.rs updates on SETUP/PLAY
            if let Ok(guard) = client_info.lock() {
                if let Some(client) = guard.as_ref() {
                    // Only start streaming if Moonlight explicitly sent the PLAY command
                    if !client_connected && client.streaming_active {
                        println!("🎮 Moonlight client connected: {}:{} (streaming_active={})",
                                 client.ip, client.rtp_port, client.streaming_active);
                        let _ = rtp_sender.update_target_if_changed(&client.ip, client.rtp_port);
                        client_connected = true;
                    }
                }
            }

            match capturer.acquire_frame() {
                Ok((resource, _)) => {
                    if let Ok(texture) = capturer.get_texture(&resource) {
                        let tex_ptr = std::mem::transmute_copy(&texture);
                        // Catch the returned packet size from C++
                        let packet_size = EncodeFrame(h_encoder, tex_ptr, args.width, args.height, out_buffer.as_mut_ptr(), out_buffer.len() as i32);
                        
                        // If we have video data and Moonlight is ready...
                        if packet_size > 0 && client_connected {
                            let encoded_data = &out_buffer[..packet_size as usize];
                            
                            // Annex B Parser: Find all 0x00 0x00 0x00 0x01 start codes
                            let mut nal_starts = Vec::new();
                            for i in 0..encoded_data.len().saturating_sub(3) {
                                if encoded_data[i] == 0 && encoded_data[i+1] == 0 && encoded_data[i+2] == 0 && encoded_data[i+3] == 1 {
                                    nal_starts.push(i + 4); // Push the index just after the start code
                                }
                            }

                            // Send each isolated NAL unit
                            for (idx, &start) in nal_starts.iter().enumerate() {
                                let end = if idx + 1 < nal_starts.len() {
                                    nal_starts[idx + 1] - 4 // End right before the next start code
                                } else {
                                    encoded_data.len() // End of the whole buffer
                                };

                                let nal_slice = &encoded_data[start..end];
                                
                                // Clean up trailing zeros that sometimes exist before the next start code
                                let clean_nal = match nal_slice.iter().rposition(|&x| x != 0) {
                                    Some(last_non_zero) => &nal_slice[..=last_non_zero],
                                    None => nal_slice, // Edge case: all zeros
                                };

                                // The last NAL unit in this frame gets the Marker Bit = true
                                let is_last_nal = idx == nal_starts.len() - 1;
                                rtp_sender.send_nal(clean_nal, is_last_nal);
                            }
                        }
                    }
                    capturer.release_frame().ok();
                }
                Err(e) if e.code().0 == 0x887A0027_u32 as i32 => {
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
                Err(e) => {
                    eprintln!("Capture error: {:?}", e);
                    break;
                }
            }
        } // End of the frame loop

        CleanupEncoder(h_encoder);
    }

    Ok(())
}
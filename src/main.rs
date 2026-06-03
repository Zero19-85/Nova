mod capture;
mod rtsp;
mod rtp;
mod pairing;                    // ← NEW: Pairing module

use clap::Parser;
use windows::core::Result;
use std::ffi::{c_void, CString};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
    #[arg(long, default_value_t = 10)]
    seconds: u32,
}

extern "C" {
    fn OpenNvEncSession(d3d11_device: *mut c_void, out_encoder: *mut *mut c_void) -> i32;
    fn InitEncoder(encoder: *mut c_void, width: i32, height: i32, codec: *const std::ffi::c_char) -> i32;
    fn InitColorConversion(device: *mut c_void, width: i32, height: i32) -> i32;
    fn EncodeFrame(encoder: *mut c_void, d3d11_texture: *mut c_void, width: i32, height: i32, out_buffer: *mut u8, max_size: i32) -> i32;
    fn CleanupEncoder(encoder: *mut c_void) -> i32;
}

fn main() -> Result<()> {
    let args = Args::parse();

    let target_fps = args.fps as f64;
    let frame_interval = Duration::from_secs_f64(1.0 / target_fps);
    let total_frames = (target_fps * args.seconds as f64) as u32;

    println!("=== Nova Phase 3 ===");
    println!("{}x{} @ {} kbps | {} fps | {} seconds\n", args.width, args.height, args.bitrate, args.fps, args.seconds);

    let capturer = capture::DesktopCapturer::new().expect("Failed to start capture");

    unsafe {
        let d3d_device_ptr: *mut c_void = std::mem::transmute(capturer.device.clone());

        let mut h_encoder: *mut c_void = std::ptr::null_mut();
        if OpenNvEncSession(d3d_device_ptr, &mut h_encoder) != 0 {
            eprintln!("Failed to open NVENC session");
            return Ok(());
        }

        let codec_cstr = CString::new(args.codec.as_str()).expect("Invalid codec string");
        if InitEncoder(h_encoder, args.width, args.height, codec_cstr.as_ptr()) != 0 {
            eprintln!("Failed to initialize encoder");
            return Ok(());
        }

        if InitColorConversion(d3d_device_ptr, args.width, args.height) != 0 {
            eprintln!("Failed to initialize Video Processor");
            return Ok(());
        }

        // Shared client info between RTSP and RTP
        let client_info: Arc<Mutex<Option<rtsp::ClientInfo>>> = Arc::new(Mutex::new(None));

        std::thread::spawn({
            let info = client_info.clone();
            move || {
                crate::rtsp::start_rtsp_server(48010, info);
            }
        });

        // === NEW: Start Pairing Server (NVIDIA handshake) ===
        tokio::spawn(async {
            crate::pairing::start_pairing_server(47989).await;
        });
        // ===================================================

        let mut rtp_sender = crate::rtp::RtpSender::new(50000, "127.0.0.1", 50002)
            .expect("Failed to create RTP sender");

        println!("RTSP + RTP + Pairing ready. Connect with Moonlight.\n");

        let mut next_frame_time = Instant::now();
        let mut out_buffer: Vec<u8> = vec![0u8; 1024 * 1024];
        let mut client_connected = false;

        for i in 0..total_frames {
            let now = Instant::now();
            if now < next_frame_time {
                std::thread::sleep(next_frame_time - now);
            }
            next_frame_time += frame_interval;

            // Check for connected client and update RTP target
            if let Ok(info) = client_info.lock() {
                if let Some(client) = info.as_ref() {
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
                        let texture_ptr: *mut c_void = std::mem::transmute(texture);
                        let size = EncodeFrame(
                            h_encoder,
                            texture_ptr,
                            args.width,
                            args.height,
                            out_buffer.as_mut_ptr(),
                            out_buffer.len() as i32,
                        );
                        if size > 0 {
                            rtp_sender.send_nal(&out_buffer[..size as usize]);
                        }
                    }
                    let _ = capturer.release_frame();
                }
                Err(e) => eprintln!("Frame {} acquire failed: {:?}", i + 1, e),
            }
        }

        println!("\nDone.");
        CleanupEncoder(h_encoder);
    }

    Ok(())
}
// WASAPI desktop loopback audio capture via C++ FFI shim.
// Captures raw PCM from the default render device output.
// Wire start_capture_thread() into the session loop once RTSP carries an audio track.

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

extern "C" {
    fn InitAudioCapture(
        out_sample_rate: *mut u32,
        out_channels: *mut u16,
        out_bits_per_sample: *mut u16,
    ) -> i32;
    fn CaptureAudioFrame(out_buffer: *mut u8, max_bytes: i32, out_frames: *mut u32) -> i32;
    fn CleanupAudio();
}

#[derive(Debug, Clone, Copy)]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
}

impl AudioFormat {
    pub fn bytes_per_frame(self) -> u32 {
        self.channels as u32 * (self.bits_per_sample as u32 / 8)
    }
}

/// Initialise WASAPI loopback and return a channel that yields raw PCM chunks
/// (interleaved, format described by the returned AudioFormat).
/// Each Vec<u8> is one or more audio frames captured from the mix buffer.
/// Drops the sender cleanly when the Receiver is dropped.
pub fn start_capture_thread() -> Result<(mpsc::Receiver<Vec<u8>>, AudioFormat), String> {
    let mut sample_rate: u32 = 0;
    let mut channels: u16 = 0;
    let mut bps: u16 = 0;

    let fmt = unsafe {
        let ret = InitAudioCapture(&mut sample_rate, &mut channels, &mut bps);
        if ret != 0 {
            return Err(format!("InitAudioCapture failed (code {})", ret));
        }
        AudioFormat { sample_rate, channels, bits_per_sample: bps }
    };

    let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(8);

    thread::spawn(move || {
        // Allocate ~1 s of audio at worst-case 48 kHz, 2 ch, 32-bit float
        let mut buf = vec![0u8; 48_000 * 2 * 4];

        loop {
            let mut frames: u32 = 0;
            let bytes = unsafe {
                CaptureAudioFrame(buf.as_mut_ptr(), buf.len() as i32, &mut frames)
            };

            if bytes > 0 {
                // Drop silently if receiver is gone (streaming stopped)
                if tx.try_send(buf[..bytes as usize].to_vec()).is_err() {
                    break;
                }
            } else if bytes < 0 {
                eprintln!("❌ CaptureAudioFrame error: {}", bytes);
                break;
            } else {
                // No data yet — yield briefly to avoid busy-spinning
                thread::sleep(Duration::from_millis(5));
            }
        }

        unsafe { CleanupAudio(); }
    });

    Ok((rx, fmt))
}

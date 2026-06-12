// GameStream audio path: WASAPI loopback capture (C++ shim) → 5ms Opus frames
// → optional AES-128-CBC encryption (rikey from /launch) → RTP on port 48000.
//
// Wire format matches Sunshine's audioBroadcastThread (stream.cpp:1595):
//   [12B RTP: 0x80, PT=97, seq BE u16, timestamp BE u32, ssrc=0] + opus payload
// timestamp advances by packetDuration (ms units) per packet; the IV for CBC
// is 16 zero bytes with BE(rikeyid + seq) in the first 4.
// Audio FEC shards (PT=127, RS(4,2)) are NOT sent yet — Moonlight treats them
// as optional; a lost packet is a 5ms dropout.

use std::collections::VecDeque;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

use aes::cipher::{generic_array::GenericArray, BlockEncrypt, KeyInit};
use audiopus::coder::Encoder as OpusEncoder;
use audiopus::{Application, Bitrate, Channels, SampleRate};

extern "C" {
    fn InitAudioCapture(
        device_id: *const u16,
        out_sample_rate: *mut u32,
        out_channels: *mut u16,
        out_bits_per_sample: *mut u16,
    ) -> i32;
    fn CaptureAudioFrame(out_buffer: *mut u8, max_bytes: i32, out_frames: *mut u32) -> i32;
    fn CleanupAudio();
    fn GetDefaultAudioDeviceId(out_id: *mut u16, cch: i32) -> i32;
    fn FindVirtualAudioSink(out_id: *mut u16, cch: i32) -> i32;
    fn SetDefaultAudioDevice(device_id: *const u16) -> i32;
}

const DEVICE_ID_CCH: usize = 512;

/// Truncate a wide-string buffer at its NUL terminator.
fn wide_id(buf: &[u16]) -> Vec<u16> {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    let mut v = buf[..len].to_vec();
    v.push(0);
    v
}

/// Client-only audio routing (Sunshine's approach — no muting involved):
/// switch the default render device to a virtual sink (Steam Streaming
/// Speakers / VB-CABLE) so application audio routes into it instead of the
/// physical speakers, loopback-capture that sink, and restore the original
/// default on drop (including panics and every send-loop exit path). The
/// host endpoint's volume/mute state is never touched.
struct SinkGuard {
    /// Original default render device, restored on drop (None = nothing to restore).
    restore_id: Option<Vec<u16>>,
    /// Device to loopback-capture from (None = current default endpoint).
    capture_id: Option<Vec<u16>>,
}

impl SinkGuard {
    fn engage(host_audio: bool) -> Self {
        if host_audio {
            println!("🔊 Audio: localAudioPlayMode=1 — playing on host speakers and streaming to client");
            return Self { restore_id: None, capture_id: None };
        }

        let mut sink = [0u16; DEVICE_ID_CCH];
        let found = unsafe { FindVirtualAudioSink(sink.as_mut_ptr(), DEVICE_ID_CCH as i32) };
        if found != 0 {
            eprintln!("⚠️  Audio: no virtual sink found (install Steam Streaming Speakers or VB-CABLE) — audio will also play on host speakers");
            return Self { restore_id: None, capture_id: None };
        }
        let sink_id = wide_id(&sink);

        let mut cur = [0u16; DEVICE_ID_CCH];
        let have_cur = unsafe { GetDefaultAudioDeviceId(cur.as_mut_ptr(), DEVICE_ID_CCH as i32) } == 0;
        let cur_id = wide_id(&cur);

        // Virtual sink is already the default (e.g. previous run died before
        // restoring) — capture it, but there's no original device to restore.
        if have_cur && cur_id == sink_id {
            println!("🎧 Audio: client-only — virtual sink is already the default output");
            return Self { restore_id: None, capture_id: Some(sink_id) };
        }

        let ret = unsafe { SetDefaultAudioDevice(sink_id.as_ptr()) };
        if ret != 0 {
            eprintln!("⚠️  Audio: could not switch default output to virtual sink (code {}) — audio will also play on host speakers", ret);
            return Self { restore_id: None, capture_id: None };
        }

        // Verify the switch actually landed — diagnoses "plays on both" cases.
        let mut now = [0u16; DEVICE_ID_CCH];
        if unsafe { GetDefaultAudioDeviceId(now.as_mut_ptr(), DEVICE_ID_CCH as i32) } == 0
            && wide_id(&now) != sink_id
        {
            eprintln!("⚠️  Audio: default-device readback does not match virtual sink — host speakers may still play");
        }

        println!("🎧 Audio: client-only — default output switched to virtual sink (host speakers silent, not muted)");
        Self {
            restore_id: if have_cur { Some(cur_id) } else { None },
            capture_id: Some(sink_id),
        }
    }
}

impl Drop for SinkGuard {
    fn drop(&mut self) {
        if let Some(id) = self.restore_id.take() {
            if unsafe { SetDefaultAudioDevice(id.as_ptr()) } == 0 {
                println!("🔊 Audio: default output restored to host speakers");
            } else {
                eprintln!("⚠️  Audio: failed to restore default output device — check Windows sound settings");
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
}

/// Initialise WASAPI loopback (on `device_id`, or the default render device
/// when None) and return a channel that yields raw PCM chunks (interleaved,
/// format described by the returned AudioFormat).
fn start_capture_thread(
    stop: Arc<AtomicBool>,
    device_id: Option<&[u16]>,
) -> Result<(mpsc::Receiver<Vec<u8>>, AudioFormat, thread::JoinHandle<()>), String> {
    let mut sample_rate: u32 = 0;
    let mut channels: u16 = 0;
    let mut bps: u16 = 0;

    let fmt = unsafe {
        let id_ptr = device_id.map_or(std::ptr::null(), |v| v.as_ptr());
        let ret = InitAudioCapture(id_ptr, &mut sample_rate, &mut channels, &mut bps);
        if ret != 0 {
            return Err(format!("InitAudioCapture failed (code {})", ret));
        }
        AudioFormat { sample_rate, channels, bits_per_sample: bps }
    };

    let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(16);

    let handle = thread::spawn(move || {
        // ~1 s of audio at worst-case 48 kHz, 2 ch, 32-bit float
        let mut buf = vec![0u8; 48_000 * 2 * 4];
        while !stop.load(Ordering::Relaxed) {
            let mut frames: u32 = 0;
            let bytes = unsafe {
                CaptureAudioFrame(buf.as_mut_ptr(), buf.len() as i32, &mut frames)
            };
            if bytes > 0 {
                match tx.try_send(buf[..bytes as usize].to_vec()) {
                    // Receiver gone — streaming stopped.
                    Err(mpsc::TrySendError::Disconnected(_)) => break,
                    // Channel full — drop this chunk instead of killing the
                    // session; the consumer will catch up.
                    _ => {}
                }
            } else if bytes < 0 {
                eprintln!("❌ CaptureAudioFrame error: {}", bytes);
                break;
            } else {
                thread::sleep(Duration::from_millis(2));
            }
        }
        unsafe { CleanupAudio(); }
    });

    Ok((rx, fmt, handle))
}

/// AES-128-CBC with PKCS7 padding — GameStream audio encryption
/// (Sunshine crypto::cipher::cbc_t with padding=true).
fn aes_cbc_encrypt(key: &[u8; 16], iv: &[u8; 16], data: &[u8]) -> Vec<u8> {
    let cipher = aes::Aes128::new(GenericArray::from_slice(key));
    let pad = 16 - (data.len() % 16); // PKCS7: 1..=16 bytes, always present
    let mut buf = data.to_vec();
    buf.extend(std::iter::repeat(pad as u8).take(pad));
    let mut prev = *iv;
    for chunk in buf.chunks_mut(16) {
        for i in 0..16 {
            chunk[i] ^= prev[i];
        }
        cipher.encrypt_block(GenericArray::from_mut_slice(chunk));
        prev.copy_from_slice(chunk);
    }
    buf
}

/// Handle to a running audio session; dropping/stopping ends the send thread,
/// which drops the capture receiver, which ends the capture thread + WASAPI.
pub struct AudioStreamer {
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl AudioStreamer {
    pub fn start(
        socket: UdpSocket,
        rikey: [u8; 16],
        rikeyid: u32,
        encrypt: bool,
        packet_duration_ms: u32,
        host_audio: bool,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = stop.clone();
        let handle = thread::spawn(move || {
            audio_send_loop(socket, rikey, rikeyid, encrypt, packet_duration_ms, host_audio, stop_flag);
        });
        Self { stop, handle: Some(handle) }
    }

    fn stop_inner(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            // Joining guarantees the SinkGuard in the send thread has dropped —
            // the original default output device is restored before this returns.
            let _ = h.join();
            println!("🎵 Audio stream stopped");
        }
    }

    pub fn stop(mut self) {
        self.stop_inner();
    }
}

/// Ctrl+C / shutdown path: dropping the streamer stops the thread and
/// restores the original default output device.
impl Drop for AudioStreamer {
    fn drop(&mut self) {
        self.stop_inner();
    }
}

fn audio_send_loop(
    socket: UdpSocket,
    rikey: [u8; 16],
    rikeyid: u32,
    encrypt: bool,
    packet_duration_ms: u32,
    host_audio: bool,
    stop: Arc<AtomicBool>,
) {
    // Routing must be decided before capture starts: client-only mode captures
    // the virtual sink by id, so the loopback never touches the host speakers.
    // The guard restores the original default device on every exit path.
    let sink_guard = SinkGuard::engage(host_audio);

    let (rx, fmt, cap_handle) = match start_capture_thread(stop.clone(), sink_guard.capture_id.as_deref()) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("🎵 Audio disabled: {}", e);
            return;
        }
    };

    send_pcm_loop(&socket, rikey, rikeyid, encrypt, packet_duration_ms, &stop, &rx, fmt);

    // Tear capture down COMPLETELY before this function returns: CleanupAudio
    // releases the shim's GLOBAL WASAPI state, so a reconnect's InitAudioCapture
    // must never overlap it — a lingering capture thread from the old session
    // would otherwise null out the new session's capture client mid-stream.
    // Joining also guarantees teardown precedes the SinkGuard drop below that
    // restores the default output device.
    stop.store(true, Ordering::Relaxed);
    drop(rx); // also unblocks the capture thread's channel sends
    let _ = cap_handle.join();
    drop(sink_guard);
}

#[allow(clippy::too_many_arguments)]
fn send_pcm_loop(
    socket: &UdpSocket,
    rikey: [u8; 16],
    rikeyid: u32,
    encrypt: bool,
    packet_duration_ms: u32,
    stop: &AtomicBool,
    rx: &mpsc::Receiver<Vec<u8>>,
    fmt: AudioFormat,
) {
    // Opus only accepts 48/24/16/12/8 kHz; the Windows shared-mode mix format
    // is essentially always 48 kHz. Resampling is out of scope for v1.
    if fmt.sample_rate != 48_000 {
        eprintln!("🎵 Audio disabled: mix format is {} Hz (need 48000)", fmt.sample_rate);
        return;
    }
    if fmt.bits_per_sample != 32 && fmt.bits_per_sample != 16 {
        eprintln!("🎵 Audio disabled: unsupported sample format ({}-bit)", fmt.bits_per_sample);
        return;
    }

    let mut encoder = match OpusEncoder::new(SampleRate::Hz48000, Channels::Stereo, Application::LowDelay) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("🎵 Audio disabled: Opus encoder init failed: {:?}", e);
            return;
        }
    };
    let _ = encoder.set_bitrate(Bitrate::BitsPerSecond(128_000));

    let samples_per_packet = (48_000 * packet_duration_ms as usize) / 1000; // per channel
    println!("🎵 Audio stream started: Opus 48kHz stereo 128kbps, {}ms packets, encrypted={}",
        packet_duration_ms, encrypt);

    let src_channels = fmt.channels.max(1) as usize;
    let mut pcm: VecDeque<f32> = VecDeque::with_capacity(48_000);
    let mut frame_buf: Vec<f32> = Vec::with_capacity(samples_per_packet * 2);
    let mut opus_buf = [0u8; 1400];
    let mut target: Option<SocketAddr> = None;
    let mut seq: u16 = 0;
    let mut timestamp: u32 = 0;
    let mut ping = [0u8; 64];

    while !stop.load(Ordering::Relaxed) {
        // Learn (and keep refreshed) the client's audio address from its pings.
        while let Ok((_n, addr)) = socket.recv_from(&mut ping) {
            if target != Some(addr) {
                println!("🎵 Learned client audio address: {}", addr);
                target = Some(addr);
            }
        }

        // Pull captured PCM, downmix to stereo f32 interleaved.
        match rx.recv_timeout(Duration::from_millis(5)) {
            Ok(chunk) => {
                let bytes_per_sample = (fmt.bits_per_sample / 8) as usize;
                let frame_stride = bytes_per_sample * src_channels;
                for frame in chunk.chunks_exact(frame_stride) {
                    for ch in 0..2 {
                        // Mono source: duplicate channel 0 into both.
                        let idx = if src_channels == 1 { 0 } else { ch };
                        let off = idx * bytes_per_sample;
                        let s = if fmt.bits_per_sample == 32 {
                            f32::from_le_bytes([frame[off], frame[off + 1], frame[off + 2], frame[off + 3]])
                        } else {
                            i16::from_le_bytes([frame[off], frame[off + 1]]) as f32 / 32768.0
                        };
                        pcm.push_back(s);
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }

        // Emit as many full Opus frames as we have samples for.
        while pcm.len() >= samples_per_packet * 2 {
            frame_buf.clear();
            frame_buf.extend(pcm.drain(..samples_per_packet * 2));

            let n = match encoder.encode_float(&frame_buf, &mut opus_buf) {
                Ok(n) => n,
                Err(e) => {
                    eprintln!("🎵 Opus encode error: {:?}", e);
                    continue;
                }
            };

            if let Some(t) = target {
                // IV = 16 zero bytes, first 4 = BE(rikeyid + seq) — Sunshine
                // stream.cpp:1630 / moonlight-common-c AudioStream.c.
                let payload: Vec<u8> = if encrypt {
                    let mut iv = [0u8; 16];
                    iv[..4].copy_from_slice(&rikeyid.wrapping_add(seq as u32).to_be_bytes());
                    aes_cbc_encrypt(&rikey, &iv, &opus_buf[..n])
                } else {
                    opus_buf[..n].to_vec()
                };

                let mut pkt = Vec::with_capacity(12 + payload.len());
                pkt.push(0x80); // V=2
                pkt.push(97);   // packetType: opus audio
                pkt.extend_from_slice(&seq.to_be_bytes());
                pkt.extend_from_slice(&timestamp.to_be_bytes());
                pkt.extend_from_slice(&[0u8; 4]); // ssrc
                pkt.extend_from_slice(&payload);
                let _ = socket.send_to(&pkt, t);
            }

            seq = seq.wrapping_add(1);
            timestamp = timestamp.wrapping_add(packet_duration_ms);
        }
    }
}

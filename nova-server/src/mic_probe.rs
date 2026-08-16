//! Stage 0 of the microphone work: **which process may render the audio?**
//!
//! Echo's Master owns every socket, so a microphone datagram arrives in a
//! LocalSystem service running in **Session 0**. The Worker, which owns all the
//! other audio, runs as the interactive user. Rendering in the Master is
//! obviously preferable — no IPC hop, and the microphone survives the Worker
//! respawns that sign-out and lock-screen transitions cause — but only if
//! Session 0 is actually permitted to render to an endpoint that a user-session
//! application can then capture.
//!
//! Nothing in this codebase's history makes that safe to assume. WGC fails
//! under SYSTEM because its broker needs a real user session; input injection
//! fails from Session 0 because `SendInput` is session-local; and the UIPI
//! "silent swallow" was an API that returned SUCCESS while doing nothing. The
//! shape of that last failure is exactly the one to fear here: `IAudioClient`
//! could return `S_OK` for every call and still put the samples somewhere no
//! user-session capture will ever see.
//!
//! So the probe does not trust an HRESULT. It renders a tone from one process
//! and **measures the signal arriving at a capture endpoint in another**, which
//! is the same thing a real application would do.
//!
//! ## The control experiment is not optional
//!
//! A null result — SYSTEM renders, listener hears nothing — has two
//! explanations: Session 0 is isolated, or the probe itself does not work. Those
//! are indistinguishable from one run. So it is always run twice:
//!
//! 1. **Control:** render as the user. The listener must hear the tone. This
//!    proves the cable, the endpoint names, the formats, and this code.
//! 2. **Subject:** render as LocalSystem in Session 0, listener unchanged.
//!
//! Only the pair of results means anything.

use std::io::Write;
use std::time::{Duration, Instant};

extern "C" {
    fn FindEndpointByName(needle: *const u16, is_capture: i32, out_id: *mut u16, cch: i32) -> i32;
    fn InitMicRender(device_id: *const u16, out_buffer_frames: *mut u32, out_hr: *mut i32) -> i32;
    fn RenderMicFrames(mono: *const i16, frames: u32, out_hr: *mut i32) -> i32;
    fn CleanupMicRender();
    fn InitProbeCapture(device_id: *const u16, out_hr: *mut i32) -> i32;
    fn ProbeCapturePeak(out_peak: *mut f32, out_frames: *mut u32) -> i32;
    fn CleanupProbeCapture();
}

const SAMPLE_RATE: u32 = 48_000;
/// Deliberately not a round number and not a harmonic of mains hum, so a
/// positive result cannot be something else on the machine.
const TONE_HZ: f32 = 440.0;
/// Loud enough to be unambiguous, short of full scale so nothing clips.
const TONE_AMPLITUDE: f32 = 0.35;
/// A peak below this is noise or nothing; above it, the tone arrived. Real
/// silence on a virtual cable measures exactly 0.0, so there is a wide margin.
const HEARD_THRESHOLD: f32 = 0.05;

/// Where the probe writes, since `nova-server.exe` is a windows-subsystem
/// binary with no console — and the Session 0 run has nowhere to print even if
/// it had one.
struct ProbeLog(Option<std::fs::File>);

impl ProbeLog {
    fn open(path: &str) -> Self {
        Self(std::fs::File::create(path).ok())
    }
    fn say(&mut self, line: &str) {
        println!("{line}");
        if let Some(f) = self.0.as_mut() {
            let _ = writeln!(f, "{line}");
            let _ = f.flush();
        }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Resolve an endpoint by friendly-name substring. `capture` picks the
/// direction: VB-CABLE presents "CABLE Input" as a render endpoint and "CABLE
/// Output" as a capture endpoint, and they are two ends of one cable — so the
/// direction is never inferable from the name alone.
fn resolve(name: &str, capture: bool) -> Option<Vec<u16>> {
    let needle = wide(name);
    let mut id = vec![0u16; 512];
    let rc = unsafe { FindEndpointByName(needle.as_ptr(), i32::from(capture), id.as_mut_ptr(), 512) };
    if rc != 0 {
        return None;
    }
    let len = id.iter().position(|&c| c == 0).unwrap_or(0);
    id.truncate(len + 1);
    Some(id)
}

/// Render a tone into `device` for `seconds`.
pub fn render(device: &str, seconds: u64, log_path: &str) -> i32 {
    let mut log = ProbeLog::open(log_path);
    log.say(&format!("=== mic probe: RENDER into \"{device}\" for {seconds}s ==="));
    log.say(&format!("identity: {}", whoami()));

    let Some(id) = resolve(device, false) else {
        log.say(&format!("FAIL: no active render endpoint matching \"{device}\""));
        return 2;
    };

    let (mut frames, mut hr) = (0u32, 0i32);
    let rc = unsafe { InitMicRender(id.as_ptr(), &mut frames, &mut hr) };
    if rc != 0 {
        log.say(&format!("FAIL: InitMicRender step {rc}, hr 0x{:08X}", hr as u32));
        return 3;
    }
    log.say(&format!("opened: {frames}-frame device buffer"));

    // 10 ms per write, which is well inside the 200 ms buffer, so the loop is
    // paced by sleep rather than by the device draining.
    const CHUNK: usize = (SAMPLE_RATE as usize) / 100;
    let mut phase = 0.0f32;
    let step = std::f32::consts::TAU * TONE_HZ / SAMPLE_RATE as f32;
    let mut chunk = vec![0i16; CHUNK];

    let began = Instant::now();
    let mut written = 0u64;
    let mut refused = 0u64;
    while began.elapsed() < Duration::from_secs(seconds) {
        for s in chunk.iter_mut() {
            *s = (phase.sin() * TONE_AMPLITUDE * i16::MAX as f32) as i16;
            phase += step;
            if phase > std::f32::consts::TAU {
                phase -= std::f32::consts::TAU;
            }
        }
        let n = unsafe { RenderMicFrames(chunk.as_ptr(), CHUNK as u32, &mut hr) };
        if n < 0 {
            log.say(&format!("FAIL: RenderMicFrames step {n}, hr 0x{:08X}", hr as u32));
            unsafe { CleanupMicRender() };
            return 4;
        }
        written += n as u64;
        if (n as usize) < CHUNK {
            refused += (CHUNK - n as usize) as u64;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    unsafe { CleanupMicRender() };
    log.say(&format!(
        "OK: rendered {written} frames ({:.1}s of audio), {refused} refused for a full buffer",
        written as f64 / SAMPLE_RATE as f64
    ));
    // A render that wrote nothing is a failure even with clean HRESULTs — which
    // is precisely the silent-success shape this probe exists to catch.
    if written == 0 {
        log.say("FAIL: every call succeeded but no frames were accepted");
        return 5;
    }
    0
}

/// Listen on `device` for `seconds` and report the loudest sample heard.
pub fn listen(device: &str, seconds: u64, log_path: &str) -> i32 {
    let mut log = ProbeLog::open(log_path);
    log.say(&format!("=== mic probe: LISTEN on \"{device}\" for {seconds}s ==="));
    log.say(&format!("identity: {}", whoami()));

    let Some(id) = resolve(device, true) else {
        log.say(&format!("FAIL: no active capture endpoint matching \"{device}\""));
        return 2;
    };

    let mut hr = 0i32;
    let rc = unsafe { InitProbeCapture(id.as_ptr(), &mut hr) };
    if rc != 0 {
        log.say(&format!("FAIL: InitProbeCapture step {rc}, hr 0x{:08X}", hr as u32));
        return 3;
    }

    let began = Instant::now();
    let mut peak = 0.0f32;
    let mut total = 0u64;
    while began.elapsed() < Duration::from_secs(seconds) {
        let (mut p, mut n) = (0.0f32, 0u32);
        if unsafe { ProbeCapturePeak(&mut p, &mut n) } != 0 {
            break;
        }
        peak = peak.max(p);
        total += n as u64;
        std::thread::sleep(Duration::from_millis(10));
    }
    unsafe { CleanupProbeCapture() };

    log.say(&format!(
        "captured {total} frames ({:.1}s), peak {peak:.4}",
        total as f64 / SAMPLE_RATE as f64
    ));
    // Distinguished deliberately: a cable delivering digital silence and a
    // cable delivering nothing at all fail for different reasons, and only the
    // frame count tells them apart.
    if total == 0 {
        log.say("RESULT: NOTHING — the capture endpoint produced no frames at all");
        return 4;
    }
    if peak >= HEARD_THRESHOLD {
        log.say("RESULT: HEARD — the tone crossed the cable");
        0
    } else {
        log.say("RESULT: SILENT — frames arrived, but they carried no signal");
        5
    }
}

/// Drive the **real** renderer with real Opus packets.
///
/// The difference from [`render`] is the point: that one writes PCM straight to
/// WASAPI to answer a question about tokens and sessions. This one goes through
/// `mic::start` — the same jitter buffer, the same Opus decoder, the same render
/// thread a client's audio uses — so everything between "a packet exists" and
/// "the cable carries sound" is exercised before a phone is involved.
///
/// What it cannot cover is the network and the seal, which have their own tests
/// in `nova_core::mic_channel`. Between the two, the only untested seam left is
/// the client's encoder.
pub fn pipeline(seconds: u64, log_path: &str) -> i32 {
    let mut log = ProbeLog::open(log_path);
    log.say(&format!("=== mic probe: PIPELINE (real renderer) for {seconds}s ==="));
    log.say(&format!("identity: {}", whoami()));

    let sink = match crate::mic::start(None) {
        Ok(s) => s,
        Err(why) => {
            log.say(&format!("FAIL: {why}"));
            return 2;
        }
    };

    let mut encoder = match audiopus::coder::Encoder::new(
        audiopus::SampleRate::Hz48000,
        audiopus::Channels::Mono,
        audiopus::Application::Voip,
    ) {
        Ok(e) => e,
        Err(e) => {
            log.say(&format!("FAIL: Opus encoder init: {e:?}"));
            return 3;
        }
    };

    // 20 ms packets, exactly what the Android client sends.
    const FRAME: usize = SAMPLE_RATE as usize * 20 / 1000;
    let mut pcm = vec![0i16; FRAME];
    let mut packet = vec![0u8; 1275];
    let mut phase = 0.0f32;
    let step = std::f32::consts::TAU * TONE_HZ / SAMPLE_RATE as f32;

    let began = Instant::now();
    let mut seq = 1u32;
    let mut sent = 0u64;
    while began.elapsed() < Duration::from_secs(seconds) {
        for s in pcm.iter_mut() {
            *s = (phase.sin() * TONE_AMPLITUDE * i16::MAX as f32) as i16;
            phase += step;
            if phase > std::f32::consts::TAU {
                phase -= std::f32::consts::TAU;
            }
        }
        match encoder.encode(&pcm[..], &mut packet[..]) {
            Ok(n) => {
                sink.submit(nova_core::mic_channel::MicPacket {
                    seq,
                    payload: packet[..n].to_vec(),
                    reordered: false,
                });
                seq += 1;
                sent += 1;
            }
            Err(e) => {
                log.say(&format!("FAIL: Opus encode: {e:?}"));
                return 4;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let s = crate::mic::stats();
    log.say(&format!(
        "submitted {sent} packets; renderer: rendered {}, concealed {}, underran {}, \
         paused {}, dropped {}, depth {} (worst {}), running {}",
        s.rendered, s.concealed, s.underran, s.paused, s.dropped_late, s.depth, s.worst_depth,
        s.running
    ));
    if s.rendered == 0 {
        log.say("FAIL: the renderer played nothing");
        return 5;
    }
    // A renderer that drops steadily is drifting or starved of CPU; at a
    // matched 20 ms cadence it should drop nothing at all.
    if s.dropped_late > sent / 10 {
        log.say("WARN: more than a tenth of packets were dropped for depth");
    }
    0
}

/// Whose token this process is running under, and in which session — the two
/// facts the whole probe is about, recorded in both logs so a pair of results
/// can never be attributed to the wrong run.
fn whoami() -> String {
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "?".into());
    let domain = std::env::var("USERDOMAIN").unwrap_or_else(|_| "?".into());
    let session = unsafe { windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId() };
    let mut own = 0u32;
    unsafe {
        let _ = windows::Win32::System::RemoteDesktop::ProcessIdToSessionId(
            std::process::id(),
            &mut own,
        );
    }
    format!("{domain}\\{user}, pid {} in session {own} (console session is {session})", std::process::id())
}

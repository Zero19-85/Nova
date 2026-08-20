//! Microphone passthrough: the client's Opus packets → VB-CABLE's input.
//!
//! The last leg of the path that starts at an Android `AudioRecord`. Sealed
//! datagrams arrive on the media socket, `echo::session` authenticates and
//! deduplicates them, and this module decodes and plays them into a virtual
//! cable so Windows applications can select "CABLE Output" as their microphone.
//!
//! ## This runs in the Master, and that was measured
//!
//! The Master is LocalSystem in **Session 0**, which is where three earlier
//! subsystems in this project turned out to be crippled: WGC's broker refuses
//! SYSTEM, `SendInput` is session-local, and UIPI silently swallowed injected
//! input while returning success. Audio was checked the same way those were
//! eventually understood — by measurement, not by reading documentation. A
//! Session 0 render into VB-CABLE was captured at full amplitude from the
//! interactive user's session (`--mic-probe`, 2026-08-16), byte-identical to a
//! user-session control run.
//!
//! So the microphone needs no Worker hop, and it inherits a real benefit from
//! that: the Master is the process that *never* restarts, so the microphone
//! survives the Worker respawns that sign-out, session swaps, and lock-screen
//! transitions cause.
//!
//! ## Why there is a thread here
//!
//! Rendering must happen at a steady cadence whatever the network is doing,
//! which is exactly what the receive path cannot promise. The network task's
//! only job is to hand packets over and get out of the way; this thread owns
//! the clock, the jitter buffer, and the WASAPI endpoint.
//!
//! It is a plain OS thread rather than a tokio task on purpose. It sleeps in
//! fixed steps and holds COM state with thread affinity of its own, and putting
//! that on the shared runtime would let a stalled render step delay unrelated
//! network work — the same coupling `learn_ticker` caused when one drain became
//! responsible for two rates.
//!
//! ## The jitter buffer is the whole problem
//!
//! Everything upstream is loss-tolerant and unordered by design. What arrives
//! here is therefore a stream with gaps, reordering, and a sender whose clock is
//! not this machine's. Three failure modes, each with its own answer:
//!
//! - **A gap** (a packet lost in flight) is concealed by Opus itself: decoding
//!   with no input asks the codec to extrapolate, which is far better than
//!   silence and much better than skipping ahead.
//! - **A starved buffer** (nothing has arrived at all) plays silence. It is
//!   deliberately *not* concealed — extrapolating through a long absence
//!   produces a robotic smear, and the honest answer to "the client stopped
//!   talking" is quiet.
//! - **Clock drift** is the one that has no natural bound. The phone's 48 kHz
//!   and the endpoint's 48 kHz differ by parts per million, so over a long call
//!   the buffer walks in one direction until it is either empty or seconds
//!   deep. It is corrected explicitly at the bounds — drop the oldest packet
//!   when too deep, which costs 20 ms of audio and claws back the latency
//!   permanently.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::Mutex;
use std::time::Duration;

use nova_core::mic_channel::MicPacket;

extern "C" {
    fn FindEndpointByName(needle: *const u16, is_capture: i32, out_id: *mut u16, cch: i32) -> i32;
    fn InitMicRender(device_id: *const u16, out_buffer_frames: *mut u32, out_hr: *mut i32) -> i32;
    fn RenderMicFrames(mono: *const i16, frames: u32, out_hr: *mut i32) -> i32;
    fn CleanupMicRender();
}

/// The endpoint the microphone is rendered into, matched as a case-insensitive
/// substring of the friendly name.
///
/// VB-CABLE rather than the bundled Virtual Audio Driver, and that is a
/// constraint rather than a preference: VAD's kernel-mode driver is code-signed
/// but **not Microsoft attestation-signed**, so Secure Boot refuses to load it
/// (CM_PROB 52). VB-CABLE is attestation-signed and loads normally. Do not
/// "fix" VAD by enabling test-signing or disabling Secure Boot.
const DEFAULT_RENDER_DEVICE: &str = "CABLE Input";

/// Opus at 48 kHz mono — what the client encodes and what the shim opens.
const SAMPLE_RATE: usize = 48_000;

/// Largest decode a single Opus packet can produce: 120 ms at 48 kHz. Sized for
/// the codec's maximum rather than the 20 ms the client actually sends, so a
/// client that changes frame size cannot overflow this.
const MAX_DECODE_SAMPLES: usize = SAMPLE_RATE * 120 / 1000;

/// How much audio to accumulate before playing any of it.
///
/// The buffer's whole purpose is to absorb variation in arrival time, so it has
/// to hold *something* before it starts or the first jitter causes an underrun.
/// Two packets is 40 ms — enough to ride out ordinary Wi-Fi variance, small
/// enough that nobody perceives it in a conversation.
const START_DEPTH: usize = 2;

/// Depth beyond which latency is clawed back by dropping the oldest packet.
///
/// This is the drift correction and the burst absorber in one. Eight packets is
/// 160 ms — past the point where added delay is worse than a 20 ms discontinuity,
/// and far enough above [`START_DEPTH`] that ordinary jitter never triggers it.
const MAX_DEPTH: usize = 8;

/// How long a starved buffer waits before it stops rendering silence.
///
/// Rendering silence keeps the device buffer fed and the endpoint alive during a
/// short gap. Doing it forever would hold the cable open for a client that has
/// stopped talking, so after this the renderer idles until packets resume.
const IDLE_AFTER: Duration = Duration::from_secs(2);

/// Playout step. One 20 ms client packet per step in the steady state.
const STEP: Duration = Duration::from_millis(20);

// ── Telemetry ───────────────────────────────────────────────────────────────
// Read by the Master's reporting task. Atomics rather than a channel because
// nothing may block the render thread to observe it.

static PACKETS_RENDERED: AtomicU64 = AtomicU64::new(0);
static CONCEALED: AtomicU64 = AtomicU64::new(0);
static UNDERRAN: AtomicU64 = AtomicU64::new(0);
static PAUSED: AtomicU64 = AtomicU64::new(0);
static DROPPED_LATE: AtomicU64 = AtomicU64::new(0);
static DEPTH: AtomicU64 = AtomicU64::new(0);
static WORST_DEPTH: AtomicU64 = AtomicU64::new(0);
static RUNNING: AtomicBool = AtomicBool::new(false);

/// Whether a session currently wants the microphone cable.
///
/// The render endpoint is opened when this goes true and **closed when it goes
/// false**, which is the whole point: before this existed, [`start`] opened
/// VB-CABLE's render endpoint at Master startup and held it for the life of the
/// process, whether or not anybody was streaming. Nova sat holding a device
/// nobody was using, across every detach, every network switch and every idle
/// hour between sessions.
///
/// Driven from `echo::session::WorkerMediaPlane`'s `begin`/`end`, so it follows
/// the session lifecycle exactly — including the paths that are easy to forget,
/// because *every* ending goes through `plane.end`: an explicit stop, a detach
/// on silence, the reaper expiring a detached session, and the tray's force-end.
///
/// An atomic rather than a channel so the render thread observes it on its own
/// clock and nothing can block a caller: `end` runs while the session lock is
/// held, and a mic renderer must never be able to stall a teardown.
static WANTED: AtomicBool = AtomicBool::new(false);

/// A session is live: open the cable if it is not already open.
///
/// Idempotent, and safe on a host with no virtual cable at all — with no render
/// thread running there is nobody to read the flag.
pub fn session_started() {
    WANTED.store(true, Ordering::Relaxed);
}

/// No session holds the microphone: release the cable.
///
/// Called for **both** end modes. A detached session is not streaming, so
/// holding its microphone open would keep the endpoint busy for the whole grace
/// period on behalf of a client that has gone.
pub fn session_ended() {
    WANTED.store(false, Ordering::Relaxed);
}

#[derive(Debug, Default, Clone, Copy)]
pub struct MicRenderStats {
    pub rendered: u64,
    /// Gaps filled by Opus concealment — packets lost in flight.
    pub concealed: u64,
    /// Empty playout steps that were followed by more audio: the client was
    /// still talking and the buffer ran dry underneath it. **This is the number
    /// that means something is wrong.**
    pub underran: u64,
    /// Empty playout steps that ran on until the renderer went idle: the client
    /// had stopped talking. Entirely normal, and the bulk of the count in any
    /// real conversation.
    pub paused: u64,
    /// Packets discarded to claw back latency. A steadily rising count is clock
    /// drift; a burst is a network stall that delivered late.
    pub dropped_late: u64,
    pub depth: u64,
    pub worst_depth: u64,
    pub running: bool,
}

pub fn stats() -> MicRenderStats {
    MicRenderStats {
        rendered: PACKETS_RENDERED.load(Ordering::Relaxed),
        concealed: CONCEALED.load(Ordering::Relaxed),
        underran: UNDERRAN.load(Ordering::Relaxed),
        paused: PAUSED.load(Ordering::Relaxed),
        dropped_late: DROPPED_LATE.load(Ordering::Relaxed),
        depth: DEPTH.load(Ordering::Relaxed),
        worst_depth: WORST_DEPTH.load(Ordering::Relaxed),
        running: RUNNING.load(Ordering::Relaxed),
    }
}

/// Tracks a run of playout steps that had nothing to play, so the run can be
/// classified once its outcome is known.
///
/// The two outcomes look identical while they are happening and mean opposite
/// things once they end, which is why this cannot be counted as it goes:
///
/// - Audio **resumes** → the client was talking the whole time and the buffer
///   ran dry underneath it. A fault, and the one worth alerting on.
/// - The run reaches the idle threshold → the client had simply stopped
///   talking. Not a fault at all, and in an ordinary conversation this is the
///   overwhelming majority of empty steps.
///
/// The first version of this counted both as one number called `starved`, which
/// made a perfectly healthy two-minute conversation report 1115 of them. That is
/// exactly the kind of metric that cries wolf until nobody reads it — and this
/// project has been burned before by a measurement that could not distinguish a
/// normal condition from a fault.
#[derive(Default)]
struct SilenceRun {
    steps: u64,
}

impl SilenceRun {
    /// One playout step with an empty buffer.
    fn empty_step(&mut self) {
        self.steps += 1;
    }

    /// Audio arrived again: the run was an underrun. Returns its length.
    fn audio_resumed(&mut self) -> u64 {
        std::mem::take(&mut self.steps)
    }

    /// The run lasted long enough that the renderer idled: it was a pause.
    /// Returns its length.
    fn went_idle(&mut self) -> u64 {
        std::mem::take(&mut self.steps)
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Resolve a render endpoint id by friendly-name substring.
fn resolve_render(name: &str) -> Option<Vec<u16>> {
    let needle = wide(name);
    let mut id = vec![0u16; 512];
    if unsafe { FindEndpointByName(needle.as_ptr(), 0, id.as_mut_ptr(), 512) } != 0 {
        return None;
    }
    let len = id.iter().position(|&c| c == 0).unwrap_or(0);
    id.truncate(len + 1);
    Some(id)
}

/// The handle the network side holds. Dropping it stops the render thread.
pub struct MicSink {
    tx: Mutex<Option<Sender<MicPacket>>>,
}

impl MicSink {
    /// Hand one authenticated packet to the renderer.
    ///
    /// Never blocks and never fails upward: the microphone is an optional
    /// feature riding the same socket as the video stream, so a wedged renderer
    /// must not be able to slow the receive path down. A send that fails means
    /// the thread is gone, which the log has already reported.
    pub fn submit(&self, packet: MicPacket) {
        if let Ok(guard) = self.tx.lock() {
            if let Some(tx) = guard.as_ref() {
                let _ = tx.send(packet);
            }
        }
    }
}

/// Start the microphone renderer.
///
/// `override_name` is `[audio] endpoint_override` from nova.toml, consulted only
/// to *refuse* — see [`collides_with_ghost_sink`].
pub fn start(override_name: Option<&str>) -> Result<MicSink, String> {
    if let Some(sink) = override_name {
        if collides_with_ghost_sink(sink) {
            return Err(format!(
                "microphone disabled: [audio] endpoint_override is \"{sink}\", which resolves to \
                 the same endpoint the microphone renders into — the host's own output would be \
                 fed back into the microphone. Point one of them somewhere else."
            ));
        }
    }

    let device = resolve_render(DEFAULT_RENDER_DEVICE).ok_or_else(|| {
        format!(
            "microphone disabled: no active render endpoint matching \"{DEFAULT_RENDER_DEVICE}\" \
             — install VB-CABLE (vb-audio.com) to enable microphone passthrough"
        )
    })?;

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("nova-mic-render".into())
        .spawn(move || render_loop(device, rx))
        .map_err(|e| format!("could not start the microphone render thread: {e}"))?;

    Ok(MicSink { tx: Mutex::new(Some(tx)) })
}

/// True when the configured ghost sink is the same endpoint the microphone
/// renders into.
///
/// Worth a dedicated check because the failure is silent and confusing rather
/// than loud: `[audio] endpoint_override` names the endpoint Nova switches the
/// system default to so the *host* goes quiet while streaming. If an operator
/// points that at "CABLE Input" — which is a perfectly reasonable-looking choice,
/// since it is a virtual sink and VB-CABLE is what this feature documents —
/// every sound the host makes is rendered into the same cable the microphone
/// feeds, and the remote user hears the game as their own microphone input.
fn collides_with_ghost_sink(sink_override: &str) -> bool {
    let (Some(sink), Some(mic)) =
        (resolve_render(sink_override), resolve_render(DEFAULT_RENDER_DEVICE))
    else {
        return false;
    };
    sink == mic
}

fn render_loop(device: Vec<u16>, rx: Receiver<MicPacket>) {
    let mut decoder = match audiopus::coder::Decoder::new(
        audiopus::SampleRate::Hz48000,
        audiopus::Channels::Mono,
    ) {
        Ok(d) => d,
        Err(e) => {
            println!("🎤 Microphone disabled: Opus decoder init failed: {e:?}");
            return;
        }
    };

    let (mut frames, mut hr) = (0u32, 0i32);
    // NOT opened here. The endpoint is acquired when a session asks for it and
    // released when the session ends — see [`WANTED`]. Opening it at startup is
    // what left Nova holding VB-CABLE across every detach and every idle hour.
    let mut open = false;
    println!("🎤 Microphone wired — the cable is opened on demand, per session");

    // Ordered by sequence, which is what makes a reordered arrival play in its
    // right place rather than out of turn.
    let mut buffer: BTreeMap<u32, Vec<u8>> = BTreeMap::new();
    let mut playing = false;
    let mut next_seq: u32 = 0;
    let mut pcm = vec![0i16; MAX_DECODE_SAMPLES];
    let mut silent_for = Duration::ZERO;
    let mut silence = SilenceRun::default();

    loop {
        // Collect whatever has arrived, waiting at most one step. The recv
        // timeout *is* the clock: it returns early when packets arrive and
        // paces the loop when they do not.
        match rx.recv_timeout(STEP) {
            Ok(packet) => {
                buffer.insert(packet.seq, packet.payload);
                while let Ok(more) = rx.try_recv() {
                    buffer.insert(more.seq, more.payload);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            // The sink was dropped: the session ended.
            Err(RecvTimeoutError::Disconnected) => break,
        }

        // ── Acquire and release the cable around the session ────────────────
        //
        // Checked here rather than on a signal so the transition happens on
        // this thread, between playout steps, where the device is not in use.
        // Closing a WASAPI render client from another thread mid-`RenderMicFrames`
        // is exactly the kind of race that produces an intermittent crash in a
        // LocalSystem service.
        let wanted = WANTED.load(Ordering::Relaxed);
        if wanted && !open {
            if unsafe { InitMicRender(device.as_ptr(), &mut frames, &mut hr) } != 0 {
                println!(
                    "🎤 Microphone unavailable this session: could not open the render \
                     endpoint (hr 0x{:08X})",
                    hr as u32
                );
                // Not fatal and not retried in a tight loop: the flag stays
                // true, so the next session start re-attempts. Everything else
                // about the stream works without a microphone.
                WANTED.store(false, Ordering::Relaxed);
            } else {
                open = true;
                RUNNING.store(true, Ordering::Relaxed);
                println!("🎤 Microphone ready — rendering into \"{DEFAULT_RENDER_DEVICE}\"");
            }
        } else if !wanted && open {
            unsafe { CleanupMicRender() };
            open = false;
            RUNNING.store(false, Ordering::Relaxed);
            // Give the operator their microphone back. Claim-once, so this is a
            // no-op when the session never sent any audio and the swap never
            // happened.
            crate::audio::restore_default_capture();
            // The jitter buffer belongs to the session that just ended. Carrying
            // it into the next one would play the departing client's last words
            // to whoever connects next — and `next_seq` from a finished session
            // would make every packet of the new one look like a late arrival
            // and be dropped, which is precisely "the mic is broken after a
            // reconnect".
            buffer.clear();
            playing = false;
            next_seq = 0;
            silent_for = Duration::ZERO;
            silence = SilenceRun::default();
            DEPTH.store(0, Ordering::Relaxed);
            println!("🎤 Microphone released — the cable is free for other applications");
        }
        if !open {
            // No session: drop whatever arrived rather than letting it pile up.
            // Packets are session-sealed, so this should be empty in practice;
            // the drain is what keeps "should be" from becoming a leak.
            buffer.clear();
            continue;
        }

        // Point Windows' default microphone at the cable — but only once audio
        // is genuinely arriving. Engaging at session start would change the
        // operator's recording device for every stream, including the ones
        // where the user never switched the microphone on, and this side of the
        // wire has no other evidence of that switch: the client simply sends,
        // or does not. Idempotent, so the cost after the first packet is one
        // atomic-free mutex check per step.
        if !buffer.is_empty() {
            crate::audio::engage_default_capture();
        }

        let depth = buffer.len() as u64;
        DEPTH.store(depth, Ordering::Relaxed);
        WORST_DEPTH.fetch_max(depth, Ordering::Relaxed);

        // Drift and burst correction. Dropping the *oldest* rather than
        // refusing the newest is deliberate: the newest packet is the one the
        // speaker just said, and the backlog in front of it is pure latency.
        while buffer.len() > MAX_DEPTH {
            if let Some((&oldest, _)) = buffer.iter().next() {
                buffer.remove(&oldest);
                DROPPED_LATE.fetch_add(1, Ordering::Relaxed);
                if oldest >= next_seq {
                    next_seq = oldest + 1;
                }
            }
        }

        if !playing {
            if buffer.len() < START_DEPTH {
                continue;
            }
            // Start from the oldest packet held, not from sequence 1: the
            // session may have been running before the microphone was switched
            // on, and waiting for a sequence that is already past would stall
            // playback forever.
            next_seq = *buffer.keys().next().expect("checked non-empty");
            playing = true;
        }

        let decoded = if let Some(payload) = buffer.remove(&next_seq) {
            next_seq = next_seq.wrapping_add(1);
            silent_for = Duration::ZERO;
            // Audio is flowing again, so any empty steps just behind us were the
            // buffer running dry mid-speech rather than the client falling quiet.
            UNDERRAN.fetch_add(silence.audio_resumed(), Ordering::Relaxed);
            match decoder.decode(Some(&payload[..]), &mut pcm[..], false) {
                Ok(n) => {
                    PACKETS_RENDERED.fetch_add(1, Ordering::Relaxed);
                    n
                }
                Err(e) => {
                    // A packet that opened under the session key but will not
                    // decode is a codec mismatch, not an attack. Skip it rather
                    // than stopping: the next one is probably fine.
                    println!("🎤 Opus decode failed for packet {}: {e:?}", next_seq.wrapping_sub(1));
                    0
                }
            }
        } else if !buffer.is_empty() {
            // Something later is waiting, so this sequence is genuinely lost
            // rather than merely late. Ask Opus to extrapolate across it.
            next_seq = next_seq.wrapping_add(1);
            silent_for = Duration::ZERO;
            // Concealment is audio too, so it ends a run for the same reason.
            UNDERRAN.fetch_add(silence.audio_resumed(), Ordering::Relaxed);
            CONCEALED.fetch_add(1, Ordering::Relaxed);
            decoder
                .decode::<&[u8], &mut [i16]>(None, &mut pcm[..], false)
                .unwrap_or(0)
        } else {
            // Nothing at all. Silence, not concealment — extrapolating through
            // a long absence smears into a robotic artefact, and the honest
            // rendering of "they stopped talking" is quiet.
            silence.empty_step();
            silent_for += STEP;
            if silent_for >= IDLE_AFTER {
                // The run outlasted the idle threshold, so it was the client
                // falling quiet rather than the buffer failing to keep up.
                PAUSED.fetch_add(silence.went_idle(), Ordering::Relaxed);
                // Stop feeding the cable entirely and wait for the client to
                // resume. `playing` resets so the buffer refills to
                // START_DEPTH before playback restarts, rather than resuming
                // into an empty buffer and starving again immediately.
                playing = false;
                continue;
            }
            let step_samples = SAMPLE_RATE * STEP.as_millis() as usize / 1000;
            pcm[..step_samples].fill(0);
            step_samples
        };

        if decoded == 0 {
            continue;
        }
        let n = unsafe { RenderMicFrames(pcm.as_ptr(), decoded as u32, &mut hr) };
        if n < 0 {
            println!("🎤 Microphone stopped: render failed (step {n}, hr 0x{:08X})", hr as u32);
            break;
        }
    }

    // Only if the loop exited while still holding it — the ordinary path
    // releases at session end, above. Calling `CleanupMicRender` twice is
    // harmless (it nulls what it releases), but this keeps the log honest.
    if open {
        unsafe { CleanupMicRender() };
    }
    // Unconditional: the render thread is ending, so this is the last chance
    // this process has to put the operator's microphone back on any path that
    // reaches here — including a render failure that broke out of the loop.
    crate::audio::restore_default_capture();
    RUNNING.store(false, Ordering::Relaxed);
    WANTED.store(false, Ordering::Relaxed);
    println!("🎤 Microphone renderer stopped");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The buffer thresholds have to stand in a specific relation or the
    /// renderer oscillates: if the drop bound were at or below the start depth,
    /// every refill would immediately trigger a drop and playback would stutter
    /// permanently. Asserted rather than left to two independent constants.
    #[test]
    fn the_cable_is_wanted_only_between_a_session_start_and_its_end() {
        // The invariant the detach bug violated. `end` runs for every ending —
        // stop, detach, reap, force-end — so "ended" must always mean "not
        // wanted", including the detach that keeps the display up.
        session_started();
        assert!(WANTED.load(Ordering::Relaxed), "a live session holds the cable");
        session_ended();
        assert!(!WANTED.load(Ordering::Relaxed), "a detach releases it");
        // Idempotent: several endings can reach here for one session (a sweep
        // detaches in the same tick the tunnel closes), and the second must not
        // resurrect anything.
        session_ended();
        assert!(!WANTED.load(Ordering::Relaxed));
    }

    #[test]
    fn the_buffer_bounds_cannot_fight_each_other() {
        assert!(
            MAX_DEPTH > START_DEPTH * 2,
            "the drop bound must sit well above the start depth, or a normal refill triggers drops"
        );
        let start_ms = START_DEPTH as u128 * STEP.as_millis();
        let max_ms = MAX_DEPTH as u128 * STEP.as_millis();
        assert!(start_ms <= 60, "startup delay of {start_ms}ms is audible in a conversation");
        assert!(max_ms <= 200, "allowing {max_ms}ms of buffer is worse than a discontinuity");
    }

    /// The distinction the split exists to make. Counted as one number, a
    /// healthy conversation looked like a thousand faults.
    #[test]
    fn a_dry_buffer_is_a_fault_only_when_audio_was_still_coming() {
        let mut run = SilenceRun::default();

        // Three empty steps, then audio resumes: the client never stopped
        // talking, so those steps were the buffer failing to keep up.
        run.empty_step();
        run.empty_step();
        run.empty_step();
        assert_eq!(run.audio_resumed(), 3, "an interrupted run is an underrun");
        assert_eq!(run.audio_resumed(), 0, "a completed run must not be counted twice");

        // A long run that ends in the renderer idling is somebody not speaking.
        for _ in 0..100 {
            run.empty_step();
        }
        assert_eq!(run.went_idle(), 100, "a run that reaches idle is a pause");
        assert_eq!(run.went_idle(), 0);

        // And the two never double-count across a boundary.
        run.empty_step();
        assert_eq!(run.audio_resumed(), 1);
        run.empty_step();
        run.empty_step();
        assert_eq!(run.went_idle(), 2);
    }

    /// A single Opus packet may legally decode up to 120 ms. The scratch buffer
    /// must fit the codec's maximum rather than the 20 ms the client happens to
    /// send, or a client that changes frame size overflows it.
    #[test]
    fn the_decode_buffer_fits_the_largest_legal_opus_packet() {
        assert_eq!(MAX_DECODE_SAMPLES, 5760);
        let step_samples = SAMPLE_RATE * STEP.as_millis() as usize / 1000;
        assert!(step_samples <= MAX_DECODE_SAMPLES);
    }
}

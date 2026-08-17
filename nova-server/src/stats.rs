//! Live session telemetry for the tray UI (Server Stats window + tooltip).
//!
//! ## Why a process-global of atomics and not a channel
//!
//! The producer is the capture loop — the TIME_CRITICAL thread whose frame
//! budget is 8.33 ms at 120 fps — and the consumer is the tray thread's 2 Hz
//! repaint. A channel would put an allocation and a wakeup on the hot path to
//! feed a reader that only ever wants the LATEST value; every intermediate
//! sample is dead on arrival. Relaxed atomics give the tray a lock-free read of
//! the current state and cost the capture loop nothing it wasn't already
//! paying.
//!
//! Nothing here is on the per-frame path at all: [`sample`] is called from the
//! ONE-PER-SECOND reporting tick the capture loops already run for the
//! `🎞  Encoder output:` log line, so the only per-frame addition anywhere is a
//! single `u32` increment of a local counter that tick then consumes.
//!
//! ## What these numbers are (and are not)
//!
//! Everything here is measured **encode-side, in the Worker** — bytes NVENC
//! produced and frames the capture loop submitted. That is deliberately not the
//! same as the wire rate: `rtp.rs` adds RTP/FEC overhead on top (the shard
//! parity alone is `fec_percentage` of every frame), and under the Master/
//! Worker split the socket lives in another process entirely. Reporting the
//! encoder's own output keeps this module free of any IPC while staying an
//! honest answer to "what is the encoder doing right now" — the tray labels it
//! `Encode` for exactly that reason. Do not relabel it as throughput.
//!
//! Both capture loops (`run_worker`'s and the monolithic `run`'s) drive this,
//! so the tray reads the same fields whichever deployment is live.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::encoder::Codec;

// Codec wire codes for the AtomicU32 — a `&'static str` can't live in an
// atomic, and storing the code keeps the whole struct lock-free.
const CODEC_NONE: u32 = 0;
const CODEC_H264: u32 = 1;
const CODEC_HEVC: u32 = 2;
const CODEC_AV1: u32 = 3;

fn codec_code(codec: Codec) -> u32 {
    match codec {
        Codec::H264 => CODEC_H264,
        Codec::Hevc => CODEC_HEVC,
        Codec::Av1 => CODEC_AV1,
    }
}

fn codec_label(code: u32, hdr: bool) -> &'static str {
    match (code, hdr) {
        (CODEC_H264, _) => "H.264",
        (CODEC_HEVC, false) => "HEVC Main8",
        (CODEC_HEVC, true) => "HEVC Main10 HDR",
        (CODEC_AV1, _) => "AV1 Main8",
        _ => "—",
    }
}

/// Process-global live telemetry. One instance, written by the capture loop,
/// read by the tray thread.
struct StreamStats {
    streaming: AtomicBool,
    /// No session, but the virtual display is still up — see
    /// [`teardown_pending`]. Separate from `streaming` because the tray needs
    /// to distinguish three states, not two: streaming, display-still-held,
    /// and idle.
    teardown_pending: AtomicBool,
    width: AtomicU32,
    height: AtomicU32,
    /// The session's negotiated frame rate (the pacing target).
    target_fps: AtomicU32,
    codec: AtomicU32,
    hdr: AtomicBool,
    /// The client's negotiated bitrate — the ceiling QoS is allowed to ramp
    /// back toward, never exceeded (`enc.config.bitrate_kbps`).
    ceiling_kbps: AtomicU32,
    /// CBR target currently programmed into NVENC. Differs from the ceiling
    /// whenever QoS has stepped the rate down for congestion — showing both is
    /// the point, since "why is my bitrate lower than I set" is exactly the
    /// question this window exists to answer.
    target_kbps: AtomicU32,
    /// Measured over the last second, ×10 so one decimal survives the integer
    /// atomic (a 119.4 fps session should not read as 119).
    measured_fps_x10: AtomicU32,
    /// Measured encoder output over the last second.
    measured_kbps: AtomicU32,
}

static STATS: StreamStats = StreamStats {
    streaming: AtomicBool::new(false),
    teardown_pending: AtomicBool::new(false),
    width: AtomicU32::new(0),
    height: AtomicU32::new(0),
    target_fps: AtomicU32::new(0),
    codec: AtomicU32::new(CODEC_NONE),
    hdr: AtomicBool::new(false),
    ceiling_kbps: AtomicU32::new(0),
    target_kbps: AtomicU32::new(0),
    measured_fps_x10: AtomicU32::new(0),
    measured_kbps: AtomicU32::new(0),
};

/// An instantaneous read of every field, taken field-by-field.
///
/// Deliberately NOT atomic as a whole: a torn read can only ever mix values
/// from two samples ~1 s apart (e.g. last second's fps beside this second's
/// bitrate), which is invisible in a 2 Hz human-facing readout. Paying for a
/// lock or a seqlock to prevent that would put contention on the capture
/// thread to fix a defect nobody can perceive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Snapshot {
    pub streaming: bool,
    pub width: u32,
    pub height: u32,
    pub target_fps: u32,
    pub measured_fps_x10: u32,
    pub codec: &'static str,
    pub hdr: bool,
    pub ceiling_kbps: u32,
    pub target_kbps: u32,
    pub measured_kbps: u32,
}

impl Snapshot {
    /// `"3840 × 2160"`, or `"—"` before anything has been configured.
    pub fn resolution_text(&self) -> String {
        if self.width == 0 || self.height == 0 {
            "—".to_string()
        } else {
            format!("{} × {}", self.width, self.height)
        }
    }

    /// `"119.4 / 120 fps"` — measured against the pacing target, so a Worker
    /// falling behind its own frame deadline is visible at a glance.
    pub fn fps_text(&self) -> String {
        if !self.streaming {
            return "—".to_string();
        }
        format!(
            "{}.{} / {} fps",
            self.measured_fps_x10 / 10,
            self.measured_fps_x10 % 10,
            self.target_fps
        )
    }

    /// `"88.2 Mbps"`, switching to Kbps below 1 Mbps so a heavily throttled
    /// session doesn't read as a flat "0.0".
    pub fn rate_text(kbps: u32) -> String {
        if kbps == 0 {
            "—".to_string()
        } else if kbps >= 1000 {
            format!("{}.{} Mbps", kbps / 1000, (kbps % 1000) / 100)
        } else {
            format!("{kbps} Kbps")
        }
    }

    /// One-line summary for the tray tooltip, which Windows caps at 127 chars —
    /// every branch here stays far inside that.
    pub fn tooltip_text(&self) -> String {
        if !self.streaming {
            return "Nova — idle (no client streaming)".to_string();
        }
        format!(
            "Nova — Streaming\n{} @ {} fps\n{} · {}",
            self.resolution_text(),
            self.measured_fps_x10 / 10,
            self.codec,
            Snapshot::rate_text(self.measured_kbps),
        )
    }
}

/// A session went live: stamp everything the negotiation settled on.
///
/// `ceiling_kbps` is the client's negotiated rate, NOT whatever QoS may have
/// already stepped down to — see [`StreamStats::ceiling_kbps`].
pub fn session_started(
    width: u32,
    height: u32,
    fps: u32,
    codec: Codec,
    hdr: bool,
    ceiling_kbps: u32,
) {
    STATS.width.store(width, Ordering::Relaxed);
    STATS.height.store(height, Ordering::Relaxed);
    STATS.target_fps.store(fps, Ordering::Relaxed);
    STATS.codec.store(codec_code(codec), Ordering::Relaxed);
    STATS.hdr.store(hdr, Ordering::Relaxed);
    STATS.ceiling_kbps.store(ceiling_kbps, Ordering::Relaxed);
    STATS.target_kbps.store(ceiling_kbps, Ordering::Relaxed);
    STATS.measured_fps_x10.store(0, Ordering::Relaxed);
    STATS.measured_kbps.store(0, Ordering::Relaxed);
    // A live session owns the display, so there is nothing pending to release
    // — cleared here rather than at each call site so no future session-start
    // path can leave the tray offering to tear down a display in use.
    STATS.teardown_pending.store(false, Ordering::Release);
    // Released last: the tray treats this flag as the gate for the whole
    // struct, so publishing it after the payload means a reader that sees
    // "streaming" always sees the geometry that goes with it.
    STATS.streaming.store(true, Ordering::Release);
}

/// The session ended — the tray goes back to its idle icon and tooltip.
///
/// Geometry/codec are zeroed rather than left as history: a stale "3840 × 2160
/// HEVC" beside an idle badge reads as a live session that has frozen.
pub fn session_ended() {
    STATS.streaming.store(false, Ordering::Release);
    STATS.width.store(0, Ordering::Relaxed);
    STATS.height.store(0, Ordering::Relaxed);
    STATS.target_fps.store(0, Ordering::Relaxed);
    STATS.codec.store(CODEC_NONE, Ordering::Relaxed);
    STATS.hdr.store(false, Ordering::Relaxed);
    STATS.ceiling_kbps.store(0, Ordering::Relaxed);
    STATS.target_kbps.store(0, Ordering::Relaxed);
    STATS.measured_fps_x10.store(0, Ordering::Relaxed);
    STATS.measured_kbps.store(0, Ordering::Relaxed);
}

/// One second of encoder output, called from the capture loops' existing 1 Hz
/// reporting tick.
///
/// `elapsed_ms` is passed rather than assumed to be exactly 1000: the tick
/// fires on `>= 1s` from a loop that can overshoot (a slow VDD/CCD call inside
/// the interval), and dividing by a hardcoded second would under-report fps for
/// the overshoot. `target_kbps` is read from NVENC's live CBR target so the
/// window shows QoS reductions as they happen.
pub fn sample(frames: u32, bytes: u64, elapsed_ms: u64, target_kbps: u32) {
    let ms = elapsed_ms.max(1);
    // ×10 for one decimal place; u64 throughout so a 4K IDR burst can't
    // overflow the intermediate.
    let fps_x10 = ((frames as u64) * 10_000 / ms).min(u32::MAX as u64) as u32;
    let kbps = (bytes.saturating_mul(8) / ms).min(u32::MAX as u64) as u32;
    STATS.measured_fps_x10.store(fps_x10, Ordering::Relaxed);
    STATS.measured_kbps.store(kbps, Ordering::Relaxed);
    STATS.target_kbps.store(target_kbps, Ordering::Relaxed);
}

/// Is a client streaming right now? Drives the tray's icon state and whether
/// "End Stream" is clickable.
pub fn is_streaming() -> bool {
    STATS.streaming.load(Ordering::Acquire)
}

/// No session is running, but the virtual display is still up (suspended for a
/// fast reconnect) — so there is still something for the tray to release.
///
/// This is the state the tray's second "End Stream" press acts on. Without it
/// the menu item greyed out the moment the stream stopped, which left the
/// suspended virtual display reachable only by waiting out
/// `[stream] idle_teardown_secs` or connecting another client.
pub fn teardown_pending() -> bool {
    STATS.teardown_pending.load(Ordering::Acquire)
}

/// Published by the Worker (and the monolithic loop) whenever the virtual
/// display's occupancy changes: true once a session has ended with the display
/// left up, false as soon as it is released or a new session claims it.
pub fn set_teardown_pending(pending: bool) {
    STATS.teardown_pending.store(pending, Ordering::Release);
}

pub fn snapshot() -> Snapshot {
    let streaming = STATS.streaming.load(Ordering::Acquire);
    let hdr = STATS.hdr.load(Ordering::Relaxed);
    Snapshot {
        streaming,
        width: STATS.width.load(Ordering::Relaxed),
        height: STATS.height.load(Ordering::Relaxed),
        target_fps: STATS.target_fps.load(Ordering::Relaxed),
        measured_fps_x10: STATS.measured_fps_x10.load(Ordering::Relaxed),
        codec: codec_label(STATS.codec.load(Ordering::Relaxed), hdr),
        hdr,
        ceiling_kbps: STATS.ceiling_kbps.load(Ordering::Relaxed),
        target_kbps: STATS.target_kbps.load(Ordering::Relaxed),
        measured_kbps: STATS.measured_kbps.load(Ordering::Relaxed),
    }
}

/// Serializes every test that touches [`STATS`], wherever it lives.
///
/// Module-level rather than inside `mod tests` because `tray`'s tests drive
/// the same process-global (the menu item's three states are defined by these
/// flags), and cargo runs tests from different modules on different threads. A
/// mutex rather than separate globals keeps the tested type identical to the
/// shipped one.
#[cfg(test)]
pub(crate) static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_lifecycle_publishes_then_clears() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        session_ended();
        assert!(!is_streaming());
        assert_eq!(snapshot().resolution_text(), "—");

        session_started(3840, 2160, 120, Codec::Hevc, true, 90_400);
        let s = snapshot();
        assert!(s.streaming);
        assert_eq!(s.resolution_text(), "3840 × 2160");
        assert_eq!(s.codec, "HEVC Main10 HDR");
        assert_eq!(s.ceiling_kbps, 90_400);

        session_ended();
        let s = snapshot();
        assert!(!s.streaming);
        // Stale geometry beside an idle badge reads as a frozen live session.
        assert_eq!(s.width, 0);
        assert_eq!(s.codec, "—");
        session_ended();
    }

    #[test]
    fn sample_rates_account_for_tick_overshoot() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        session_started(1920, 1080, 60, Codec::H264, false, 20_000);

        // Exactly one second, 60 frames, 2.5 MB => 20 Mbps.
        sample(60, 2_500_000, 1000, 20_000);
        let s = snapshot();
        assert_eq!(s.measured_fps_x10, 600);
        assert_eq!(s.fps_text(), "60.0 / 60 fps");
        assert_eq!(s.measured_kbps, 20_000);
        assert_eq!(Snapshot::rate_text(s.measured_kbps), "20.0 Mbps");

        // The same 60 frames over a 2 s overshoot is 30 fps, not 60 — the
        // whole reason sample() takes elapsed_ms instead of assuming 1000.
        sample(60, 2_500_000, 2000, 20_000);
        assert_eq!(snapshot().measured_fps_x10, 300);

        // A QoS step-down must be visible against an unchanged ceiling — that
        // contrast is the whole point of carrying both numbers.
        sample(60, 1_000_000, 1000, 8_000);
        let s = snapshot();
        assert_eq!(s.target_kbps, 8_000);
        assert_eq!(s.ceiling_kbps, 20_000);
        session_ended();
    }

    #[test]
    fn rate_text_switches_units_and_handles_zero() {
        assert_eq!(Snapshot::rate_text(0), "—");
        assert_eq!(Snapshot::rate_text(750), "750 Kbps");
        assert_eq!(Snapshot::rate_text(88_200), "88.2 Mbps");
    }

    #[test]
    fn tooltip_stays_within_the_windows_127_char_cap() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        session_started(3840, 2160, 120, Codec::Hevc, true, 150_000);
        sample(120, 20_000_000, 1000, 150_000);
        let tip = snapshot().tooltip_text();
        assert!(tip.chars().count() < 127, "tooltip too long: {tip:?}");
        session_ended();
        assert!(snapshot().tooltip_text().chars().count() < 127);
    }
}

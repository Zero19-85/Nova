//! Bitrate policy: what a session is *allowed* to ask for, and the closed-loop
//! controller that decides what it actually gets.
//!
//! Two layers that must not be confused, because they answer different
//! questions and fail in different ways:
//!
//! 1. **The budget** ([`video_budget`]) is decided ONCE, at negotiation, from
//!    facts that cannot change mid-session: the session's geometry, its frame
//!    rate, and the bandwidth the audio pipelines need. It is a *ceiling* — the
//!    highest rate this session may ever run at — and it is deliberately
//!    generous, because a ceiling that bites during normal play is a bug.
//! 2. **The controller** ([`QosController`]) walks up and down *underneath*
//!    that ceiling in response to what the link actually does.
//!
//! The layering is what makes both correct. Before the budget existed, a client
//! that asked for 100 Mbps at 1080p got it, and the controller's recovery probe
//! dutifully climbed all the way back to it after every loss episode — spending
//! the whole session rediscovering that a rate the *resolution* never justified
//! saturates the link. Capping the ceiling fixes the probe for free, since
//! `ramp_target` converges on the ceiling it is given.
//!
//! Both layers are pure: no sockets, no NVENC, no Windows. The controller
//! reaches the encoder through `encoder`'s atomics, which is what lets the
//! Worker and the monolithic capture loop share one implementation (they had
//! already drifted apart once — that is how dynamic bitrate came to be
//! completely dead in the split deployment).

use std::time::{Duration, Instant};

use crate::encoder;

// ── Layer 1: the session budget ───────────────────────────────────────────────

/// Bitrate anchors: what each resolution tier is allowed to reach **at 60 fps**,
/// in Kbps.
///
/// Roughly 2x Moonlight's own recommended bitrate for the same mode. The factor
/// is the whole design: Moonlight's numbers are what a mode *needs* to look
/// good, and an operator who deliberately asks for more should get headroom
/// rather than an argument. What they should NOT get is the ability to ask for a
/// rate no amount of 1080p detail can consume, which is where the received
/// wisdom "just send what the client requested" ends up — the client's slider
/// goes to 150 Mbps regardless of the mode it is set to.
///
/// Tune here, not at the call sites. Interpolated on pixel count (see
/// [`resolution_ceiling`]), so intermediate and ultrawide modes land on the same
/// curve rather than needing their own rows.
const TIERS: [(u64, u32); 4] = [
    (1280 * 720, 20_000),
    (1920 * 1080, 40_000),
    (2560 * 1440, 70_000),
    (3840 * 2160, 120_000),
];

/// Exponent applied to the frame-rate ratio when scaling a tier anchor away
/// from 60 fps.
///
/// Deliberately sub-linear: doubling the frame rate does not double the
/// information, because consecutive frames of a 120 fps stream are more similar
/// to each other than consecutive frames of a 60 fps one — inter-frame
/// prediction gets *better* as cadence rises. `0.75` puts 120 fps at ~1.68x
/// rather than 2x, and 30 fps at ~0.59x rather than 0.5x.
const FPS_EXPONENT: f64 = 0.75;

/// The one bitrate floor in the system, shared by both layers deliberately.
///
/// It is the smallest video budget a reservation may leave behind AND the rate
/// [`QosController`] refuses to reduce past. Two separate floors would let the
/// layers contradict each other — a ceiling below the controller's floor makes
/// "reduce on congestion" and "never go below" mutually unsatisfiable.
pub const FLOOR_KBPS: u32 = 1_000;

/// The largest share of a session's ceiling the audio reservation may claim.
///
/// The reservation is a fixed number of Kbps, which is the right shape for the
/// 20-90 Mbps sessions Nova normally serves and the wrong shape for a 2 Mbps
/// one: 512 Kbps of a 40 Mbps ceiling is noise, and 512 Kbps of a 2 Mbps
/// ceiling is a quarter of the picture. Capping the reservation proportionally
/// means a low-bitrate session degrades its audio share instead of gutting its
/// video — and, more importantly, that raising `audio_reserve_kbps` for a fat
/// link can never quietly wreck a thin one.
const MAX_RESERVE_FRACTION: u32 = 4; // i.e. at most 1/4 of the ceiling

/// What a session is allowed to encode at, and what shaped that number.
///
/// Carries the inputs as well as the result so the caller can log a decision
/// the operator can audit — a clamp that silently halves someone's bitrate is a
/// support call, not a feature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoBudget {
    /// What the client asked for.
    pub requested_kbps: u32,
    /// The resolution/frame-rate ceiling that applied.
    pub resolution_cap_kbps: u32,
    /// Bandwidth actually held back for audio — the configured reservation, or
    /// less on a session too small to afford it (see [`MAX_RESERVE_FRACTION`]).
    pub audio_reserve_kbps: u32,
    /// What the video encoder gets. This is the number that becomes the
    /// session's NVENC ceiling and [`QosController`]'s ramp target.
    pub video_kbps: u32,
}

impl VideoBudget {
    /// True when the resolution ceiling actually reduced the request.
    pub fn was_capped(&self) -> bool {
        self.requested_kbps > self.resolution_cap_kbps
    }

    /// A log line, or `None` when the budget changed nothing and silence is
    /// correct.
    pub fn describe(&self, width: u32, height: u32, fps: u32) -> Option<String> {
        if self.video_kbps == self.requested_kbps {
            return None;
        }
        let cap = if self.was_capped() {
            format!(
                " — {}x{}@{fps} is capped at {} Kbps",
                width, height, self.resolution_cap_kbps
            )
        } else {
            String::new()
        };
        let reserve = if self.audio_reserve_kbps > 0 {
            format!(", {} Kbps reserved for audio", self.audio_reserve_kbps)
        } else {
            String::new()
        };
        Some(format!(
            "🎚️  Video budget: {} Kbps requested → {} Kbps{cap}{reserve}",
            self.requested_kbps, self.video_kbps
        ))
    }
}

/// The highest bitrate this geometry and frame rate can justify, in Kbps.
///
/// Interpolates [`TIERS`] on pixel count and scales by frame rate. Below the
/// smallest tier the anchor scales linearly with pixels (a 480p stream gets
/// proportionally less than 720p); above the largest it does the same upward,
/// so an 8K session is bounded by arithmetic rather than by a missing row.
pub fn resolution_ceiling(width: u32, height: u32, fps: u32) -> u32 {
    let pixels = width as u64 * height as u64;
    if pixels == 0 {
        return FLOOR_KBPS;
    }

    let (lo_px, lo_kbps) = TIERS[0];
    let (hi_px, hi_kbps) = TIERS[TIERS.len() - 1];

    let at_60fps = if pixels <= lo_px {
        // Proportional to pixels below the first anchor.
        ((lo_kbps as u64 * pixels) / lo_px) as u32
    } else if pixels >= hi_px {
        ((hi_kbps as u64 * pixels) / hi_px).min(u32::MAX as u64) as u32
    } else {
        // Linear interpolation between the two bracketing anchors.
        let mut result = hi_kbps;
        for pair in TIERS.windows(2) {
            let (a_px, a_kbps) = pair[0];
            let (b_px, b_kbps) = pair[1];
            if pixels > a_px && pixels <= b_px {
                let span = b_px - a_px;
                let step = pixels - a_px;
                let gain = (b_kbps - a_kbps) as u64;
                result = a_kbps + ((gain * step) / span) as u32;
                break;
            }
        }
        result
    };

    // Frame-rate scaling. Floats are fine here: this runs once per session, not
    // per frame.
    let scale = (fps.max(1) as f64 / 60.0).powf(FPS_EXPONENT);
    let scaled = (at_60fps as f64 * scale).round();
    scaled.clamp(FLOOR_KBPS as f64, u32::MAX as f64) as u32
}

/// Decide what the video encoder gets, from what the client asked for.
///
/// Order is load-bearing: cap first, reserve second. Reserving from the *raw*
/// request would let a client dodge the reservation by asking for more, which is
/// exactly backwards — the audio pipelines need their slice of the rate we
/// actually intend to send, not of an aspiration.
pub fn video_budget(
    requested_kbps: u32,
    width: u32,
    height: u32,
    fps: u32,
    audio_reserve_kbps: u32,
) -> VideoBudget {
    let resolution_cap_kbps = resolution_ceiling(width, height, fps);

    // A zero request means the client's ANNOUNCE carried no
    // `maximumBitrateKbps` at all. Passed through untouched rather than
    // "clamped" to something: this layer's job is to bound what was asked for,
    // and inventing a bitrate for a client that named none would hide a
    // negotiation failure behind a plausible-looking number.
    if requested_kbps == 0 {
        return VideoBudget {
            requested_kbps: 0,
            resolution_cap_kbps,
            audio_reserve_kbps: 0,
            video_kbps: 0,
        };
    }

    let capped = requested_kbps.min(resolution_cap_kbps);

    // Never claim more than a quarter of the session, and never reserve so much
    // that video drops below the controller's own floor — unless the session was
    // already below it, in which case the client asked for something tiny on
    // purpose and the reservation yields rather than *raising* its bitrate.
    let reserve = audio_reserve_kbps
        .min(capped / MAX_RESERVE_FRACTION)
        .min(capped.saturating_sub(FLOOR_KBPS.min(capped)));
    let video_kbps = capped.saturating_sub(reserve).max(1);

    VideoBudget {
        requested_kbps,
        resolution_cap_kbps,
        audio_reserve_kbps: reserve,
        video_kbps,
    }
}

// ── Layer 2: the closed-loop controller ───────────────────────────────────────

/// One ramp-back step: +10% of the current bitrate, never past `target`.
///
/// `+ cur / 10` (not `* 11 / 10`) so the arithmetic cannot overflow at any
/// plausible bitrate, and `max(1)` on the increment guarantees forward
/// progress — an integer +10% of a very low bitrate would otherwise round to
/// zero and the ramp would stall below the target forever.
fn qos_ramp_step(cur: u32, target: u32) -> u32 {
    cur.saturating_add((cur / 10).max(1)).min(target)
}

/// Dynamic-bitrate (QoS) controller: AIMD **with memory of the rate that
/// failed**. Shared by the Worker and monolithic capture loops so the two can
/// never drift apart (they already had once — that is how dynamic bitrate came
/// to be completely dead in the split deployment).
///
/// ### Why memory is required (live 2026-08-07)
///
/// The first version ramped back to the client's full negotiated ceiling, which
/// produced a permanent sawtooth — straight from the log:
///
/// ```text
/// 📉 72320 → 📈 79552 → 📉 63641 → 📈 70005 → 77005 → 84705 → 90400
/// 📉 72320 → 📈 79552 → 📉 63641 → 📈 70005 → 77005 → 84705 → 90400 → …
/// ```
///
/// Every ~12 s it climbed back to the one bitrate already proven
/// unsustainable, re-saturated the link and took another loss hit: four such
/// cycles in one session, each a visible freeze. At the ceiling the wire rate
/// also starved the ENet control channel until the client's control peer timed
/// out and the whole session dropped, needing a `/resume`.
///
/// ### Control law
///
/// * **Remember** the bitrate that was applied when congestion fired.
/// * **Drop fast** — apply the pending 20% cut at once ([`Self::REDUCE_COOLDOWN`]
///   collapses a burst of reports into one step rather than a slide to the floor).
/// * **Ramp to a safe target, not the ceiling** — climb 10% at a time toward
///   [`Self::ramp_target`] (90% of what failed) and then HOLD. Each further
///   failure ratchets that target down, so it converges on what the link
///   actually sustains instead of rediscovering the cliff.
/// * **Probe slowly** — after [`Self::PROBE_INTERVAL`] parked and clean, relax the
///   remembered failure point 5% so a one-off blip can't cap the session's
///   quality forever. Deliberately ~20× slower than the ramp.
///
/// The `ceiling_kbps` every method takes is [`VideoBudget::video_kbps`], not the
/// client's raw request — see this module's header for why that matters to the
/// probe.
pub struct QosController {
    /// Ceiling this state belongs to; a change means a new session and stale
    /// memory (see [`Self::tick`]).
    ceiling_seen: u32,
    /// Bitrate live when congestion last fired. `None` = nothing has failed
    /// this session, so the full ceiling is fair game.
    known_bad_kbps: Option<u32>,
    /// Reduce/ramp cooldown clock.
    last_event: Instant,
    /// Last upward relaxation of the target (the slow probe).
    last_probe: Instant,
}

impl QosController {
    /// Minimum gap between two reductions.
    const REDUCE_COOLDOWN: Duration = Duration::from_secs(2);
    /// Quiet period before stepping the bitrate back up.
    const RAMP_INTERVAL: Duration = Duration::from_secs(3);
    /// Quiet period parked at the target before probing above it. Short so a
    /// link that has actually healed recovers in seconds, not minutes — during
    /// genuine congestion the reduce path keeps firing and resets this clock,
    /// so it only elapses once the link is truly quiet.
    const PROBE_INTERVAL: Duration = Duration::from_secs(10);

    fn a_past_instant() -> Instant {
        // checked_sub: Instant is QPC-since-boot on Windows, so plain
        // subtraction panics when the process starts <30 s after power-on (the
        // Phase 15.3 crash-loop). Starting the clocks in the past lets the
        // first congestion signal act at once instead of waiting out a cooldown
        // that never applied to anything.
        Instant::now()
            .checked_sub(Duration::from_secs(120))
            .unwrap_or_else(Instant::now)
    }

    pub fn new() -> Self {
        let past = Self::a_past_instant();
        Self { ceiling_seen: 0, known_bad_kbps: None, last_event: past, last_probe: past }
    }

    /// Forget everything a session learned about the link. Called at each new
    /// session (see the Worker's Configure arm) — the failure memory is
    /// per-link-episode, and a reconnect after a MoCA/Wi-Fi glitch deserves a
    /// clean slate. Without this the memory leaked across reconnects whenever
    /// two sessions negotiated the same ceiling (live 2026-08-08: an 11-session
    /// MoCA-failure run left a fresh session capped at 3.5 Mbps by the previous
    /// session's collapse, because the ceiling-change reset never fired).
    pub fn reset(&mut self) {
        self.known_bad_kbps = None;
        self.last_event = Self::a_past_instant();
        self.last_probe = Self::a_past_instant();
    }

    /// What recovery is allowed to climb to: 90% of whatever failed, clamped to
    /// the ceiling and the floor. With no failure on record, the ceiling.
    fn ramp_target(&self, ceiling_kbps: u32) -> u32 {
        match self.known_bad_kbps {
            Some(bad) => (bad / 10 * 9).clamp(FLOOR_KBPS.min(ceiling_kbps), ceiling_kbps),
            None => ceiling_kbps,
        }
    }

    /// Record the bitrate that was live when congestion fired. Only ratchets
    /// DOWN within a session: a failure at a lower rate means the link is worse
    /// than we thought, while one at a higher rate is stale news.
    fn note_congestion(&mut self, applied_kbps: u32) {
        if applied_kbps == 0 {
            return;
        }
        self.known_bad_kbps = Some(match self.known_bad_kbps {
            Some(bad) => bad.min(applied_kbps),
            None => applied_kbps,
        });
    }

    /// Relax the remembered failure point toward the ceiling by a QUARTER of
    /// the remaining gap each probe, so recovery time is BOUNDED regardless of
    /// how deep congestion drove the stream: geometric convergence reaches the
    /// ceiling in ~13 probes from any depth, versus the old fixed +5% which
    /// took ~80 minutes to climb back from a 3.5 Mbps collapse (live
    /// 2026-08-08). Big steps far from the ceiling (fast recovery through the
    /// safe zone) taper to small steps near it (cautious where the cliff was) —
    /// the shape you actually want. Clearing the memory once the target reaches
    /// the ceiling lets a fully recovered link return to full quality.
    fn relax_target(&mut self, ceiling_kbps: u32) {
        let Some(bad) = self.known_bad_kbps else { return };
        if bad >= ceiling_kbps {
            self.known_bad_kbps = None;
            return;
        }
        // A quarter of the remaining gap, but never a smaller step than
        // ceiling/16 — pure geometric convergence crawls in tiny steps near
        // the top (the gap, and thus gap/4, shrinks toward 1), which would
        // drag the last stretch of the recovery out to dozens of probes. The
        // floor makes the tail snap to full instead: once within ~ceiling/16
        // of the ceiling the next step clears it and the memory is forgotten.
        let step = ((ceiling_kbps - bad) / 4).max(ceiling_kbps / 16).max(1);
        let relaxed = bad.saturating_add(step);
        self.known_bad_kbps = if relaxed >= ceiling_kbps { None } else { Some(relaxed) };
    }

    /// One tick. Idle cost: one atomic swap plus two `Instant::elapsed`.
    pub fn tick(&mut self, ceiling_kbps: u32, fps: u32) {
        if ceiling_kbps == 0 {
            return; // no session
        }
        // A different ceiling means a different session — the old session's
        // failure memory says nothing about this one's link budget.
        if ceiling_kbps != self.ceiling_seen {
            self.ceiling_seen = ceiling_kbps;
            self.known_bad_kbps = None;
        }

        if let Some(reduced) = encoder::take_congestion_bitrate() {
            if self.last_event.elapsed() >= Self::REDUCE_COOLDOWN {
                let applied = encoder::get_stream_bitrate_kbps().max(0) as u32;
                self.note_congestion(applied);
                encoder::reconfigure_bitrate(reduced, fps);
                encoder::set_stream_bitrate_kbps(reduced as i32);
                self.last_event = Instant::now();
                self.last_probe = Instant::now();
                println!(
                    "📉 Congestion: bitrate → {} Kbps ({}% of {} ceiling) — \
                     {} Kbps failed, will hold at {} Kbps",
                    reduced,
                    reduced * 100 / ceiling_kbps,
                    ceiling_kbps,
                    applied,
                    self.ramp_target(ceiling_kbps),
                );
            }
            return;
        }

        let cur = encoder::get_stream_bitrate_kbps().max(0) as u32;
        if cur == 0 {
            return;
        }
        let target = self.ramp_target(ceiling_kbps);
        if cur < target {
            if self.last_event.elapsed() >= Self::RAMP_INTERVAL {
                let ramped = qos_ramp_step(cur, target);
                encoder::reconfigure_bitrate(ramped, fps);
                encoder::set_stream_bitrate_kbps(ramped as i32);
                self.last_event = Instant::now();
                self.last_probe = Instant::now();
                let held = if ramped == target { " — holding here" } else { "" };
                println!("📈 Congestion: ramped bitrate → {ramped} Kbps (+10%, target {target}){held}");
            }
        } else if target < ceiling_kbps && self.last_probe.elapsed() >= Self::PROBE_INTERVAL {
            // Parked at the safe target and clean for a full minute: the link
            // may have recovered, so lift the target and let the ramp above
            // walk up to it.
            self.relax_target(ceiling_kbps);
            self.last_probe = Instant::now();
            println!(
                "🔎 Congestion: {}s clean at {} Kbps — probing up to {} Kbps",
                Self::PROBE_INTERVAL.as_secs(),
                cur,
                self.ramp_target(ceiling_kbps),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Layer 1: the budget ───────────────────────────────────────────────────

    /// The headline case from the ask: a 1080p session must not be allowed to
    /// run at 100 Mbps just because the client's slider goes that high.
    #[test]
    fn a_1080p_session_cannot_ask_for_100_megabits() {
        let b = video_budget(100_000, 1920, 1080, 60, 0);
        assert!(b.was_capped());
        assert_eq!(b.resolution_cap_kbps, 40_000);
        assert_eq!(b.video_kbps, 40_000);
    }

    /// A request BELOW the ceiling is honoured exactly. The cap is a ceiling,
    /// not a target — inflating a modest request would be a bandwidth bug
    /// dressed up as a quality feature.
    #[test]
    fn a_modest_request_is_never_raised() {
        let b = video_budget(8_000, 1920, 1080, 60, 0);
        assert!(!b.was_capped());
        assert_eq!(b.video_kbps, 8_000);
        assert!(b.describe(1920, 1080, 60).is_none(), "an unchanged budget must log nothing");
    }

    /// Every tier anchor must land on its documented value at 60 fps, and the
    /// curve must rise monotonically with pixel count — an interpolation that
    /// dipped between anchors would give some intermediate mode a *lower*
    /// ceiling than a smaller one.
    #[test]
    fn the_tier_curve_hits_its_anchors_and_never_dips() {
        assert_eq!(resolution_ceiling(1280, 720, 60), 20_000);
        assert_eq!(resolution_ceiling(1920, 1080, 60), 40_000);
        assert_eq!(resolution_ceiling(2560, 1440, 60), 70_000);
        assert_eq!(resolution_ceiling(3840, 2160, 60), 120_000);

        // Walk the whole range in 16-pixel-wide steps at a fixed height.
        let mut prev = 0;
        for w in (640..=7680).step_by(16) {
            let c = resolution_ceiling(w, w * 9 / 16, 60);
            assert!(c >= prev, "ceiling dipped at {w}px wide: {c} < {prev}");
            prev = c;
        }
    }

    /// Intermediate and ultrawide modes must interpolate, not snap to a tier.
    /// 3440x1440 has ~1.34x the pixels of 2560x1440, so it must sit between the
    /// 1440p and 4K anchors rather than being treated as either.
    #[test]
    fn ultrawide_lands_between_the_anchors() {
        let uw = resolution_ceiling(3440, 1440, 60);
        assert!(
            uw > 70_000 && uw < 120_000,
            "3440x1440 should interpolate between 1440p and 4K, got {uw}"
        );
    }

    /// Frame rate scales the ceiling sub-linearly: more frames need more bits,
    /// but not proportionally, because inter-frame prediction improves as
    /// cadence rises.
    #[test]
    fn frame_rate_scales_the_ceiling_sublinearly() {
        let at_60 = resolution_ceiling(1920, 1080, 60);
        let at_120 = resolution_ceiling(1920, 1080, 120);
        let at_30 = resolution_ceiling(1920, 1080, 30);

        assert!(at_120 > at_60, "120 fps must allow more than 60");
        assert!(at_120 < at_60 * 2, "120 fps must not allow double — prediction improves");
        assert!(at_30 < at_60 && at_30 > at_60 / 2);

        // A 4K120 session must still clear the ~90 Mbps such clients actually
        // negotiate, or the cap would bite a mode Nova is known to serve well.
        assert!(
            resolution_ceiling(3840, 2160, 120) > 90_000,
            "4K120 must not be capped below what it already streams"
        );
    }

    /// The reservation comes off the top of the *capped* ceiling, so a client
    /// cannot dodge it by requesting more.
    #[test]
    fn the_audio_reserve_comes_off_the_capped_ceiling() {
        // Request above the cap: reserve applies to the cap.
        let b = video_budget(100_000, 1920, 1080, 60, 512);
        assert_eq!(b.audio_reserve_kbps, 512);
        assert_eq!(b.video_kbps, 40_000 - 512);

        // Request below the cap: reserve applies to the request.
        let b = video_budget(20_000, 1920, 1080, 60, 512);
        assert_eq!(b.audio_reserve_kbps, 512);
        assert_eq!(b.video_kbps, 20_000 - 512);
    }

    /// The reservation must never starve video on a thin link. 512 Kbps is
    /// noise on a 40 Mbps session and a quarter of a 2 Mbps one, so it is capped
    /// proportionally — raising the knob for a fat link can never wreck a thin
    /// one.
    #[test]
    fn the_audio_reserve_cannot_starve_a_low_bitrate_session() {
        // A tiny session: the reserve yields rather than gutting the picture.
        let b = video_budget(1_200, 640, 360, 30, 512);
        assert!(b.audio_reserve_kbps < 512, "reserve must shrink on a thin link");
        assert!(b.audio_reserve_kbps <= b.requested_kbps / 4);
        assert!(b.video_kbps >= 900, "video must survive: got {}", b.video_kbps);

        // At or below the controller's floor the reserve steps aside entirely
        // rather than driving video under it.
        let b = video_budget(FLOOR_KBPS, 640, 360, 30, 512);
        assert_eq!(b.video_kbps, FLOOR_KBPS);
        assert_eq!(b.audio_reserve_kbps, 0);

        // And a pathological reserve can never invert the budget.
        for requested in [500u32, 1_000, 2_000, 10_000, 90_000] {
            let b = video_budget(requested, 1920, 1080, 60, 100_000);
            assert!(b.video_kbps > 0, "video budget hit zero at {requested}");
            assert!(b.video_kbps <= requested, "budget must never exceed the request");
        }
    }

    /// A clamp that changes the stream must be auditable in the log; one that
    /// does not must stay silent.
    #[test]
    fn a_binding_budget_explains_itself() {
        let b = video_budget(100_000, 1920, 1080, 60, 512);
        let line = b.describe(1920, 1080, 60).expect("a binding budget must log");
        assert!(line.contains("100000"), "{line}");
        assert!(line.contains("39488"), "{line}");
        assert!(line.contains("reserved for audio"), "{line}");
    }

    /// A client that named no bitrate must pass through untouched — inventing
    /// one here would mask a negotiation failure as a working session.
    #[test]
    fn a_zero_request_is_passed_through_not_invented() {
        let b = video_budget(0, 1920, 1080, 60, 512);
        assert_eq!(b.video_kbps, 0);
        assert_eq!(b.audio_reserve_kbps, 0);
        assert!(b.describe(1920, 1080, 60).is_none());
    }

    /// Degenerate geometry must produce a usable ceiling rather than a zero or
    /// a panic: `apply_configure_start` feeds this straight to NVENC.
    #[test]
    fn a_degenerate_mode_still_yields_a_usable_ceiling() {
        assert!(resolution_ceiling(2, 2, 24) >= FLOOR_KBPS);
        assert!(resolution_ceiling(0, 0, 0) >= FLOOR_KBPS, "degenerate input must not panic");
    }

    // ── Layer 2: the controller ───────────────────────────────────────────────

    /// The QoS ramp must climb, converge exactly on its target, and never
    /// overshoot — overshooting would push the encoder above the rate the
    /// controller decided is safe, which is the failure the loop exists to stop.
    #[test]
    fn qos_ramp_converges_on_target_without_overshoot() {
        let target = 81_360u32; // 90% of a 90400 Kbps failure
        let mut cur = 72_320; // where one 20% congestion cut lands
        let mut steps = 0;
        while cur < target {
            let next = qos_ramp_step(cur, target);
            assert!(next > cur, "ramp must make progress: {cur} -> {next}");
            assert!(next <= target, "ramp overshot the target: {next} > {target}");
            cur = next;
            steps += 1;
            assert!(steps < 1000, "ramp failed to converge");
        }
        assert_eq!(cur, target);

        // At or above the target: clamped, never climbing further.
        assert_eq!(qos_ramp_step(target, target), target);
        assert_eq!(qos_ramp_step(target + 5_000, target), target);

        // Very low bitrates must still make progress — an integer +10% of a
        // small value rounds to zero and would stall the ramp forever.
        assert!(qos_ramp_step(1, 10_000) > 1);
        assert!(qos_ramp_step(9, 10_000) > 9);
    }

    /// The whole point of AIMD-with-memory: recovery must NOT return to the
    /// negotiated ceiling after a failure. The previous ramp-to-ceiling policy
    /// produced a live 12-second freeze sawtooth (see QosController's docs).
    #[test]
    fn qos_holds_below_the_rate_that_failed() {
        let ceiling = 90_400u32;
        let mut qos = QosController::new();

        // Nothing has failed yet ⇒ the full ceiling is fair game.
        assert_eq!(qos.ramp_target(ceiling), ceiling);

        // The ceiling itself failed ⇒ hold at 90% of it, never back at 90400.
        qos.note_congestion(90_400);
        let target = qos.ramp_target(ceiling);
        assert_eq!(target, 81_360);
        assert!(target < ceiling, "must not ramp back to a rate known to fail");

        // A LOWER failure means the link is worse than we thought — ratchet down.
        qos.note_congestion(63_641);
        assert!(qos.ramp_target(ceiling) < target, "target must ratchet down");
        assert_eq!(qos.ramp_target(ceiling), 57_276);

        // A HIGHER failure is stale news and must not raise the target.
        let tightest = qos.ramp_target(ceiling);
        qos.note_congestion(88_000);
        assert_eq!(qos.ramp_target(ceiling), tightest);

        // Repeated failures can never drive the target below the floor.
        for _ in 0..200 {
            let t = qos.ramp_target(ceiling);
            qos.note_congestion(t);
            assert!(t >= FLOOR_KBPS.min(ceiling), "target fell through the floor: {t}");
        }

        // A zero reading (no session yet) must not poison the memory.
        let before = qos.ramp_target(ceiling);
        qos.note_congestion(0);
        assert_eq!(qos.ramp_target(ceiling), before);
    }

    /// The probe must let a recovered link climb back in BOUNDED time from any
    /// depth — the live 2026-08-08 MoCA failure drove the stream to ~3.5 Mbps,
    /// where the old fixed +5% probe needed ~80 minutes to recover. Geometric
    /// relaxation (a quarter of the gap per probe) must reach the ceiling in a
    /// handful of steps even from the floor.
    #[test]
    fn qos_probe_recovers_in_bounded_time_from_any_depth() {
        let ceiling = 90_400u32;
        for start in [70_000u32, 8_640, FLOOR_KBPS] {
            let mut qos = QosController::new();
            qos.note_congestion(start);

            let mut prev = qos.ramp_target(ceiling);
            let mut probes = 0;
            while qos.ramp_target(ceiling) < ceiling {
                qos.relax_target(ceiling);
                let now = qos.ramp_target(ceiling);
                assert!(now >= prev, "probe must not move the target backwards");
                prev = now;
                probes += 1;
                assert!(probes < 40, "recovery from {start} took {probes} probes — not bounded");
            }
            // ~13 probes max from the floor; at PROBE_INTERVAL that is well
            // under 3 minutes, versus ~80 min for the old policy.
            assert!(probes <= 20, "recovery from {start} took {probes} probes");
            assert!(qos.known_bad_kbps.is_none(), "a recovered link should forget the failure");
        }

        // Relaxing with nothing remembered is a no-op, not a panic.
        let mut qos = QosController::new();
        qos.relax_target(ceiling);
        assert_eq!(qos.ramp_target(ceiling), ceiling);
    }

    /// A new session must NOT inherit the previous session's link memory — the
    /// bug that left a fresh 4K session capped at 3.5 Mbps after a MoCA-failure
    /// session collapsed, because every reconnect negotiated the same ceiling
    /// so the ceiling-change reset never fired.
    #[test]
    fn qos_reset_clears_cross_session_memory() {
        let ceiling = 90_400u32;
        let mut qos = QosController::new();

        // Previous session collapsed to the floor.
        for _ in 0..50 {
            let t = qos.ramp_target(ceiling);
            qos.note_congestion(t);
        }
        assert!(qos.ramp_target(ceiling) < ceiling, "precondition: memory is capped");

        // A new session begins.
        qos.reset();
        assert_eq!(qos.ramp_target(ceiling), ceiling, "fresh session must start at the full ceiling");
        assert!(qos.known_bad_kbps.is_none());
    }

    /// The two layers composed: a capped session's recovery probe must converge
    /// on the CAP, never on the client's raw request. This is the bug the
    /// budget exists to prevent — the old controller climbed back to 100 Mbps
    /// on a 1080p stream after every loss episode.
    #[test]
    fn recovery_converges_on_the_capped_ceiling_not_the_request() {
        let budget = video_budget(100_000, 1920, 1080, 60, 512);
        let ceiling = budget.video_kbps;
        let mut qos = QosController::new();

        qos.note_congestion(ceiling);
        assert!(qos.ramp_target(ceiling) < ceiling);

        // Probe all the way back up.
        while qos.ramp_target(ceiling) < ceiling {
            qos.relax_target(ceiling);
        }
        assert_eq!(
            qos.ramp_target(ceiling), ceiling,
            "a fully recovered link returns to the CAP"
        );
        assert!(
            qos.ramp_target(ceiling) < budget.requested_kbps,
            "and never to the raw request"
        );
    }
}

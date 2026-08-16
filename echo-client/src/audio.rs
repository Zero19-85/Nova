//! Downstream game audio: jitter buffer and playout scheduling.
//!
//! The client half of [`nova_core::audio_channel`]. The host seals one Opus
//! packet per 20 ms frame and sends it on the punched path; this decides *when*
//! each one plays, and what to do at each of the three ways that goes wrong.
//!
//! It is the mirror of the host's `mic.rs`, and deliberately the same design
//! rather than a fresh one — that buffer has run a real conversation end to end,
//! and its three answers were each paid for.
//!
//! ## What this module does NOT do: decode
//!
//! It schedules *encoded* packets and hands them out one playout step at a time.
//! Decoding happens in the platform layer above (Android `MediaCodec`), and that
//! split is forced rather than chosen: `audiopus` builds libopus from C through
//! cmake, and it does not cross-compile for Android — verified, not assumed:
//!
//! ```text
//! $ cargo ndk -t arm64-v8a build -p echo-android
//! error: could not find native static library `opus`
//! ```
//!
//! That is the same wall the workspace manifest documents for `aws-lc-rs`, and
//! the same reason the microphone encodes in Kotlin (`MicCapture`) rather than
//! here. Keeping the *scheduling* in Rust anyway is what makes it testable on the
//! host, shared with the desktop client, and identical in behaviour to the path
//! that already works upstream.
//!
//! **One consequence is a real downgrade and is called out rather than buried:**
//! `MediaCodec` exposes no packet-loss-concealment entry point, so a gap is
//! filled with silence where the host's microphone path fills it with genuine
//! Opus PLC. [`PlayoutStep::Conceal`] is still a distinct answer from
//! [`PlayoutStep::Silence`] — the caller knows a packet was *lost* rather than
//! never sent, the counters stay honest, and the day a decoder with PLC is
//! available the fix is in the platform layer alone.

use std::collections::BTreeMap;
use std::time::Duration;

use nova_core::audio_channel::{AudioError, AudioReceiver, AudioStats};
use nova_core::media_crypto::SessionKeys;

/// How much audio to accumulate before playing any of it.
///
/// The buffer exists to absorb variation in arrival time, so it has to hold
/// something before it starts or the first jitter causes an underrun. Two
/// packets is 40 ms — enough to ride out ordinary Wi-Fi variance, small enough
/// that nobody perceives it.
pub const START_DEPTH: usize = 2;

/// Depth beyond which latency is clawed back by dropping the oldest packet.
///
/// The drift correction and the burst absorber in one. Eight packets is 160 ms —
/// past the point where added delay is worse than a 20 ms discontinuity, and far
/// enough above [`START_DEPTH`] that ordinary jitter never reaches it.
pub const MAX_DEPTH: usize = 8;

/// Playout step: one host packet per step in the steady state.
pub const STEP: Duration = Duration::from_millis(20);

/// Consecutive empty steps after which playout idles and stops asking for
/// silence.
///
/// Expressed in steps rather than a wall-clock deadline because this buffer is
/// driven by its caller's clock (the audio device), not by a timer of its own —
/// so steps are the only unit that stays true if that clock drifts. 100 steps is
/// 2 seconds, matching the host's `IDLE_AFTER`.
pub const IDLE_AFTER_STEPS: u64 = 100;

/// What the caller should render for one playout step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlayoutStep {
    /// Decode and play this packet.
    Packet(Vec<u8>),
    /// A packet was lost. Conceal one frame.
    ///
    /// Distinct from [`Silence`](Self::Silence) even though the current Android
    /// decoder renders both the same way. The distinction is what keeps
    /// `concealed` and `underran` from collapsing into one meaningless number,
    /// and it is the seam a real PLC implementation slots into later.
    Conceal,
    /// Nothing to play: the buffer is empty, or has not filled to
    /// [`START_DEPTH`] yet. Render silence.
    Silence,
    /// Nothing to play and nothing expected. The caller may stop the device
    /// until packets resume.
    Idle,
}

/// Renderer-side counters, the exact split the host's microphone path uses.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PlayoutStats {
    /// Packets handed out for decoding.
    pub rendered: u64,
    /// Gaps a packet was lost in — concealed rather than skipped.
    pub concealed: u64,
    /// Empty steps that were followed by more audio: the host was still sending
    /// and the buffer ran dry underneath it. **This is the one that means
    /// something is wrong.**
    pub underran: u64,
    /// Empty steps that ran on until playout idled: the host had gone quiet.
    /// Entirely normal, and the bulk of the count in any real session.
    pub paused: u64,
    /// Packets discarded to claw back latency. A steadily rising count is clock
    /// drift; a burst is a network stall that delivered late.
    pub dropped_late: u64,
    /// Packets held right now.
    pub depth: u64,
    /// The deepest the buffer has ever been.
    pub worst_depth: u64,
}

/// Tracks a run of empty playout steps so it can be classified once its outcome
/// is known.
///
/// The two outcomes look identical while they are happening and mean opposite
/// things once they end, which is why neither can be counted as it goes:
///
/// - Audio **resumes** → the host was sending the whole time and the buffer ran
///   dry underneath it. A fault, and the one worth alerting on.
/// - The run reaches the idle threshold → the host had simply gone quiet. Not a
///   fault, and in ordinary use the overwhelming majority of empty steps.
///
/// The host's first version counted both as one number called `starved`, and a
/// perfectly healthy two-minute conversation reported 1115 of them. Repeating
/// that here would repeat the same false alarm on the other side of the link.
#[derive(Default)]
struct SilenceRun {
    steps: u64,
}

impl SilenceRun {
    fn empty_step(&mut self) {
        self.steps += 1;
    }

    /// Audio arrived again: the run was an underrun. Returns its length.
    fn audio_resumed(&mut self) -> u64 {
        std::mem::take(&mut self.steps)
    }

    /// The run lasted until playout idled: it was a pause. Returns its length.
    fn went_idle(&mut self) -> u64 {
        std::mem::take(&mut self.steps)
    }
}

/// Opens sealed audio datagrams and schedules them for playout.
///
/// Single-threaded by construction: the receive task owns it and both
/// [`accept`](Self::accept) and [`next_step`](Self::next_step) take `&mut self`.
/// Nothing here blocks, so the caller's audio clock is never held up by it.
pub struct AudioBuffer {
    receiver: AudioReceiver,
    /// Ordered by sequence, which is what lets a reordered arrival play in its
    /// right place rather than out of turn.
    packets: BTreeMap<u32, Vec<u8>>,
    playing: bool,
    next_seq: u32,
    silence: SilenceRun,
    idle: bool,
    stats: PlayoutStats,
}

impl AudioBuffer {
    pub fn new(keys: SessionKeys) -> Self {
        Self {
            receiver: AudioReceiver::new(keys),
            packets: BTreeMap::new(),
            playing: false,
            next_seq: 0,
            silence: SilenceRun::default(),
            idle: true,
            stats: PlayoutStats::default(),
        }
    }

    /// Network-side counters: what arrived, as distinct from what was done with
    /// it. Reported alongside [`stats`](Self::stats) because either one alone
    /// misattributes a fault — silence with healthy network counters is a
    /// playout problem, and the reverse is a path problem.
    pub fn network_stats(&self) -> (AudioStats, u32) {
        (self.receiver.stats(), self.receiver.highest_sequence())
    }

    pub fn stats(&self) -> PlayoutStats {
        PlayoutStats {
            depth: self.packets.len() as u64,
            ..self.stats
        }
    }

    /// Open one datagram and buffer it.
    ///
    /// Errors are returned rather than logged: at 50 packets a second a bad path
    /// would turn a log line into an amplifier, so the caller throttles.
    pub fn accept(&mut self, datagram: &[u8]) -> Result<(), AudioError> {
        let Some(packet) = self.receiver.open(datagram)? else {
            // Authentic but not usable — a duplicate or too late. Already
            // counted by the receiver; not an error.
            return Ok(());
        };

        // A packet whose slot has already played is not useful, and inserting it
        // would make it play a second time out of turn once the map wrapped
        // around to it. The window in `AudioReceiver` catches the far-behind
        // case; this catches the one just behind the playout point.
        if self.playing && packet.seq < self.next_seq {
            self.stats.dropped_late += 1;
            return Ok(());
        }

        self.packets.insert(packet.seq, packet.payload);
        let depth = self.packets.len() as u64;
        if depth > self.stats.worst_depth {
            self.stats.worst_depth = depth;
        }

        // Drift and burst correction. Dropping the OLDEST rather than refusing
        // the newest is deliberate: the newest packet is what the host is
        // playing right now, and the backlog in front of it is pure latency.
        while self.packets.len() > MAX_DEPTH {
            let Some(&oldest) = self.packets.keys().next() else { break };
            self.packets.remove(&oldest);
            self.stats.dropped_late += 1;
            if oldest >= self.next_seq {
                self.next_seq = oldest + 1;
            }
        }
        Ok(())
    }

    /// Produce one playout step. The caller's audio device is the clock: call
    /// this once per [`STEP`] of audio it needs.
    pub fn next_step(&mut self) -> PlayoutStep {
        if !self.playing {
            if self.packets.len() < START_DEPTH {
                return self.empty_step();
            }
            // Start from the oldest packet held, not from sequence 1: the host
            // may have been sending before this client began playing, and
            // waiting for a sequence already past would stall playout forever.
            self.next_seq = *self.packets.keys().next().expect("checked non-empty");
            self.playing = true;
        }

        if let Some(payload) = self.packets.remove(&self.next_seq) {
            self.next_seq = self.next_seq.wrapping_add(1);
            self.idle = false;
            // Audio is flowing, so any empty steps just behind us were the
            // buffer running dry mid-stream rather than the host going quiet.
            self.stats.underran += self.silence.audio_resumed();
            self.stats.rendered += 1;
            return PlayoutStep::Packet(payload);
        }

        // The slot is missing. If later packets are already here it was lost in
        // flight — conceal it and move on, rather than stalling for something
        // that is never coming.
        if self.packets.keys().any(|&seq| seq > self.next_seq) {
            self.next_seq = self.next_seq.wrapping_add(1);
            self.idle = false;
            self.stats.underran += self.silence.audio_resumed();
            self.stats.concealed += 1;
            return PlayoutStep::Conceal;
        }

        self.empty_step()
    }

    /// One step with nothing to play, classified only by how the run ends.
    fn empty_step(&mut self) -> PlayoutStep {
        if self.idle {
            return PlayoutStep::Idle;
        }
        self.silence.empty_step();
        if self.silence.steps >= IDLE_AFTER_STEPS {
            self.stats.paused += self.silence.went_idle();
            self.idle = true;
            // Playing restarts from whatever arrives next: after a pause this
            // long the old sequence point is meaningless, and holding it would
            // make the first packet back look impossibly late.
            self.playing = false;
            return PlayoutStep::Idle;
        }
        PlayoutStep::Silence
    }
}

/// Shared handle between the receive task and whatever drives the audio device.
///
/// Two owners by nature: a tokio task feeds it from the socket, and the
/// platform's audio thread pulls from it on the device's clock. Those are
/// different threads with different clocks, so the buffer lives behind a mutex —
/// a plain `std::sync::Mutex`, deliberately, because the audio thread is not
/// async and must never be asked to await.
///
/// Every critical section is a `BTreeMap` insert or remove. Nothing decodes,
/// allocates a device buffer, or touches a socket while the lock is held, so the
/// audio thread cannot be stalled by the network task or vice versa.
///
/// Created before the session exists and **armed** once the host grants keys.
/// Unarmed it answers [`PlayoutStep::Idle`] and discards datagrams, so the
/// platform layer can start its audio thread whenever it likes without
/// sequencing it against the handshake.
pub struct AudioPlayout {
    inner: std::sync::Mutex<Option<AudioBuffer>>,
}

impl Default for AudioPlayout {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioPlayout {
    pub fn new() -> Self {
        Self { inner: std::sync::Mutex::new(None) }
    }

    /// Session keys arrived: start opening datagrams.
    ///
    /// Replaces any previous buffer outright rather than reusing it. A new
    /// session means a new key and a sequence space that restarts at 1, so
    /// carrying over the old playout point would make every packet of the new
    /// session read as impossibly late.
    pub fn arm(&self, keys: SessionKeys) {
        *self.lock() = Some(AudioBuffer::new(keys));
    }

    /// The session ended. Later datagrams — a straggler, or a replay — must not
    /// reach a buffer whose keys no longer describe a live session.
    pub fn disarm(&self) {
        *self.lock() = None;
    }

    pub fn is_armed(&self) -> bool {
        self.lock().is_some()
    }

    pub fn accept(&self, datagram: &[u8]) -> Result<(), AudioError> {
        match self.lock().as_mut() {
            Some(buf) => buf.accept(datagram),
            None => Ok(()),
        }
    }

    pub fn next_step(&self) -> PlayoutStep {
        match self.lock().as_mut() {
            Some(buf) => buf.next_step(),
            None => PlayoutStep::Idle,
        }
    }

    /// Playout counters, network counters, and the highest sequence seen —
    /// together, because attributing a fault needs both halves.
    pub fn stats(&self) -> Option<(PlayoutStats, AudioStats, u32)> {
        let guard = self.lock();
        let buf = guard.as_ref()?;
        let (net, highest) = buf.network_stats();
        Some((buf.stats(), net, highest))
    }

    /// Poison-proof: the mutex guards a buffer of audio, and a panic elsewhere
    /// must not take the audio thread down with it — the worst a recovered lock
    /// can cost here is one glitched frame.
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<AudioBuffer>> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nova_core::audio_channel::AudioSender;

    fn rig() -> (AudioSender, AudioBuffer) {
        let keys = SessionKeys::generate();
        (AudioSender::new(keys.clone()), AudioBuffer::new(keys))
    }

    /// Feed `n` packets, returning the datagrams so a test can withhold some.
    fn packets(tx: &mut AudioSender, n: usize) -> Vec<Vec<u8>> {
        (0..n).map(|i| tx.datagram(format!("packet {i}").as_bytes()).unwrap()).collect()
    }

    #[test]
    fn holds_until_start_depth_then_plays_in_order() {
        let (mut tx, mut buf) = rig();
        let ds = packets(&mut tx, 3);

        buf.accept(&ds[0]).unwrap();
        assert_eq!(buf.next_step(), PlayoutStep::Idle, "one packet is not enough to start");

        buf.accept(&ds[1]).unwrap();
        buf.accept(&ds[2]).unwrap();
        assert_eq!(buf.next_step(), PlayoutStep::Packet(b"packet 0".to_vec()));
        assert_eq!(buf.next_step(), PlayoutStep::Packet(b"packet 1".to_vec()));
        assert_eq!(buf.next_step(), PlayoutStep::Packet(b"packet 2".to_vec()));
        assert_eq!(buf.stats().rendered, 3);
    }

    /// Tier 1: a hole with audio behind it is concealed, not waited for.
    #[test]
    fn a_lost_packet_is_concealed_and_playout_continues() {
        let (mut tx, mut buf) = rig();
        let ds = packets(&mut tx, 4);

        buf.accept(&ds[0]).unwrap();
        // ds[1] never arrives.
        buf.accept(&ds[2]).unwrap();
        buf.accept(&ds[3]).unwrap();

        assert_eq!(buf.next_step(), PlayoutStep::Packet(b"packet 0".to_vec()));
        assert_eq!(buf.next_step(), PlayoutStep::Conceal, "the gap must not stall playout");
        assert_eq!(buf.next_step(), PlayoutStep::Packet(b"packet 2".to_vec()));
        assert_eq!(buf.stats().concealed, 1);
    }

    /// Tier 2, first half: a dry buffer that refills was an UNDERRUN — the fault
    /// case. Counted only once audio actually comes back.
    #[test]
    fn a_dry_buffer_that_refills_counts_as_underran_not_paused() {
        let (mut tx, mut buf) = rig();
        let ds = packets(&mut tx, 4);

        buf.accept(&ds[0]).unwrap();
        buf.accept(&ds[1]).unwrap();
        buf.next_step();
        buf.next_step();

        for _ in 0..5 {
            assert_eq!(buf.next_step(), PlayoutStep::Silence);
        }
        assert_eq!(buf.stats().underran, 0, "a run in progress cannot be classified yet");

        buf.accept(&ds[2]).unwrap();
        assert!(matches!(buf.next_step(), PlayoutStep::Packet(_)));
        assert_eq!(buf.stats().underran, 5, "the run is an underrun once audio returns");
        assert_eq!(buf.stats().paused, 0);
    }

    /// Tier 2, second half: the same run, left to run out, is a PAUSE. This is
    /// the split that stopped a healthy session reporting 1115 faults.
    #[test]
    fn a_dry_buffer_that_never_refills_counts_as_paused_not_underran() {
        let (mut tx, mut buf) = rig();
        let ds = packets(&mut tx, 2);
        buf.accept(&ds[0]).unwrap();
        buf.accept(&ds[1]).unwrap();
        buf.next_step();
        buf.next_step();

        for _ in 0..IDLE_AFTER_STEPS {
            buf.next_step();
        }
        assert_eq!(buf.next_step(), PlayoutStep::Idle, "playout stops asking after the threshold");
        assert_eq!(buf.stats().paused, IDLE_AFTER_STEPS);
        assert_eq!(buf.stats().underran, 0, "going quiet is not a fault");
    }

    /// Tier 3: drift correction sheds the OLDEST, because the backlog in front
    /// of the newest packet is pure latency.
    #[test]
    fn drift_correction_drops_the_oldest_not_the_newest() {
        let (mut tx, mut buf) = rig();
        let ds = packets(&mut tx, MAX_DEPTH + 3);
        for d in &ds {
            buf.accept(d).unwrap();
        }

        assert_eq!(buf.stats().depth as usize, MAX_DEPTH);
        assert_eq!(buf.stats().dropped_late, 3);

        // What survives is the TAIL: playout resumes at packet 3, not packet 0.
        assert_eq!(buf.next_step(), PlayoutStep::Packet(b"packet 3".to_vec()));
    }

    /// After a pause, playout must restart from whatever arrives next. Holding
    /// the old sequence point would make the first packet back look late and
    /// silently discard it — a session that never recovers its audio.
    #[test]
    fn playout_restarts_cleanly_after_an_idle_pause() {
        let (mut tx, mut buf) = rig();
        let ds = packets(&mut tx, 2);
        buf.accept(&ds[0]).unwrap();
        buf.accept(&ds[1]).unwrap();
        buf.next_step();
        buf.next_step();
        for _ in 0..=IDLE_AFTER_STEPS {
            buf.next_step();
        }

        // Much later packets, far ahead of where playout stopped.
        for _ in 0..40 {
            let _ = tx.datagram(b"skipped").unwrap();
        }
        let a = tx.datagram(b"back again").unwrap();
        let b = tx.datagram(b"and more").unwrap();
        buf.accept(&a).unwrap();
        buf.accept(&b).unwrap();

        assert_eq!(buf.next_step(), PlayoutStep::Packet(b"back again".to_vec()));
    }

    /// The reversal `audio_channel` implements, proven at the buffer: a packet
    /// that arrives out of order but before its slot plays must be used.
    #[test]
    fn a_reordered_packet_still_plays_in_its_right_place() {
        let (mut tx, mut buf) = rig();
        let ds = packets(&mut tx, 3);

        buf.accept(&ds[2]).unwrap();
        buf.accept(&ds[0]).unwrap();
        buf.accept(&ds[1]).unwrap();

        assert_eq!(buf.next_step(), PlayoutStep::Packet(b"packet 0".to_vec()));
        assert_eq!(buf.next_step(), PlayoutStep::Packet(b"packet 1".to_vec()));
        assert_eq!(buf.next_step(), PlayoutStep::Packet(b"packet 2".to_vec()));
        assert_eq!(buf.stats().concealed, 0, "reordering is not loss");
    }

    /// A packet arriving after its slot has already played cannot be inserted —
    /// it would play a second time, out of turn, as a stutter.
    #[test]
    fn a_packet_arriving_after_its_slot_played_is_dropped() {
        let (mut tx, mut buf) = rig();
        let ds = packets(&mut tx, 4);
        buf.accept(&ds[0]).unwrap();
        buf.accept(&ds[2]).unwrap();
        buf.next_step(); // plays 0
        buf.next_step(); // conceals the missing 1

        buf.accept(&ds[1]).unwrap(); // too late now
        assert_eq!(buf.next_step(), PlayoutStep::Packet(b"packet 2".to_vec()));
        assert!(buf.stats().dropped_late >= 1);
    }

    #[test]
    fn a_foreign_key_is_refused_and_never_reaches_playout() {
        let (_tx, mut buf) = rig();
        let mut stranger = AudioSender::new(SessionKeys::generate());
        let forged = stranger.datagram(b"not yours").unwrap();

        assert_eq!(buf.accept(&forged), Err(AudioError::Authentication));
        assert_eq!(buf.stats().depth, 0);
        assert_eq!(buf.next_step(), PlayoutStep::Idle);
    }

    /// Network and playout counters must stay separately attributable — that is
    /// the whole reason both are reported.
    /// An unarmed handle must be safe to poll and safe to feed — the platform's
    /// audio thread starts before the handshake finishes, every session.
    #[test]
    fn an_unarmed_playout_is_inert_rather_than_a_panic() {
        let playout = AudioPlayout::new();
        assert!(!playout.is_armed());
        assert_eq!(playout.next_step(), PlayoutStep::Idle);
        assert_eq!(playout.accept(b"anything at all"), Ok(()));
        assert!(playout.stats().is_none());
    }

    /// Re-arming must not carry the old sequence point across: a new session
    /// restarts at 1, and a stale playout point would read every packet of it as
    /// late and play none of them.
    #[test]
    fn rearming_resets_the_sequence_point() {
        let playout = AudioPlayout::new();

        let first = SessionKeys::generate();
        playout.arm(first.clone());
        let mut tx = AudioSender::new(first);
        for _ in 0..40 {
            let d = tx.datagram(b"old session").unwrap();
            playout.accept(&d).unwrap();
        }
        playout.next_step();

        let second = SessionKeys::generate();
        playout.arm(second.clone());
        let mut tx2 = AudioSender::new(second);
        for i in 0..START_DEPTH {
            let d = tx2.datagram(format!("new {i}").as_bytes()).unwrap();
            playout.accept(&d).unwrap();
        }
        assert_eq!(playout.next_step(), PlayoutStep::Packet(b"new 0".to_vec()));
    }

    #[test]
    fn disarming_stops_stragglers_from_the_old_session() {
        let playout = AudioPlayout::new();
        let keys = SessionKeys::generate();
        playout.arm(keys.clone());
        let mut tx = AudioSender::new(keys);

        playout.disarm();
        let straggler = tx.datagram(b"after the end").unwrap();
        assert_eq!(playout.accept(&straggler), Ok(()));
        assert_eq!(playout.next_step(), PlayoutStep::Idle);
    }

    #[test]
    fn network_and_playout_counters_are_reported_apart() {
        let (mut tx, mut buf) = rig();
        let ds = packets(&mut tx, 3);
        buf.accept(&ds[0]).unwrap();
        buf.accept(&ds[2]).unwrap();

        let (net, highest) = buf.network_stats();
        assert_eq!(net.accepted, 2);
        assert_eq!(net.lost(highest), 1, "the network dropped one");
        assert_eq!(buf.stats().concealed, 0, "playout has not run yet — nothing concealed");
    }
}

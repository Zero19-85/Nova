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
/// Four packets is 80 ms. This was 2 (40 ms), inherited from the host's
/// microphone buffer, and **measurement showed the window was too narrow at both
/// ends**:
///
/// ```text
/// arrived 500/500, played 501/500 | 3 silence, 0 concealed, 3 drift-dropped, depth 5
/// arrived 506/500, played 506/500 | 2 silence, 0 concealed, 2 drift-dropped, depth 7
/// arrived 502/500, played 502/500 | 6 silence, 0 concealed, 6 drift-dropped, depth 6
/// ```
///
/// Arrival and playout rates match exactly and nothing is lost, yet the buffer
/// manages to run *dry* and *overflow* in the same ten seconds — and `silence`
/// equals `drift-dropped` on every line. That pairing has one explanation:
/// packets arrive in bursts rather than evenly, as Wi-Fi aggregation and
/// power-save deliver them, so depth swings further than the window spans and
/// clips at both ends. Each cycle cost one pop going empty and one going full.
///
/// The microphone's 40 ms was tuned for the opposite direction, where a phone's
/// own uplink paces the packets. Nothing about it transferred.
pub const START_DEPTH: usize = 4;

/// Depth beyond which latency is clawed back by dropping the oldest packet.
///
/// Fourteen packets is 280 ms. Sized as the burst amplitude the measurement
/// above implies, plus headroom — not as a latency target, because the buffer
/// only reaches this depth when a burst puts it there, and sits near
/// [`START_DEPTH`] the rest of the time.
///
/// It is deliberately not larger. This is the one bound on how far behind live
/// audio can drift, and every packet above [`START_DEPTH`] is latency the
/// listener pays for.
pub const MAX_DEPTH: usize = 14;

/// The depth playout tries to sit at once it is running.
///
/// [`START_DEPTH`] is where playout *begins*; this is where it *stays*. They are
/// the same number, but they answer different questions and a later retune may
/// want them apart.
pub const TARGET_DEPTH: usize = START_DEPTH;

/// Largest packet, in bytes, that counts as a quiet moment.
///
/// The host encodes with `Application::LowDelay` and never disables VBR, so
/// libopus spends bits in proportion to what the audio is doing: a 20 ms frame
/// of stereo at 128 kbps averages around 320 bytes, and near-silence collapses
/// to a few dozen. Packet size is therefore a free, decoder-free proxy for
/// loudness — which is what makes [`AudioBuffer::next_step`]'s latency shedding
/// inaudible rather than merely infrequent.
///
/// Deliberately well under the average: this must never fire on ordinary
/// content, only on a genuine lull.
const QUIET_PACKET_BYTES: usize = 120;

/// Shed at most one packet per this many steps: 500 ms.
///
/// Gradual on purpose. Shedding a whole backlog at once would be a large jump
/// forward in the audio; spread out, each 20 ms of skipped quiet is inaudible
/// and a full buffer returns to target in a few seconds.
const SHED_INTERVAL_STEPS: u64 = 25;

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
    /// Quiet packets skipped to walk the buffer back down to [`TARGET_DEPTH`].
    ///
    /// Counted apart from [`Self::dropped_late`] because they are the opposite
    /// kind of event: a drift drop is a hard 20 ms excision wherever it lands,
    /// while these are chosen for being inaudible. A healthy session sheds a few
    /// after each network hiccup and none the rest of the time.
    pub shed: u64,
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
    /// Steps since the last quiet packet was shed, rate-limiting the walk back
    /// down to [`TARGET_DEPTH`].
    steps_since_shed: u64,
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
            steps_since_shed: 0,
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
            // Start near the NEWEST packet held, keeping exactly START_DEPTH
            // behind it, and discard the rest.
            //
            // Not from sequence 1 — the host may have been sending before this
            // client began playing, and waiting for a sequence already past
            // would stall playout forever. But not from the oldest held packet
            // either, which is what this used to do and which is audible at the
            // start of every song:
            //
            // The host's loopback capture goes quiet between tracks, so a track
            // beginning delivers a burst. Starting at the oldest packet of that
            // burst begins playout already far behind live, and the MAX_DEPTH
            // correction then spends the next several seconds hard-dropping
            // packets to claw the latency back — one 20 ms excision at a time,
            // which is the "garbled until it settles" sound.
            //
            // Discarding the backlog here costs nothing audible, because none of
            // it has been played yet. It is the same latency, shed in one silent
            // step instead of dozens of loud ones.
            let newest = *self.packets.keys().next_back().expect("checked non-empty");
            let start = newest.saturating_sub(START_DEPTH as u32 - 1);
            let stale = self.packets.len();
            self.packets.retain(|&seq, _| seq >= start);
            self.stats.dropped_late += (stale - self.packets.len()) as u64;
            self.next_seq = start;
            self.playing = true;
        }

        // Latency shedding — the buffer's way back DOWN to target.
        //
        // Without this the depth only ever ratchets up. Arrival and playout
        // rates are identical in the steady state, so whatever depth a burst or
        // a loss event leaves behind is kept forever: a live run sat at 13 of a
        // maximum 14 for eighty seconds, which is 260 ms of latency bought by
        // one Wi-Fi hiccup a minute earlier and never given back. MAX_DEPTH is
        // no help — it is a ceiling, not a spring.
        //
        // So when we are above target, and the packet due right now is quiet
        // enough to be a lull, it is skipped and the next one plays in its
        // place. That is a true splice: no hole is left behind, so nothing is
        // concealed and the listener hears 20 ms less of a silence they were
        // never attending to. Rate-limited so a deep buffer walks back to target
        // over a few seconds rather than jumping.
        self.steps_since_shed += 1;
        if self.packets.len() > TARGET_DEPTH && self.steps_since_shed >= SHED_INTERVAL_STEPS {
            let quiet = self
                .packets
                .get(&self.next_seq)
                .is_some_and(|p| p.len() <= QUIET_PACKET_BYTES);
            if quiet {
                self.packets.remove(&self.next_seq);
                self.next_seq = self.next_seq.wrapping_add(1);
                self.stats.shed += 1;
                self.steps_since_shed = 0;
            }
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

    /// [`stats`](Self::stats) with zeroes when unarmed, so a caller rendering a
    /// stats view never has to special-case "no session yet".
    pub fn stats_or_zero(&self) -> (PlayoutStats, AudioStats, u32) {
        self.stats().unwrap_or_default()
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

    /// Get playout running, so a test about steady-state behaviour does not have
    /// to restate the start condition.
    ///
    /// Expressed in terms of [`START_DEPTH`] rather than a literal, because
    /// these tests once hardcoded it and every one of them broke the day it was
    /// retuned from measurement — which is noise, not signal.
    fn primed() -> (AudioSender, AudioBuffer, u32) {
        let (mut tx, mut buf) = rig();
        for d in packets(&mut tx, START_DEPTH) {
            buf.accept(&d).unwrap();
        }
        for _ in 0..START_DEPTH {
            assert!(matches!(buf.next_step(), PlayoutStep::Packet(_)));
        }
        (tx, buf, START_DEPTH as u32)
    }

    #[test]
    fn holds_until_start_depth_then_plays_in_order() {
        let (mut tx, mut buf) = rig();
        let ds = packets(&mut tx, START_DEPTH);

        buf.accept(&ds[0]).unwrap();
        assert_eq!(buf.next_step(), PlayoutStep::Idle, "one packet is not enough to start");

        for d in ds.iter().skip(1) {
            buf.accept(d).unwrap();
        }
        for i in 0..START_DEPTH {
            assert_eq!(
                buf.next_step(),
                PlayoutStep::Packet(format!("packet {i}").into_bytes()),
                "in order, from the first packet when there is no backlog"
            );
        }
        assert_eq!(buf.stats().rendered as usize, START_DEPTH);
    }

    /// Tier 1: a hole with audio behind it is concealed, not waited for.
    #[test]
    fn a_lost_packet_is_concealed_and_playout_continues() {
        let (mut tx, mut buf, _) = primed();
        let _lost_in_flight = tx.datagram(b"never arrives").unwrap();
        let after = tx.datagram(b"arrives").unwrap();
        buf.accept(&after).unwrap();

        assert_eq!(buf.next_step(), PlayoutStep::Conceal, "the gap must not stall playout");
        assert_eq!(buf.next_step(), PlayoutStep::Packet(b"arrives".to_vec()));
        assert_eq!(buf.stats().concealed, 1);
    }

    /// Tier 2, first half: a dry buffer that refills was an UNDERRUN — the fault
    /// case. Counted only once audio actually comes back.
    #[test]
    fn a_dry_buffer_that_refills_counts_as_underran_not_paused() {
        let (mut tx, mut buf, _) = primed();

        for _ in 0..5 {
            assert_eq!(buf.next_step(), PlayoutStep::Silence);
        }
        assert_eq!(buf.stats().underran, 0, "a run in progress cannot be classified yet");

        let back = tx.datagram(b"back again").unwrap();
        buf.accept(&back).unwrap();
        assert!(matches!(buf.next_step(), PlayoutStep::Packet(_)));
        assert_eq!(buf.stats().underran, 5, "the run is an underrun once audio returns");
        assert_eq!(buf.stats().paused, 0);
    }

    /// Tier 2, second half: the same run, left to run out, is a PAUSE. This is
    /// the split that stopped a healthy session reporting 1115 faults.
    #[test]
    fn a_dry_buffer_that_never_refills_counts_as_paused_not_underran() {
        let (_tx, mut buf, _) = primed();

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
        assert_eq!(buf.stats().dropped_late, 3, "the three OLDEST went, not the newest");

        // And what survives is the tail — the newest packet is still there to be
        // reached, which is the property that makes dropping the oldest correct.
        assert_eq!(
            buf.next_step(),
            PlayoutStep::Packet(format!("packet {}", MAX_DEPTH + 3 - START_DEPTH).into_bytes()),
        );
    }

    /// After a pause, playout must restart from whatever arrives next. Holding
    /// the old sequence point would make the first packet back look late and
    /// silently discard it — a session that never recovers its audio.
    #[test]
    fn playout_restarts_cleanly_after_an_idle_pause() {
        let (mut tx, mut buf, _) = primed();
        for _ in 0..=IDLE_AFTER_STEPS {
            buf.next_step();
        }

        // Much later packets, far ahead of where playout stopped.
        for _ in 0..40 {
            let _ = tx.datagram(b"skipped").unwrap();
        }
        let resumed: Vec<Vec<u8>> = (0..START_DEPTH)
            .map(|i| tx.datagram(format!("back {i}").as_bytes()).unwrap())
            .collect();
        for d in &resumed {
            buf.accept(d).unwrap();
        }

        assert_eq!(buf.next_step(), PlayoutStep::Packet(b"back 0".to_vec()));
    }

    /// The garbled song start, as a test.
    ///
    /// A track beginning delivers a burst, because the host's loopback capture
    /// was quiet between tracks. Playout must begin near the NEWEST packet — it
    /// used to begin at the oldest, which starts already far behind live and
    /// leaves MAX_DEPTH to claw the latency back one audible 20 ms excision at a
    /// time.
    #[test]
    fn a_burst_at_the_start_of_a_track_plays_from_the_newest_not_the_oldest() {
        let (mut tx, mut buf) = rig();
        // Forty packets — 800 ms — arriving at once.
        let burst = packets(&mut tx, 40);
        for d in &burst {
            buf.accept(d).unwrap();
        }

        // The first thing heard is near the END of the burst, not its start.
        let first = buf.next_step();
        assert_eq!(
            first,
            PlayoutStep::Packet(format!("packet {}", 40 - START_DEPTH).into_bytes()),
            "playout must start within START_DEPTH of live"
        );

        // And what is left is exactly the start depth, so nothing has to be
        // clawed back afterwards — the whole point.
        assert_eq!(buf.stats().depth as usize, START_DEPTH - 1);

        // The rest of the burst plays out cleanly, with no drops after the start.
        let dropped_at_start = buf.stats().dropped_late;
        for _ in 0..START_DEPTH - 1 {
            assert!(matches!(buf.next_step(), PlayoutStep::Packet(_)));
        }
        assert_eq!(
            buf.stats().dropped_late,
            dropped_at_start,
            "the backlog is shed once, silently, not repeatedly during playout"
        );
    }

    /// The steady-state failure the measurement caught: bursty arrival against a
    /// window too narrow to hold it, clipping at BOTH ends. With the widened
    /// window the same pattern must produce neither.
    #[test]
    fn a_bursty_arrival_pattern_neither_starves_nor_overflows() {
        let (mut tx, mut buf) = rig();

        // Prime, then run bursts of 6 against a steady 6-step drain — the shape
        // the log showed, where arrival and playout rates match exactly but the
        // arrivals clump.
        for _ in 0..START_DEPTH {
            let d = tx.datagram(b"prime").unwrap();
            buf.accept(&d).unwrap();
        }
        for _ in 0..START_DEPTH {
            buf.next_step();
        }

        // Loud payloads, so latency shedding stays out of this test: it is about
        // whether the WINDOW fits a burst, not about what the buffer does when
        // it is deep and the music happens to be quiet.
        let loud = vec![b'x'; QUIET_PACKET_BYTES + 1];
        for _ in 0..30 {
            for _ in 0..6 {
                let d = tx.datagram(&loud).unwrap();
                buf.accept(&d).unwrap();
            }
            for _ in 0..6 {
                assert!(
                    matches!(buf.next_step(), PlayoutStep::Packet(_)),
                    "a burst that fits the window must never yield silence"
                );
            }
        }

        assert_eq!(buf.stats().dropped_late, 0, "and must never overflow it either");
        assert_eq!(buf.stats().concealed, 0);
    }

    /// A buffer left deep by a hiccup must walk back down to target on its own.
    ///
    /// The live failure: one Wi-Fi loss burst left depth at 13 of a maximum 14,
    /// and it stayed there for eighty seconds — 260 ms of latency bought once
    /// and never given back, because arrival and playout rates are identical and
    /// MAX_DEPTH is a ceiling rather than a spring.
    #[test]
    fn a_deep_buffer_walks_back_down_to_target_during_quiet() {
        let (mut tx, mut buf, _) = primed();
        let quiet = vec![0u8; QUIET_PACKET_BYTES];

        // A hiccup mid-playback: a stalled path delivers its backlog at once.
        // Depth can only get deep this way — a buffer that starts fresh trims to
        // START_DEPTH, so the deep state is always something that happens later.
        for _ in 0..MAX_DEPTH {
            let d = tx.datagram(&quiet).unwrap();
            buf.accept(&d).unwrap();
        }
        assert!(buf.stats().depth as usize > TARGET_DEPTH, "the hiccup left it deep");

        // Play on, one arrival per step, exactly as the steady state does.
        for _ in 0..SHED_INTERVAL_STEPS * (MAX_DEPTH as u64) {
            let d = tx.datagram(&quiet).unwrap();
            buf.accept(&d).unwrap();
            buf.next_step();
        }

        assert!(
            buf.stats().depth as usize <= TARGET_DEPTH + 1,
            "depth {} never came back to target {TARGET_DEPTH}",
            buf.stats().depth,
        );
        assert!(buf.stats().shed > 0);
        assert_eq!(buf.stats().concealed, 0, "shedding must leave no hole to conceal");
    }

    /// And the property that keeps it inaudible: it must NOT fire on real
    /// content, however deep the buffer gets. A loud packet is never skipped.
    #[test]
    fn latency_shedding_never_touches_loud_audio() {
        let (mut tx, mut buf, _) = primed();
        let loud = vec![0u8; QUIET_PACKET_BYTES + 1];

        for _ in 0..MAX_DEPTH {
            let d = tx.datagram(&loud).unwrap();
            buf.accept(&d).unwrap();
        }
        assert!(buf.stats().depth as usize > TARGET_DEPTH);

        for _ in 0..SHED_INTERVAL_STEPS * 4 {
            let d = tx.datagram(&loud).unwrap();
            buf.accept(&d).unwrap();
            buf.next_step();
        }

        assert_eq!(
            buf.stats().shed,
            0,
            "latency must be paid rather than stolen from audible content"
        );
    }

    /// The reversal `audio_channel` implements, proven at the buffer: a packet
    /// that arrives out of order but before its slot plays must be used.
    #[test]
    fn a_reordered_packet_still_plays_in_its_right_place() {
        let (mut tx, mut buf) = rig();
        let ds = packets(&mut tx, START_DEPTH);

        // Newest first, then the rest — the shape a reordered path delivers.
        buf.accept(&ds[START_DEPTH - 1]).unwrap();
        for d in &ds[..START_DEPTH - 1] {
            buf.accept(d).unwrap();
        }

        for i in 0..START_DEPTH {
            assert_eq!(
                buf.next_step(),
                PlayoutStep::Packet(format!("packet {i}").into_bytes()),
                "arrival order must not become playout order"
            );
        }
        assert_eq!(buf.stats().concealed, 0, "reordering is not loss");
    }

    /// A packet arriving after its slot has already played cannot be inserted —
    /// it would play a second time, out of turn, as a stutter.
    #[test]
    fn a_packet_arriving_after_its_slot_played_is_dropped() {
        let (mut tx, mut buf, _) = primed();
        let late = tx.datagram(b"late").unwrap();
        let after = tx.datagram(b"after").unwrap();

        buf.accept(&after).unwrap();
        assert_eq!(buf.next_step(), PlayoutStep::Conceal, "late's slot passes unfilled");
        assert_eq!(buf.next_step(), PlayoutStep::Packet(b"after".to_vec()));

        let before = buf.stats().dropped_late;
        buf.accept(&late).unwrap(); // its slot is gone
        assert_eq!(buf.stats().dropped_late, before + 1);
        assert_eq!(buf.stats().depth, 0, "it must not be queued to play out of turn");
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

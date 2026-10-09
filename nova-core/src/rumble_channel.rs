//! Sealed host-to-client controller rumble.
//!
//! ## Why a datagram channel, and not the control tunnel
//!
//! The control tunnel is the obvious home for "the host has something to tell
//! the client", and it cannot carry this. It is strictly request/response and
//! client-driven: Echo reads the tunnel only while it is waiting on the reply
//! to a call it made (`echo-client`'s `ControlChannel::call`). A line the host
//! wrote unprompted would sit in the TLS buffer until the client next happened
//! to ask for something — on a quiet session, the next RTT probe, seconds away.
//! A rumble that lands seconds after the explosion is worse than none.
//!
//! It is also the wrong guarantee. [`crate::rudp`] is reliable *and ordered*,
//! so one lost datagram stalls everything behind it for a retransmit, and the
//! input channel already learned what that does to a real-time stream. Rumble
//! wants neither property, because its payload is not an event:
//!
//! ## Every datagram is the whole state
//!
//! A datagram carries the absolute motor speeds of **every** pad, never a
//! change. So a datagram that arrives supersedes everything older, a lost one
//! is repaired by any later one, and a reordered old one is simply ignored —
//! the receiver keeps the highest counter it has applied and drops anything at
//! or below it, which is also the replay defence.
//!
//! What absolute state cannot fix on its own is a lost *last* datagram: if
//! "motors off" is the final thing the host says and it is lost, nothing later
//! repairs it and the controller shakes forever. Two mechanisms close that,
//! and they deliberately sit on opposite ends:
//!
//! - **The host repeats.** Every change goes out at once and again at
//!   [`REPEAT_AFTER`], so a stop survives a short burst of loss; and while any
//!   motor is running the state is re-sent every [`REFRESH_INTERVAL`].
//! - **The client times out.** A running motor that has not been re-confirmed
//!   within [`WATCHDOG`] is stopped. That covers what no amount of host-side
//!   repetition can: a host that crashed, a network that went away, a session
//!   that ended mid-rumble. The watchdog is several refresh intervals long so
//!   ordinary loss never trips it.
//!
//! ## Authorization is the key, not the address
//!
//! Sealed with the session's [`SessionKeys`] under [`STREAM_RUMBLE`], like every
//! other Echo side channel. A forged datagram cannot drive a motor, and a
//! captured audio datagram cannot be replayed as one.
//!
//! ## Wire format
//!
//! ```text
//!   [0xE7][flags u8][counter u32 BE][sealed …]
//!
//!   sealed = AES-128-GCM(STREAM_RUMBLE, counter) over:
//!     ([low u16 BE][high u16 BE]) × PADS
//! ```
//!
//! `low` is the low-frequency (large, left) motor and `high` the high-frequency
//! (small, right) one — XInput's and GameStream's order, and the order
//! `Windows.Gaming.Input.GamepadVibration` names them `LeftMotor`/`RightMotor`.
//! Both are full-range `u16`; ViGEm reports 8-bit speeds, which the host widens
//! so that full speed is `0xFFFF`, not `0xFF00`.

use std::time::{Duration, Instant};

use crate::demux::ECHO_RUMBLE;
use crate::media_crypto::{CryptoError, SessionKeys, CRYPTO_OVERHEAD, STREAM_RUMBLE};

/// Pads described by every datagram — GameStream's controller limit, and
/// `MAX_PADS` in the host's `input.rs`.
pub const PADS: usize = 4;

/// Bytes before the sealed payload: tag, flags, counter.
pub const HEADER_LEN: usize = 6;

/// Plaintext length: two `u16` speeds per pad.
const PLAINTEXT_LEN: usize = PADS * 4;

/// Total datagram size. Fixed, so a receiver can reject a wrong-sized datagram
/// before spending a decryption on it.
pub const DATAGRAM_LEN: usize = HEADER_LEN + PLAINTEXT_LEN + CRYPTO_OVERHEAD;

/// The AAD's frame-type slot. Rumble has no frame types; a fixed value keeps the
/// tag committing to "this is rumble".
const PURPOSE: u8 = 0;

/// When a change is sent again, measured from the change. The first send is
/// immediate; these are the repeats that let a stop survive a loss burst.
///
/// Short, because a repeat is only useful while the original might still have
/// been lost: by 100 ms a running effect has been refreshed anyway, and a stop
/// that has survived three sends has survived anything Wi-Fi does routinely.
pub const REPEAT_AFTER: [Duration; 2] = [Duration::from_millis(30), Duration::from_millis(100)];

/// While any motor is running, the full state is re-sent this often. This is
/// the heartbeat the client's [`WATCHDOG`] listens for.
pub const REFRESH_INTERVAL: Duration = Duration::from_millis(250);

/// A running motor not re-confirmed within this long is stopped by the client.
///
/// Four refresh intervals, so it takes three consecutive lost refreshes to trip
/// it on a healthy session — and when it does trip on a dead one, the
/// controller stops within about a second rather than shaking until the app is
/// closed.
pub const WATCHDOG: Duration = Duration::from_millis(1000);

/// One pad's two motors.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Motors {
    /// Low-frequency, large, left motor.
    pub low: u16,
    /// High-frequency, small, right motor.
    pub high: u16,
}

impl Motors {
    pub const OFF: Motors = Motors { low: 0, high: 0 };

    pub fn is_off(&self) -> bool {
        self.low == 0 && self.high == 0
    }
}

/// Every pad's motors. Index is the GameStream controller number.
pub type State = [Motors; PADS];

fn any_running(state: &State) -> bool {
    state.iter().any(|m| !m.is_off())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RumbleError {
    /// Not a rumble datagram, or not exactly [`DATAGRAM_LEN`] bytes.
    Malformed,
    /// The tag did not verify: forged, corrupted, or sealed under another key.
    Authentication,
    /// The 32-bit counter space is exhausted. Reusing a counter would repeat a
    /// GCM nonce, so the sender stops instead. Unreachable in practice: at a
    /// sustained 100 datagrams a second this is over a year of one session.
    Exhausted,
}

impl std::fmt::Display for RumbleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed => write!(f, "not a well-formed rumble datagram"),
            Self::Authentication => write!(
                f,
                "rumble datagram failed authentication — forged, corrupted, or not ours"
            ),
            Self::Exhausted => write!(f, "rumble counter space exhausted — session must restart"),
        }
    }
}

impl std::error::Error for RumbleError {}

// ── The wire ────────────────────────────────────────────────────────────────

/// Seals whole-state datagrams. Host side.
#[derive(Debug)]
pub struct RumbleSender {
    keys: SessionKeys,
    counter: u32,
}

impl RumbleSender {
    pub fn new(keys: SessionKeys) -> Self {
        // From 1, so the receiver's zero means "nothing applied yet" with no
        // separate sentinel — the convention every Echo channel follows.
        Self { keys, counter: 1 }
    }

    pub fn seal(&mut self, state: &State) -> Result<Vec<u8>, RumbleError> {
        if self.counter == u32::MAX {
            return Err(RumbleError::Exhausted);
        }
        let counter = self.counter;
        self.counter += 1;

        let mut plaintext = [0u8; PLAINTEXT_LEN];
        for (pad, motors) in state.iter().enumerate() {
            plaintext[pad * 4..pad * 4 + 2].copy_from_slice(&motors.low.to_be_bytes());
            plaintext[pad * 4 + 2..pad * 4 + 4].copy_from_slice(&motors.high.to_be_bytes());
        }

        let sealed = self.keys.seal(STREAM_RUMBLE, counter, PURPOSE, &plaintext);
        let mut out = Vec::with_capacity(DATAGRAM_LEN);
        out.push(ECHO_RUMBLE);
        out.push(0); // flags, reserved
        out.extend_from_slice(&counter.to_be_bytes());
        out.extend_from_slice(&sealed);
        Ok(out)
    }
}

/// Opens whole-state datagrams, keeping only the newest. Client side.
#[derive(Debug)]
pub struct RumbleReceiver {
    keys: SessionKeys,
    /// Highest counter applied. Zero until the first datagram.
    newest: u32,
}

impl RumbleReceiver {
    pub fn new(keys: SessionKeys) -> Self {
        Self { keys, newest: 0 }
    }

    /// Open one datagram. `Ok(Some(state))` when it is newer than anything
    /// applied so far; `Ok(None)` for a repeat, a reordered old one, or a
    /// replay — all of which describe a state that has been superseded.
    pub fn open(&mut self, datagram: &[u8]) -> Result<Option<State>, RumbleError> {
        if datagram.len() != DATAGRAM_LEN || datagram.first() != Some(&ECHO_RUMBLE) {
            return Err(RumbleError::Malformed);
        }
        let counter = u32::from_be_bytes([datagram[2], datagram[3], datagram[4], datagram[5]]);

        let plaintext = match self.keys.open(STREAM_RUMBLE, counter, PURPOSE, &datagram[HEADER_LEN..])
        {
            Ok(p) => p,
            Err(CryptoError::Authentication) => return Err(RumbleError::Authentication),
            Err(_) => return Err(RumbleError::Malformed),
        };
        if plaintext.len() != PLAINTEXT_LEN {
            return Err(RumbleError::Malformed);
        }
        // Authenticated before this comparison, so an attacker cannot use a
        // forged high counter to make the receiver ignore the real stream.
        if counter <= self.newest {
            return Ok(None);
        }
        self.newest = counter;

        let mut state = [Motors::OFF; PADS];
        for (pad, motors) in state.iter_mut().enumerate() {
            let at = pad * 4;
            motors.low = u16::from_be_bytes([plaintext[at], plaintext[at + 1]]);
            motors.high = u16::from_be_bytes([plaintext[at + 2], plaintext[at + 3]]);
        }
        Ok(Some(state))
    }
}

// ── The host's schedule ─────────────────────────────────────────────────────

/// What the host owes the client: the current state, and when to say it next.
///
/// Pure scheduling over an injected clock, so the repeat/refresh policy is
/// testable without a socket or a sleep. The caller polls it from a timer it
/// already has and sends whatever comes back.
#[derive(Debug)]
pub struct RumbleTx {
    sender: RumbleSender,
    state: State,
    /// When the state last changed — the anchor [`REPEAT_AFTER`] counts from.
    changed_at: Instant,
    /// How many repeats of the current change have gone out.
    repeats_sent: usize,
    /// When the next datagram is owed, or `None` when nothing is owed: the
    /// motors are off and every repeat of the stop has been sent.
    due: Option<Instant>,
}

impl RumbleTx {
    pub fn new(keys: SessionKeys, now: Instant) -> Self {
        Self {
            sender: RumbleSender::new(keys),
            state: [Motors::OFF; PADS],
            changed_at: now,
            repeats_sent: REPEAT_AFTER.len(),
            due: None,
        }
    }

    /// Record one pad's motors. Returns whether anything changed.
    ///
    /// A change is owed immediately. An unchanged value is ignored outright —
    /// many games call `XInputSetState` every frame with the same speeds, and
    /// re-anchoring the repeat burst on each of those would turn a quiet
    /// steady effect into a datagram per frame.
    pub fn set(&mut self, pad: usize, motors: Motors, now: Instant) -> bool {
        let Some(slot) = self.state.get_mut(pad) else {
            return false;
        };
        if *slot == motors {
            return false;
        }
        *slot = motors;
        self.changed_at = now;
        self.repeats_sent = 0;
        self.due = Some(now);
        true
    }

    /// Stop every motor. What a session that ends while a game is rumbling must
    /// say before it goes — the client's watchdog would stop it within a second
    /// anyway, but a second of an unexplained shaking controller is a bug report.
    pub fn stop_all(&mut self, now: Instant) -> bool {
        let mut changed = false;
        for pad in 0..PADS {
            changed |= self.set(pad, Motors::OFF, now);
        }
        changed
    }

    pub fn state(&self) -> State {
        self.state
    }

    /// When the next datagram is owed, for a caller that wants to sleep exactly
    /// that long rather than poll.
    pub fn next_due(&self) -> Option<Instant> {
        self.due
    }

    /// The datagram owed at `now`, if one is.
    pub fn poll(&mut self, now: Instant) -> Result<Option<Vec<u8>>, RumbleError> {
        match self.due {
            Some(due) if due <= now => {}
            _ => return Ok(None),
        }
        let datagram = self.sender.seal(&self.state)?;

        // Schedule the next one. Repeats of the latest change come first and
        // are anchored to the CHANGE, not to this send, so a late poll cannot
        // stretch the burst; after them, a running motor refreshes and a
        // stopped one goes quiet.
        self.due = if let Some(after) = REPEAT_AFTER.get(self.repeats_sent) {
            self.repeats_sent += 1;
            Some((self.changed_at + *after).max(now))
        } else if any_running(&self.state) {
            Some(now + REFRESH_INTERVAL)
        } else {
            None
        };
        Ok(Some(datagram))
    }
}

// ── The client's view ───────────────────────────────────────────────────────

/// The motor state a client should be applying, with the watchdog built in.
#[derive(Debug)]
pub struct RumbleRx {
    receiver: RumbleReceiver,
    state: State,
    /// When the last fresh datagram arrived.
    heard_at: Option<Instant>,
}

impl RumbleRx {
    pub fn new(keys: SessionKeys) -> Self {
        Self { receiver: RumbleReceiver::new(keys), state: [Motors::OFF; PADS], heard_at: None }
    }

    /// Accept one datagram. Returns whether it changed the state.
    pub fn accept(&mut self, datagram: &[u8], now: Instant) -> Result<bool, RumbleError> {
        match self.receiver.open(datagram)? {
            Some(state) => {
                // Heard from, even when the state is unchanged: a refresh of a
                // running effect is exactly what keeps the watchdog quiet.
                self.heard_at = Some(now);
                let changed = state != self.state;
                self.state = state;
                Ok(changed)
            }
            None => Ok(false),
        }
    }

    /// What to drive the motors at, right now.
    ///
    /// All off once a running motor has gone unconfirmed past [`WATCHDOG`]. The
    /// stored state is left alone, so a refresh arriving late — a long Wi-Fi
    /// stall rather than a dead host — resumes the effect it describes.
    pub fn state(&self, now: Instant) -> State {
        match self.heard_at {
            Some(at) if now.saturating_duration_since(at) <= WATCHDOG => self.state,
            _ => [Motors::OFF; PADS],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demux::{classify, Class};

    fn keys() -> SessionKeys {
        SessionKeys::generate()
    }

    fn rumble(low: u16, high: u16) -> Motors {
        Motors { low, high }
    }

    #[test]
    fn a_state_round_trips_and_classifies_as_rumble() {
        let k = keys();
        let mut tx = RumbleSender::new(k.clone());
        let mut rx = RumbleReceiver::new(k);

        let mut state = [Motors::OFF; PADS];
        state[0] = rumble(0xFFFF, 0x1234);
        state[3] = rumble(1, 0);
        let dg = tx.seal(&state).unwrap();

        assert_eq!(dg.len(), DATAGRAM_LEN, "size is fixed so it can be pre-checked");
        assert_eq!(classify(&dg), Class::EchoRumble);
        assert_eq!(rx.open(&dg), Ok(Some(state)));
    }

    /// The property absolute state buys: an older datagram arriving late must
    /// not roll the motors back to what they were before.
    #[test]
    fn a_reordered_or_replayed_datagram_changes_nothing() {
        let k = keys();
        let mut tx = RumbleSender::new(k.clone());
        let mut rx = RumbleReceiver::new(k);

        let mut on = [Motors::OFF; PADS];
        on[0] = rumble(500, 500);
        let first = tx.seal(&on).unwrap();
        let second = tx.seal(&[Motors::OFF; PADS]).unwrap();

        assert_eq!(rx.open(&second), Ok(Some([Motors::OFF; PADS])));
        assert_eq!(rx.open(&first), Ok(None), "the older 'on' must not restart the motor");
        assert_eq!(rx.open(&second), Ok(None), "a replay is inert");
    }

    #[test]
    fn a_foreign_or_tampered_datagram_is_refused() {
        let mut tx = RumbleSender::new(keys());
        let mut rx = RumbleReceiver::new(keys());
        let dg = tx.seal(&[Motors::OFF; PADS]).unwrap();
        assert_eq!(rx.open(&dg), Err(RumbleError::Authentication));

        let k = keys();
        let mut tx = RumbleSender::new(k.clone());
        let mut rx = RumbleReceiver::new(k);
        let mut dg = tx.seal(&[Motors::OFF; PADS]).unwrap();
        *dg.last_mut().unwrap() ^= 1;
        assert_eq!(rx.open(&dg), Err(RumbleError::Authentication));

        assert_eq!(rx.open(&[]), Err(RumbleError::Malformed));
        assert_eq!(rx.open(&[ECHO_RUMBLE; 10]), Err(RumbleError::Malformed));
    }

    /// Sealed under the audio stream id with the same key and counter, a
    /// datagram must not open as rumble — the stream id is what keeps a
    /// captured downstream audio packet from becoming a motor command.
    #[test]
    fn audio_sealed_under_the_same_key_does_not_open_as_rumble() {
        let k = keys();
        let mut rx = RumbleReceiver::new(k.clone());
        let sealed = k.seal(crate::media_crypto::STREAM_AUDIO, 1, PURPOSE, &[0u8; PLAINTEXT_LEN]);
        let mut dg = vec![ECHO_RUMBLE, 0, 0, 0, 0, 1];
        dg.extend_from_slice(&sealed);
        assert_eq!(rx.open(&dg), Err(RumbleError::Authentication));
    }

    // ── Schedule ──

    /// Drain everything owed between `from` and `to`, stepping in 1 ms ticks
    /// like a polling caller would. Returns the send times.
    fn drain(tx: &mut RumbleTx, from: Instant, to: Instant) -> Vec<Duration> {
        let mut sent = Vec::new();
        let mut now = from;
        while now <= to {
            if tx.poll(now).unwrap().is_some() {
                sent.push(now - from);
            }
            now += Duration::from_millis(1);
        }
        sent
    }

    #[test]
    fn a_change_goes_out_at_once_and_is_repeated() {
        let t0 = Instant::now();
        let mut tx = RumbleTx::new(keys(), t0);
        assert_eq!(tx.poll(t0).unwrap(), None, "nothing owed before anything happens");

        assert!(tx.set(0, rumble(0, 0), t0) == false, "off → off is not a change");
        assert!(tx.set(0, rumble(9000, 0), t0));
        let sent = drain(&mut tx, t0, t0 + Duration::from_millis(120));
        assert_eq!(
            sent,
            vec![Duration::ZERO, REPEAT_AFTER[0], REPEAT_AFTER[1]],
            "immediately, then each repeat"
        );
    }

    /// A running motor is refreshed for as long as it runs — that is the
    /// heartbeat the client's watchdog depends on.
    #[test]
    fn a_running_motor_is_refreshed_and_a_stopped_one_goes_quiet() {
        let t0 = Instant::now();
        let mut tx = RumbleTx::new(keys(), t0);
        tx.set(1, rumble(1, 1), t0);

        let window = Duration::from_millis(1200);
        let sent = drain(&mut tx, t0, t0 + window);
        let refreshes = sent.iter().filter(|d| **d > REPEAT_AFTER[1]).count();
        assert!(refreshes >= 4, "expected a refresh every {REFRESH_INTERVAL:?}, got {sent:?}");
        for pair in sent.windows(2) {
            assert!(pair[1] - pair[0] <= REFRESH_INTERVAL, "a gap longer than the refresh: {sent:?}");
        }

        // Stop it: the stop is sent and repeated, then nothing more is owed.
        let t1 = t0 + window + Duration::from_millis(1);
        assert!(tx.stop_all(t1));
        let sent = drain(&mut tx, t1, t1 + Duration::from_secs(3));
        assert_eq!(sent.len(), 1 + REPEAT_AFTER.len(), "a stop is sent, repeated, then silence");
        assert_eq!(tx.next_due(), None);
    }

    /// Games call XInputSetState every frame with the same speeds. That must
    /// not re-anchor the burst, or a steady effect costs a datagram per frame.
    #[test]
    fn re_setting_the_same_value_owes_nothing_new() {
        let t0 = Instant::now();
        let mut tx = RumbleTx::new(keys(), t0);
        tx.set(0, rumble(7, 7), t0);
        drain(&mut tx, t0, t0 + Duration::from_millis(150));

        let t1 = t0 + Duration::from_millis(151);
        assert!(!tx.set(0, rumble(7, 7), t1));
        let due = tx.next_due().expect("still running, so a refresh is scheduled");
        assert!(due > t1, "an unchanged value must not make anything due right now");
    }

    // ── End to end, including the watchdog ──

    #[test]
    fn the_client_follows_the_host_through_loss() {
        let k = keys();
        let t0 = Instant::now();
        let mut host = RumbleTx::new(k.clone(), t0);
        let mut client = RumbleRx::new(k);

        host.set(0, rumble(40000, 20000), t0);
        // The first send is lost; the first repeat lands.
        let lost = host.poll(t0).unwrap().unwrap();
        drop(lost);
        let repeat = host.poll(t0 + REPEAT_AFTER[0]).unwrap().unwrap();
        assert!(client.accept(&repeat, t0 + REPEAT_AFTER[0]).unwrap());
        assert_eq!(client.state(t0 + REPEAT_AFTER[0])[0], rumble(40000, 20000));
    }

    /// The case no host-side repetition can cover: the host goes away while a
    /// motor is running. The controller must stop on its own.
    #[test]
    fn a_running_motor_stops_when_the_host_goes_silent() {
        let k = keys();
        let t0 = Instant::now();
        let mut host = RumbleTx::new(k.clone(), t0);
        let mut client = RumbleRx::new(k);

        host.set(2, rumble(1000, 0), t0);
        let dg = host.poll(t0).unwrap().unwrap();
        client.accept(&dg, t0).unwrap();

        assert_eq!(client.state(t0 + WATCHDOG)[2], rumble(1000, 0), "within the watchdog");
        assert!(
            client.state(t0 + WATCHDOG + Duration::from_millis(1)).iter().all(Motors::is_off),
            "past it, everything stops"
        );
    }

    /// …and the refresh is what keeps a long effect from tripping it.
    #[test]
    fn refreshes_keep_a_long_effect_alive() {
        let k = keys();
        let t0 = Instant::now();
        let mut host = RumbleTx::new(k.clone(), t0);
        let mut client = RumbleRx::new(k);
        host.set(0, rumble(3, 3), t0);

        let mut now = t0;
        let end = t0 + Duration::from_secs(5);
        while now <= end {
            if let Some(dg) = host.poll(now).unwrap() {
                client.accept(&dg, now).unwrap();
            }
            assert_eq!(client.state(now)[0], rumble(3, 3), "dropped out at {:?}", now - t0);
            now += Duration::from_millis(5);
        }
    }

    /// The watchdog and the refresh are two halves of one contract and live in
    /// different processes. Pin the relationship rather than trusting both
    /// numbers to be edited together.
    #[test]
    fn the_watchdog_tolerates_several_lost_refreshes() {
        assert!(WATCHDOG >= REFRESH_INTERVAL * 3, "one or two lost refreshes must not stop a motor");
        assert!(REPEAT_AFTER.iter().all(|d| *d < REFRESH_INTERVAL));
    }
}

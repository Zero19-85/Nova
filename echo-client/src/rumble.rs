//! Controller rumble from the host, held for the platform to apply.
//!
//! The client half of [`nova_core::rumble_channel`]. The host sends the whole
//! motor state of every pad, repeatedly; this keeps the newest and answers the
//! platform's question "what should pad N be doing right now?".
//!
//! ## Pulled, not pushed
//!
//! Same shape as [`crate::audio::AudioPlayout`] and for a related reason: the
//! platform already has a thread that owns the physical controller (Xbox's
//! 250 Hz pad loop, Android's input handler), and the motors must be driven
//! from there. A callback from the network task would put `Gamepad.Vibration`
//! on a thread that does not own the pad, and would need re-delivering whenever
//! the platform swaps which physical controller is slot 0. Polling lets the
//! owner apply the latest state on its own clock, and a poll that lands on a
//! newly attached controller simply applies it there.
//!
//! ## Silence is safe
//!
//! Unarmed, disarmed, or past the watchdog, every pad reads as stopped. So a
//! session that ends mid-rumble, a host that crashes, or a handover gap all
//! leave the controller still — the failure that matters most here is a pad
//! that keeps shaking with nothing on screen to explain it.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Instant;

use nova_core::media_crypto::SessionKeys;
use nova_core::rumble_channel::{Motors, RumbleError, RumbleRx, State, PADS};

/// Counters for a diagnostics overlay.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RumbleStats {
    /// Datagrams that opened and carried a newer state.
    pub accepted: u64,
    /// Datagrams refused — wrong key, corrupt, or malformed.
    pub refused: u64,
    /// Accepted datagrams that changed some motor.
    pub changes: u64,
}

pub struct RumblePlayout {
    inner: std::sync::Mutex<Option<RumbleRx>>,
    accepted: AtomicU64,
    refused: AtomicU64,
    changes: AtomicU64,
}

impl Default for RumblePlayout {
    fn default() -> Self {
        Self::new()
    }
}

impl RumblePlayout {
    pub fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(None),
            accepted: AtomicU64::new(0),
            refused: AtomicU64::new(0),
            changes: AtomicU64::new(0),
        }
    }

    /// Session keys arrived. Replaces any previous receiver outright: a new
    /// session's counter restarts at 1, and the old high-water mark would make
    /// every one of its datagrams read as stale.
    pub fn arm(&self, keys: SessionKeys) {
        *self.lock() = Some(RumbleRx::new(keys));
    }

    /// The session ended. Every pad reads as stopped from here on.
    pub fn disarm(&self) {
        *self.lock() = None;
    }

    /// Accept one datagram from the demultiplexer. Unarmed, it is dropped.
    pub fn accept(&self, datagram: &[u8]) -> Result<(), RumbleError> {
        let mut guard = self.lock();
        let Some(rx) = guard.as_mut() else {
            return Ok(());
        };
        match rx.accept(datagram, Instant::now()) {
            Ok(changed) => {
                self.accepted.fetch_add(1, Relaxed);
                if changed {
                    self.changes.fetch_add(1, Relaxed);
                }
                Ok(())
            }
            Err(e) => {
                self.refused.fetch_add(1, Relaxed);
                Err(e)
            }
        }
    }

    /// What every pad should be doing right now. All stopped when unarmed or
    /// when the host has gone quiet past the watchdog.
    pub fn state(&self) -> State {
        match self.lock().as_ref() {
            Some(rx) => rx.state(Instant::now()),
            None => [Motors::OFF; PADS],
        }
    }

    /// One pad's motors. Out-of-range pads read as stopped.
    pub fn motors(&self, pad: usize) -> Motors {
        self.state().get(pad).copied().unwrap_or(Motors::OFF)
    }

    pub fn stats(&self) -> RumbleStats {
        RumbleStats {
            accepted: self.accepted.load(Relaxed),
            refused: self.refused.load(Relaxed),
            changes: self.changes.load(Relaxed),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<RumbleRx>> {
        // A poisoned lock still holds a usable receiver; the platform's pad
        // thread must never panic because some other thread once did.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nova_core::rumble_channel::RumbleTx;

    #[test]
    fn unarmed_every_pad_is_still_and_datagrams_are_dropped() {
        let keys = SessionKeys::generate();
        let now = Instant::now();
        let mut host = RumbleTx::new(keys.clone(), now);
        host.set(0, Motors { low: 9, high: 9 }, now);
        let dg = host.poll(now).unwrap().unwrap();

        let p = RumblePlayout::new();
        assert!(p.accept(&dg).is_ok());
        assert_eq!(p.motors(0), Motors::OFF, "nothing applies before the grant");

        p.arm(keys);
        p.accept(&dg).unwrap();
        assert_eq!(p.motors(0), Motors { low: 9, high: 9 });

        p.disarm();
        assert_eq!(p.motors(0), Motors::OFF, "a finished session stops the motors");
        assert_eq!(p.motors(99), Motors::OFF, "out of range is just stopped");
    }

    /// A handover re-arms with the new session's keys. The new session's first
    /// datagram carries counter 1, which the OLD receiver had long since passed;
    /// reusing it would ignore the new session until its counter caught up.
    #[test]
    fn re_arming_accepts_the_next_sessions_first_datagram() {
        let p = RumblePlayout::new();
        let now = Instant::now();

        let old = SessionKeys::generate();
        p.arm(old.clone());
        let mut host = RumbleTx::new(old, now);
        for n in 1..20u16 {
            host.set(0, Motors { low: n, high: 0 }, now);
            p.accept(&host.poll(now).unwrap().unwrap()).unwrap();
        }

        let new = SessionKeys::generate();
        p.arm(new.clone());
        let mut host = RumbleTx::new(new, now);
        host.set(1, Motors { low: 5, high: 6 }, now);
        p.accept(&host.poll(now).unwrap().unwrap()).unwrap();
        assert_eq!(p.motors(1), Motors { low: 5, high: 6 });
        assert_eq!(p.motors(0), Motors::OFF, "nothing carries over from the old session");
    }

    #[test]
    fn a_foreign_datagram_is_refused_and_counted() {
        let p = RumblePlayout::new();
        p.arm(SessionKeys::generate());
        let now = Instant::now();
        let mut stranger = RumbleTx::new(SessionKeys::generate(), now);
        stranger.set(0, Motors { low: 1, high: 1 }, now);
        assert!(p.accept(&stranger.poll(now).unwrap().unwrap()).is_err());
        assert_eq!(p.stats().refused, 1);
        assert_eq!(p.motors(0), Motors::OFF);
    }
}

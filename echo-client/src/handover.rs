//! Surviving a network change without ending the session.
//!
//! A phone that moves from Wi-Fi to 5G loses its path and keeps its session.
//! Those are two different facts, and every part of this module exists because
//! the code below it conflates them by default:
//!
//! - The **socket** is dead. Its NAT mapping was on an interface that no longer
//!   exists, and on Android a UDP socket bound to a departed network does not
//!   reliably error — `send` succeeds into nothing. Nothing about it is
//!   recoverable.
//! - The **path** is dead. A new interface means a new public address, so the
//!   punch has to run again from scratch.
//! - The **session** is not dead. Nova detaches rather than tears down when a
//!   client goes quiet (`echo::session`'s `detached_since`), holds the virtual
//!   display, the desktop arrangement and whatever is running on it for
//!   `[stream] detach_grace_secs`, and lets the same device reclaim it —
//!   `⚡ reclaiming its detached session N` in the host log.
//! - The **picture** is not dead either, and this is the part that was being
//!   thrown away. The decoder holds a perfectly good last frame, the Surface is
//!   still valid, and nothing about the display pipeline is wrong. Tearing it
//!   down because the network moved is what produces the black screen.
//!
//! So a handover is: keep the decoder, throw the socket away, re-run the
//! cascade, reclaim the session, ask for an IDR, resume. This module owns the
//! loop that does that, and it owns it *above* [`crate::session`] rather than
//! inside it — `open_path` and `stream` stay single-attempt functions that
//! either work or say why not, which is what makes them testable and what keeps
//! the CLI's one-shot behaviour unchanged.
//!
//! ## The failure this is really for
//!
//! Not the clean case. The clean case — interface goes away, `sendto` starts
//! returning `ENETUNREACH`, `stream()` returns an error — was always going to be
//! survivable. The one that produced the standing bug is the **silent** one: the
//! swap completes, the socket stays "valid", datagrams leave for an address
//! nothing routes to any more, and every layer reports health. The control
//! tunnel has its own retransmits and takes seconds to give up. The receive loop
//! is simply quiet. There is no error to propagate.
//!
//! That is why [`Liveness`] exists and why it watches *frames* rather than
//! sockets, errors, or the tunnel: video arriving is the only end-to-end
//! statement that the whole path still works. Everything else is an opinion held
//! by one layer about its own neighbours.
//!
//! ## What this deliberately does not do
//!
//! It does not re-pair, re-key by hand, or cache anything from the previous
//! grant. Keys are minted fresh per session on the host *by design* — see the
//! detached-takeover invariant in CLAUDE.md — so a client that reused old media
//! keys after a reclaim would decrypt nothing and look exactly like a codec
//! fault. Each attempt runs the full `start_session` handshake and takes the
//! keys it is given.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use nova_core::identity::Identity;

use crate::receiver::{DecodedFrame, FrameSink, ReceiveStats};
use crate::session::{self, ConnectOptions, Event, OpenPath, Progress, StreamOptions, Uplink};

// ── The network epoch ────────────────────────────────────────────────────────

/// Bumped whenever the platform observes that the network underneath us has
/// changed.
///
/// Process-global rather than session-scoped, and that is not laziness: the fact
/// it records is a property of the device, not of a session. It also means the
/// JNI entry point that raises it needs no handle — like `nativeRelease`, there
/// is no session state to get wrong, and the Android connectivity callback fires
/// at exactly the moments a handle is most likely to be mid-teardown.
static NETWORK_EPOCH: AtomicU64 = AtomicU64::new(0);

/// The current epoch. An attempt captures this before it starts and compares
/// afterwards; a difference means the ground moved under it.
pub fn epoch() -> u64 {
    NETWORK_EPOCH.load(Ordering::SeqCst)
}

/// Tell the supervisor the network changed. Returns the new epoch.
///
/// Safe to call spuriously and safe to call often — Android's
/// `ConnectivityManager` is generous with callbacks, and the cost of a false
/// positive is one reconnect that would have worked anyway, against the cost of
/// a false negative which is the stall timeout (seconds) instead of
/// milliseconds.
pub fn network_changed() -> u64 {
    NETWORK_EPOCH.fetch_add(1, Ordering::SeqCst) + 1
}

// ── Liveness ─────────────────────────────────────────────────────────────────

/// When video was last seen, shared between the sink and the supervisor.
///
/// Milliseconds since an arbitrary origin, in one atomic, because the writer is
/// the receive path at up to 120 Hz and the reader is a 100 ms poll. A mutex
/// here would be a lock taken on the hot path to serve a loop that could not
/// care less about precision.
#[derive(Debug)]
pub struct Liveness {
    origin: Instant,
    last_frame_ms: AtomicU64,
    frames: AtomicU64,
}

impl Liveness {
    fn new() -> Self {
        Self { origin: Instant::now(), last_frame_ms: AtomicU64::new(0), frames: AtomicU64::new(0) }
    }

    fn mark(&self) {
        let ms = self.origin.elapsed().as_millis() as u64;
        self.last_frame_ms.store(ms, Ordering::Relaxed);
        self.frames.fetch_add(1, Ordering::Relaxed);
    }

    /// How long since the last frame, or `None` if none has ever arrived.
    ///
    /// `None` is deliberately distinct from "a long time": before the first
    /// frame there is nothing to be silent *relative to*, and treating a
    /// still-connecting session as stalled would abort every attempt during the
    /// handshake it needs to complete.
    pub fn silent_for(&self) -> Option<Duration> {
        if self.frames.load(Ordering::Relaxed) == 0 {
            return None;
        }
        let last = self.last_frame_ms.load(Ordering::Relaxed);
        let now = self.origin.elapsed().as_millis() as u64;
        Some(Duration::from_millis(now.saturating_sub(last)))
    }

    /// Frames delivered on this attempt.
    pub fn frames(&self) -> u64 {
        self.frames.load(Ordering::Relaxed)
    }
}

/// Wraps the caller's sink and stamps [`Liveness`] on the way past.
///
/// Every other method delegates verbatim. In particular `take_keyframe_request`
/// and `take_invalidation_request` **must** delegate: they are how a sink that
/// lost its reference chain asks for repair, and a wrapper that silently
/// answered the default `false`/`None` would leave a real decoder waiting for a
/// keyframe forever under an infinite GOP. This project has already had that bug
/// once, from the other direction.
struct WatchdogSink<'a, S: FrameSink> {
    inner: &'a mut S,
    live: Arc<Liveness>,
}

impl<S: FrameSink> FrameSink for WatchdogSink<'_, S> {
    fn on_frame(&mut self, frame: DecodedFrame) {
        self.live.mark();
        self.inner.on_frame(frame);
    }

    fn take_keyframe_request(&mut self) -> bool {
        self.inner.take_keyframe_request()
    }

    fn take_invalidation_request(&mut self) -> Option<(u32, u32)> {
        self.inner.take_invalidation_request()
    }
}

// ── Keeping the uplink across attempts ───────────────────────────────────────

/// Holds the platform's one-shot uplink receivers and lends a fresh pair to each
/// attempt.
///
/// [`Uplink`]'s receivers are consumed by `session::stream` and dropped when it
/// returns, so a supervisor that handed the same `Uplink` to a second attempt
/// would be handing over closed channels. The platform cannot simply rebuild
/// them either: the *senders* live in the JNI handle and in Kotlin's input path,
/// and swapping those under a UI thread on every reconnect is a race nobody
/// needs.
///
/// So the real receivers are drained here, permanently, by pump tasks that
/// forward into whichever per-attempt channel is currently installed.
///
/// **Traffic that arrives while no attempt is installed is dropped, and that is
/// the point.** Replaying a key-down captured before a three-second gap lands a
/// stuck key on a machine the user is also sitting at; a pointer delta from
/// before the gap describes a position that no longer exists; a microphone
/// packet from four seconds ago is not audio, it is an echo. The correct
/// behaviour for every one of these is to discard, not to buffer.
pub struct UplinkRelay {
    input: Option<Slot>,
    mic: Option<Slot>,
    audio: Option<Arc<crate::audio::AudioPlayout>>,
    control: Option<Slot>,
}

type Slot = Arc<std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>>>;

impl UplinkRelay {
    /// Take ownership of the platform's uplink and start draining it.
    ///
    /// Must be called from inside a tokio runtime — it spawns one pump task per
    /// channel present. The tasks end when the platform's senders are dropped,
    /// which happens when the JNI handle is closed.
    pub fn spawn(source: Uplink) -> Self {
        let Uplink { input, mic, audio, control } = source;
        Self {
            input: input.map(pump),
            mic: mic.map(pump),
            audio,
            control: control.map(pump),
        }
    }

    /// A fresh [`Uplink`] for one attempt.
    ///
    /// Installing the new sender drops the previous one, which closes the old
    /// attempt's channel — so a `stream` call that is still unwinding cannot
    /// keep receiving input meant for its replacement.
    pub fn next(&self) -> Uplink {
        Uplink {
            input: self.input.as_ref().map(install),
            mic: self.mic.as_ref().map(install),
            // An `Arc`, not a one-shot receiver: the platform's audio thread
            // polls this same handle for the life of the app, so it is shared
            // across attempts rather than rebuilt for each one.
            audio: self.audio.clone(),
            control: self.control.as_ref().map(install),
        }
    }
}

fn pump(mut rx: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>) -> Slot {
    let slot: Slot = Arc::new(std::sync::Mutex::new(None));
    let target = slot.clone();
    tokio::spawn(async move {
        while let Some(item) = rx.recv().await {
            // The lock is held only long enough to clone a sender handle; the
            // send itself is on an unbounded channel and never blocks. Nothing
            // here can stall the platform thread that produced the item.
            let tx = target.lock().unwrap_or_else(|e| e.into_inner()).clone();
            if let Some(tx) = tx {
                let _ = tx.send(item);
            }
        }
    });
    slot
}

fn install(slot: &Slot) -> tokio::sync::mpsc::UnboundedReceiver<Vec<u8>> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
    rx
}

// ── Policy ───────────────────────────────────────────────────────────────────

/// How patient the supervisor is, and with what.
#[derive(Debug, Clone)]
pub struct HandoverPolicy {
    /// Total time a session may spend trying to come back before the supervisor
    /// gives up and reports failure.
    ///
    /// **Must stay below the host's `[stream] detach_grace_secs`** (600 s by
    /// default). Past that the host has torn the session down, so a successful
    /// reconnect gets a *new* session on a *newly built* display — the reclaim
    /// fast path is gone and the user watches the desktop rearrange itself. The
    /// default is deliberately far below it: a user staring at a frozen picture
    /// for a minute has already decided the app is broken.
    pub resume_window: Duration,

    /// Video silence that counts as an interruption — but only alongside
    /// [`Self::control_stall_timeout`]. See that field.
    pub stall_timeout: Duration,

    /// Control-plane silence required before video silence is believed.
    ///
    /// **Video going quiet does not mean the network is gone, and acting as
    /// though it does makes a host-side pause worse.** A Nova host stops
    /// producing frames for several seconds on every Worker respawn — sign-out,
    /// sign-in, the SYSTEM-fallback upgrade — while its Master, which owns every
    /// socket, keeps answering control calls throughout. Measured on the dev box
    /// 2026-08-24: `Host exited 0.6s after spawn … backing off 4s`, then two
    /// more pipe cycles before `replaying ConfigureStart`.
    ///
    /// A video-only watchdog fires in the middle of that, tears down a perfectly
    /// good path, and reconnects — which cannot help, because the host was never
    /// the problem's network end, and which costs the client its place in the
    /// queue while the host is still coming back. So both signals must agree:
    /// no frames AND no answers.
    ///
    /// Shorter than the video timeout because a control round trip is cheap and
    /// frequent (`RTT_PROBE_INTERVAL`), so its silence is the more decisive of
    /// the two once video has already stopped.
    pub control_stall_timeout: Duration,

    /// Gap before the first retry.
    ///
    /// Not zero. An interface change is not atomic: Android reports the new
    /// network before it necessarily has an address, and a punch from an
    /// interface that is still coming up burns the fastest attempt on the least
    /// likely one to work.
    pub first_backoff: Duration,

    /// Ceiling on the backoff.
    ///
    /// Low, because the thing being waited for is not a busy server that needs
    /// protecting — it is a phone's radio. Once that settles the next attempt
    /// works, and every second of backoff past that point is a second of frozen
    /// picture bought for nothing.
    pub max_backoff: Duration,
}

impl Default for HandoverPolicy {
    fn default() -> Self {
        Self {
            resume_window: Duration::from_secs(60),
            stall_timeout: Duration::from_millis(1500),
            control_stall_timeout: Duration::from_millis(1200),
            first_backoff: Duration::from_millis(250),
            max_backoff: Duration::from_secs(3),
        }
    }
}

/// Why an attempt ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Interruption {
    /// The platform told us the network changed.
    NetworkChanged { epoch: u64 },
    /// No video for [`HandoverPolicy::stall_timeout`] — the silent swap.
    Stalled { silent_for: Duration },
    /// A layer below returned an error.
    Failed { reason: String },
    /// The host understood the request and said no.
    ///
    /// Kept apart from [`Interruption::Failed`] because the two want opposite
    /// treatment. A failure is retried, because it is usually the network and
    /// the network usually comes back. A refusal is a decision — the seat is
    /// taken, a Moonlight client is streaming — and nothing this side does will
    /// change it. See [`crate::session::REFUSED_PREFIX`].
    Refused { reason: String },
}

impl Interruption {
    /// The wire name, for the event stream Kotlin branches on. This is API.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NetworkChanged { .. } => "network_changed",
            Self::Stalled { .. } => "stalled",
            Self::Failed { .. } => "failed",
            Self::Refused { .. } => "refused",
        }
    }
}

impl std::fmt::Display for Interruption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NetworkChanged { epoch } => write!(f, "the network changed (epoch {epoch})"),
            Self::Stalled { silent_for } => {
                write!(f, "no video for {} ms", silent_for.as_millis())
            }
            Self::Failed { reason } => write!(f, "{reason}"),
            Self::Refused { reason } => write!(f, "{reason}"),
        }
    }
}

/// How the supervised session ended.
#[derive(Debug)]
pub enum Outcome {
    /// The caller asked it to stop. The normal ending.
    Stopped(ReceiveStats),
    /// The resume window ran out. Carries the last reason, which is the one
    /// worth showing a user — the earlier ones are the reasons it kept trying.
    GaveUp { last: Interruption, attempts: u32 },
}

// ── The supervisor ───────────────────────────────────────────────────────────

/// Run a session, and keep running it across network changes until the caller
/// stops it or the resume window closes.
///
/// `uplink` is a factory rather than a value because [`Uplink`]'s channels are
/// one-shot: `stream` consumes the receivers and drops them when it returns, so
/// every attempt needs a fresh pair. The platform layer rebuilds them and swaps
/// its senders over.
///
/// **Input queued during a gap is discarded, not replayed.** That falls out of
/// the factory design and it is the behaviour we want: replaying a key-down
/// captured before a three-second gap lands a stuck key on a machine the user is
/// also sitting at, and every input packet older than the gap describes a
/// pointer position that no longer means anything.
#[allow(clippy::too_many_arguments)]
pub async fn supervise<S, P, F>(
    identity: &Identity,
    connect: &ConnectOptions,
    stream_opts: &StreamOptions,
    sink: &mut S,
    progress: &mut P,
    stop: tokio::sync::watch::Receiver<bool>,
    mut uplink: F,
    policy: HandoverPolicy,
) -> Outcome
where
    S: FrameSink,
    P: Progress,
    F: FnMut() -> Uplink,
{
    let mut attempts: u32 = 0;
    let mut backoff = policy.first_backoff;
    // Started at the first *interruption*, not here. A session that runs for an
    // hour and then loses its network deserves the full window, and one clocked
    // from session start would have spent it long ago.
    let mut resuming_since: Option<Instant> = None;

    loop {
        attempts += 1;
        let live = Arc::new(Liveness::new());
        let started_epoch = epoch();

        let interruption = match attempt(
            identity,
            connect,
            stream_opts,
            sink,
            progress,
            &stop,
            uplink(),
            &live,
            &policy,
            started_epoch,
        )
        .await
        {
            Attempt::Stopped(stats) => return Outcome::Stopped(stats),
            Attempt::Interrupted(reason) => reason,
        };

        // A user-initiated stop that raced the interruption still wins: the
        // caller's intent outranks anything the network did.
        if *stop.borrow() {
            return Outcome::Stopped(ReceiveStats::default());
        }

        // The picture stays. Nothing below this point touches the decoder or the
        // Surface — that is the whole contract with the platform layer, and this
        // event is how it learns to draw a "reconnecting" overlay *over* the
        // held frame rather than in place of it.
        progress.event(Event::PathInterrupted {
            reason: interruption.as_str().to_string(),
            detail: interruption.to_string(),
            attempt: attempts,
            frames_this_attempt: live.frames(),
        });

        // A refusal ends it now, without waiting out the resume window.
        //
        // Everything else in this loop is retried because the thing that broke
        // is expected to come back on its own — an interface settles, a punch
        // finds a route, a stall clears. A refusal is not that. The host
        // understood the request and declined it for a reason that will still be
        // true on the next attempt and the twenty after it, and the only thing
        // that clears it is a person doing something: stopping the Moonlight
        // client, releasing the seat from the other device.
        //
        // Retrying anyway is not merely useless, it actively hides the answer.
        // Each attempt is a full rendezvous, punch and TLS handshake against a
        // host that already said no, and while they run the UI shows
        // "Reconnecting…" — so the one message the user needed, which the host
        // wrote in plain English and sent immediately, was buried under a minute
        // of retry chatter (live 2026-08-24).
        //
        // The trade, stated: a refusal that WOULD have cleared within the window
        // — the other client quitting a few seconds later — now needs the user
        // to tap again instead of recovering by itself. That is the right way
        // round. Being told "your PC is busy, a Moonlight client is streaming"
        // in under a second is worth more than an automatic recovery that only
        // sometimes happens and never explains itself.
        if let Interruption::Refused { .. } = interruption {
            return Outcome::GaveUp { last: interruption, attempts };
        }

        let since = *resuming_since.get_or_insert_with(Instant::now);
        if since.elapsed() >= policy.resume_window {
            return Outcome::GaveUp { last: interruption, attempts };
        }

        // Wait out the backoff, but wake immediately on a stop or on a *newer*
        // network change — if the radio settles onto a third interface while we
        // are sleeping, the sleep is time spent waiting to attempt something
        // already stale.
        let woke_early = wait_for_retry(backoff, &stop, started_epoch).await;
        if *stop.borrow() {
            return Outcome::Stopped(ReceiveStats::default());
        }
        backoff = if woke_early {
            policy.first_backoff
        } else {
            (backoff * 2).min(policy.max_backoff)
        };

        progress.event(Event::PathResuming {
            attempt: attempts + 1,
            window_left_ms: policy.resume_window.saturating_sub(since.elapsed()).as_millis() as u64,
        });
    }
}

enum Attempt {
    Stopped(ReceiveStats),
    Interrupted(Interruption),
}

/// One pass of the cascade: bind, punch, stream, until something ends it.
#[allow(clippy::too_many_arguments)]
async fn attempt<S: FrameSink, P: Progress>(
    identity: &Identity,
    connect: &ConnectOptions,
    stream_opts: &StreamOptions,
    sink: &mut S,
    progress: &mut P,
    stop: &tokio::sync::watch::Receiver<bool>,
    uplink: Uplink,
    live: &Arc<Liveness>,
    policy: &HandoverPolicy,
    started_epoch: u64,
) -> Attempt {
    // A previous attempt's healthy control channel must not vouch for this one.
    session::reset_control_liveness();

    // Every attempt binds its own socket inside `open_path`. Reusing one across
    // a handover is not an optimisation that costs a little latency — it is a
    // socket holding a mapping on an interface that is gone, and it will
    // "succeed" at sending forever.
    //
    // Raced against the epoch, because `open_path` is not otherwise
    // interruptible and its budget is long: the WAN punch alone blasts for 8
    // seconds. On a phone that is walking between networks the ground can move
    // twice inside one attempt, and without this the second change waits out the
    // first attempt's full timeout before anything reacts to it.
    let path: OpenPath = tokio::select! {
        opened = session::open_path(identity, connect, progress) => match opened {
            Ok(path) => path,
            Err(reason) => return Attempt::Interrupted(classify(reason, started_epoch)),
        },
        moved = epoch_changed(started_epoch) => {
            return Attempt::Interrupted(Interruption::NetworkChanged { epoch: moved });
        }
        _ = stopped(stop.clone()) => {
            return Attempt::Stopped(ReceiveStats::default());
        }
    };

    // The attempt's own stop signal: set by the caller's stop, by a network
    // epoch bump, or by the stall watchdog. `stream` already takes exactly this
    // shape, so the whole interruption mechanism costs it no changes.
    let (attempt_stop_tx, attempt_stop_rx) = tokio::sync::watch::channel(false);
    let reason: Arc<std::sync::Mutex<Option<Interruption>>> = Arc::new(std::sync::Mutex::new(None));

    let watcher = tokio::spawn(watch_for_interruption(
        attempt_stop_tx,
        reason.clone(),
        stop.clone(),
        live.clone(),
        policy.clone(),
        started_epoch,
        stream_opts.detach_on_exit.clone(),
    ));

    let mut watched = WatchdogSink { inner: sink, live: live.clone() };
    let outcome = session::stream(
        identity,
        &connect.host_fingerprint,
        path,
        stream_opts,
        &mut watched,
        progress,
        attempt_stop_rx,
        uplink,
    )
    .await;
    watcher.abort();

    // Put the flag back for the next attempt. The watcher raised it on its way
    // out; leaving it raised would make a later *deliberate* stop go silent too,
    // and a session nobody says goodbye to holds the host's pipeline until its
    // grace period expires — locking out every other paired device meanwhile.
    stream_opts.detach_on_exit.store(false, Ordering::SeqCst);

    // The watcher's reason outranks whatever `stream` said on the way out.
    // `stream` reports the *symptom* of an interruption it was asked to make —
    // "control tunnel closed" for a stop it was handed — while the watcher
    // recorded the cause. Reporting the symptom is how a network handover ends
    // up in a log as a TLS error.
    if let Some(reason) = reason.lock().unwrap_or_else(|e| e.into_inner()).take() {
        return Attempt::Interrupted(reason);
    }

    match outcome {
        // `stream` returning cleanly means the caller's stop was set: the only
        // other way out is an error.
        Ok(stats) => Attempt::Stopped(stats),
        Err(reason) => Attempt::Interrupted(classify(reason, started_epoch)),
    }
}

/// Ends the attempt when the caller stops, the network moves, or video goes
/// quiet.
async fn watch_for_interruption(
    attempt_stop: tokio::sync::watch::Sender<bool>,
    reason: Arc<std::sync::Mutex<Option<Interruption>>>,
    mut stop: tokio::sync::watch::Receiver<bool>,
    live: Arc<Liveness>,
    policy: HandoverPolicy,
    started_epoch: u64,
    detach_on_exit: Arc<std::sync::atomic::AtomicBool>,
) {
    // Raised BEFORE `attempt_stop` on every interruption path, and never on the
    // caller's-stop path. Ordering matters: `stream` reads it during the cleanup
    // that `attempt_stop` triggers, so a flag set afterwards would arrive to
    // find the goodbye already sent and the host's session already torn down.
    let interrupt = |what: Interruption| {
        detach_on_exit.store(true, Ordering::SeqCst);
        *reason.lock().unwrap_or_else(|e| e.into_inner()) = Some(what);
        let _ = attempt_stop.send(true);
    };

    // Fast enough that the epoch path costs a fraction of the backoff it saves,
    // slow enough to be free. The stall path is bounded by `stall_timeout`
    // anyway, so this tick is not the resolution of that measurement.
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = tick.tick() => {
                let now = epoch();
                if now != started_epoch {
                    interrupt(Interruption::NetworkChanged { epoch: now });
                    return;
                }
                // BOTH signals, never video alone — see
                // `HandoverPolicy::control_stall_timeout`. A host that has
                // stopped encoding but is still answering is a host that is
                // coming back on its own, and the only thing a reconnect adds
                // there is a teardown.
                if let Some(silent) = live.silent_for() {
                    if silent >= policy.stall_timeout {
                        match session::control_idle() {
                            // The host is still answering: this is a host-side
                            // pause, not a lost path. Hold the picture and wait.
                            Some(idle) if idle < policy.control_stall_timeout => {}
                            // Answered once, and has now stopped: the path is
                            // gone. `None` means it never answered on this
                            // attempt, which with video also silent is the same
                            // conclusion.
                            _ => {
                                interrupt(Interruption::Stalled { silent_for: silent });
                                return;
                            }
                        }
                    }
                }
            }
            // The caller stopping is not an interruption: no reason is recorded,
            // so `attempt` reports `Stopped` and the supervisor returns.
            Ok(()) = stop.changed() => {
                if *stop.borrow() {
                    let _ = attempt_stop.send(true);
                    return;
                }
            }
        }
    }
}

/// Turn an error string into an interruption, preferring the epoch when one
/// moved.
///
/// An error raised *after* the network changed is almost always a consequence of
/// it — "relay lookup failed", "punch timed out", "control tunnel" — and
/// reporting the consequence rather than the cause is how a handover gets
/// diagnosed as a relay outage.
fn classify(reason: String, started_epoch: u64) -> Interruption {
    let now = epoch();
    if now != started_epoch {
        // The epoch is tested FIRST, and it outranks a refusal deliberately. If
        // the ground moved mid-attempt then whatever came back describes a path
        // that no longer exists — including a refusal, which the host may well
        // not repeat once we ask again from the interface we actually have.
        return Interruption::NetworkChanged { epoch: now };
    }
    match reason.strip_prefix(session::REFUSED_PREFIX) {
        Some(refusal) => Interruption::Refused { reason: refusal.to_string() },
        None => Interruption::Failed { reason },
    }
}

/// Resolves with the new epoch once it differs from `known`.
///
/// Polled rather than notified: the epoch is raised from a JNI call on Android's
/// connectivity callback, which has no runtime handle to wake a `Notify` with,
/// and 100 ms is far below the time any reconnect takes.
async fn epoch_changed(known: u64) -> u64 {
    loop {
        let now = epoch();
        if now != known {
            return now;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Resolves when the caller sets `stop`.
async fn stopped(mut stop: tokio::sync::watch::Receiver<bool>) {
    loop {
        if *stop.borrow() {
            return;
        }
        if stop.changed().await.is_err() {
            // Every sender gone: nothing can ever stop us, so this future must
            // never resolve — returning here would abort a healthy attempt.
            std::future::pending::<()>().await;
        }
    }
}

/// Sleep for `backoff`, returning early (`true`) if the caller stopped or the
/// network moved again.
async fn wait_for_retry(
    backoff: Duration,
    stop: &tokio::sync::watch::Receiver<bool>,
    known_epoch: u64,
) -> bool {
    let deadline = Instant::now() + backoff;
    let mut stop = stop.clone();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        tokio::select! {
            _ = tokio::time::sleep(left.min(Duration::from_millis(50))) => {
                if epoch() != known_epoch {
                    return true;
                }
            }
            Ok(()) = stop.changed() => {
                if *stop.borrow() {
                    return true;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn liveness_is_silent_about_a_session_that_has_seen_nothing() {
        // The distinction that stops a still-connecting session being aborted by
        // its own watchdog before the first frame can arrive.
        let live = Liveness::new();
        assert_eq!(live.silent_for(), None);
        live.mark();
        assert!(live.silent_for().is_some());
    }

    #[test]
    fn an_error_after_the_network_moved_is_reported_as_the_move() {
        let start = epoch();
        assert!(matches!(
            classify("relay lookup: connection refused".into(), start),
            Interruption::Failed { .. }
        ));
        network_changed();
        assert!(matches!(
            classify("relay lookup: connection refused".into(), start),
            Interruption::NetworkChanged { .. }
        ));
    }

    #[test]
    fn the_epoch_only_ever_moves_forward() {
        let a = epoch();
        let b = network_changed();
        assert!(b > a);
        assert_eq!(epoch(), b);
    }

    #[test]
    fn a_host_refusal_is_classified_apart_from_a_failure() {
        // The distinction the whole fix rests on. Both arrive as an `Err(String)`
        // from the same call; only the tag tells them apart, and getting this
        // wrong in either direction is bad — a misread failure gives up on a
        // recoverable network blip, a misread refusal spins for a minute.
        let start = epoch();
        let refusal = format!(
            "{}a Moonlight client is streaming right now (moonlight_active)",
            session::REFUSED_PREFIX
        );
        match classify(refusal, start) {
            Interruption::Refused { reason } => {
                assert_eq!(reason, "a Moonlight client is streaming right now (moonlight_active)");
                // And the tag must not survive into anything a person reads.
                assert!(!reason.contains(session::REFUSED_PREFIX));
            }
            other => panic!("a tagged refusal must classify as Refused, got {other:?}"),
        }
        assert!(
            matches!(classify("connection reset".into(), start), Interruption::Failed { .. }),
            "an untagged error is still an ordinary failure"
        );
    }

    #[test]
    fn a_refusal_names_itself_on_the_wire_and_reads_as_the_hosts_own_words() {
        // `as_str` is what Kotlin branches on to decide between "the PC is busy"
        // and "Reconnecting…", and Display is what it puts on screen. Both are
        // API; a rename here is a silent UI regression.
        let r = Interruption::Refused { reason: "the seat is taken".into() };
        assert_eq!(r.as_str(), "refused");
        assert_eq!(r.to_string(), "the seat is taken");
    }

    #[test]
    fn a_network_change_outranks_a_refusal() {
        // A refusal collected from an interface that has since gone away says
        // nothing about the one we now have, so it must be retried rather than
        // reported as the host's final word. Ordering inside `classify`.
        let start = epoch();
        let refusal = format!("{}the seat is taken", session::REFUSED_PREFIX);
        network_changed();
        assert!(
            matches!(classify(refusal, start), Interruption::NetworkChanged { .. }),
            "the ground moving outranks whatever the old path came back with"
        );
    }

    /// The resume window has to stay inside the host's grace period, or the
    /// reclaim fast path is gone by the time the client gets back.
    #[test]
    fn the_default_resume_window_fits_inside_the_hosts_detach_grace() {
        const HOST_DEFAULT_DETACH_GRACE: Duration = Duration::from_secs(600);
        assert!(HandoverPolicy::default().resume_window < HOST_DEFAULT_DETACH_GRACE);
    }

    /// An ordinary caller says goodbye; only the supervisor makes an exit
    /// silent. A default of `true` would leave every CLI run holding the host's
    /// pipeline until its grace period expired.
    #[test]
    fn a_plain_stream_says_goodbye_on_the_way_out() {
        let opts = StreamOptions::default();
        assert!(!opts.detach_on_exit.load(Ordering::SeqCst));
    }

    /// Video silence alone must not trigger a handover while the host is still
    /// answering — that is a Worker respawn, and reconnecting through one costs
    /// the session for a problem the network end never had.
    #[test]
    fn a_host_that_still_answers_is_not_a_lost_path() {
        let policy = HandoverPolicy::default();
        let video_silent = policy.stall_timeout + Duration::from_millis(500);

        // Host answered 200 ms ago: a pause, not a loss.
        let control_recent = Duration::from_millis(200);
        assert!(control_recent < policy.control_stall_timeout);
        assert!(video_silent >= policy.stall_timeout);

        // Both quiet: a genuinely lost path.
        let control_gone = policy.control_stall_timeout + Duration::from_millis(500);
        assert!(control_gone >= policy.control_stall_timeout);
    }

    /// The control timeout has to be reachable while video silence is still
    /// being measured, or the second signal could never agree in time.
    #[test]
    fn the_control_timeout_is_not_longer_than_the_video_one() {
        let policy = HandoverPolicy::default();
        assert!(policy.control_stall_timeout <= policy.stall_timeout);
    }

    #[tokio::test]
    async fn an_epoch_that_moves_wakes_the_open_path_race() {
        let start = epoch();
        let waiter = tokio::spawn(async move { epoch_changed(start).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let raised = network_changed();
        let seen = tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .expect("epoch_changed must not hang once the epoch moves")
            .unwrap();
        assert!(seen >= raised);
    }

    /// With every sender dropped, nothing can ever stop the attempt — so the
    /// future must never resolve. Resolving would abort a healthy session the
    /// moment its stop channel was garbage collected.
    #[tokio::test]
    async fn a_stop_channel_with_no_senders_never_fires() {
        let (tx, rx) = tokio::sync::watch::channel(false);
        drop(tx);
        assert!(
            tokio::time::timeout(Duration::from_millis(200), stopped(rx)).await.is_err(),
            "stopped() resolved on a dead channel"
        );
    }
}

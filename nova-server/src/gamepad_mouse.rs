//! Controller mouse mode — drive the Windows cursor from a gamepad.
//!
//! Moonlight's clients ship this as a client-side convenience. Nova implements
//! it **host-side instead**, and that placement is the whole point: every
//! client Nova will ever serve — Moonlight on any platform, Echo on Android,
//! and the Xbox frontend that has not been written yet — sends the same
//! `NV_MULTI_CONTROLLER_PACKET`, so a host-side implementation is one that no
//! future client has to re-implement or can implement differently. A client
//! that knows nothing about mouse mode gets it anyway.
//!
//! ## How it is entered and left
//!
//! **Start + Select together**, on the rising edge of the pair. The chord is
//! deliberately one nobody plays with by accident and one that exists on every
//! controller layout Nova sees, including the Xbox pad this will ship against.
//! Pressing it again leaves.
//!
//! ## What happens to the game while mouse mode is on
//!
//! Gamepad forwarding **stops**. Not "is ignored" — stops, with a neutral frame
//! sent to the virtual pad first. That ordering is load-bearing: if the pad
//! simply went quiet, the game would keep the last state it was given, and the
//! last state at the moment of the toggle has Start and Select held down. Games
//! read Start as pause. So mouse mode would open the pause menu on its way in,
//! and leaving it would open the menu again on its way out.
//!
//! For the same reason the chord is **swallowed until both buttons are
//! released**. The toggle fires on the press, but the player is still holding
//! the buttons for some tens of milliseconds afterward, and those frames must
//! not reach the game either.
//!
//! ## Why a driver thread rather than moving on each packet
//!
//! A stick is a *velocity*, not a position: holding it half-right means "keep
//! moving right at half speed", which only makes sense against elapsed time.
//! Moving the cursor once per arriving packet would tie cursor speed to the
//! client's input packet rate — so the same stick deflection would travel at
//! different speeds on different clients, and stall completely whenever the
//! client had nothing new to send (which is exactly what a *held* stick looks
//! like on a client that only transmits on change).
//!
//! So the packet path only records the stick position, and a dedicated thread
//! integrates it at a fixed [`TICK`]. Cursor speed then depends on the stick
//! and the clock, and on nothing else.
//!
//! ## Sub-pixel accumulation
//!
//! `SendInput` moves in whole pixels. At a 125 Hz tick, anything under 125
//! px/s rounds to zero every tick and the cursor simply does not move — which
//! removes precisely the slow, careful movement a stick is worst at and most
//! needs. The remainder is carried between ticks instead, so a gentle push
//! moves the cursor slowly rather than not at all.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use crate::input::ControllerInput;

/// `NV_MULTI_CONTROLLER_PACKET` button bits. Identical to XInput's — Nova
/// already relies on that by handing `button_flags` straight to ViGEm's
/// `XButtons`, so these are the same numbers viewed from the wire side.
const BTN_START: u16 = 0x0010;
const BTN_SELECT: u16 = 0x0020;
const CHORD: u16 = BTN_START | BTN_SELECT;

/// How often the cursor is integrated. 125 Hz matches a typical USB mouse's
/// report rate: fast enough that motion reads as continuous, slow enough that
/// the thread is invisible next to the capture loop it shares a machine with.
const TICK: Duration = Duration::from_millis(8);

/// Stick deflection below this is treated as centred. Sticks rest a little off
/// centre and wear worse with age; without this the cursor drifts on its own,
/// which is the single most irritating way for this feature to fail.
const DEADZONE: f32 = 0.13;

/// Cursor speed at full deflection, in pixels per second. Chosen to cross a
/// 4K width in about two and a half seconds — quick enough to be usable,
/// slow enough to stay controllable at the far end of the curve.
const MAX_SPEED_PX_S: f32 = 1600.0;

/// Exponent applied to the deflection after the deadzone. Above 1 it buys
/// precision where a stick is weakest: small pushes stay slow and controllable,
/// while full deflection still reaches [`MAX_SPEED_PX_S`]. Linear response
/// makes fine positioning nearly impossible.
const RESPONSE_CURVE: f32 = 2.0;

/// Trigger travel past which a trigger counts as pressed. Triggers rest at 0
/// and are analogue; this is a click, so it needs one threshold and a little
/// hysteresis would only add ways to leave a button stuck.
const TRIGGER_THRESHOLD: u8 = 96;

/// What the caller should do with the packet it just handed to [`intercept`].
pub enum Verdict {
    /// Forward to the virtual pad unchanged.
    Forward(ControllerInput),
    /// Forward this **neutral** frame instead — everything released. Used on
    /// both toggle edges and while the chord is still held, so the game never
    /// sees Start or Select from a press that was meant for Nova.
    Release(ControllerInput),
    /// Mouse mode owns this packet; the game sees nothing.
    Swallow,
}

/// Live cursor-driving state, shared with the driver thread.
struct Shared {
    /// Right stick, as most recently reported. Stored as the raw wire i16s so
    /// the packet path does no arithmetic.
    stick_x: AtomicI32,
    stick_y: AtomicI32,
    /// Bumped every time mouse mode is entered. The driver thread compares it
    /// so a stale generation cannot keep moving a cursor for a session that has
    /// already ended.
    generation: AtomicU32,
}

static ACTIVE: AtomicBool = AtomicBool::new(false);
static SHARED: Shared = Shared {
    stick_x: AtomicI32::new(0),
    stick_y: AtomicI32::new(0),
    generation: AtomicU32::new(0),
};

/// Per-controller edge-detection state. Behind one mutex because it is touched
/// only on the input path, at packet rate, and the alternative — an atomic per
/// field per pad — buys nothing measurable and costs clarity.
#[derive(Default)]
struct EdgeState {
    /// Whether the chord was held on the previous packet, so the toggle fires
    /// once per press rather than continuously while held.
    chord_held: bool,
    /// Set at every toggle, cleared once BOTH chord buttons are released.
    swallow_until_release: bool,
    left_click: bool,
    right_click: bool,
}

fn edges() -> &'static Mutex<EdgeState> {
    static EDGES: OnceLock<Mutex<EdgeState>> = OnceLock::new();
    EDGES.get_or_init(|| Mutex::new(EdgeState::default()))
}

/// Whether mouse mode is currently on.
pub fn is_active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

/// Inspect a gamepad packet before it reaches the virtual pad.
///
/// This is the entire public surface on the input path: everything about the
/// mode — entering, leaving, cursor movement, clicks — is decided here or by
/// the thread this starts.
pub fn intercept(input: ControllerInput) -> Verdict {
    let mut st = edges().lock().unwrap_or_else(|e| e.into_inner());

    let chord = input.button_flags & CHORD == CHORD;
    let toggled = chord && !st.chord_held;
    st.chord_held = chord;

    if toggled {
        st.swallow_until_release = true;
        let now_active = !ACTIVE.fetch_xor(true, Ordering::SeqCst);
        if now_active {
            SHARED.stick_x.store(0, Ordering::Relaxed);
            SHARED.stick_y.store(0, Ordering::Relaxed);
            SHARED.generation.fetch_add(1, Ordering::SeqCst);
            start_driver();
            println!("🖱️  Gamepad: mouse mode ON (Start+Select) — pad input held from the game");
        } else {
            // Leaving with a trigger still held would strand a mouse button
            // down on the host with nothing left to release it.
            release_clicks(&mut st);
            println!("🖱️  Gamepad: mouse mode OFF — pad input resumed");
        }
        // Whichever direction the toggle went, the game must see the chord
        // released rather than pressed.
        return Verdict::Release(neutral(&input));
    }

    if st.swallow_until_release {
        if input.button_flags & CHORD == 0 {
            st.swallow_until_release = false;
        } else if !ACTIVE.load(Ordering::Relaxed) {
            // Still holding the chord after leaving mouse mode: keep the pad
            // neutral so the game does not see a late Start.
            return Verdict::Release(neutral(&input));
        }
    }

    if !ACTIVE.load(Ordering::Relaxed) {
        return Verdict::Forward(input);
    }

    // Mouse mode owns the packet from here. The stick is a velocity the driver
    // thread integrates; the triggers are events, so they are edge-detected
    // here where the packet already is.
    SHARED.stick_x.store(input.right_stick_x as i32, Ordering::Relaxed);
    SHARED.stick_y.store(input.right_stick_y as i32, Ordering::Relaxed);

    let want_left = input.right_trigger >= TRIGGER_THRESHOLD;
    let want_right = input.left_trigger >= TRIGGER_THRESHOLD;
    if want_left != st.left_click {
        st.left_click = want_left;
        crate::input::inject_mouse_button_direct(crate::input::MouseButton::Left, want_left);
    }
    if want_right != st.right_click {
        st.right_click = want_right;
        crate::input::inject_mouse_button_direct(crate::input::MouseButton::Right, want_right);
    }

    Verdict::Swallow
}

/// Release anything mouse mode is holding and hand the pad back.
///
/// Called from `input::stop_session`, for the same reason held modifiers and
/// touch contacts are released there: a client that vanished mid-session leaves
/// the host holding input it can never take back itself, and a stuck mouse
/// button is one the operator has to fix with a physical mouse.
pub fn stop_session() {
    let mut st = edges().lock().unwrap_or_else(|e| e.into_inner());
    release_clicks(&mut st);
    *st = EdgeState::default();
    if ACTIVE.swap(false, Ordering::SeqCst) {
        SHARED.generation.fetch_add(1, Ordering::SeqCst);
        println!("🖱️  Gamepad: mouse mode released — session ended");
    }
}

fn release_clicks(st: &mut EdgeState) {
    if st.left_click {
        st.left_click = false;
        crate::input::inject_mouse_button_direct(crate::input::MouseButton::Left, false);
    }
    if st.right_click {
        st.right_click = false;
        crate::input::inject_mouse_button_direct(crate::input::MouseButton::Right, false);
    }
}

/// The same packet with everything released, keeping the routing fields so the
/// virtual pad it lands on is the one it came from.
fn neutral(input: &ControllerInput) -> ControllerInput {
    ControllerInput {
        controller_number: input.controller_number,
        active_gamepad_mask: input.active_gamepad_mask,
        button_flags: 0,
        left_trigger: 0,
        right_trigger: 0,
        left_stick_x: 0,
        left_stick_y: 0,
        right_stick_x: 0,
        right_stick_y: 0,
    }
}

/// Map one axis' raw wire value to pixels per second.
///
/// Split out and pure so the curve is testable without a cursor: the deadzone,
/// the rescale that keeps the curve continuous at its edge, and the exponent
/// are the three things most likely to be got wrong.
fn axis_speed(raw: i32) -> f32 {
    // i16::MIN has no positive counterpart; clamping keeps full deflection
    // symmetric instead of one pixel faster to the left.
    let norm = (raw as f32 / i16::MAX as f32).clamp(-1.0, 1.0);
    let magnitude = norm.abs();
    if magnitude <= DEADZONE {
        return 0.0;
    }
    // Rescale so movement starts at zero speed just outside the deadzone
    // rather than jumping straight to DEADZONE's share of full speed.
    let scaled = (magnitude - DEADZONE) / (1.0 - DEADZONE);
    scaled.powf(RESPONSE_CURVE) * MAX_SPEED_PX_S * norm.signum()
}

/// Start the cursor driver for this activation, if it is not already running.
fn start_driver() {
    static RUNNING: AtomicBool = AtomicBool::new(false);
    if RUNNING.swap(true, Ordering::SeqCst) {
        return; // a driver is already live; the generation bump retargets it
    }
    let generation = SHARED.generation.load(Ordering::SeqCst);

    std::thread::Builder::new()
        .name("nova-gamepad-mouse".into())
        .spawn(move || {
            // Desktop attachment is thread-local and thread-affine (see
            // input::sync_desktop_for_input), so this thread must attach
            // itself. Doing it once here rather than per tick: the shared
            // resync in send_input_synced covers a desktop switch mid-mode.
            crate::input::sync_desktop_for_input(false);

            // Carried between ticks so movement below one pixel per tick still
            // accumulates into motion instead of rounding away.
            let (mut rem_x, mut rem_y) = (0.0f32, 0.0f32);
            let mut gen = generation;

            loop {
                std::thread::sleep(TICK);

                let current = SHARED.generation.load(Ordering::SeqCst);
                if !ACTIVE.load(Ordering::Relaxed) {
                    RUNNING.store(false, Ordering::SeqCst);
                    return;
                }
                if current != gen {
                    // A new activation: drop the old remainder so a stale
                    // fraction cannot nudge the cursor on the way in.
                    gen = current;
                    rem_x = 0.0;
                    rem_y = 0.0;
                }

                let dt = TICK.as_secs_f32();
                let vx = axis_speed(SHARED.stick_x.load(Ordering::Relaxed));
                // Stick up is positive; screen Y grows downward.
                let vy = -axis_speed(SHARED.stick_y.load(Ordering::Relaxed));

                rem_x += vx * dt;
                rem_y += vy * dt;
                let dx = rem_x.trunc();
                let dy = rem_y.trunc();
                rem_x -= dx;
                rem_y -= dy;

                if dx != 0.0 || dy != 0.0 {
                    crate::input::inject_mouse_move_relative(dx as i32, dy as i32);
                }
            }
        })
        .map_err(|e| {
            RUNNING.store(false, Ordering::SeqCst);
            println!("⚠️  Gamepad: could not start the mouse-mode driver ({e}) — mode disabled");
            ACTIVE.store(false, Ordering::SeqCst);
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pad(button_flags: u16) -> ControllerInput {
        ControllerInput {
            controller_number: 0,
            active_gamepad_mask: 1,
            button_flags,
            left_trigger: 0,
            right_trigger: 0,
            left_stick_x: 0,
            left_stick_y: 0,
            right_stick_x: 0,
            right_stick_y: 0,
        }
    }

    /// Tests share process-global mode state, so they must not interleave.
    fn reset() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        let guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        ACTIVE.store(false, Ordering::SeqCst);
        *edges().lock().unwrap_or_else(|e| e.into_inner()) = EdgeState::default();
        guard
    }

    #[test]
    fn the_deadzone_holds_a_resting_stick_still() {
        // A stick resting slightly off centre must not walk the cursor across
        // the screen on its own.
        assert_eq!(axis_speed(0), 0.0);
        assert_eq!(axis_speed((i16::MAX as f32 * 0.10) as i32), 0.0);
        assert_eq!(axis_speed(-(i16::MAX as f32 * 0.10) as i32), 0.0);
    }

    #[test]
    fn the_curve_is_continuous_and_symmetric() {
        // Just outside the deadzone movement starts from nothing rather than
        // jumping, and full deflection reaches the stated top speed.
        let just_outside = axis_speed((i16::MAX as f32 * (DEADZONE + 0.005)) as i32);
        assert!(just_outside > 0.0 && just_outside < 5.0, "got {just_outside}");
        assert!((axis_speed(i16::MAX as i32) - MAX_SPEED_PX_S).abs() < 1.0);
        // i16::MIN has no positive twin; left must not be faster than right.
        assert!((axis_speed(i16::MIN as i32) + MAX_SPEED_PX_S).abs() < 1.0);
    }

    /// The headline behaviour: the chord toggles, and it toggles once per press
    /// rather than on every packet while it is held.
    #[test]
    fn the_chord_toggles_once_per_press() {
        let _g = reset();
        assert!(!is_active());

        assert!(matches!(intercept(pad(CHORD)), Verdict::Release(_)));
        assert!(is_active(), "the chord did not enter mouse mode");

        // Held, not re-pressed: must not toggle straight back out. It is
        // swallowed rather than released, because the pad was already
        // neutralised on the toggle edge and mouse mode now owns every frame.
        assert!(matches!(intercept(pad(CHORD)), Verdict::Swallow));
        assert!(is_active(), "a held chord toggled twice");

        assert!(matches!(intercept(pad(0)), Verdict::Swallow));
        assert!(is_active());

        assert!(matches!(intercept(pad(CHORD)), Verdict::Release(_)));
        assert!(!is_active(), "the chord did not leave mouse mode");
    }

    /// The game must never see the chord itself — on the way in it would read
    /// as pause, and on the way out it would read as pause again.
    #[test]
    fn the_game_never_sees_the_chord() {
        let _g = reset();

        // Entering: neutral, not the pressed chord.
        match intercept(pad(CHORD)) {
            Verdict::Release(n) => assert_eq!(n.button_flags, 0),
            _ => panic!("entering must release the pad"),
        }
        // Leave, still holding the buttons.
        assert!(matches!(intercept(pad(0)), Verdict::Swallow));
        assert!(matches!(intercept(pad(CHORD)), Verdict::Release(_)));
        assert!(!is_active());

        // Chord still physically held after the toggle: the pad stays neutral
        // rather than handing the game a Start press.
        match intercept(pad(CHORD | 0x1000)) {
            Verdict::Release(n) => assert_eq!(n.button_flags, 0, "late chord reached the game"),
            _ => panic!("a held chord must keep the pad neutral"),
        }
        // Released, so ordinary forwarding resumes.
        match intercept(pad(0x1000)) {
            Verdict::Forward(f) => assert_eq!(f.button_flags, 0x1000),
            _ => panic!("forwarding did not resume after the chord was released"),
        }
    }

    #[test]
    fn ordinary_input_is_forwarded_untouched_when_the_mode_is_off() {
        let _g = reset();
        let mut input = pad(0x1000 | BTN_START); // A + Start, no Select
        input.right_stick_x = 20_000;
        match intercept(input) {
            Verdict::Forward(f) => {
                assert_eq!(f.button_flags, 0x1000 | BTN_START);
                assert_eq!(f.right_stick_x, 20_000, "the stick must reach the game intact");
            }
            _ => panic!("Start alone is not the chord and must forward"),
        }
        assert!(!is_active(), "Start alone must not enter mouse mode");
    }

    /// A session that ends mid-mode must not leave the host in mouse mode, and
    /// must not leave a mouse button held down.
    #[test]
    fn ending_a_session_releases_the_mode() {
        let _g = reset();
        intercept(pad(CHORD));
        assert!(is_active());
        stop_session();
        assert!(!is_active());
        // And the next packet forwards normally.
        assert!(matches!(intercept(pad(0x2000)), Verdict::Forward(_)));
    }
}

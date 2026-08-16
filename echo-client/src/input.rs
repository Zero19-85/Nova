//! Building the input packets Nova already knows how to inject.
//!
//! ## Why the GameStream wire format, and not something nicer
//!
//! Nova's host-side injection stack is large and hard-won: `input.rs` handles
//! UIPI, secure-desktop resynchronisation, modifier bracketing, ViGEm gamepad
//! emulation, and the SYSTEM-helper detour that makes typing at the Windows
//! logon screen work. All of it hangs off one function that takes a
//! GameStream-format packet, and the Master's routing (helper vs Worker, and
//! the gamepad exception) keys off that packet's bytes.
//!
//! Emitting the same bytes means Echo inherits every one of those behaviours
//! for free and cannot drift from the Moonlight path. A cleaner Echo-native
//! encoding would have meant a second injection path, a second set of
//! secure-desktop bugs, and two formats to keep in agreement forever.
//!
//! ## Endianness is not uniform, and that is not a mistake here
//!
//! The header magic is little-endian; mouse coordinates and scroll amounts are
//! big-endian; the keyboard's key code is little-endian again. That is what the
//! protocol does, mirrored from `moonlight-android`'s packet classes, and the
//! host parses exactly this. Every field below is annotated with the order it
//! is written in, because a silent endianness flip produces input that is
//! plausible but wrong — the most expensive kind to debug.

/// `NV_INPUT_HEADER` is 8 bytes: a length, then the packet magic.
const HEADER_LEN: usize = 8;

// Magics, matching `nova-server/src/input.rs`. Kept as their own constants
// rather than imported: `echo-client` must not depend on the host crate, and
// these are wire values that can only change by breaking the protocol.
const KEY_DOWN: u32 = 0x0000_0003;
const KEY_UP: u32 = 0x0000_0004;
const MOUSE_MOVE_ABS: u32 = 0x0000_0005;
const MOUSE_MOVE_REL: u32 = 0x0000_0007;
const MOUSE_BUTTON_DOWN: u32 = 0x0000_0008;
const MOUSE_BUTTON_UP: u32 = 0x0000_0009;
const SCROLL: u32 = 0x0000_000A;

/// Mouse buttons, matching `moonlight-android`'s `MouseButtonPacket`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MouseButton {
    Left = 1,
    Middle = 2,
    Right = 3,
    X1 = 4,
    X2 = 5,
}

impl MouseButton {
    /// Map a small integer from the JNI boundary. Returns `None` rather than
    /// defaulting, so a bad value is a refused call and not a stray click.
    pub fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::Left),
            2 => Some(Self::Middle),
            3 => Some(Self::Right),
            4 => Some(Self::X1),
            5 => Some(Self::X2),
            _ => None,
        }
    }
}

/// Start a packet: 4-byte length of everything after this field, then the
/// magic. Both little-endian.
fn header(magic: u32, body_len: usize) -> Vec<u8> {
    let mut p = Vec::with_capacity(HEADER_LEN + body_len);
    // The host reads the magic at [4..8] and derives everything else from the
    // packet's real length, so this field is informational — it is written
    // correctly anyway, because a wrong value would mislead anyone reading a
    // capture.
    p.extend_from_slice(&((body_len + 4) as u32).to_le_bytes());
    p.extend_from_slice(&magic.to_le_bytes());
    p
}

/// Relative mouse motion — the one games actually read, via raw input.
///
/// Returns `None` for a zero delta: the host discards those anyway, and not
/// sending them keeps a resting mouse completely silent on the wire.
pub fn mouse_move_relative(dx: i16, dy: i16) -> Option<Vec<u8>> {
    if dx == 0 && dy == 0 {
        return None;
    }
    let mut p = header(MOUSE_MOVE_REL, 4);
    p.extend_from_slice(&dx.to_be_bytes()); // big-endian
    p.extend_from_slice(&dy.to_be_bytes());
    Some(p)
}

/// Absolute cursor position, as a fraction of the client's own view.
///
/// The host divides `x/width` and `y/height` and applies the result to the
/// capture rect, so `width`/`height` must describe the surface the user
/// actually touched — not the stream's resolution, which may differ once the
/// video is letterboxed into a window.
///
/// Layout note: the host reads width and height from the **end** of the packet
/// to tolerate protocol variants that insert a reserved field. This builds the
/// 16-byte form, where the two readings coincide.
pub fn mouse_move_absolute(x: i16, y: i16, width: i16, height: i16) -> Option<Vec<u8>> {
    if width <= 0 || height <= 0 {
        return None; // a zero reference would divide by zero on the host
    }
    let mut p = header(MOUSE_MOVE_ABS, 8);
    p.extend_from_slice(&x.to_be_bytes());
    p.extend_from_slice(&y.to_be_bytes());
    p.extend_from_slice(&width.to_be_bytes()); // at len-4
    p.extend_from_slice(&height.to_be_bytes()); // at len-2
    Some(p)
}

/// Press or release a mouse button.
pub fn mouse_button(button: MouseButton, down: bool) -> Vec<u8> {
    let mut p = header(if down { MOUSE_BUTTON_DOWN } else { MOUSE_BUTTON_UP }, 1);
    p.push(button as u8);
    p
}

/// Vertical scroll, in WHEEL_DELTA units (120 per notch) — the host passes the
/// amount to `SendInput` unchanged.
pub fn scroll(amount: i16) -> Option<Vec<u8>> {
    if amount == 0 {
        return None;
    }
    let mut p = header(SCROLL, 2);
    p.extend_from_slice(&amount.to_be_bytes()); // big-endian
    Some(p)
}

/// Press or release a key.
///
/// `key_code` is a **Windows virtual-key code**; the host masks it to its low
/// byte. `modifiers` is the GameStream modifier bitmask, which the host uses to
/// bracket synthetic shifted characters — it is not a substitute for sending
/// the modifier keys' own down/up events.
pub fn keyboard(key_code: u16, modifiers: u8, down: bool) -> Vec<u8> {
    let mut p = header(if down { KEY_DOWN } else { KEY_UP }, 4);
    // keyAction. The host ignores it (the magic already says down or up), but
    // it is part of the packet and captures should read correctly.
    p.push(if down { 0x03 } else { 0x04 });
    p.extend_from_slice(&key_code.to_le_bytes()); // little-endian, unlike the mouse
    p.push(modifiers);
    p
}

/// Release everything that could be held down.
///
/// A key-down travels as its own packet, so anything that stops input between
/// the down and the up strands that key held on the host — backgrounding the
/// app mid-keystroke, toggling input off, or ending the session. A stuck
/// modifier is not merely confusing: held Ctrl or Alt turns ordinary typing
/// into shortcuts, and Windows has accessibility combinations that a stuck
/// modifier can reach by accident.
///
/// Idempotent by construction — releasing a key that is not held is a no-op on
/// the host — so this can be sent whenever there is any doubt.
pub fn release_all() -> Vec<Vec<u8>> {
    // Windows virtual-key codes for both sides of every modifier, matching the
    // ones `Keycodes` can produce.
    const MODIFIERS: [u16; 8] = [
        0xA0, // VK_LSHIFT
        0xA1, // VK_RSHIFT
        0xA2, // VK_LCONTROL
        0xA3, // VK_RCONTROL
        0xA4, // VK_LMENU  (left Alt)
        0xA5, // VK_RMENU  (right Alt)
        0x5B, // VK_LWIN
        0x5C, // VK_RWIN
    ];

    let mut out = Vec::with_capacity(MODIFIERS.len() + 5);
    for vk in MODIFIERS {
        out.push(keyboard(vk, 0, false));
    }
    for button in [
        MouseButton::Left,
        MouseButton::Middle,
        MouseButton::Right,
        MouseButton::X1,
        MouseButton::X2,
    ] {
        out.push(mouse_button(button, false));
    }
    out
}

/// Drop what the host cannot use, and supersede what is already stale.
///
/// ## Why relative motion is deliberately NOT merged
///
/// It used to be, and that was the cause of a pointer that "hops". Summing a
/// run of small deltas into one large one preserves the total distance but
/// destroys the *shape* of the motion: the host performs a single `SendInput`
/// of a big jump instead of several small ones, so the cursor teleports between
/// resting points instead of sweeping. Live 2026-08-16 the host was applying
/// 60–90 relative packets per second while Android delivered several times
/// that — every merged packet was a visible hop.
///
/// Merging bought exactly one thing: fewer messages, which mattered only while
/// input rode the reliable control channel and each message cost a round trip.
/// On unreliable datagrams a batch already carries many packets, so individual
/// deltas travel in the *same* datagram at a cost of 14 bytes each. There is
/// nothing left to buy and smoothness to lose.
///
/// Absolute positions are still superseded, because that is lossless: only the
/// newest position means anything, and replaying older ones would drag the
/// pointer backwards through places the user never pointed.
pub fn coalesce(packets: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = Vec::with_capacity(packets.len());

    for packet in packets {
        let magic = magic_of(&packet);
        let previous = out.last().map(|p| magic_of(p));

        match (magic, previous) {
            (Some(MOUSE_MOVE_ABS), Some(Some(MOUSE_MOVE_ABS))) => {
                // Superseded: an older position tells the host nothing once a
                // newer one exists.
                *out.last_mut().expect("previous exists") = packet;
            }
            _ => out.push(packet),
        }
    }

    // Zero motion is a packet the host would discard; dropping it saves the
    // bytes.
    out.retain(|p| match magic_of(p) {
        Some(MOUSE_MOVE_REL) => {
            i16::from_be_bytes([p[8], p[9]]) != 0 || i16::from_be_bytes([p[10], p[11]]) != 0
        }
        _ => true,
    });
    out
}

// ── Backlog accounting ──────────────────────────────────────────────────────

/// Largest batch the sender has drained in one tick this session, and the most
/// recent one.
///
/// A process-wide pair rather than plumbed-through state, because the client
/// runs exactly one session at a time and the alternative was threading a
/// counter through four signatures to reach the JNI stats call.
///
/// These exist to answer, from the client side, the question the host cannot:
/// **is input queueing here?** A pointer that keeps moving after the hand stops
/// is a backlog draining, and the only way to tell a backlog from a slow link
/// is to look at how much work each tick finds waiting. A healthy tick drains a
/// handful; a tick that repeatedly drains its cap is behind, and every packet
/// after that point is describing motion the user already finished making.
static BATCH_HIGH_WATER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static BATCH_LAST: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Record how many packets one tick drained, before coalescing.
pub fn record_batch(len: usize) {
    use std::sync::atomic::Ordering;
    BATCH_LAST.store(len, Ordering::Relaxed);
    BATCH_HIGH_WATER.fetch_max(len, Ordering::Relaxed);
}

/// `(most recent batch, largest batch this session)`.
pub fn batch_stats() -> (usize, usize) {
    use std::sync::atomic::Ordering;
    (BATCH_LAST.load(Ordering::Relaxed), BATCH_HIGH_WATER.load(Ordering::Relaxed))
}

/// Peak input-event rate and batch size observed by the platform layer, plus
/// whether pointer capture is held.
///
/// Set from the UI side, which is the only place that can see them, so the
/// session task can fold them into the report it sends the host. The point is
/// to stop the user having to read numbers off a phone screen and retype them —
/// four rounds of debugging arrived as fragmented transcriptions, and a figure
/// that has to be copied by hand is one that gets copied wrong or not at all.
static UI_PEAK_RATE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static UI_PEAK_SAMPLES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static UI_CAPTURE_HELD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn record_ui_state(peak_rate: u32, peak_samples: u32, capture_held: bool) {
    use std::sync::atomic::Ordering;
    UI_PEAK_RATE.store(peak_rate, Ordering::Relaxed);
    UI_PEAK_SAMPLES.store(peak_samples, Ordering::Relaxed);
    UI_CAPTURE_HELD.store(capture_held, Ordering::Relaxed);
}

/// `(peak events/sec, peak samples per event, capture held)`.
pub fn ui_state() -> (u32, u32, bool) {
    use std::sync::atomic::Ordering;
    (
        UI_PEAK_RATE.load(Ordering::Relaxed),
        UI_PEAK_SAMPLES.load(Ordering::Relaxed),
        UI_CAPTURE_HELD.load(Ordering::Relaxed),
    )
}

fn magic_of(packet: &[u8]) -> Option<u32> {
    packet
        .get(4..8)
        .map(|b| u32::from_le_bytes(b.try_into().expect("4 bytes")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The host reads the magic from bytes 4..8, little-endian. Every builder
    /// must agree, or input is silently dropped as "unrecognized magic".
    fn magic_of(packet: &[u8]) -> u32 {
        u32::from_le_bytes(packet[4..8].try_into().unwrap())
    }

    #[test]
    fn every_packet_carries_the_magic_the_host_dispatches_on() {
        assert_eq!(magic_of(&mouse_move_relative(1, 1).unwrap()), MOUSE_MOVE_REL);
        assert_eq!(magic_of(&mouse_move_absolute(1, 1, 2, 2).unwrap()), MOUSE_MOVE_ABS);
        assert_eq!(magic_of(&mouse_button(MouseButton::Left, true)), MOUSE_BUTTON_DOWN);
        assert_eq!(magic_of(&mouse_button(MouseButton::Left, false)), MOUSE_BUTTON_UP);
        assert_eq!(magic_of(&scroll(120).unwrap()), SCROLL);
        assert_eq!(magic_of(&keyboard(0x41, 0, true)), KEY_DOWN);
        assert_eq!(magic_of(&keyboard(0x41, 0, false)), KEY_UP);
    }

    /// Each builder must meet the host's minimum length, or `handle_input_packet`
    /// returns early and the input vanishes with no error anywhere.
    #[test]
    fn packets_meet_the_hosts_minimum_lengths() {
        assert!(mouse_move_relative(1, 1).unwrap().len() >= 12);
        assert!(mouse_move_absolute(1, 1, 2, 2).unwrap().len() >= 12);
        assert!(mouse_button(MouseButton::Left, true).len() >= 9);
        assert!(scroll(1).unwrap().len() >= 10);
        assert!(keyboard(0x41, 0, true).len() >= 12);
    }

    /// Mouse deltas are big-endian. A flip here still produces motion, just in
    /// wildly wrong directions and magnitudes — which reads as "the mouse is
    /// broken" rather than "the bytes are backwards".
    #[test]
    fn mouse_deltas_are_big_endian_and_survive_negative_values() {
        let p = mouse_move_relative(-2, 300).unwrap();
        assert_eq!(i16::from_be_bytes([p[8], p[9]]), -2);
        assert_eq!(i16::from_be_bytes([p[10], p[11]]), 300);
    }

    /// The host reads width and height from the END of the packet, not a fixed
    /// offset. This is the invariant that keeps that legal.
    #[test]
    fn absolute_reference_dimensions_sit_at_the_end_where_the_host_looks() {
        let p = mouse_move_absolute(10, 20, 1920, 1080).unwrap();
        let len = p.len();
        assert_eq!(i16::from_be_bytes([p[8], p[9]]), 10);
        assert_eq!(i16::from_be_bytes([p[10], p[11]]), 20);
        assert_eq!(i16::from_be_bytes([p[len - 4], p[len - 3]]), 1920);
        assert_eq!(i16::from_be_bytes([p[len - 2], p[len - 1]]), 1080);
    }

    /// The key code is little-endian here while mouse coordinates are big —
    /// asserted explicitly because the inconsistency is the trap.
    #[test]
    fn the_key_code_is_little_endian_unlike_the_mouse_fields() {
        let p = keyboard(0x1234, 0x02, true);
        assert_eq!(u16::from_le_bytes([p[9], p[10]]), 0x1234);
        assert_eq!(p[11], 0x02, "modifier mask");
    }

    /// A resting mouse must be silent: these are sent at pointer-event rates,
    /// and a no-op packet per event would be pure overhead on the control path.
    #[test]
    fn no_op_events_produce_no_packet() {
        assert!(mouse_move_relative(0, 0).is_none());
        assert!(scroll(0).is_none());
        assert!(mouse_move_absolute(1, 1, 0, 100).is_none(), "zero reference is refused");
    }

    /// Relative motion must survive as individual deltas.
    ///
    /// This is the regression that guards the "hopping pointer" fix: summing a
    /// burst into one large delta preserves distance but not smoothness, and
    /// the host turns each packet into one `SendInput`, so merged packets are
    /// visible teleports.
    #[test]
    fn a_burst_of_motion_keeps_every_delta_so_the_pointer_sweeps() {
        let burst: Vec<Vec<u8>> = (0..50)
            .map(|_| mouse_move_relative(3, -2).unwrap())
            .collect();

        let out = coalesce(burst);
        assert_eq!(out.len(), 50, "every sample is a separate movement of the cursor");
        for p in &out {
            assert_eq!(i16::from_be_bytes([p[8], p[9]]), 3);
            assert_eq!(i16::from_be_bytes([p[10], p[11]]), -2);
        }
    }

    /// Order against clicks must be exact: a click has to land where the
    /// pointer was when the user pressed it.
    #[test]
    fn order_is_preserved_around_clicks() {
        let out = coalesce(vec![
            mouse_move_relative(1, 0).unwrap(),
            mouse_move_relative(2, 0).unwrap(),
            mouse_button(MouseButton::Left, true),
            mouse_move_relative(5, 0).unwrap(),
            mouse_button(MouseButton::Left, false),
        ]);

        assert_eq!(out.len(), 5, "nothing is merged away");
        assert_eq!(magic_of(&out[0]), MOUSE_MOVE_REL);
        assert_eq!(magic_of(&out[1]), MOUSE_MOVE_REL);
        assert_eq!(magic_of(&out[2]), MOUSE_BUTTON_DOWN);
        assert_eq!(i16::from_be_bytes([out[3][8], out[3][9]]), 5, "the move after the click");
        assert_eq!(magic_of(&out[4]), MOUSE_BUTTON_UP);
    }

    /// Absolute positions supersede rather than accumulate — summing them would
    /// send the pointer off to the corner of the world.
    #[test]
    fn absolute_positions_keep_only_the_newest() {
        let out = coalesce(vec![
            mouse_move_absolute(10, 10, 100, 100).unwrap(),
            mouse_move_absolute(20, 20, 100, 100).unwrap(),
            mouse_move_absolute(30, 40, 100, 100).unwrap(),
        ]);
        assert_eq!(out.len(), 1);
        assert_eq!(i16::from_be_bytes([out[0][8], out[0][9]]), 30);
        assert_eq!(i16::from_be_bytes([out[0][10], out[0][11]]), 40);
    }

    /// Motion that nets out to nothing is still real motion and must be sent.
    ///
    /// The opposite used to be true, back when runs were summed. It was wrong
    /// even then for anything but a resting hand: a flick out and back is two
    /// movements the user made and the host should reproduce, not a no-op.
    #[test]
    fn motion_that_cancels_out_is_still_two_movements() {
        let out = coalesce(vec![
            mouse_move_relative(5, 5).unwrap(),
            mouse_move_relative(-5, -5).unwrap(),
        ]);
        assert_eq!(out.len(), 2, "a flick out and back is not a no-op");
    }

    /// Keystrokes must survive a burst untouched — they are not idempotent and
    /// merging any two of them would lose characters.
    #[test]
    fn keystrokes_are_never_merged() {
        let out = coalesce(vec![
            keyboard(0x41, 0, true),
            keyboard(0x41, 0, false),
            keyboard(0x42, 0, true),
            keyboard(0x42, 0, false),
        ]);
        assert_eq!(out.len(), 4, "every key event is meaningful on its own");
    }

    /// Every modifier and button must be released, and every packet must be an
    /// *up* — a stray down here would create the exact stuck key this is meant
    /// to clear.
    #[test]
    fn release_all_sends_only_up_events_for_every_modifier_and_button() {
        let packets = release_all();

        for p in &packets {
            let magic = magic_of(p);
            assert!(
                magic == KEY_UP || magic == MOUSE_BUTTON_UP,
                "release_all must only ever release, got magic {magic:#x}"
            );
        }

        let keys: Vec<u16> = packets
            .iter()
            .filter(|p| magic_of(p) == KEY_UP)
            .map(|p| u16::from_le_bytes([p[9], p[10]]))
            .collect();
        for vk in [0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0x5B, 0x5C] {
            assert!(keys.contains(&vk), "modifier {vk:#x} must be released");
        }

        let buttons: Vec<u8> = packets
            .iter()
            .filter(|p| magic_of(p) == MOUSE_BUTTON_UP)
            .map(|p| p[8])
            .collect();
        assert_eq!(buttons, vec![1, 2, 3, 4, 5], "all five buttons, in order");
    }

    /// It is sent at moments of doubt, so coalescing must not quietly discard
    /// any of it.
    #[test]
    fn release_all_survives_coalescing_intact() {
        let before = release_all();
        let after = coalesce(before.clone());
        assert_eq!(after, before, "no release may be merged away");
    }

    #[test]
    fn button_codes_round_trip_and_reject_nonsense() {
        for code in 1..=5u8 {
            let b = MouseButton::from_code(code).expect("valid button");
            assert_eq!(b as u8, code);
        }
        assert!(MouseButton::from_code(0).is_none());
        assert!(MouseButton::from_code(6).is_none());
    }
}

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

/// `NV_MULTI_CONTROLLER_PACKET`, magic `MULTI_CONTROLLER_MAGIC_GEN5`. Unlike
/// every other packet in this file, its body is little-endian throughout —
/// see [`gamepad`].
const MULTI_CONTROLLER: u32 = 0x0000_000C;

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
/// magic. **The length is big-endian and the magic is little-endian** — the
/// first of this protocol's several endianness reversals, and the one that is
/// easiest to get wrong because the two fields are adjacent.
fn header(magic: u32, body_len: usize) -> Vec<u8> {
    let mut p = Vec::with_capacity(HEADER_LEN + body_len);
    // `NV_INPUT_HEADER.size` is big-endian, matching the table at the top of
    // `nova-server/src/input.rs` and what Sunshine reads.
    //
    // Nova itself never looks at this field: it dispatches on the magic at
    // [4..8] and takes the length from the datagram framing, so the value here
    // is informational. That is exactly why it is worth writing correctly —
    // nothing will ever fail because it is wrong, so a wrong value survives
    // indefinitely and misleads whoever next reads a capture. It was
    // little-endian until 2026-08-22.
    p.extend_from_slice(&((body_len + 4) as u32).to_be_bytes());
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

// ── Gamepad ─────────────────────────────────────────────────────────────────

// The four sentinel fields a real client writes. Nova's parser reads only the
// magic and the value fields and never looks at these, so they exist to keep a
// packet capture honest — and so a stricter host later is not a mystery to
// debug.
const GAMEPAD_HEADER_B: i16 = 0x001A;
const GAMEPAD_MID_B: i16 = 0x0014;
const GAMEPAD_TAIL_A: i16 = 0x009C;
const GAMEPAD_TAIL_B: i16 = 0x0055;

/// One controller's complete physical state.
///
/// `NV_MULTI_CONTROLLER_PACKET` is a **snapshot, not an event**: every packet
/// carries all buttons, both triggers and all four axes, and the host applies
/// it wholesale to a ViGEm virtual pad. Android hands the platform layer
/// discrete `KeyEvent`s and batched `MotionEvent` axes instead, so whoever
/// calls [`gamepad`] owns the running state and re-sends the whole thing on any
/// change. There is no "press A" packet to send, and building one from a single
/// event would release everything else the user is holding.
///
/// [`Default`] is the neutral resting state — nothing held, sticks centred,
/// triggers released — which is what an arriving controller should announce.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GamepadState {
    /// XInput's `XINPUT_GAMEPAD` button bits, unchanged.
    ///
    /// GameStream's low 16 `buttonFlags` are bit-for-bit identical to XInput's,
    /// which is why the host hands them to `vigem_client::XButtons` with no
    /// translation table at all. Turning `KEYCODE_BUTTON_*` into these bits is
    /// the platform layer's job; nothing here reinterprets them.
    pub buttons: u16,
    /// Analog triggers, `0..=255`.
    pub left_trigger: u8,
    pub right_trigger: u8,
    /// Stick axes over the full `i16` range, **Y positive is up**.
    ///
    /// That is XInput's convention, and the host forwards these to ViGEm
    /// untouched. Android's `AXIS_Y` is positive *down*, so the platform layer
    /// must negate. A sign flip here reads as inverted look — a preference
    /// someone forgot to expose — rather than as a defect, which is what makes
    /// it worth stating at the type.
    pub left_stick_x: i16,
    pub left_stick_y: i16,
    pub right_stick_x: i16,
    pub right_stick_y: i16,
}

/// Build a 34-byte `NV_MULTI_CONTROLLER_PACKET`.
///
/// ## The body is little-endian — all of it
///
/// The mouse builders above write their coordinates big-endian and the keyboard
/// writes its key code little-endian. This packet is little-endian end to end.
/// That is not a tidier rule arriving late; it is what
/// `nova-server/src/input.rs::parse_multi_controller` reads. A flip produces
/// sticks that slam to the rails and buttons that fire at random — input that
/// looks like broken hardware rather than backwards bytes, which is the
/// expensive kind of wrong.
///
/// ## `controller_number` and `active_mask` are the plug, not the state
///
/// The host derives ViGEm's *plug and unplug* from `active_mask & (1 <<
/// controller_number)`: a set bit plugs a virtual Xbox 360 pad into that slot,
/// a clear bit unplugs it. So announcing an arriving controller is this packet
/// with its bit set and a [`GamepadState::default()`] body; removing one is the
/// same packet with the bit clear.
///
/// **Ending a session needs one clear-bit packet per slot that was ever live.**
/// [`release_all`] deliberately does not cover gamepads: it is a fixed list of
/// modifiers and mouse buttons precisely because it needs no state to be
/// correct, and which pads exist is exactly the state it refuses to carry.
///
/// The host ignores slots at or beyond its `MAX_PADS` of 4 without comment, so
/// `controller_number` must stay in `0..=3`.
///
/// Returns a packet unconditionally — unlike [`mouse_move_relative`] or
/// [`scroll`], an all-zero snapshot is meaningful rather than a no-op. It is
/// how a controller says everything was let go, and dropping it would strand
/// whatever was held at the moment the user released it.
pub fn gamepad(controller_number: u8, active_mask: u16, state: &GamepadState) -> Vec<u8> {
    // 26 bytes of body after the 8-byte header = 34 total. Offsets in the
    // comments are from the start of the packet, matching the table in
    // `nova-server/src/input.rs`.
    let mut p = header(MULTI_CONTROLLER, 26);
    p.extend_from_slice(&GAMEPAD_HEADER_B.to_le_bytes()); // @8  headerB
    p.extend_from_slice(&(controller_number as i16).to_le_bytes()); // @10 controllerNumber
    p.extend_from_slice(&active_mask.to_le_bytes()); // @12 activeGamepadMask
    p.extend_from_slice(&GAMEPAD_MID_B.to_le_bytes()); // @14 midB
    p.extend_from_slice(&state.buttons.to_le_bytes()); // @16 buttonFlags
    p.push(state.left_trigger); // @18 leftTrigger
    p.push(state.right_trigger); // @19 rightTrigger
    p.extend_from_slice(&state.left_stick_x.to_le_bytes()); // @20 leftStickX
    p.extend_from_slice(&state.left_stick_y.to_le_bytes()); // @22 leftStickY
    p.extend_from_slice(&state.right_stick_x.to_le_bytes()); // @24 rightStickX
    p.extend_from_slice(&state.right_stick_y.to_le_bytes()); // @26 rightStickY
    p.extend_from_slice(&GAMEPAD_TAIL_A.to_le_bytes()); // @28 tailA
    // @30 buttonFlags2 — Sunshine's extended buttons (paddles, touchpad, misc).
    // XInput has no equivalent, so Nova parses the packet without them and
    // forwards nothing to ViGEm. Zero until there is somewhere for them to go.
    p.extend_from_slice(&0u16.to_le_bytes());
    p.extend_from_slice(&GAMEPAD_TAIL_B.to_le_bytes()); // @32 tailB
    debug_assert_eq!(p.len(), 34, "the host reads fixed offsets up to 34");
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
            (Some(MULTI_CONTROLLER), Some(Some(MULTI_CONTROLLER)))
                if controller_number_of(&packet)
                    == out.last().and_then(|p| controller_number_of(p)) =>
            {
                // Superseded, for the same reason as an absolute position: the
                // packet is a full state snapshot, so an older one describes a
                // moment a newer one has already replaced in its entirety.
                //
                // Only for the *same* controller. Two pads are independent —
                // pad 0's snapshot says nothing about pad 1's, and letting one
                // supersede the other would drop a second player's input
                // whenever both were pushed in the same tick. That is also why
                // this compares against the immediately previous packet only,
                // exactly as the absolute case does: an interleaved two-player
                // burst simply does not collapse, which costs a few dozen bytes
                // and cannot cost anyone a button press.
                *out.last_mut().expect("previous exists") = packet;
            }
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

/// The controller slot a multi-controller packet addresses, or `None` for any
/// other packet.
///
/// Exists for [`coalesce`], so one pad's snapshots can supersede each other
/// without ever superseding another pad's. Reads offset 10 little-endian and
/// narrows exactly as the host does.
fn controller_number_of(packet: &[u8]) -> Option<u8> {
    if magic_of(packet) != Some(MULTI_CONTROLLER) {
        return None;
    }
    packet
        .get(10..12)
        .map(|b| i16::from_le_bytes(b.try_into().expect("2 bytes")) as u8)
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
        assert_eq!(magic_of(&gamepad(0, 0x0001, &GamepadState::default())), MULTI_CONTROLLER);
    }

    /// `NV_INPUT_HEADER.size` is big-endian while the magic beside it is
    /// little-endian.
    ///
    /// This test is the only thing holding the field correct. Nova never reads
    /// it — it dispatches on the magic and takes the length from the datagram
    /// framing — so a regression here breaks nothing at runtime and would be
    /// found only by someone puzzling over a packet capture, which is the
    /// slowest possible way to find it.
    #[test]
    fn the_header_length_is_big_endian_beside_a_little_endian_magic() {
        // 34-byte gamepad packet: 30 bytes follow the length field.
        let p = gamepad(0, 0x0001, &GamepadState::default());
        assert_eq!(&p[0..4], &[0x00, 0x00, 0x00, 30], "length is big-endian");
        assert_eq!(u32::from_be_bytes(p[0..4].try_into().unwrap()) as usize, p.len() - 4);

        // And every other builder agrees, since they share `header`.
        for packet in [
            mouse_move_relative(1, 1).unwrap(),
            mouse_move_absolute(1, 1, 2, 2).unwrap(),
            mouse_button(MouseButton::Left, true),
            scroll(120).unwrap(),
            keyboard(0x41, 0, true),
        ] {
            assert_eq!(
                u32::from_be_bytes(packet[0..4].try_into().unwrap()) as usize,
                packet.len() - 4,
                "every packet's length field counts the bytes after it"
            );
        }
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

    // ── Gamepad ─────────────────────────────────────────────────────────────

    /// What `parse_multi_controller` produces on the host.
    #[derive(Debug, PartialEq, Eq)]
    struct ParsedPad {
        controller_number: u8,
        active_gamepad_mask: u16,
        button_flags: u16,
        left_trigger: u8,
        right_trigger: u8,
        left_stick_x: i16,
        left_stick_y: i16,
        right_stick_x: i16,
        right_stick_y: i16,
    }

    /// A transcription of `nova-server/src/input.rs::parse_multi_controller`,
    /// offset for offset.
    ///
    /// Written to read like the original rather than like idiomatic test code:
    /// the whole value of this test is that the two can be compared by eye. If
    /// it is ever tidied into something cleverer it stops proving anything.
    fn parse_multi_controller(p: &[u8]) -> ParsedPad {
        assert!(p.len() >= 34, "the host returns None below 34 bytes");
        assert_eq!(
            u32::from_le_bytes(p[4..8].try_into().unwrap()),
            MULTI_CONTROLLER,
            "the host returns None on a magic it does not know"
        );
        ParsedPad {
            controller_number: i16::from_le_bytes(p[10..12].try_into().unwrap()) as u8,
            active_gamepad_mask: u16::from_le_bytes(p[12..14].try_into().unwrap()),
            button_flags: u16::from_le_bytes(p[16..18].try_into().unwrap()),
            left_trigger: p[18],
            right_trigger: p[19],
            left_stick_x: i16::from_le_bytes(p[20..22].try_into().unwrap()),
            left_stick_y: i16::from_le_bytes(p[22..24].try_into().unwrap()),
            right_stick_x: i16::from_le_bytes(p[24..26].try_into().unwrap()),
            right_stick_y: i16::from_le_bytes(p[26..28].try_into().unwrap()),
        }
    }

    #[test]
    fn the_gamepad_magic_lands_where_the_host_dispatches_on_it() {
        let p = gamepad(0, 0x0001, &GamepadState::default());
        assert_eq!(magic_of(&p), MULTI_CONTROLLER);
        assert_eq!(&p[4..8], &[0x0C, 0x00, 0x00, 0x00], "magic is little-endian");
    }

    /// Every field, at the offset the host reads it from.
    ///
    /// Values are deliberately byte-asymmetric so a big-endian slip cannot pass
    /// by coincidence, and every field differs from every other so a transposed
    /// pair (the easy mistake with four axes) fails loudly.
    #[test]
    fn every_gamepad_field_round_trips_through_the_hosts_offsets() {
        let state = GamepadState {
            buttons: 0x1234,
            left_trigger: 0x7F,
            right_trigger: 0xC8,
            left_stick_x: 4386,    // 0x1122
            left_stick_y: -13124,  // 0xCCBC
            right_stick_x: 21862,  // 0x5566
            right_stick_y: -21863, // 0xAA99
        };
        let parsed = parse_multi_controller(&gamepad(2, 0x000B, &state));

        assert_eq!(
            parsed,
            ParsedPad {
                controller_number: 2,
                active_gamepad_mask: 0x000B,
                button_flags: 0x1234,
                left_trigger: 0x7F,
                right_trigger: 0xC8,
                left_stick_x: 4386,
                left_stick_y: -13124,
                right_stick_x: 21862,
                right_stick_y: -21863,
            }
        );
    }

    /// The packet is exactly 34 bytes and the sentinels sit where a real client
    /// puts them. Nova ignores all four, so nothing here fails at runtime — the
    /// test is what keeps a capture readable and a stricter host from being a
    /// surprise.
    #[test]
    fn the_gamepad_packet_is_34_bytes_with_its_sentinels_in_place() {
        let p = gamepad(1, 0x0002, &GamepadState::default());
        assert_eq!(p.len(), 34);
        assert_eq!(i16::from_le_bytes([p[8], p[9]]), 0x001A, "headerB");
        assert_eq!(i16::from_le_bytes([p[14], p[15]]), 0x0014, "midB");
        assert_eq!(i16::from_le_bytes([p[28], p[29]]), 0x009C, "tailA");
        assert_eq!(i16::from_le_bytes([p[32], p[33]]), 0x0055, "tailB");
        assert_eq!(
            u16::from_le_bytes([p[30], p[31]]),
            0,
            "buttonFlags2 has no XInput equivalent and must stay zero"
        );
    }

    /// The trap this file already warns about, asserted at the byte level: the
    /// mouse writes big-endian and this packet writes little, so the two live
    /// side by side and only an explicit check separates them.
    #[test]
    fn the_gamepad_body_is_little_endian_unlike_the_mouse_fields() {
        let p = gamepad(0, 0x0001, &GamepadState { buttons: 0x1234, ..Default::default() });
        assert_eq!([p[16], p[17]], [0x34, 0x12], "buttonFlags is little-endian");

        let p = gamepad(0, 0x0001, &GamepadState { left_stick_x: 4386, ..Default::default() });
        assert_eq!([p[20], p[21]], [0x22, 0x11], "leftStickX is little-endian");
    }

    /// `active_mask` is what plugs and unplugs the virtual pad host-side, so
    /// both directions are part of the wire contract, not a client convention.
    #[test]
    fn the_active_mask_bit_is_what_plugs_and_unplugs_a_slot() {
        for slot in 0..4u8 {
            let arrived =
                parse_multi_controller(&gamepad(slot, 1 << slot, &GamepadState::default()));
            assert_eq!(arrived.controller_number, slot);
            assert_ne!(
                arrived.active_gamepad_mask & (1 << slot),
                0,
                "slot {slot} must read as present"
            );

            let removed = parse_multi_controller(&gamepad(slot, 0, &GamepadState::default()));
            assert_eq!(
                removed.active_gamepad_mask & (1 << slot),
                0,
                "slot {slot} must read as gone"
            );
        }
    }

    /// A neutral snapshot is not a no-op and must never be dropped: it is how a
    /// controller says everything was let go. Dropping it strands whatever was
    /// held at the instant the user released it — the gamepad equivalent of a
    /// stuck modifier.
    #[test]
    fn a_neutral_gamepad_snapshot_is_still_sent() {
        let neutral = gamepad(0, 0x0001, &GamepadState::default());
        assert_eq!(neutral.len(), 34);

        let out = coalesce(vec![neutral.clone()]);
        assert_eq!(out, vec![neutral], "coalesce must not discard a released pad");
    }

    /// Snapshots supersede within one controller, because only the newest state
    /// of a pad means anything.
    #[test]
    fn gamepad_snapshots_supersede_within_one_controller() {
        let out = coalesce(vec![
            gamepad(0, 0x0001, &GamepadState { left_stick_x: 100, ..Default::default() }),
            gamepad(0, 0x0001, &GamepadState { left_stick_x: 200, ..Default::default() }),
            gamepad(0, 0x0001, &GamepadState { left_stick_x: 300, ..Default::default() }),
        ]);
        assert_eq!(out.len(), 1);
        assert_eq!(parse_multi_controller(&out[0]).left_stick_x, 300);
    }

    /// ...and never across controllers. This is the two-player regression: one
    /// pad's state says nothing about another's, so merging them would silently
    /// drop a second player's input whenever both moved in the same tick.
    #[test]
    fn gamepad_snapshots_never_supersede_across_controllers() {
        let out = coalesce(vec![
            gamepad(0, 0x0003, &GamepadState { left_stick_x: 100, ..Default::default() }),
            gamepad(1, 0x0003, &GamepadState { left_stick_x: 200, ..Default::default() }),
        ]);
        assert_eq!(out.len(), 2, "both players survive");
        assert_eq!(parse_multi_controller(&out[0]).controller_number, 0);
        assert_eq!(parse_multi_controller(&out[1]).controller_number, 1);
    }

    /// Order against other input is exact, and a snapshot does not reach back
    /// past an unrelated packet to supersede an older one — the same rule as a
    /// mouse move around a click.
    #[test]
    fn a_gamepad_snapshot_does_not_supersede_across_another_packet() {
        let out = coalesce(vec![
            gamepad(0, 0x0001, &GamepadState { buttons: 0x1000, ..Default::default() }),
            keyboard(0x41, 0, true),
            gamepad(0, 0x0001, &GamepadState { buttons: 0x2000, ..Default::default() }),
        ]);
        assert_eq!(out.len(), 3, "nothing is merged across the keystroke");
        assert_eq!(magic_of(&out[1]), KEY_DOWN);
        assert_eq!(parse_multi_controller(&out[0]).button_flags, 0x1000);
        assert_eq!(parse_multi_controller(&out[2]).button_flags, 0x2000);
    }

    /// `controller_number_of` must answer only for the packet it understands.
    /// If it answered `None` for everything else *and* for a malformed gamepad
    /// packet alike, coalesce's guard would compare `None == None` across two
    /// unrelated packet types and merge them into one.
    #[test]
    fn controller_number_is_read_only_from_a_multi_controller_packet() {
        assert_eq!(
            controller_number_of(&gamepad(3, 0x0008, &GamepadState::default())),
            Some(3)
        );
        assert_eq!(controller_number_of(&keyboard(0x41, 0, true)), None);
        assert_eq!(controller_number_of(&mouse_button(MouseButton::Left, true)), None);
        assert_eq!(controller_number_of(&[]), None);
    }
}

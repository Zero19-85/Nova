//! Input injection for Moonlight's 0x0206 INPUT_DATA messages: gamepad,
//! mouse, and keyboard.
//!
//! - **Gamepad**: split-seat passthrough — controller packets are mirrored
//!   onto a virtual Xbox 360 controller via ViGEmBus, so the remote player's
//!   gamepad drives games on the host.
//! - **Mouse / keyboard**: injected directly into the host session via the
//!   Win32 `SendInput` API, so the remote player also drives the desktop
//!   (mouse moves, clicks, scroll, and key presses).
//!
//! Wire format verified against moonlight-common-c's Input.h
//! (NV_MULTI_CONTROLLER_PACKET, magic = MULTI_CONTROLLER_MAGIC_GEN5):
//!
//!   [NV_INPUT_HEADER]                 8 bytes  (size: BE u32, magic: LE u32)
//!   headerB           : i16 LE        offset 8   (sentinel 0x001A)
//!   controllerNumber  : i16 LE        offset 10
//!   activeGamepadMask : u16 LE        offset 12
//!   midB              : i16 LE        offset 14  (sentinel 0x0014)
//!   buttonFlags       : u16 LE        offset 16
//!   leftTrigger       : u8            offset 18
//!   rightTrigger      : u8            offset 19
//!   leftStickX        : i16 LE        offset 20
//!   leftStickY        : i16 LE        offset 22
//!   rightStickX       : i16 LE        offset 24
//!   rightStickY       : i16 LE        offset 26
//!   tailA             : i16 LE        offset 28  (sentinel 0x009C)
//!   buttonFlags2      : u16 LE        offset 30  (Sunshine-only extended
//!                                                  buttons — paddles/touchpad/
//!                                                  misc; no XInput equivalent,
//!                                                  not forwarded to ViGEm)
//!   tailB             : i16 LE        offset 32  (sentinel 0x0055)
//!                                                = 34 bytes total
//!
//! Moonlight's low-16-bit buttonFlags happen to be bit-for-bit identical to
//! XInput's XINPUT_GAMEPAD button flags (UP/DOWN/LEFT/RIGHT/START/BACK/
//! LTHUMB/RTHUMB/LB/RB/GUIDE/A/B/X/Y), so it maps directly onto
//! vigem_client::XButtons with no translation table.
//!
//! Mouse/keyboard packet magics and layouts are documented above the
//! relevant `inject_*` functions further down in this file.

use std::sync::atomic::{AtomicI32, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use vigem_client::{Client, TargetId, XButtons, XGamepad, Xbox360Wired};
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    MapVirtualKeyW, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT,
    KEYBD_EVENT_FLAGS, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE,
    MAPVK_VK_TO_VSC_EX, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_LEFTDOWN,
    MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL,
    MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, VIRTUAL_KEY, VK_CONTROL, VK_F11, VK_LCONTROL,
    VK_LMENU, VK_LSHIFT, VK_LWIN, VK_MENU, VK_RCONTROL, VK_RMENU, VK_RSHIFT, VK_RWIN, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN, XBUTTON1, XBUTTON2,
};

const MULTI_CONTROLLER_MAGIC_GEN5: u32 = 0x0000_000C;
const PACKET_LEN: usize = 34;
const MAX_PADS: usize = 4;

// ---------------------------------------------------------------------
// Mouse & keyboard packet magics (NV_INPUT_HEADER-dispatched, same 8-byte
// header as the multi-controller packet above: size: BE u32, magic: LE u32).
//
// The canonical moonlight-common-c/src/Input.h header ships as an empty
// submodule in every Moonlight/Sunshine checkout available on this machine,
// so these were cross-referenced from Sunshine's src/input.cpp dispatch
// table (case labels for MOUSE_MOVE_REL_MAGIC_GEN5, MOUSE_MOVE_ABS_MAGIC,
// MOUSE_BUTTON_*_EVENT_MAGIC_GEN5, SCROLL_MAGIC_GEN5, KEY_*_EVENT_MAGIC) and
// moonlight-android's KeyboardPacket.java (KEY_DOWN=0x03/KEY_UP=0x04 match
// directly) plus the contiguous "GEN5" numbering ending at the *confirmed*
// MULTI_CONTROLLER_MAGIC_GEN5 = 0x0C above. If any of these are off,
// `handle_input_packet` logs the raw magic for unrecognized packets so it
// can be corrected from a live capture.
//
// KEY_DOWN_EVENT_MAGIC/KEY_UP_EVENT_MAGIC = 0x03/0x04 match moonlight-
// android's KeyboardPacket.KEY_DOWN/KEY_UP constants by value (not just by
// name) and have been confirmed against a live client.
// ---------------------------------------------------------------------
const KEY_DOWN_EVENT_MAGIC: u32 = 0x0000_0003;
const KEY_UP_EVENT_MAGIC: u32 = 0x0000_0004;
const MOUSE_MOVE_ABS_MAGIC: u32 = 0x0000_0005;
const MOUSE_MOVE_REL_MAGIC_GEN5: u32 = 0x0000_0007;
const MOUSE_BUTTON_DOWN_MAGIC_GEN5: u32 = 0x0000_0008;
const MOUSE_BUTTON_UP_MAGIC_GEN5: u32 = 0x0000_0009;
const SCROLL_MAGIC_GEN5: u32 = 0x0000_000A;

// NV_MOUSE_BUTTON_PACKET button values (moonlight-android MouseButtonPacket.java).
const BUTTON_LEFT: u8 = 1;
const BUTTON_MIDDLE: u8 = 2;
const BUTTON_RIGHT: u8 = 3;
const BUTTON_X1: u8 = 4;
const BUTTON_X2: u8 = 5;

// NV_KEYBOARD_PACKET modifiers bitmask (moonlight-android KeyboardPacket.java).
const MODIFIER_SHIFT: u8 = 0x01;
const MODIFIER_CTRL: u8 = 0x02;
const MODIFIER_ALT: u8 = 0x04;
const MODIFIER_META: u8 = 0x08;

#[derive(Debug, Clone, Copy)]
struct ControllerInput {
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

/// Parse the payload of a 0x0206 INPUT_DATA message (i.e. everything after
/// the `[u16 type][u16 len]` control envelope header) as a
/// NV_MULTI_CONTROLLER_PACKET. Returns `None` for short/unrecognized packets
/// (e.g. the older GEN4 layout without buttonFlags2, which we don't bother
/// translating).
fn parse_multi_controller(payload: &[u8]) -> Option<ControllerInput> {
    if payload.len() < PACKET_LEN {
        return None;
    }
    let magic = u32::from_le_bytes(payload[4..8].try_into().unwrap());
    if magic != MULTI_CONTROLLER_MAGIC_GEN5 {
        return None;
    }
    Some(ControllerInput {
        controller_number: i16::from_le_bytes(payload[10..12].try_into().unwrap()) as u8,
        active_gamepad_mask: u16::from_le_bytes(payload[12..14].try_into().unwrap()),
        button_flags: u16::from_le_bytes(payload[16..18].try_into().unwrap()),
        left_trigger: payload[18],
        right_trigger: payload[19],
        left_stick_x: i16::from_le_bytes(payload[20..22].try_into().unwrap()),
        left_stick_y: i16::from_le_bytes(payload[22..24].try_into().unwrap()),
        right_stick_x: i16::from_le_bytes(payload[24..26].try_into().unwrap()),
        right_stick_y: i16::from_le_bytes(payload[26..28].try_into().unwrap()),
    })
}

struct PadSlot {
    target: Xbox360Wired<Arc<Client>>,
    plugged: bool,
}

struct GamepadManager {
    pads: [PadSlot; MAX_PADS],
}

impl GamepadManager {
    fn connect() -> Result<Self, vigem_client::Error> {
        let client = Arc::new(Client::connect()?);
        let pads = std::array::from_fn(|_| PadSlot {
            target: Xbox360Wired::new(client.clone(), TargetId::XBOX360_WIRED),
            plugged: false,
        });
        Ok(Self { pads })
    }

    fn apply(&mut self, input: ControllerInput) {
        let idx = input.controller_number as usize;
        if idx >= MAX_PADS {
            return;
        }
        let want_active = input.active_gamepad_mask & (1 << idx) != 0;
        let slot = &mut self.pads[idx];

        if want_active && !slot.plugged {
            match slot.target.plugin().and_then(|_| slot.target.wait_ready()) {
                Ok(()) => {
                    slot.plugged = true;
                    println!("🎮 ViGEm: plugged in virtual Xbox 360 controller #{}", idx);
                }
                Err(e) => {
                    println!("⚠️  ViGEm: failed to plug in controller #{}: {:?}", idx, e);
                    return;
                }
            }
        } else if !want_active && slot.plugged {
            let _ = slot.target.unplug();
            slot.plugged = false;
            println!("🎮 ViGEm: unplugged virtual Xbox 360 controller #{}", idx);
        }

        if slot.plugged {
            let gamepad = XGamepad {
                buttons: XButtons(input.button_flags),
                left_trigger: input.left_trigger,
                right_trigger: input.right_trigger,
                thumb_lx: input.left_stick_x,
                thumb_ly: input.left_stick_y,
                thumb_rx: input.right_stick_x,
                thumb_ry: input.right_stick_y,
            };
            if let Err(e) = slot.target.update(&gamepad) {
                println!("⚠️  ViGEm: controller #{} update failed: {:?}", idx, e);
            }
        }
    }

    fn unplug_all(&mut self) {
        for (idx, slot) in self.pads.iter_mut().enumerate() {
            if slot.plugged {
                let _ = slot.target.unplug();
                slot.plugged = false;
                println!("🎮 ViGEm: unplugged virtual Xbox 360 controller #{}", idx);
            }
        }
    }
}

static MANAGER: OnceLock<Mutex<Option<GamepadManager>>> = OnceLock::new();

fn manager() -> &'static Mutex<Option<GamepadManager>> {
    MANAGER.get_or_init(|| Mutex::new(None))
}

/// Connect to ViGEmBus for a new streaming session. Safe to call even if
/// ViGEmBus isn't installed — logs a warning and leaves gamepad passthrough
/// disabled for the session rather than failing the stream.
pub fn start_session() {
    let mut guard = manager().lock().unwrap();
    if guard.is_some() {
        return;
    }
    match GamepadManager::connect() {
        Ok(m) => {
            println!("🎮 ViGEm: connected to ViGEmBus — gamepad passthrough enabled");
            *guard = Some(m);
        }
        Err(e) => {
            println!("⚠️  ViGEm: could not connect to ViGEmBus ({:?}) — gamepad passthrough disabled. \
                Install the ViGEmBus driver (https://github.com/ViGEm/ViGEmBus) to enable split-seat controller support.", e);
        }
    }
}

/// Unplug any virtual controllers and drop the ViGEmBus connection at the
/// end of a streaming session.
///
/// Also releases any modifier keys (SHIFT/CTRL/ALT/META) that were left
/// "held" via SendInput — e.g. the client disconnected mid-keypress without
/// ever sending the matching KEY_UP. Without this, HELD_MODIFIERS (and the
/// real OS keyboard state) would carry a stuck modifier into the next
/// session.
pub fn stop_session() {
    let held = HELD_MODIFIERS.swap(0, Ordering::SeqCst);
    if held & MODIFIER_SHIFT != 0 {
        send_key_event(VK_SHIFT, true);
    }
    if held & MODIFIER_CTRL != 0 {
        send_key_event(VK_CONTROL, true);
    }
    if held & MODIFIER_ALT != 0 {
        send_key_event(VK_MENU, true);
    }
    if held & MODIFIER_META != 0 {
        send_key_event(VK_LWIN, true);
    }

    let mut guard = manager().lock().unwrap();
    if let Some(mut m) = guard.take() {
        m.unplug_all();
    }
}

/// Handle a decrypted 0x0206 INPUT_DATA payload (control.rs): dispatches on
/// the NV_INPUT_HEADER magic (offset 4, LE u32) to gamepad passthrough
/// (ViGEmBus) or mouse/keyboard injection (SendInput). Unrecognized/short
/// packets are logged once via the `_` arm so the magic table above can be
/// corrected from a live capture if needed.
pub fn handle_input_packet(payload: &[u8]) {
    if payload.len() < 8 {
        return;
    }
    let magic = u32::from_le_bytes(payload[4..8].try_into().unwrap());

    match magic {
        MULTI_CONTROLLER_MAGIC_GEN5 => {
            if let Some(input) = parse_multi_controller(payload) {
                let mut guard = manager().lock().unwrap();
                if let Some(m) = guard.as_mut() {
                    m.apply(input);
                }
            }
        }
        MOUSE_MOVE_ABS_MAGIC => inject_mouse_move_abs(payload),
        MOUSE_MOVE_REL_MAGIC_GEN5 => inject_mouse_move_rel(payload),
        MOUSE_BUTTON_DOWN_MAGIC_GEN5 => inject_mouse_button(payload, true),
        MOUSE_BUTTON_UP_MAGIC_GEN5 => inject_mouse_button(payload, false),
        SCROLL_MAGIC_GEN5 => inject_scroll(payload),
        KEY_DOWN_EVENT_MAGIC => inject_keyboard(payload, false),
        KEY_UP_EVENT_MAGIC => inject_keyboard(payload, true),
        // 10-byte controller capability/status packets (8-byte
        // NV_INPUT_HEADER + 2 bytes) carry no actionable input — ignore
        // silently rather than logging as unrecognized.
        _ if payload.len() == 10 => {}
        _ => {
            println!("⌨️  Input: unrecognized 0x0206 magic 0x{:08x} ({} bytes)", magic, payload.len());
        }
    }
}

// ---------------------------------------------------------------------
// Mouse & keyboard injection via SendInput.
//
// Per project requirements, mouse positioning is ALWAYS injected as an
// absolute SendInput move (MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
// 0-65535 normalized to the Win32 virtual screen — SM_XVIRTUALSCREEN/
// SM_YVIRTUALSCREEN/SM_CXVIRTUALSCREEN/SM_CYVIRTUALSCREEN). Even
// client-relative deltas (NV_REL_MOUSE_MOVE_PACKET) are resolved against the
// host's current cursor position and re-injected absolutely — true relative
// SendInput moves would let the client's and host's notions of cursor
// position drift apart (desync).
//
// Plain MOUSEEVENTF_ABSOLUTE (without VIRTUALDESK) maps 0-65535 onto the GDI
// *primary* monitor's bounds, which is NOT necessarily the display
// `capture::DesktopCapturer` is duplicating — on a multi-monitor host, DXGI
// output 0 of adapter 0 isn't guaranteed to be the primary, and during a
// Virtual Desktop session the virtual display becomes primary while other
// physical paths are detached rather than removed. So every absolute move
// below is computed in desktop coordinates (the same top-left-origin,
// Y-increases-downward space as DXGI's DesktopCoordinates / GetCursorPos —
// matching the wire format, so no axis is ever flipped) against the ACTIVE
// CAPTURE RECT (see `set_active_capture_rect`), then converted to the
// VIRTUALDESK 0-65535 space via `virtual_desktop_to_absolute`.
// ---------------------------------------------------------------------

/// Position (`origin_x`/`origin_y`, desktop coordinates — i.e.
/// `DXGI_OUTPUT_DESC::DesktopCoordinates.left/top`) and size (`width`/
/// `height`) of the display `capture::DesktopCapturer` is currently
/// duplicating. `lib.rs` calls [`set_active_capture_rect`] after creating or
/// rebinding the capturer — including following the Virtual Desktop
/// activate/deactivate handoff — so this always reflects what's actually
/// being streamed. Mouse-move injection maps onto THIS rect, not onto
/// `GetSystemMetrics(SM_CXSCREEN/SM_CYSCREEN)` (the GDI primary monitor,
/// which may be a different display).
static CAPTURE_ORIGIN_X: AtomicI32 = AtomicI32::new(0);
static CAPTURE_ORIGIN_Y: AtomicI32 = AtomicI32::new(0);
static CAPTURE_WIDTH: AtomicI32 = AtomicI32::new(0);
static CAPTURE_HEIGHT: AtomicI32 = AtomicI32::new(0);

/// Records the desktop-coordinate rect of the display currently being
/// captured. See [`CAPTURE_ORIGIN_X`] and friends.
pub fn set_active_capture_rect(origin_x: i32, origin_y: i32, width: u32, height: u32) {
    CAPTURE_ORIGIN_X.store(origin_x, Ordering::Relaxed);
    CAPTURE_ORIGIN_Y.store(origin_y, Ordering::Relaxed);
    CAPTURE_WIDTH.store(width as i32, Ordering::Relaxed);
    CAPTURE_HEIGHT.store(height as i32, Ordering::Relaxed);
}

fn active_capture_rect() -> (i32, i32, i32, i32) {
    (
        CAPTURE_ORIGIN_X.load(Ordering::Relaxed),
        CAPTURE_ORIGIN_Y.load(Ordering::Relaxed),
        CAPTURE_WIDTH.load(Ordering::Relaxed),
        CAPTURE_HEIGHT.load(Ordering::Relaxed),
    )
}

/// Converts a point in desktop coordinates (top-left origin, Y increasing
/// downward — the same space as [`active_capture_rect`], DXGI's
/// `DesktopCoordinates`, and `GetCursorPos`) into the
/// `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK` 0-65535 space, which
/// SendInput maps onto the identical rect (`SM_XVIRTUALSCREEN`/
/// `SM_YVIRTUALSCREEN`, sized `SM_CXVIRTUALSCREEN`/`SM_CYVIRTUALSCREEN`).
/// Same origin and axis direction on both sides, so neither axis is flipped
/// here.
fn virtual_desktop_to_absolute(x: f64, y: f64) -> Option<(i32, i32)> {
    let vs_x = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) } as f64;
    let vs_y = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) } as f64;
    let vs_w = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) } as f64;
    let vs_h = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) } as f64;
    if vs_w <= 0.0 || vs_h <= 0.0 {
        return None;
    }

    let nx = (((x - vs_x) / vs_w) * 65535.0).clamp(0.0, 65535.0) as i32;
    let ny = (((y - vs_y) / vs_h) * 65535.0).clamp(0.0, 65535.0) as i32;
    Some((nx, ny))
}

fn send_mouse_input(mi: MOUSEINPUT) {
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi },
    };
    unsafe {
        SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
    }
}

fn send_key_input(ki: KEYBDINPUT) {
    let input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki },
    };
    unsafe {
        SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
    }
}

/// NV_ABS_MOUSE_MOVE_PACKET body (after the 8-byte NV_INPUT_HEADER):
///   x      : i16 BE  @8   cursor X in client stream-space
///   y      : i16 BE  @10  cursor Y in client stream-space
///   ...    : (an optional reserved i16 may sit here depending on protocol
///              version — width/height are read from the *end* of the
///              packet instead of a fixed offset to tolerate either layout)
///   width  : i16 BE  @len-4  client's reference width for `x`
///   height : i16 BE  @len-2  client's reference height for `y`
///
/// `x/width` and `y/height` give the cursor's fractional position within the
/// client's view (top-left origin, Y increasing downward — both ends of the
/// wire format agree, so this fraction is applied directly with no flip).
/// That fraction is applied to the active capture rect (see
/// [`active_capture_rect`]) to get a desktop-coordinate point, which
/// [`virtual_desktop_to_absolute`] converts to SendInput's 0-65535
/// VIRTUALDESK space.
fn inject_mouse_move_abs(payload: &[u8]) {
    if payload.len() < 16 {
        return;
    }
    let len = payload.len();
    let x = i16::from_be_bytes([payload[8], payload[9]]) as f64;
    let y = i16::from_be_bytes([payload[10], payload[11]]) as f64;
    let client_width = i16::from_be_bytes([payload[len - 4], payload[len - 3]]) as f64;
    let client_height = i16::from_be_bytes([payload[len - 2], payload[len - 1]]) as f64;
    if client_width <= 0.0 || client_height <= 0.0 {
        return;
    }

    let (origin_x, origin_y, capture_w, capture_h) = active_capture_rect();
    if capture_w <= 0 || capture_h <= 0 {
        return;
    }

    let frac_x = (x / client_width).clamp(0.0, 1.0);
    let frac_y = (y / client_height).clamp(0.0, 1.0);
    let target_x = origin_x as f64 + frac_x * capture_w as f64;
    let target_y = origin_y as f64 + frac_y * capture_h as f64;

    let Some((nx, ny)) = virtual_desktop_to_absolute(target_x, target_y) else {
        return;
    };

    send_mouse_input(MOUSEINPUT {
        dx: nx,
        dy: ny,
        mouseData: 0,
        dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
        time: 0,
        dwExtraInfo: 0,
    });
}

/// NV_REL_MOUSE_MOVE_PACKET body:
///   deltaX : i16 BE @8
///   deltaY : i16 BE @10
///
/// Resolved against the host's current cursor position (GetCursorPos),
/// clamped to the active capture rect (see [`active_capture_rect`]), and
/// re-injected as an absolute move — see the module-level note on why
/// relative SendInput moves aren't used.
fn inject_mouse_move_rel(payload: &[u8]) {
    if payload.len() < 12 {
        return;
    }
    let dx = i16::from_be_bytes([payload[8], payload[9]]) as i32;
    let dy = i16::from_be_bytes([payload[10], payload[11]]) as i32;

    let mut pos = POINT::default();
    if unsafe { GetCursorPos(&mut pos) }.is_err() {
        return;
    }

    let (origin_x, origin_y, capture_w, capture_h) = active_capture_rect();
    if capture_w <= 0 || capture_h <= 0 {
        return;
    }

    let new_x = (pos.x + dx).clamp(origin_x, origin_x + capture_w - 1);
    let new_y = (pos.y + dy).clamp(origin_y, origin_y + capture_h - 1);

    let Some((nx, ny)) = virtual_desktop_to_absolute(new_x as f64, new_y as f64) else {
        return;
    };

    send_mouse_input(MOUSEINPUT {
        dx: nx,
        dy: ny,
        mouseData: 0,
        dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
        time: 0,
        dwExtraInfo: 0,
    });
}

/// NV_MOUSE_BUTTON_PACKET body:
///   button : u8 @8  (BUTTON_LEFT=1, BUTTON_MIDDLE=2, BUTTON_RIGHT=3,
///                     BUTTON_X1=4, BUTTON_X2=5)
fn inject_mouse_button(payload: &[u8], down: bool) {
    if payload.len() < 9 {
        return;
    }
    let (flag, mouse_data) = match payload[8] {
        BUTTON_LEFT => (if down { MOUSEEVENTF_LEFTDOWN } else { MOUSEEVENTF_LEFTUP }, 0u32),
        BUTTON_MIDDLE => (if down { MOUSEEVENTF_MIDDLEDOWN } else { MOUSEEVENTF_MIDDLEUP }, 0u32),
        BUTTON_RIGHT => (if down { MOUSEEVENTF_RIGHTDOWN } else { MOUSEEVENTF_RIGHTUP }, 0u32),
        BUTTON_X1 => (if down { MOUSEEVENTF_XDOWN } else { MOUSEEVENTF_XUP }, XBUTTON1 as u32),
        BUTTON_X2 => (if down { MOUSEEVENTF_XDOWN } else { MOUSEEVENTF_XUP }, XBUTTON2 as u32),
        _ => return,
    };

    send_mouse_input(MOUSEINPUT {
        dx: 0,
        dy: 0,
        mouseData: mouse_data,
        dwFlags: flag,
        time: 0,
        dwExtraInfo: 0,
    });
}

/// NV_SCROLL_PACKET body:
///   scrollAmt1 : i16 BE @8  (signed, in Windows WHEEL_DELTA=120 units —
///                            passed straight through as MOUSEINPUT.mouseData)
fn inject_scroll(payload: &[u8]) {
    if payload.len() < 10 {
        return;
    }
    let amount = i16::from_be_bytes([payload[8], payload[9]]) as i32;
    if amount == 0 {
        return;
    }

    send_mouse_input(MOUSEINPUT {
        dx: 0,
        dy: 0,
        mouseData: amount as u32,
        dwFlags: MOUSEEVENTF_WHEEL,
        time: 0,
        dwExtraInfo: 0,
    });
}

/// Tracks which modifier keys (MODIFIER_SHIFT/CTRL/ALT/META bits) Nova has
/// most recently injected as "held down", via explicit keyboard packets for
/// the modifier keys themselves. Used to avoid double-pressing a modifier
/// that's already held when bracketing a keystroke with synthetic modifiers
/// (see `inject_keyboard`).
static HELD_MODIFIERS: AtomicU8 = AtomicU8::new(0);

/// Maps a Windows virtual-key code to the MODIFIER_* bit it corresponds to,
/// if any (covers both the generic and left/right-specific VK constants).
fn modifier_bit_for_vk(vk: VIRTUAL_KEY) -> Option<u8> {
    match vk {
        VK_SHIFT | VK_LSHIFT | VK_RSHIFT => Some(MODIFIER_SHIFT),
        VK_CONTROL | VK_LCONTROL | VK_RCONTROL => Some(MODIFIER_CTRL),
        VK_MENU | VK_LMENU | VK_RMENU => Some(MODIFIER_ALT),
        VK_LWIN | VK_RWIN => Some(MODIFIER_META),
        _ => None,
    }
}

/// Inject a single key press/release via SendInput. Translates `vk` to a
/// hardware scan code where possible (MapVirtualKeyW + MAPVK_VK_TO_VSC_EX),
/// including the extended-key prefix (0xE0/0xE1) for keys like arrows,
/// Ins/Del/Home/End/PgUp/PgDn, the numpad divide/enter, and right-side
/// Ctrl/Alt — so titles that read scan codes/raw input see real hardware
/// input. Falls back to the bare virtual-key code if no scan code exists.
fn send_key_event(vk: VIRTUAL_KEY, release: bool) {
    let scan = unsafe { MapVirtualKeyW(vk.0 as u32, MAPVK_VK_TO_VSC_EX) };

    let mut flags = if release { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) };
    // Scan-code mode: wVk MUST be 0 (VIRTUAL_KEY(0)) and KEYEVENTF_SCANCODE
    // set, with KEYEVENTF_EXTENDEDKEY added for the 0xE0/0xE1-prefixed
    // extended keys (arrows, Ins/Del/Home/End/PgUp/PgDn, numpad Enter/Divide,
    // right Ctrl/Alt, etc.) per MapVirtualKeyW(MAPVK_VK_TO_VSC_EX)'s output.
    let (wvk, wscan) = if scan != 0 {
        flags |= KEYEVENTF_SCANCODE;
        if scan & 0xFF00 == 0xE000 || scan & 0xFF00 == 0xE100 {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        (VIRTUAL_KEY(0), (scan & 0xFF) as u16)
    } else {
        // No scan-code mapping for this VK (e.g. VK_LWIN on some systems) —
        // fall back to a plain virtual-key event.
        (vk, 0u16)
    };

    send_key_input(KEYBDINPUT {
        wVk: wvk,
        wScan: wscan,
        dwFlags: flags,
        time: 0,
        dwExtraInfo: 0,
    });
}

/// Simulate Win+F11 — used by `app_launcher::launch_app` to toggle the Xbox
/// app's immersive "Fullscreen experience" shell once it has focus.
pub fn send_win_f11() {
    send_key_event(VK_LWIN, false);
    send_key_event(VK_F11, false);
    send_key_event(VK_F11, true);
    send_key_event(VK_LWIN, true);
}

/// NV_KEYBOARD_PACKET body (after the 8-byte NV_INPUT_HEADER):
///   keyAction : u8     @8   unused — press/release is already determined by
///                            the NV_INPUT_HEADER magic (KEY_DOWN/UP_EVENT_MAGIC)
///   keyCode   : u16 LE @9   low byte = Windows VK code; high byte = 0x80,
///                            a legacy NVIDIA convention (mask with 0xFF)
///   modifiers : u8    @11   MODIFIER_SHIFT/CTRL/ALT/META bitmask
///   zero2     : u16 LE @12  unused/reserved
fn inject_keyboard(payload: &[u8], release: bool) {
    // 8B header + 1B keyAction + 2B keyCode + 1B modifiers.
    if payload.len() < 12 {
        return;
    }
    let key_code = u16::from_le_bytes([payload[9], payload[10]]);
    let vk = VIRTUAL_KEY(key_code & 0x00FF);
    let modifiers = payload[11];

    if let Some(bit) = modifier_bit_for_vk(vk) {
        // Real modifier key: track held/released state so the synthetic
        // bracketing below doesn't double up on a modifier already held.
        if release {
            HELD_MODIFIERS.fetch_and(!bit, Ordering::SeqCst);
        } else {
            HELD_MODIFIERS.fetch_or(bit, Ordering::SeqCst);
        }
        send_key_event(vk, release);
        return;
    }

    if release {
        send_key_event(vk, true);
        return;
    }

    // Synthetic modifier presses (mirrors Sunshine's send_key_and_modifiers):
    // if the client says SHIFT/CTRL/ALT was held for this keystroke but we
    // aren't already holding it ourselves, bracket the key with a synthetic
    // press/release of that modifier.
    let held = HELD_MODIFIERS.load(Ordering::SeqCst);
    let mut synthetic = Vec::new();
    if modifiers & MODIFIER_SHIFT != 0 && held & MODIFIER_SHIFT == 0 {
        synthetic.push(VK_SHIFT);
    }
    if modifiers & MODIFIER_CTRL != 0 && held & MODIFIER_CTRL == 0 {
        synthetic.push(VK_CONTROL);
    }
    if modifiers & MODIFIER_ALT != 0 && held & MODIFIER_ALT == 0 {
        synthetic.push(VK_MENU);
    }

    for &m in &synthetic {
        send_key_event(m, false);
    }
    send_key_event(vk, false);
    for &m in synthetic.iter().rev() {
        send_key_event(m, true);
    }
}

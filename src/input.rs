//! Input injection for Moonlight's 0x0206 INPUT_DATA messages: gamepad,
//! mouse, and keyboard.
//!
//! - **Gamepad**: split-seat passthrough — controller packets are mirrored
//!   onto a virtual Xbox 360 controller via ViGEmBus, so the remote player's
//!   gamepad drives games on the host.
//! - **Mouse / keyboard**: injected directly into the host session via the
//!   Win32 `SendInput` API, so the remote player also drives the desktop
//!   (mouse moves, clicks, scroll, and key presses). Also works on the
//!   Winlogon secure desktop (UAC prompts, Ctrl+Alt+Del, lock/PIN screen) —
//!   see `sync_desktop_for_input`/`SecureDesktopGuard` below for how.
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

use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicI32, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use vigem_client::{Client, TargetId, XButtons, XGamepad, Xbox360Wired};
use windows::Win32::Security::{ImpersonateLoggedOnUser, RevertToSelf};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, GetThreadDesktop, OpenInputDesktop, SetThreadDesktop, DESKTOP_ACCESS_FLAGS,
    DESKTOP_CONTROL_FLAGS, HDESK,
};
use windows::Win32::System::Threading::GetCurrentThreadId;
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
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
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

impl Drop for GamepadManager {
    fn drop(&mut self) {
        // Safety net for any code path that drops GamepadManager without calling
        // unplug_all() explicitly — e.g. a panic between start_session and
        // stop_session. unplug_all() is idempotent (checks slot.plugged) so
        // calling it here after an explicit stop_session is a no-op.
        self.unplug_all();
    }
}

static MANAGER: OnceLock<Mutex<Option<GamepadManager>>> = OnceLock::new();

fn manager() -> &'static Mutex<Option<GamepadManager>> {
    MANAGER.get_or_init(|| Mutex::new(None))
}

/// Pinned ViGEmBus release used by the auto-installer. Locked to v1.22.0.0 —
/// the last stable release compatible with vigem-client 0.3.x. Update when
/// bumping the vigem-client crate version.
const VIGEMBUS_MSI_URL: &str =
    "https://github.com/nefarius/ViGEmBus/releases/download/v1.22.0.0/ViGEmBusSetup_x64.msi";

/// Downloads and silently installs the ViGEmBus kernel driver MSI.
/// Nova runs with `requireAdministrator` in its manifest, so the spawned
/// `msiexec` child process inherits the elevated token — no UAC prompt.
/// The MSI is downloaded to the system TEMP directory and removed after install.
fn auto_install_vigembus() -> Result<(), String> {
    let msi_path = std::env::temp_dir().join("ViGEmBusSetup_x64.msi");

    // PowerShell 5.1 (Windows default) negotiates TLS 1.0 by default.
    // GitHub enforces TLS 1.2+ and drops TLS 1.0 connections with
    // "The connection was closed unexpectedly". Explicitly set TLS 1.2
    // before Invoke-WebRequest so the download succeeds on stock Windows.
    let ps_download = format!(
        "[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12; \
         $ProgressPreference='SilentlyContinue'; \
         Invoke-WebRequest -Uri '{url}' -OutFile '{out}' -UseBasicParsing",
        url = VIGEMBUS_MSI_URL,
        out = msi_path.display(),
    );
    println!("📦 Downloading ViGEmBus from {VIGEMBUS_MSI_URL}");
    let dl = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", &ps_download])
        .status()
        .map_err(|e| format!("PowerShell spawn failed: {e}"))?;
    if !dl.success() {
        return Err(format!("ViGEmBus download failed (powershell exit {:?})", dl.code()));
    }
    if !msi_path.exists() {
        return Err(format!("MSI not found at {} after download", msi_path.display()));
    }

    println!("🔧 Installing ViGEmBus silently (msiexec /qn /norestart)…");
    let install = std::process::Command::new("msiexec")
        .args(["/i", &msi_path.to_string_lossy(), "/qn", "/norestart"])
        .status()
        .map_err(|e| format!("msiexec spawn failed: {e}"))?;

    let _ = std::fs::remove_file(&msi_path);

    if install.success() {
        println!("✅ ViGEmBus installed successfully");
        Ok(())
    } else {
        Err(format!(
            "msiexec exited {:?} — try installing manually from https://github.com/nefarius/ViGEmBus/releases",
            install.code()
        ))
    }
}

/// Connect to ViGEmBus for a new streaming session.
///
/// On `BusNotFound` (driver not installed), spawns a background thread to
/// download and silently install the official ViGEmBus MSI. The current
/// session proceeds without gamepad support; controller passthrough is
/// available automatically on the next client connection. This keeps the
/// streaming startup path non-blocking — the download (≈3 MB) and MSI
/// install run entirely off the capture / network threads.
pub fn start_session() {
    {
        let mut guard = manager().lock().unwrap();
        if guard.is_some() {
            return;
        }
        match GamepadManager::connect() {
            Ok(m) => {
                println!("🎮 ViGEm: connected to ViGEmBus — gamepad passthrough enabled");
                *guard = Some(m);
                return;
            }
            Err(vigem_client::Error::BusNotFound) => {
                println!("⚠️  ViGEm: ViGEmBus driver not found — \
                    launching background installer (gamepad passthrough \
                    will be active on the next client connection)");
                // guard is intentionally dropped here so the background
                // thread can re-acquire it after the install completes.
            }
            Err(e) => {
                println!("⚠️  ViGEm: could not connect to ViGEmBus ({e:?}) — gamepad passthrough disabled. \
                    Install the ViGEmBus driver (https://github.com/ViGEm/ViGEmBus) to enable split-seat controller support.");
                return;
            }
        }
    } // lock released before background thread starts

    std::thread::spawn(|| {
        match auto_install_vigembus() {
            Ok(()) => {
                // Brief settle: the ViGEmBus kernel service starts
                // asynchronously after the MSI completes.
                std::thread::sleep(std::time::Duration::from_secs(3));
                let mut guard = manager().lock().unwrap();
                if guard.is_none() {
                    match GamepadManager::connect() {
                        Ok(m) => {
                            println!("🎮 ViGEm: connected after auto-install — \
                                gamepad passthrough ready for next session");
                            *guard = Some(m);
                        }
                        Err(e) => println!("⚠️  ViGEm: still could not connect after install ({e:?})"),
                    }
                }
            }
            Err(e) => println!("⚠️  ViGEmBus auto-install failed: {e}"),
        }
    });
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

// ---------------------------------------------------------------------
// Secure-desktop KBM injection (Task 1, 2026-07-20).
//
// Bug: mouse/keyboard freeze whenever the host is on the Winlogon secure
// desktop (UAC prompt, Ctrl+Alt+Del, PIN/lock screen). Root cause is NOT a
// UIPI privilege check on SendInput itself — it's that `SendInput` dispatches
// to whatever desktop the CALLING THREAD is attached to (`GetThreadDesktop`),
// and a normal elevated-user thread cannot even attach to Winlogon: its ACL
// admits only SYSTEM (same wall `capture/dda.rs` hit and solved for video —
// see that module's doc comment for the confirmed-live 0x800700AA finding).
// Evidence this is the right diagnosis, not a driver-level dead end: gamepad
// input (ViGEmBus, a kernel bus device) was NEVER reported frozen on the
// secure desktop — only mouse/keyboard, which is exactly the SendInput/
// desktop-attachment-specific path.
//
// UIPI "SILENT SWALLOW" — why SYSTEM IMPERSONATION IS NOT ENOUGH (2026-08-06,
// live-confirmed): with the guard below engaged, the log shows a successful
// SYSTEM impersonation, a successful Winlogon attach, and ZERO SendInput
// rejections — and the PIN field still receives nothing. Reason: the ACL
// checks that impersonation satisfies (`OpenInputDesktop`/`SetThreadDesktop`,
// and DXGI duplication on the capture side) are kernel-object checks, which
// DO honour a thread's impersonation token. Injected input reaching the
// credential provider is instead gated by UIPI/integrity in win32k, which
// evaluates the injecting process's PRIMARY token — and the Worker's primary
// token is the interactive user (High integrity), below Winlogon's System
// integrity. UIPI accepts the call at the API boundary (SendInput returns the
// event count, so there is nothing to log) and drops the event before the UI
// sees it. That asymmetry — capture works, input silently doesn't — is the
// signature of this trap, and it is why the fix has to change the PRIMARY
// token of whatever process calls SendInput, not the thread identity:
// `service::spawn_input_helper` + the `--system-input-helper` mode
// (`lib.rs::run_input_helper`). The guard below is still the right and
// necessary mechanism for the DESKTOP ATTACH inside that helper (and remains
// the fallback path when no helper is available).
//
// Rejected alternative: routing input packets back to the SYSTEM Master for
// injection there (Master/Worker split — Master is already an immortal
// LocalSystem service, so it looks like the privileged place to inject from).
// This CANNOT work and must not be attempted: `SendInput` is session-local,
// and the Master lives in Session 0. A process's desktop attachment is
// constrained by its WINDOW STATION, and a Session 0 service is bound to
// Session 0's `Winsta0` — it cannot `SetProcessWindowStation` onto the
// console session's window station, so it can never reach that session's
// `Winlogon` (or even `Default`) desktop. Sunshine looks like a
// counter-example but isn't: its privileged helper does not inject from
// Session 0 either — it spawns/keeps its input path inside the console
// session and syncs THAT thread's desktop (`misc.cpp::syncThreadDesktop`,
// called from `input.cpp::send_input`). The Worker — already in the console
// session — is therefore the only correct injector; it just needs SYSTEM
// IDENTITY for the desktop attach, which is exactly what the service's
// `--system-token` impersonation handoff provides below.
//
// Rejected alternative: a new kernel-mode virtual HID keyboard/mouse driver
// (ViGEmBus itself is confirmed gamepad-only — no KBM support upstream; the
// one community project with that scope, Ryochan7/FakerInput, has been
// archived/unmaintained since Jan 2024). Shipping an unmaintained driver
// under Windows' 2026 WHCP-only kernel trust policy is exactly the failure
// this codebase already hit with the bundled VAD audio driver (problem code
// 52 — see the Phase 15.6 notes): a new binary dependency with no path to
// re-signing if Windows tightens further. The fix below adds zero new
// dependencies — it reuses the SYSTEM-impersonation/desktop-attach technique
// `capture/dda.rs` already proved live on this exact box.
//
// Design: the control-stream ENet loop (`control::start_control_server`) is
// a single dedicated OS thread for the process's lifetime (see lib.rs) with
// no windows/hooks of its own — the same precondition that let the DDA
// capture thread attach to Winlogon. `sync_desktop_for_input`, called once
// per packet from that thread, cheaply no-ops (one atomic load) unless
// `capture::desktop_switch`'s transition counter has moved, in which case it
// engages or releases `SecureDesktopGuard` — impersonate the service-supplied
// SYSTEM token, `OpenInputDesktop`, `SetThreadDesktop`. Thread-local, not a
// process-global: this state is only meaningful for the specific OS thread
// that owns it, and thread-locals naturally forbid any future caller on a
// different thread from misusing stale desktop-attachment state.
// ---------------------------------------------------------------------

thread_local! {
    /// `Some` while this thread is impersonating SYSTEM and attached to the
    /// secure desktop; `None` on the ordinary interactive desktop.
    static SECURE_DESKTOP_GUARD: RefCell<Option<SecureDesktopGuard>> = const { RefCell::new(None) };
    /// Last `desktop_switch::switch_generation()` this thread observed, so
    /// the common case (no transition since the last packet) costs one
    /// atomic load instead of re-deriving desktop state every packet.
    /// `u64::MAX` forces the first call to always evaluate.
    static LAST_DESKTOP_GENERATION: Cell<u64> = const { Cell::new(u64::MAX) };
}

/// Holds the SYSTEM impersonation + secure-desktop thread-attachment for as
/// long as the secure desktop is up. `Drop` restores this thread to whatever
/// desktop it had before (saved via `GetThreadDesktop` in [`engage`]) and
/// reverts impersonation — mirrors `capture::dda::DdaCapturer`'s teardown,
/// except this guard is long-lived and toggles many times per process, so it
/// must explicitly restore the previous desktop rather than just exiting.
struct SecureDesktopGuard {
    previous_desktop: HDESK,
    attached_desktop: HDESK,
    impersonating: bool,
}

impl SecureDesktopGuard {
    /// `GENERIC_ALL` for desktop access rights — what `SetThreadDesktop`
    /// needs (matches `capture::dda`'s `DESKTOP_GENERIC_ALL`).
    const DESKTOP_GENERIC_ALL: DESKTOP_ACCESS_FLAGS = DESKTOP_ACCESS_FLAGS(0x1000_0000);

    /// `DF_ALLOWOTHERACCOUNTHOOK` — the flag Sunshine passes to
    /// `OpenInputDesktop` (`misc.cpp::syncThreadDesktop`). It permits
    /// processes of other accounts to hook this desktop, which is what makes
    /// the handle usable for injection from a thread whose token differs from
    /// the desktop's owner (our case exactly: an elevated-user process
    /// impersonating SYSTEM to reach Winlogon).
    const DF_ALLOWOTHERACCOUNTHOOK: DESKTOP_CONTROL_FLAGS = DESKTOP_CONTROL_FLAGS(0x0001);

    fn engage() -> Option<Self> {
        unsafe {
            let previous_desktop = match GetThreadDesktop(GetCurrentThreadId()) {
                Ok(h) => h,
                Err(e) => {
                    eprintln!("⚠️  Input: GetThreadDesktop failed ({e:?}) — cannot safely attach to secure desktop");
                    return None;
                }
            };

            // Best-effort, like capture::dda's identical step: impersonation is
            // only NEEDED when the ambient thread identity isn't already
            // SYSTEM. Under the service's pre-login fallback (--system-fallback,
            // see service.rs) the whole process already IS SYSTEM-in-session,
            // so there's no token to impersonate and none is required — try
            // the desktop-attach regardless and let the OS be the judge (task/
            // manual launch with no SYSTEM identity at all fails here exactly
            // as before, gracefully).
            let impersonating = match crate::service::system_impersonation_token() {
                Some(tok) => ImpersonateLoggedOnUser(tok).is_ok(),
                None => false,
            };

            let attached_desktop = match OpenInputDesktop(
                Self::DF_ALLOWOTHERACCOUNTHOOK, false, Self::DESKTOP_GENERIC_ALL,
            ) {
                Ok(hdesk) => hdesk,
                Err(e) => {
                    eprintln!("⚠️  Input: OpenInputDesktop failed: {e:?} — \
                        secure-desktop input will stay frozen until it dismisses");
                    if impersonating {
                        let _ = RevertToSelf();
                    }
                    return None;
                }
            };
            if let Err(e) = SetThreadDesktop(attached_desktop) {
                eprintln!("⚠️  Input: SetThreadDesktop(secure) failed: {e:?}");
                let _ = CloseDesktop(attached_desktop);
                if impersonating {
                    let _ = RevertToSelf();
                }
                return None;
            }

            println!("🔐 Input: attached to secure desktop for mouse/keyboard injection");
            Some(Self { previous_desktop, attached_desktop, impersonating })
        }
    }
}

impl Drop for SecureDesktopGuard {
    fn drop(&mut self) {
        unsafe {
            // Restore BEFORE releasing impersonation/closing the handle —
            // this thread must always end up attached to a valid desktop.
            if let Err(e) = SetThreadDesktop(self.previous_desktop) {
                eprintln!("⚠️  Input: restoring previous desktop failed: {e:?} \
                    (mouse/keyboard may misbehave until the next transition)");
            }
            let _ = CloseDesktop(self.attached_desktop);
            if self.impersonating {
                let _ = RevertToSelf();
            }
        }
        println!("🔐 Input: secure desktop dismissed — back to the interactive desktop");
    }
}

/// **RE-ENABLED (2026-08-06)** — the 2026-07-30 report that motivated
/// disabling this ("user lost ALL local physical mouse/keyboard control at
/// the login screen") was a misdiagnosis, clarified by the user: local
/// physical input never froze. What died at the PIN screen was the REMOTE
/// Moonlight mouse/keyboard — which is precisely the Winlogon isolation
/// this guard exists to bridge (an elevated-user thread's `SendInput` is
/// discarded while the secure desktop has input focus), so disabling the
/// guard was CAUSING the observed symptom, not preventing a worse one.
/// With it engaged, the injecting thread impersonates the service-supplied
/// SYSTEM-in-session token and attaches to the Winlogon desktop —
/// Sunshine's model (it injects from a SYSTEM process with its input
/// thread synced to the current input desktop) — so remote PIN entry
/// works. The physical keyboard/mouse are hardware input and are never
/// touched by any of this.
const SECURE_DESKTOP_INPUT_ENABLED: bool = true;

/// Set in the `--system-input-helper` process (see [`set_always_follow_input_desktop`]).
static ALWAYS_FOLLOW_INPUT_DESKTOP: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Switch this process into "always attach to the input desktop" mode — what
/// the SYSTEM input helper wants, and what Sunshine does unconditionally.
///
/// The default (Worker) mode consults `capture::desktop_switch` and only
/// attaches while that monitor reports a secure desktop. The helper has no
/// such monitor (it is a bare process with no capture loop), and it exists
/// ONLY to serve secure-desktop interludes, so it should simply follow
/// whatever desktop currently has input focus: attach once at startup, and
/// re-attach reactively whenever an injection reports it went nowhere.
pub fn set_always_follow_input_desktop() {
    ALWAYS_FOLLOW_INPUT_DESKTOP.store(true, Ordering::Relaxed);
}

/// Attach the CALLING thread to the desktop that currently has input focus,
/// for the life of that thread (or until a re-sync moves it). Used by the
/// SYSTEM input helper at startup so the first packet doesn't have to pay a
/// rejected-injection round trip to discover the desktop.
///
/// Thread-affine, like everything else in this section: call it from the same
/// thread that will call [`handle_input_packet`].
pub fn attach_to_input_desktop() {
    if sync_desktop_for_input(true) {
        return;
    }
    // Already attached (or the attach failed — `engage` logs the reason, and
    // `send_input_synced`'s reactive path retries on the first real packet).
}

/// Engage/release [`SecureDesktopGuard`] on this thread to track
/// `capture::desktop_switch`'s view of which desktop currently has input
/// focus. Must only be called from the control-stream thread (the only
/// caller of [`handle_input_packet`]) — see the module note above for why
/// that thread specifically is safe to reattach.
///
/// The cheap path (no transition since the last packet) is one atomic load.
/// `force` skips that shortcut and reconciles unconditionally — used by
/// [`send_input_synced`] when the OS reports an injection went nowhere,
/// which is the authoritative signal that this thread is on the wrong
/// desktop (Sunshine does the same in `send_input`: retry once after a
/// `syncThreadDesktop`).
///
/// Returns `true` when the attachment state actually changed, so a caller
/// retrying a failed injection knows whether a retry is worthwhile.
fn sync_desktop_for_input(force: bool) -> bool {
    if !SECURE_DESKTOP_INPUT_ENABLED {
        return false;
    }
    let always_follow = ALWAYS_FOLLOW_INPUT_DESKTOP.load(Ordering::Relaxed);
    let gen = crate::capture::desktop_switch::switch_generation();
    if !force {
        if always_follow {
            // Helper mode: the only steady state is "attached". One
            // thread-local bool check, no desktop-switch monitor needed
            // (there isn't one in the helper process).
            if SECURE_DESKTOP_GUARD.with(|s| s.borrow().is_some()) {
                return false;
            }
        } else if LAST_DESKTOP_GENERATION.with(|c| c.get() == gen) {
            return false;
        }
    }

    let secure = always_follow
        || matches!(
            crate::capture::desktop_switch::current_input_desktop(),
            crate::capture::desktop_switch::InputDesktop::Secure
                | crate::capture::desktop_switch::InputDesktop::ScreenSaver
        );

    SECURE_DESKTOP_GUARD.with(|slot| {
        let mut slot = slot.borrow_mut();
        let changed = match (secure, slot.is_some()) {
            (true, false) => {
                *slot = SecureDesktopGuard::engage();
                slot.is_some()
            }
            (true, true) if force => {
                // Already attached, but injection still failed: the secure
                // desktop we hold may be a STALE one (a dismissed prompt
                // replaced by a fresh Winlogon instance gets a different
                // desktop object). Drop and re-open to bind the live one.
                *slot = None; // Drop restores the previous desktop + reverts
                *slot = SecureDesktopGuard::engage();
                slot.is_some()
            }
            (false, true) => {
                *slot = None; // Drop restores + reverts
                true
            }
            _ => false,
        };
        // Record the generation only once the desired state was actually
        // REACHED. Stamping it unconditionally (the original bug) meant a
        // failed engage was never retried: the guard stayed disengaged for
        // the entire secure-desktop interlude, so every remote keystroke at
        // the PIN screen was silently discarded.
        let settled = secure == slot.is_some();
        if settled {
            LAST_DESKTOP_GENERATION.with(|c| c.set(gen));
        }
        changed
    })
}

/// Inject one `INPUT` event, re-syncing this thread's desktop and retrying
/// once if the OS accepted nothing.
///
/// `SendInput` returns the number of events inserted; 0 means the event was
/// blocked — overwhelmingly because the calling thread is attached to a
/// different desktop than the one with input focus (the Winlogon/PIN-screen
/// case), which no amount of privilege on the packet itself can fix. This
/// mirrors Sunshine's `platform/windows/input.cpp::send_input` retry-after-
/// resync, and is what makes injection robust even when the desktop-switch
/// monitor misses (or is slow to see) a transition.
fn send_input_synced(input: INPUT) {
    const SIZE: i32 = std::mem::size_of::<INPUT>() as i32;
    unsafe {
        if SendInput(&[input], SIZE) == 1 {
            return;
        }
        // Nothing injected — reconcile the desktop attachment and retry once.
        if sync_desktop_for_input(true) && SendInput(&[input], SIZE) == 1 {
            return;
        }
    }
    // Still refused. Rate-limit: a wrong-desktop condition affects every
    // packet, and this runs on the control thread at up to ~200 Hz.
    let n = INPUT_REJECTED.fetch_add(1, Ordering::Relaxed);
    if n == 0 || n % 500 == 0 {
        println!(
            "⚠️  Input: SendInput injected nothing ({} total) — desktop={:?}, \
             secure-desktop attach={}",
            n + 1,
            crate::capture::desktop_switch::current_input_desktop(),
            SECURE_DESKTOP_GUARD.with(|s| s.borrow().is_some()),
        );
    }
}

/// Count of injections the OS refused even after a desktop re-sync — drives
/// the rate-limited diagnostic in [`send_input_synced`].
static INPUT_REJECTED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// True if this INPUT_DATA payload is a gamepad packet (dispatched to
/// ViGEmBus rather than `SendInput`).
///
/// Master uses this to keep gamepad traffic on the Worker even while
/// mouse/keyboard is detoured to the SYSTEM input helper: ViGEmBus is a kernel
/// bus device and was never subject to the UIPI swallow (gamepad input was
/// never among the symptoms), and routing it to the helper would have the
/// helper connect its OWN ViGEm client — a second virtual pad appearing, and
/// the Worker's going idle, every time the screen locked.
pub fn is_gamepad_packet(payload: &[u8]) -> bool {
    payload.len() >= 8
        && u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]])
            == MULTI_CONTROLLER_MAGIC_GEN5
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
    sync_desktop_for_input(false);
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
// Mouse injection via SendInput — two distinct paths by packet type.
//
// ABS packets (MOUSE_MOVE_ABS_MAGIC) carry a fractional position within the
// client's view — used for desktop cursor control.  These are resolved to
// desktop coordinates and injected as MOUSEEVENTF_ABSOLUTE |
// MOUSEEVENTF_VIRTUALDESK (0-65535 normalized to the Win32 virtual screen).
// Plain MOUSEEVENTF_ABSOLUTE (without VIRTUALDESK) maps onto the GDI *primary*
// monitor only; VIRTUALDESK covers the full multi-monitor desktop, which is
// essential during a Virtual Desktop session where the VDD is primary.
// Coordinates are computed against the ACTIVE CAPTURE RECT (set by
// `set_active_capture_rect` after every rebind) via `virtual_desktop_to_absolute`.
//
// REL packets (MOUSE_MOVE_REL_MAGIC_GEN5) carry raw camera/look deltas —
// game input, not desktop cursor control.  Games consume these through the
// Win32 raw-input path (WM_INPUT / GetRawInputData), not absolute cursor
// position.  These are injected as plain MOUSEEVENTF_MOVE (no ABSOLUTE flag),
// passing the wire delta straight to the OS in one SendInput call.
//
// The old REL path converted deltas to absolute by calling GetCursorPos then
// 4×GetSystemMetrics, which was 5 kernel transitions per packet.  At 100–200
// REL packets/s during a camera pan that was 500–1000 syscalls/s of overhead,
// plus subtle jitter when the game warped the cursor on its own frame.
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
    send_input_synced(INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi },
    });
}

fn send_key_input(ki: KEYBDINPUT) {
    send_input_synced(INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki },
    });
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
/// Passed directly to SendInput as a relative move (no `MOUSEEVENTF_ABSOLUTE`
/// flag). Games read camera deltas via raw input (`WM_INPUT` /
/// `GetRawInputData`), not the absolute cursor position, so this is both
/// correct and optimal: one `SendInput` call, zero extra syscalls.
fn inject_mouse_move_rel(payload: &[u8]) {
    if payload.len() < 12 {
        return;
    }
    let dx = i16::from_be_bytes([payload[8], payload[9]]) as i32;
    let dy = i16::from_be_bytes([payload[10], payload[11]]) as i32;
    if dx == 0 && dy == 0 {
        return;
    }
    send_mouse_input(MOUSEINPUT {
        dx,
        dy,
        mouseData: 0,
        dwFlags: MOUSEEVENTF_MOVE,
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

// ─── ViGEmBus driver startup check ────────────────────────────────────────────

/// Pinned official ViGEmBus release — the virtual Xbox 360 controller bus
/// driver that gamepad passthrough plugs into. v1.22.0 is the final release
/// (the project is in maintenance mode); GitHub release-asset URLs are
/// permanent, so pinning is safe.
const VIGEMBUS_SETUP_URL: &str =
    "https://github.com/nefarius/ViGEmBus/releases/download/v1.22.0/ViGEmBus_1.22.0_x64_x86_arm64.exe";
const VIGEMBUS_RELEASES_URL: &str = "https://github.com/nefarius/ViGEmBus/releases/latest";

/// Marker written next to the exe when the user declines the install prompt.
/// Nova auto-starts at every logon via the NovaServerBoot task, so without
/// the marker the prompt would nag on every boot. Delete it to be asked again.
const VIGEM_DECLINED_MARKER: &str = "vigem_install_declined.flag";

/// Startup probe for the ViGEmBus driver (virtual Xbox 360 controller bus).
/// Runs on a background thread — a missing driver must not delay pairing or
/// streaming startup; only gamepad passthrough depends on it. When absent,
/// offers to download and run the official installer. `GamepadManager`
/// connects per-session, so a mid-run install takes effect on the next
/// stream without restarting Nova.
pub fn check_vigem_driver_at_startup() {
    std::thread::spawn(|| match Client::connect() {
        Ok(_) => println!("🎮 ViGEmBus driver present — virtual Xbox 360 controller passthrough ready"),
        Err(e) => {
            println!("⚠️  ViGEmBus driver not detected ({e:?}) — gamepad passthrough unavailable");
            offer_vigem_install();
        }
    });
}

fn vigem_declined_marker_path() -> Option<std::path::PathBuf> {
    Some(std::env::current_exe().ok()?.parent()?.join(VIGEM_DECLINED_MARKER))
}

/// Yes/No prompt on the host's screen, mirroring the RetroArch installer
/// consent flow in app_launcher.rs. Declining writes the marker file so the
/// prompt is one-time; the missing driver is still logged on every start.
fn offer_vigem_install() {
    use windows::core::w;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, IDYES, MB_ICONQUESTION, MB_SETFOREGROUND, MB_TOPMOST, MB_YESNO,
    };

    let marker = vigem_declined_marker_path();
    if marker.as_ref().map_or(false, |m| m.exists()) {
        println!("   ↳ install prompt suppressed ({VIGEM_DECLINED_MARKER} present — delete it to be asked again)");
        return;
    }

    let result = unsafe {
        MessageBoxW(
            HWND(std::ptr::null_mut()),
            w!("Controller passthrough needs the ViGEmBus driver (virtual Xbox 360 controller), which is not installed.\n\nVideo, audio, mouse and keyboard streaming are unaffected.\n\nDownload and install ViGEmBus now?"),
            w!("Nova — Gamepad Driver"),
            MB_YESNO | MB_ICONQUESTION | MB_TOPMOST | MB_SETFOREGROUND,
        )
    };
    if result != IDYES {
        println!("   ↳ ViGEmBus install declined — writing {VIGEM_DECLINED_MARKER} (delete it to be asked again)");
        if let Some(m) = marker {
            let _ = std::fs::write(m, "Delete this file to re-enable Nova's ViGEmBus install prompt.\r\n");
        }
        return;
    }
    download_and_run_vigem_setup();
}

/// Download the pinned installer to %TEMP% (same Invoke-WebRequest pattern as
/// the RetroArch bootstrap in app_launcher.rs) and run it interactively — it
/// is a signed driver installer with its own wizard, and silent-install flags
/// vary between versions, so the user clicks through it. Falls back to
/// opening the releases page in the browser on any failure.
fn download_and_run_vigem_setup() {
    let setup_path = std::env::temp_dir().join("ViGEmBus_Setup.exe");
    println!("⬇️  Downloading ViGEmBus 1.22.0 → {}", setup_path.display());

    let download_ps = format!(
        "$ProgressPreference='SilentlyContinue'; Invoke-WebRequest -Uri '{url}' -OutFile '{out}'",
        url = VIGEMBUS_SETUP_URL,
        out = setup_path.display(),
    );
    let downloaded = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", &download_ps])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
        && setup_path.exists();

    if !downloaded {
        println!("⚠️  ViGEmBus download failed — opening the releases page in the default browser");
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", "", VIGEMBUS_RELEASES_URL])
            .spawn();
        return;
    }

    println!("🚀 Running ViGEmBus installer...");
    match std::process::Command::new(&setup_path).status() {
        Ok(status) if status.success() => match Client::connect() {
            Ok(_) => println!("✅ ViGEmBus installed — gamepad passthrough active from the next session"),
            Err(e) => println!("⚠️  ViGEmBus installed but the bus is not reachable yet ({e:?}) — a reboot may be required"),
        },
        Ok(status) => println!("⚠️  ViGEmBus installer exited with code {:?}", status.code()),
        Err(e) => println!("⚠️  Failed to launch ViGEmBus installer: {}", e),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Locks the Master-side routing predicate that keeps gamepad traffic on
    /// the Worker while mouse/keyboard detours to the SYSTEM input helper (see
    /// `is_gamepad_packet`'s doc comment for why that split matters).
    #[test]
    fn gamepad_packets_are_distinguished_from_kbm() {
        let mut pad = vec![0u8; 34];
        pad[4..8].copy_from_slice(&MULTI_CONTROLLER_MAGIC_GEN5.to_le_bytes());
        assert!(is_gamepad_packet(&pad));

        for magic in [
            KEY_DOWN_EVENT_MAGIC,
            KEY_UP_EVENT_MAGIC,
            MOUSE_MOVE_ABS_MAGIC,
            MOUSE_MOVE_REL_MAGIC_GEN5,
            MOUSE_BUTTON_DOWN_MAGIC_GEN5,
            MOUSE_BUTTON_UP_MAGIC_GEN5,
            SCROLL_MAGIC_GEN5,
        ] {
            let mut kbm = vec![0u8; 16];
            kbm[4..8].copy_from_slice(&magic.to_le_bytes());
            assert!(!is_gamepad_packet(&kbm), "magic {magic:#x} misrouted as gamepad");
        }

        // Truncated payloads must never be classified as gamepad (they would
        // otherwise be dropped by the helper instead of injected).
        assert!(!is_gamepad_packet(&[0u8; 4]));
        assert!(!is_gamepad_packet(&[]));
    }
}

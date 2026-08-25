//! Absolute touch injection — native Windows contacts, not a synthesised mouse.
//!
//! Echo's absolute-touch mode feeds real `POINTER_TOUCH_INFO` contacts into the
//! host through a synthetic pointer device, so Windows treats the phone as a
//! touchscreen: touch-sized hit targets, edge swipes, press-and-hold as a
//! right-click, and multi-touch reaching applications that read `WM_POINTER` /
//! `WM_TOUCH`. None of that is reachable by moving the mouse cursor and clicking
//! it, which is what `input::inject_mouse_move_abs` does and remains the right
//! thing for the pointer mode beside this one.
//!
//! ## The contract of `InjectSyntheticPointerInput`, and why this module has state
//!
//! The API is **frame-based, not event-based**. Every call must describe *every
//! contact currently on the glass* — a three-finger gesture is one call carrying
//! three `POINTER_TYPE_INFO` entries, not three calls. Injecting only the contact
//! that changed silently lifts the others, because absence from the frame is how
//! a lift is expressed.
//!
//! So the wire carries per-contact transitions (down / update / up, one packet
//! each, which is what a client can cheaply produce from a `MotionEvent`) and
//! this module reassembles them into frames. `CONTACTS` is that reassembly
//! buffer, and it is the reason this is a module rather than six lines inside
//! `input.rs`.
//!
//! Three rules that are easy to get wrong and produce input that is plausible
//! but wrong — the expensive kind:
//!
//! 1. **A contact's flags describe its transition, not its state.** `DOWN` on
//!    the frame where it lands, `UPDATE` on every frame after, `UP` on the frame
//!    where it lifts. A contact that reports `DOWN` twice is a second tap, and
//!    one that never reports `UP` is a finger Windows believes is still pressed
//!    — which is why [`release_all`] exists and why `stop_session` calls it
//!    unconditionally.
//! 2. **`UP` carries neither `INRANGE` nor `INCONTACT`.** Leaving them set is
//!    accepted by the call and leaves the contact hovering forever.
//! 3. **A lifted contact is forgotten only *after* the frame that reported its
//!    `UP`.** Removing it when the packet arrives means the lift never goes out.
//!
//! ## Coordinates
//!
//! `ptPixelLocation` is in **virtual-desktop pixels** — the same space
//! `input::current_capture_rect` describes and DXGI captures — not the 0–65535
//! normalised space `SendInput`'s `MOUSEEVENTF_ABSOLUTE` uses. The conversion the
//! mouse path performs (`virtual_desktop_to_absolute`) is therefore deliberately
//! absent here; applying it would land every touch in the desktop's top-left
//! corner.

use std::sync::Mutex;

use windows::Win32::Foundation::{HANDLE, HWND, POINT, RECT};
use windows::Win32::UI::Controls::{
    CreateSyntheticPointerDevice, DestroySyntheticPointerDevice, HSYNTHETICPOINTERDEVICE,
    POINTER_FEEDBACK_DEFAULT, POINTER_TYPE_INFO, POINTER_TYPE_INFO_0,
};
use windows::Win32::UI::Input::Pointer::{
    InjectSyntheticPointerInput, POINTER_CHANGE_NONE, POINTER_FLAGS, POINTER_FLAG_DOWN,
    POINTER_FLAG_INCONTACT, POINTER_FLAG_INRANGE, POINTER_FLAG_UP, POINTER_FLAG_UPDATE,
    POINTER_INFO, POINTER_TOUCH_INFO,
};
use windows::Win32::UI::WindowsAndMessaging::{PT_TOUCH, TOUCH_FLAG_NONE, TOUCH_MASK_CONTACTAREA};

/// Contacts the synthetic device is created with, and the ceiling on
/// simultaneous fingers. Ten matches what Windows reports for a mainstream
/// touchscreen and what the client's pointer tracking bounds itself to; a device
/// created for fewer refuses any frame that exceeds it.
pub const MAX_CONTACTS: usize = 10;

/// Half-width of the square reported as the contact patch, in desktop pixels.
///
/// `TOUCH_MASK_CONTACTAREA` is set because Windows sizes some of its touch
/// affordances from the patch — a zero-area contact is legal and lands, but
/// press-and-hold and drag thresholds then behave as if the user held a stylus.
/// This is roughly a fingertip at desktop scale; nothing downstream is sensitive
/// to the exact value.
const CONTACT_RADIUS_PX: i32 = 4;

/// One contact the host currently believes is on the glass.
#[derive(Clone, Copy)]
struct Contact {
    /// Client-assigned pointer id, stable for the life of one finger — which is
    /// what lets Windows track a drag rather than seeing a new tap per frame.
    id: u32,
    x: i32,
    y: i32,
    /// Set by an `up`; the contact is injected once more with `POINTER_FLAG_UP`
    /// and only then forgotten. Rule 3 in the module docs.
    lifting: bool,
    /// False until this contact has appeared in an injected frame, which is what
    /// selects `DOWN` over `UPDATE`.
    injected: bool,
}

/// The live contact set, at most [`MAX_CONTACTS`] long.
static CONTACTS: Mutex<Vec<Contact>> = Mutex::new(Vec::new());

/// The synthetic device, created lazily on first use.
///
/// `HSYNTHETICPOINTERDEVICE` is a raw handle and so not `Send`; it is valid from
/// any thread in the session that created it, which is how it is used here (the
/// input path is not pinned to a thread). The wrapper states that claim where it
/// is made, rather than hiding it behind an `AtomicIsize`.
struct Device(HSYNTHETICPOINTERDEVICE);
unsafe impl Send for Device {}

static DEVICE: Mutex<Option<Device>> = Mutex::new(None);

/// Whether device creation has already failed, so it is logged and retried once
/// per session rather than on every packet.
static CREATE_FAILED: Mutex<bool> = Mutex::new(false);

/// What one wire packet says happened to one contact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchEvent {
    Down,
    Update,
    Up,
    /// The gesture was taken over by something else — the client's own
    /// three-finger keyboard gesture, an incoming call, the app backgrounding.
    /// Treated exactly like [`TouchEvent::Up`]: Windows has no notion of a
    /// cancelled contact, and leaving it down is the one outcome that strands a
    /// finger on the host.
    Cancel,
}

impl TouchEvent {
    /// Map the wire byte. `None` for anything else, so an unknown event is a
    /// dropped packet rather than a guessed gesture.
    pub fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::Down),
            1 => Some(Self::Update),
            2 => Some(Self::Up),
            3 => Some(Self::Cancel),
            _ => None,
        }
    }
}

/// Apply one contact transition, then inject the frame it produces.
///
/// `x`/`y` are virtual-desktop pixels, already resolved against the capture rect
/// by the caller — this module does no coordinate maths, so there is exactly one
/// place (`input.rs`) where the capture rect is applied for both touch and mouse
/// and no chance of the two drifting apart.
pub fn apply(id: u32, event: TouchEvent, x: i32, y: i32) {
    let mut contacts = CONTACTS.lock().unwrap_or_else(|e| e.into_inner());
    let existing = contacts.iter().position(|c| c.id == id);

    match event {
        TouchEvent::Down => match existing {
            // A second `down` for a live id is the client repeating itself — a
            // retransmit, or a view that restarted the gesture. Treated as a
            // move: re-reporting DOWN registers a second tap, and double-taps
            // the user never made are worse than a contact that arrives late.
            Some(i) => {
                contacts[i].x = x;
                contacts[i].y = y;
            }
            None => {
                if contacts.len() >= MAX_CONTACTS {
                    // The device cannot carry it. Dropping the extra finger
                    // beats failing the whole frame and losing the nine that
                    // are already down.
                    return;
                }
                contacts.push(Contact { id, x, y, lifting: false, injected: false });
            }
        },
        TouchEvent::Update => match existing {
            Some(i) => {
                // A move for a contact already lifting is stale ordering; the
                // lift wins, because resurrecting it strands the finger.
                if contacts[i].lifting {
                    return;
                }
                contacts[i].x = x;
                contacts[i].y = y;
            }
            // A move with no matching down — the down was lost, or this is the
            // tail of a gesture from a previous session. Ignored rather than
            // synthesised into a press: an invented press is a click the user
            // did not make.
            None => return,
        },
        TouchEvent::Up | TouchEvent::Cancel => match existing {
            Some(i) => {
                contacts[i].x = x;
                contacts[i].y = y;
                contacts[i].lifting = true;
                // A contact that never made it into a frame was never pressed,
                // so there is nothing to lift and nothing to inject.
                if !contacts[i].injected {
                    contacts.remove(i);
                    return;
                }
            }
            None => return,
        },
    }

    inject_frame(&mut contacts);
}

/// Lift every contact the host believes is held, and drop the device.
///
/// Called from `input::stop_session`, and safe with nothing held. A session that
/// ends mid-gesture — the client backgrounded, the network gone — otherwise
/// leaves Windows with a finger pressed on the desktop, which no later session
/// can clear because the contact ids belonged to a client that no longer exists.
pub fn release_all() {
    {
        let mut contacts = CONTACTS.lock().unwrap_or_else(|e| e.into_inner());
        // Only contacts that were actually injected have a press to undo.
        contacts.retain(|c| c.injected);
        if !contacts.is_empty() {
            println!("👆 Touch: lifting {} stranded contact(s) at session end", contacts.len());
            for c in contacts.iter_mut() {
                c.lifting = true;
            }
            inject_frame(&mut contacts);
        }
        contacts.clear();
    }

    let taken = DEVICE.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(d) = taken {
        unsafe { DestroySyntheticPointerDevice(d.0) };
    }
    // Cleared so the next session gets one fresh creation attempt: the ordinary
    // reason creation fails is a desktop state (the lock screen) that the next
    // session will not be in.
    *CREATE_FAILED.lock().unwrap_or_else(|e| e.into_inner()) = false;
}

/// Build and inject the frame describing the whole contact set, then retire
/// whatever it lifted.
fn inject_frame(contacts: &mut Vec<Contact>) {
    let Some(device) = ensure_device() else {
        // No device: forget the state rather than accumulating contacts that
        // will never be injected, so a later successful creation starts clean.
        contacts.clear();
        return;
    };

    let mut frame: Vec<POINTER_TYPE_INFO> = Vec::with_capacity(contacts.len());
    for c in contacts.iter() {
        let flags = if c.lifting {
            // Rule 2: neither INRANGE nor INCONTACT on a lift.
            POINTER_FLAG_UP
        } else if c.injected {
            POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT
        } else {
            POINTER_FLAG_DOWN | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT
        };
        frame.push(touch_info(c, flags));
    }

    if frame.is_empty() {
        return;
    }

    if let Err(e) = unsafe { InjectSyntheticPointerInput(device, &frame) } {
        // Logged rather than swallowed: the ordinary causes are UIPI (an
        // elevated window under the contact) and the secure desktop, both of
        // which an operator can act on, and both of which are indistinguishable
        // from "touch does nothing" without this line.
        println!(
            "👆 Touch: InjectSyntheticPointerInput failed ({e}) — {} contact(s)",
            frame.len()
        );
    }

    // Rule 3: retire lifted contacts only now, after their UP has gone out.
    contacts.retain(|c| !c.lifting);
    for c in contacts.iter_mut() {
        c.injected = true;
    }
}

/// One `POINTER_TYPE_INFO` for one contact.
fn touch_info(c: &Contact, flags: POINTER_FLAGS) -> POINTER_TYPE_INFO {
    let point = POINT { x: c.x, y: c.y };
    let patch = RECT {
        left: c.x - CONTACT_RADIUS_PX,
        top: c.y - CONTACT_RADIUS_PX,
        right: c.x + CONTACT_RADIUS_PX,
        bottom: c.y + CONTACT_RADIUS_PX,
    };
    POINTER_TYPE_INFO {
        r#type: PT_TOUCH,
        Anonymous: POINTER_TYPE_INFO_0 {
            touchInfo: POINTER_TOUCH_INFO {
                pointerInfo: POINTER_INFO {
                    pointerType: PT_TOUCH,
                    pointerId: c.id,
                    frameId: 0,
                    pointerFlags: flags,
                    // Both null, deliberately. The source device is the
                    // synthetic one named in the injection call, and a null
                    // target lets Windows hit-test the point — naming an HWND
                    // would deliver every contact to that window regardless of
                    // where the user actually touched.
                    sourceDevice: HANDLE::default(),
                    hwndTarget: HWND::default(),
                    ptPixelLocation: point,
                    ptPixelLocationRaw: point,
                    // Himetric left zero: it is an alternative spelling of the
                    // same position for high-resolution digitisers, and Windows
                    // derives it when the pixel location is supplied.
                    ptHimetricLocation: POINT::default(),
                    ptHimetricLocationRaw: POINT::default(),
                    // Zero means "stamp it on arrival". Supplying the client's
                    // own clock would put the host's touch timeline on another
                    // machine's time base, and gesture recognition — flicks,
                    // press-and-hold — is timing.
                    dwTime: 0,
                    historyCount: 1,
                    InputData: 0,
                    dwKeyStates: 0,
                    PerformanceCount: 0,
                    ButtonChangeType: POINTER_CHANGE_NONE,
                },
                touchFlags: TOUCH_FLAG_NONE,
                touchMask: TOUCH_MASK_CONTACTAREA,
                rcContact: patch,
                rcContactRaw: patch,
                orientation: 0,
                pressure: 0,
            },
        },
    }
}

/// The synthetic device, created lazily on the first contact.
///
/// Lazy rather than created at session start because it is a real resource in
/// the user session and most sessions never use absolute touch — the mode is
/// opt-in on the client. A creation failure is remembered so the input path does
/// not retry it per packet.
fn ensure_device() -> Option<HSYNTHETICPOINTERDEVICE> {
    let mut guard = DEVICE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(d) = guard.as_ref() {
        return Some(d.0);
    }

    let mut failed = CREATE_FAILED.lock().unwrap_or_else(|e| e.into_inner());
    if *failed {
        return None;
    }

    // POINTER_FEEDBACK_DEFAULT gives injected contacts the same visual feedback
    // a real finger gets. That is wanted here: the person touching the glass is
    // on another device with no tactile sense of where the host thinks their
    // finger landed, and the ripple is the only cue that it landed at all.
    match unsafe {
        CreateSyntheticPointerDevice(PT_TOUCH, MAX_CONTACTS as u32, POINTER_FEEDBACK_DEFAULT)
    } {
        Ok(d) => {
            println!("👆 Touch: synthetic touchscreen created ({MAX_CONTACTS} contacts)");
            *guard = Some(Device(d));
            Some(d)
        }
        Err(e) => {
            println!(
                "⚠️  Touch: CreateSyntheticPointerDevice failed ({e}) — absolute touch \
                 unavailable this session; the client's pointer mode is unaffected"
            );
            *failed = true;
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Everything up to `inject_frame` is bookkeeping and testable without a
    // display. `inject_frame` itself degrades to "clear the set" when no device
    // can be created, which is the case in a test process — and on a box where
    // one *can* be created, pressing a contact and lifting it in the same call
    // is harmless.

    fn reset() {
        CONTACTS.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    fn ids() -> Vec<u32> {
        CONTACTS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|c| c.id)
            .collect()
    }

    #[test]
    fn update_without_down_is_ignored() {
        reset();
        apply(3, TouchEvent::Update, 100, 100);
        assert!(ids().is_empty(), "a move with no press must not invent a contact");
    }

    #[test]
    fn event_codes_map_exactly() {
        assert_eq!(TouchEvent::from_code(0), Some(TouchEvent::Down));
        assert_eq!(TouchEvent::from_code(1), Some(TouchEvent::Update));
        assert_eq!(TouchEvent::from_code(2), Some(TouchEvent::Up));
        assert_eq!(TouchEvent::from_code(3), Some(TouchEvent::Cancel));
        assert_eq!(TouchEvent::from_code(4), None);
    }

    #[test]
    fn a_press_and_lift_leaves_nothing_held() {
        reset();
        apply(1, TouchEvent::Down, 10, 10);
        apply(1, TouchEvent::Up, 10, 10);
        assert!(ids().is_empty());
    }

    #[test]
    fn release_all_is_safe_with_nothing_held() {
        reset();
        release_all();
        assert!(ids().is_empty());
    }
}

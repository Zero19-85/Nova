//! Desktop-switch detection — **Phase 15.1b: detection only**.
//!
//! ## What this watches
//!
//! Windows renders UAC elevation prompts, the logon screen, and Ctrl+Alt+Del on
//! the **secure desktop** (`WinSta0\Winlogon`). WGC — Nova's primary capture
//! backend — is bound to the interactive desktop (`WinSta0\Default`) and
//! delivers no frames while the secure desktop is up. This module detects the
//! switch, so that Phase 2's `DesktopManager` can swap to the DDA backend for
//! the duration and back again.
//!
//! **This phase performs no swap.** The monitor observes, logs transitions, and
//! exposes a stable read API ([`current_input_desktop`],
//! [`switch_generation`]) that runs side-effect-free alongside the WGC path.
//!
//! ## How detection works (Sunshine-pattern, two layers)
//!
//! 1. **Event-driven (primary):** `SetWinEventHook(EVENT_SYSTEM_DESKTOPSWITCH,
//!    WINEVENT_OUTOFCONTEXT)`. Out-of-context WinEvent callbacks are delivered
//!    during message retrieval, so the monitor thread runs a message pump.
//! 2. **Poll (fallback + belt-and-suspenders):** every pump timeout
//!    ([`POLL_INTERVAL_MS`]) the thread re-queries the input desktop via
//!    `OpenInputDesktop` + `GetUserObjectInformationW(UOI_NAME)`. This catches
//!    a hook that failed to install (the monitor still works, just with poll
//!    latency) and any switch whose event was coalesced or missed.
//!
//! A subtle but load-bearing classification rule: `OpenInputDesktop` commonly
//! fails with `E_ACCESSDENIED` **while the secure desktop is up**, because the
//! Winlogon desktop's ACL only admits SYSTEM/winlogon. Failure to open the
//! input desktop is therefore itself evidence of the secure desktop, not an
//! error to discard — we classify it as [`InputDesktop::Secure`] (and log the
//! underlying error at the transition). Once the two-process model lands
//! (Phase 2), the host's SYSTEM-derived token will be able to open it for real.
//!
//! ## Why the state is module-global
//!
//! There is exactly one input desktop per interactive session, and the
//! `WINEVENTPROC` callback carries no user-data pointer — so the state lives in
//! process-wide atomics and [`DesktopSwitchMonitor`] is the lifecycle handle
//! (spawn/stop) around them. Spawning is idempotent; a second handle observes
//! the same state.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc;
use std::thread;

use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, WPARAM};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, GetUserObjectInformationW, OpenInputDesktop, DESKTOP_CONTROL_FLAGS,
    DESKTOP_READOBJECTS, UOI_NAME,
};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, MsgWaitForMultipleObjects, PeekMessageW, PostThreadMessageW,
    TranslateMessage, EVENT_SYSTEM_DESKTOPSWITCH, MSG, PM_REMOVE, QS_ALLINPUT,
    WINEVENT_OUTOFCONTEXT, WM_QUIT,
};

/// Poll cadence for the fallback re-query (also the message-pump wake
/// interval). 250 ms keeps worst-case detection latency well under the
/// ~2-second window a UAC prompt realistically stays up, without measurable
/// cost — the query is two cheap kernel calls.
const POLL_INTERVAL_MS: u32 = 250;

/// Which desktop currently receives input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum InputDesktop {
    /// `WinSta0\Default` — the ordinary interactive desktop. WGC works.
    Default = 0,
    /// `WinSta0\Winlogon` — the secure desktop (UAC prompt, logon screen,
    /// Ctrl+Alt+Del), **or** the input desktop could not be opened at all
    /// (see module docs — access-denied here usually *means* secure desktop).
    /// WGC delivers nothing; Phase 2 swaps to DDA on this state.
    Secure = 1,
    /// `WinSta0\Screen-saver`.
    ScreenSaver = 2,
    /// A desktop name we don't recognize (third-party virtual desktops, etc.).
    Other = 3,
    /// No query has completed yet (monitor not started or first query pending).
    Unknown = 4,
}

impl InputDesktop {
    fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Default,
            1 => Self::Secure,
            2 => Self::ScreenSaver,
            3 => Self::Other,
            _ => Self::Unknown,
        }
    }
}

// ── Process-wide detection state ──────────────────────────────────────────────

/// Latest classified input desktop (`InputDesktop as u8`).
static CURRENT: AtomicU8 = AtomicU8::new(InputDesktop::Unknown as u8);
/// Increments on every observed transition. Phase 2's swap logic compares
/// generations instead of kinds so a fast Default→Secure→Default flip that
/// lands between two reads is still visible as "something changed".
static GENERATION: AtomicU64 = AtomicU64::new(0);
/// True while a monitor thread is running — makes `spawn` idempotent.
static MONITOR_RUNNING: AtomicBool = AtomicBool::new(false);
/// Monitor thread id, for `PostThreadMessageW(WM_QUIT)` on stop.
static MONITOR_THREAD_ID: AtomicU32 = AtomicU32::new(0);

/// The most recently observed input desktop. [`InputDesktop::Unknown`] until
/// the monitor's first query completes. Lock-free — safe to call from the
/// capture hot loop.
pub fn current_input_desktop() -> InputDesktop {
    InputDesktop::from_u8(CURRENT.load(Ordering::Acquire))
}

/// Monotonic transition counter (0 until the first transition). See
/// [`GENERATION`] for the rationale. The swap logic currently keys off the
/// desktop KIND (a sub-frame Default→Secure→Default flash needs no action, so
/// missing one is fine); the counter stays as the transition-observability API
/// (tests, diagnostics, and any future consumer that must not miss flips).
#[allow(dead_code)] // consumed by tests/diagnostics only, by design (see above)
pub fn switch_generation() -> u64 {
    GENERATION.load(Ordering::Acquire)
}

// ── Query + classification ────────────────────────────────────────────────────

/// Opens the current input desktop and classifies it by name. `Err` from
/// `OpenInputDesktop` is *classified*, not propagated — see module docs.
fn query_input_desktop() -> (InputDesktop, String) {
    unsafe {
        let hdesk = match OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_READOBJECTS) {
            Ok(h) => h,
            Err(e) => {
                // Cannot open the input desktop — on a stock system this is the
                // Winlogon desktop refusing our token. Treat as secure.
                return (
                    InputDesktop::Secure,
                    format!("<unopenable: {e:?} — classified Secure>"),
                );
            }
        };

        let mut name_buf = [0u16; 128];
        let mut needed: u32 = 0;
        let ok = GetUserObjectInformationW(
            HANDLE(hdesk.0),
            UOI_NAME,
            Some(name_buf.as_mut_ptr() as *mut _),
            (name_buf.len() * 2) as u32,
            Some(&mut needed),
        );
        let _ = CloseDesktop(hdesk);

        if ok.is_err() {
            // Opened but unnameable — vanishingly rare (mid-switch teardown).
            // Don't flap the state machine over it.
            return (InputDesktop::Unknown, "<unnameable>".to_string());
        }

        let len = name_buf.iter().position(|&c| c == 0).unwrap_or(name_buf.len());
        let name = String::from_utf16_lossy(&name_buf[..len]);
        let kind = if name.eq_ignore_ascii_case("Default") {
            InputDesktop::Default
        } else if name.eq_ignore_ascii_case("Winlogon") {
            InputDesktop::Secure
        } else if name.eq_ignore_ascii_case("Screen-saver") {
            InputDesktop::ScreenSaver
        } else {
            InputDesktop::Other
        };
        (kind, name)
    }
}

/// Re-queries the input desktop and publishes the result, logging transitions
/// only (this runs 4×/second on the poll path — steady state must be silent).
/// `source` says which layer noticed first ("event" / "poll" / "startup").
fn update_state(source: &str) {
    let (kind, name) = query_input_desktop();
    if kind == InputDesktop::Unknown {
        return; // transient mid-switch read — keep the last known state
    }

    let prev = InputDesktop::from_u8(CURRENT.swap(kind as u8, Ordering::AcqRel));
    if prev != kind {
        let gen = GENERATION.fetch_add(1, Ordering::AcqRel) + 1;
        println!(
            "🖥️  Input desktop switch [{source}] #{gen}: {prev:?} → {kind:?} (\"{name}\"){}",
            match kind {
                InputDesktop::Secure =>
                    " — WGC cannot capture this desktop; Phase 2 will swap to DDA here",
                _ => "",
            }
        );
    }
}

// ── WinEvent hook ─────────────────────────────────────────────────────────────

/// `WINEVENTPROC` for `EVENT_SYSTEM_DESKTOPSWITCH`. The event carries no
/// payload identifying WHICH desktop is now active, so it only triggers the
/// same re-query the poll path uses.
unsafe extern "system" fn desktop_switch_event_proc(
    _hook: HWINEVENTHOOK,
    event: u32,
    _hwnd: HWND,
    _id_object: i32,
    _id_child: i32,
    _event_thread: u32,
    _event_time: u32,
) {
    if event == EVENT_SYSTEM_DESKTOPSWITCH {
        update_state("event");
    }
}

// ── Monitor thread + lifecycle handle ─────────────────────────────────────────

/// Lifecycle handle for the process-wide desktop-switch monitor.
///
/// `spawn()` starts the background thread (idempotent — a second call logs and
/// returns an inert handle that observes the same global state). The thread is
/// stopped by `stop()` or by dropping the handle. Readers don't need the
/// handle: [`current_input_desktop`] / [`switch_generation`] are free
/// functions over the shared state.
pub struct DesktopSwitchMonitor {
    thread: Option<thread::JoinHandle<()>>,
}

impl DesktopSwitchMonitor {
    /// Start monitoring. Publishes the initial desktop state synchronously-ish
    /// (first update runs as the thread's first action), then transitions are
    /// event-driven with a 250 ms poll fallback.
    pub fn spawn() -> Self {
        if MONITOR_RUNNING.swap(true, Ordering::AcqRel) {
            println!("⚠️  DesktopSwitchMonitor already running — reusing the existing monitor");
            return Self { thread: None };
        }

        // Hand the thread id back so stop() can post WM_QUIT to the pump.
        let (tid_tx, tid_rx) = mpsc::channel::<u32>();

        let thread = thread::Builder::new()
            .name("nova-desktop-switch".into())
            .spawn(move || {
                let _ = tid_tx.send(unsafe { GetCurrentThreadId() });
                monitor_thread_main();
                MONITOR_RUNNING.store(false, Ordering::Release);
            })
            .ok();

        match (&thread, tid_rx.recv()) {
            (Some(_), Ok(tid)) => MONITOR_THREAD_ID.store(tid, Ordering::Release),
            _ => {
                // Spawn failed (or the thread died instantly) — detection is a
                // resilience feature, not a stream-critical one: log and run on.
                println!("⚠️  DesktopSwitchMonitor failed to start — secure-desktop detection disabled this run");
                MONITOR_RUNNING.store(false, Ordering::Release);
            }
        }

        Self { thread }
    }

    /// Stop the monitor thread (posts `WM_QUIT` to its pump and joins).
    /// The last published state and generation remain readable.
    #[allow(dead_code)] // lib.rs currently relies on Drop; explicit stop is for Phase 2's manager.
    pub fn stop(mut self) {
        self.stop_inner();
    }

    fn stop_inner(&mut self) {
        if let Some(handle) = self.thread.take() {
            let tid = MONITOR_THREAD_ID.swap(0, Ordering::AcqRel);
            if tid != 0 {
                unsafe {
                    let _ = PostThreadMessageW(tid, WM_QUIT, WPARAM(0), LPARAM(0));
                }
            }
            let _ = handle.join();
        }
    }
}

impl Drop for DesktopSwitchMonitor {
    fn drop(&mut self) {
        self.stop_inner();
    }
}

/// Thread body: install the WinEvent hook, then pump messages with a
/// [`POLL_INTERVAL_MS`] timeout — out-of-context WinEvent callbacks are
/// delivered during message retrieval, and every timeout doubles as the poll
/// fallback.
fn monitor_thread_main() {
    // Hook first, then the initial query — a switch landing between the two is
    // caught by the event; one landing before the hook by the initial query.
    let hook: HWINEVENTHOOK = unsafe {
        SetWinEventHook(
            EVENT_SYSTEM_DESKTOPSWITCH,
            EVENT_SYSTEM_DESKTOPSWITCH,
            None,
            Some(desktop_switch_event_proc),
            0, // all processes
            0, // all threads
            WINEVENT_OUTOFCONTEXT,
        )
    };
    if hook.is_invalid() {
        println!(
            "⚠️  SetWinEventHook(EVENT_SYSTEM_DESKTOPSWITCH) failed — \
             falling back to {POLL_INTERVAL_MS} ms polling only"
        );
    } else {
        println!("👁️  Desktop-switch monitor active (event hook + {POLL_INTERVAL_MS} ms poll fallback)");
    }

    update_state("startup");

    let mut msg = MSG::default();
    'pump: loop {
        // Wake on queue input (delivers the WinEvent callback) or timeout (poll).
        let _ = unsafe { MsgWaitForMultipleObjects(None, false, POLL_INTERVAL_MS, QS_ALLINPUT) };

        while unsafe { PeekMessageW(&mut msg, HWND::default(), 0, 0, PM_REMOVE) }.as_bool() {
            if msg.message == WM_QUIT {
                break 'pump;
            }
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        // Poll-path re-query. Also runs right after event-driven updates, which
        // is harmless: update_state only logs/bumps on an actual transition.
        update_state("poll");
    }

    if !hook.is_invalid() {
        unsafe {
            let _ = UnhookWinEvent(hook);
        }
    }
    println!("👁️  Desktop-switch monitor stopped");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Live query against the real input desktop — `cargo test` runs on the
    /// interactive desktop, so this exercises the OpenInputDesktop +
    /// GetUserObjectInformationW path end-to-end and pins the classification.
    /// (Same live-detection test style as `virtual_display::detect_vdd_devnode`.)
    #[test]
    fn classifies_the_interactive_desktop_as_default() {
        let (kind, name) = query_input_desktop();
        assert_eq!(
            kind,
            InputDesktop::Default,
            "expected the test runner's input desktop to classify as Default, got {kind:?} (name: {name})"
        );
    }

    /// The publish path: an observed state change must bump the generation
    /// exactly once, and a repeat observation of the same state must not.
    #[test]
    fn update_state_bumps_generation_only_on_transition() {
        // First update transitions Unknown → Default (live query).
        update_state("test");
        let gen_after_first = switch_generation();
        assert!(gen_after_first >= 1, "first update should record a transition");
        assert_eq!(current_input_desktop(), InputDesktop::Default);

        // Same desktop again — no transition, no bump.
        update_state("test");
        assert_eq!(switch_generation(), gen_after_first);
    }
}

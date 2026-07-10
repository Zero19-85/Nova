//! Process-death display recovery (Apollo/Sunshine parity).
//!
//! If Nova dies while a stream has the virtual display as the only active
//! output, the CCD database keeps that headless topology and the physical
//! monitor stays black — the exact failure Phase 13.2's boot healing repairs
//! after the fact. These hooks stop it from happening in the first place by
//! running [`crate::virtual_display::emergency_restore_for_shutdown`]
//! **synchronously, on the notification thread, before the process is allowed
//! to die**:
//!
//! 1. **Console control handler** (`SetConsoleCtrlHandler`) — CTRL_CLOSE /
//!    CTRL_LOGOFF / CTRL_SHUTDOWN. Registered AFTER tokio's own watchers so
//!    (handlers run in LIFO order) ours executes first, restores the display,
//!    then returns FALSE to chain into tokio's handler — which notifies the
//!    capture loop and parks the handler thread, giving the graceful teardown
//!    time to finish as well. Plain CTRL_C is passed through untouched: the
//!    process isn't force-killed in that case, so the ordered teardown at the
//!    end of `run()` handles it.
//!
//! 2. **Session-monitor window** — a dedicated thread owning an invisible
//!    top-level window, mirroring Sunshine's `SunshineSessionMonitorClass`
//!    (main.cpp). Needed because the tray thread already owns windows: on
//!    logoff/shutdown Windows delivers WM_QUERYENDSESSION → WM_ENDSESSION to
//!    every window-owning process and may terminate it as soon as its windows
//!    have answered — potentially BEFORE the console handler chain runs. The
//!    restore executes inside WM_ENDSESSION, which Windows waits on (Apollo
//!    blocks there via `lifetime::exit_sunshine(0, false)` for the same
//!    reason).
//!
//! Both paths funnel into the same claim-once snapshot, so whichever fires
//! first does the restore and the others no-op — including the normal
//! `deactivate_after_stream` teardown if the process turns out to get enough
//! time for it.

use windows::core::w;
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Console::{
    SetConsoleCtrlHandler, CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::SetProcessShutdownParameters;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
    PostQuitMessage, RegisterClassW, TranslateMessage, MSG, WINDOW_EX_STYLE, WINDOW_STYLE,
    WM_CLOSE, WM_DESTROY, WM_ENDSESSION, WM_QUERYENDSESSION, WNDCLASSW,
};

/// SHUTDOWN_NORETRY for `SetProcessShutdownParameters` (winbase.h) — don't
/// show the "this app is preventing shutdown" retry dialog for Nova.
const SHUTDOWN_NORETRY: u32 = 0x0000_0001;

/// Console control handler — runs on a system-spawned thread. For the
/// terminate-after-return events, restore the display synchronously HERE;
/// returning FALSE then chains to tokio's handler (registered earlier), which
/// wakes the capture loop and blocks this thread so the graceful teardown can
/// also complete.
unsafe extern "system" fn console_ctrl_handler(ctrl_type: u32) -> BOOL {
    match ctrl_type {
        ct if ct == CTRL_CLOSE_EVENT || ct == CTRL_LOGOFF_EVENT || ct == CTRL_SHUTDOWN_EVENT => {
            crate::virtual_display::emergency_restore_for_shutdown();
            BOOL(0)
        }
        _ => BOOL(0),
    }
}

/// Register the console hook. MUST be called after tokio's
/// `signal::windows::ctrl_close/ctrl_shutdown/ctrl_logoff` watchers exist:
/// console handlers run most-recently-registered first, and the emergency
/// restore has to happen before tokio's handler parks the thread.
pub fn install_console_hook() {
    unsafe {
        if let Err(e) = SetConsoleCtrlHandler(Some(console_ctrl_handler), true) {
            println!("⚠️  SetConsoleCtrlHandler(emergency restore): {e}");
        } else {
            println!("🛡️  Emergency display-restore console hook installed");
        }
    }
}

unsafe extern "system" fn session_monitor_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_QUERYENDSESSION => {
            // "OK to end the session" — the actual work happens in
            // WM_ENDSESSION, which the system delivers next and waits on.
            LRESULT(1)
        }
        WM_ENDSESSION => {
            // wparam == TRUE ⇒ the session IS ending (FALSE = another app
            // vetoed it). Windows may terminate us the moment this returns,
            // so the restore must complete synchronously first.
            if wparam.0 != 0 {
                println!("🛑 WM_ENDSESSION — restoring display state before termination");
                crate::virtual_display::emergency_restore_for_shutdown();
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Spawn the session-monitor thread: invisible top-level window + message
/// pump. Runs for the life of the process.
pub fn spawn_session_monitor() {
    let result = std::thread::Builder::new()
        .name("nova-session-monitor".to_string())
        .spawn(|| unsafe {
            // Shutdown level 0x100 (Sunshine parity): LOW priority — Nova is
            // notified after ordinary apps (including the streamed game) have
            // closed, so the display restore is the last word on the topology.
            let _ = SetProcessShutdownParameters(0x100, SHUTDOWN_NORETRY);

            let class_name = w!("NovaSessionMonitorClass");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(session_monitor_wndproc),
                hInstance: GetModuleHandleW(None).unwrap_or_default().into(),
                lpszClassName: class_name,
                ..Default::default()
            };
            if RegisterClassW(&wc) == 0 {
                println!("⚠️  Session monitor: RegisterClassW failed — WM_ENDSESSION restore unavailable");
                return;
            }
            // A real (invisible) top-level window, NOT message-only: HWND_MESSAGE
            // windows never receive WM_QUERYENDSESSION/WM_ENDSESSION.
            let hwnd = match CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class_name,
                w!("Nova Session Monitor"),
                WINDOW_STYLE(0),
                0, 0, 0, 0,
                None,
                None,
                wc.hInstance,
                None,
            ) {
                Ok(h) => h,
                Err(e) => {
                    println!("⚠️  Session monitor: CreateWindowExW failed ({e}) — WM_ENDSESSION restore unavailable");
                    return;
                }
            };
            let _ = hwnd;
            println!("🛡️  Session monitor window active — WM_ENDSESSION display restore armed");

            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        });
    if let Err(e) = result {
        println!("⚠️  Could not spawn session-monitor thread: {e}");
    }
}

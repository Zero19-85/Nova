use std::sync::{mpsc, Arc, Mutex};

use tokio::sync::watch;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NIM_SETVERSION, NOTIFYICONDATAW, NOTIFYICON_VERSION_4, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DispatchMessageW,
    GetCursorPos, HICON, HMENU, HWND_MESSAGE, IDI_APPLICATION, LoadIconW, MF_SEPARATOR, MF_STRING,
    MSG, PeekMessageW, PostMessageW, PostQuitMessage, RegisterClassExW, SetForegroundWindow,
    TPM_BOTTOMALIGN, TPM_RETURNCMD, TPM_RIGHTALIGN, TrackPopupMenu, TranslateMessage,
    WM_CONTEXTMENU, WM_DESTROY, WM_RBUTTONUP, WM_USER, WINDOW_EX_STYLE, WNDCLASSEXW,
    WS_OVERLAPPED, PM_REMOVE,
};

/// Commands the pairing layer (or any other subsystem) can send to the tray.
pub enum TrayCmd {
    /// Show a generic balloon notification: (title, body).
    Notify(String, String),
    /// Tear down the tray icon and exit the thread.
    Quit,
}

/// Spawn the tray thread.
///
/// * `rx`          — inbound commands (balloon requests, quit, etc.)
/// * `shutdown_tx` — sending `true` breaks the main capture loop cleanly
/// * `global_pin`  — shared PIN slot; the tray writes here when the user
///                   enters a PIN via the context menu dialog
pub fn spawn(
    rx: mpsc::Receiver<TrayCmd>,
    shutdown_tx: Arc<watch::Sender<bool>>,
    global_pin: Arc<Mutex<String>>,
) {
    std::thread::Builder::new()
        .name("nova-tray".to_string())
        .spawn(move || unsafe { tray_main(rx, shutdown_tx, global_pin) })
        .expect("failed to spawn tray thread");
}

// ── Constants ──────────────────────────────────────────────────────────────

const TRAY_ID: u32     = 1;
const WM_TRAYICON: u32 = WM_USER + 1;
const IDM_ENTER_PIN: u32 = 1001;
const IDM_QUIT: u32      = 1002;

// Static UTF-16 string literals — avoids the windows::w! macro whose import
// path differs across crate versions.
const CLASS_NAME_W: &[u16] = &[
    b'N' as u16, b'o' as u16, b'v' as u16, b'a' as u16,
    b'T' as u16, b'r' as u16, b'a' as u16, b'y' as u16,
    b'W' as u16, b'n' as u16, b'd' as u16, 0,
];
const NOVA_W: &[u16] = &[b'N' as u16, b'o' as u16, b'v' as u16, b'a' as u16, 0];
const ENTER_PIN_LABEL_W: &[u16] = &[
    b'E' as u16, b'n' as u16, b't' as u16, b'e' as u16, b'r' as u16, b' ' as u16,
    b'P' as u16, b'a' as u16, b'i' as u16, b'r' as u16, b'i' as u16, b'n' as u16,
    b'g' as u16, b' ' as u16, b'P' as u16, b'I' as u16, b'N' as u16,
    b'.' as u16, b'.' as u16, b'.' as u16, 0,
];
const QUIT_LABEL_W: &[u16] = &[
    b'Q' as u16, b'u' as u16, b'i' as u16, b't' as u16, b' ' as u16,
    b'N' as u16, b'o' as u16, b'v' as u16, b'a' as u16, 0,
];

// ── Main tray thread ───────────────────────────────────────────────────────

unsafe fn tray_main(
    rx: mpsc::Receiver<TrayCmd>,
    shutdown_tx: Arc<watch::Sender<bool>>,
    global_pin: Arc<Mutex<String>>,
) {
    let hmodule   = GetModuleHandleW(PCWSTR::null()).unwrap_or_default();
    let hinstance = HINSTANCE(hmodule.0);

    let class_name = PCWSTR(CLASS_NAME_W.as_ptr());
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wnd_proc),
        hInstance: hinstance,
        lpszClassName: class_name,
        ..Default::default()
    };
    RegisterClassExW(&wc);

    let hwnd = match CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        class_name,
        PCWSTR(NOVA_W.as_ptr()),
        WS_OVERLAPPED,
        0, 0, 0, 0,
        HWND_MESSAGE, // message-only — never shown on screen
        None,
        hinstance,
        None,
    ) {
        Ok(h) => h,
        Err(e) => { eprintln!("tray: CreateWindowExW failed: {e}"); return; }
    };

    // Load the embedded app icon (resource ID 1, written by build.rs + rc.exe).
    // Fall back to the generic Windows icon for dev builds without assets/.
    let hicon = LoadIconW(hinstance, PCWSTR(TRAY_ID as *const u16))
        .unwrap_or_else(|_| LoadIconW(None, IDI_APPLICATION).unwrap_or_default());

    let mut base_nid = make_nid(hwnd);
    base_nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    base_nid.uCallbackMessage = WM_TRAYICON;
    base_nid.hIcon = hicon;
    copy_wide(&mut base_nid.szTip, "Nova Game Streaming");
    let _ = Shell_NotifyIconW(NIM_ADD, &base_nid);

    // NOTIFYICON_VERSION_4: LOWORD(lParam) = event, HIWORD(lParam) = icon ID.
    let mut ver_nid = make_nid(hwnd);
    ver_nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
    let _ = Shell_NotifyIconW(NIM_SETVERSION, &ver_nid);

    let mut msg = MSG::default();
    loop {
        // Drain Win32 messages; intercept tray callbacks before dispatching.
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            if msg.message == 0x0012 {
                // WM_QUIT
                let _ = Shell_NotifyIconW(NIM_DELETE, &make_nid(hwnd));
                return;
            }

            // Tray icon callback.  With NOTIFYICON_VERSION_4,
            // LOWORD(lParam) contains the mouse/keyboard event code.
            if msg.message == WM_TRAYICON {
                let event = msg.lParam.0 as u32 & 0xFFFF;
                if event == WM_RBUTTONUP || event == WM_CONTEXTMENU {
                    let mut pt = POINT::default();
                    let _ = GetCursorPos(&mut pt);
                    match show_context_menu(hwnd, pt.x, pt.y) {
                        IDM_ENTER_PIN => {
                            // Show the input dialog on the tray thread (blocking).
                            // Writing the PIN to global_pin wakes the pairing
                            // async task that is polling the same Mutex.
                            match prompt_pin() {
                                Some(pin) => {
                                    *global_pin.lock().unwrap() = pin;
                                    show_balloon(hwnd, "Nova — PIN Accepted",
                                        "PIN received. Pairing will complete shortly.");
                                }
                                None => {
                                    show_balloon(hwnd, "Nova — PIN Cancelled",
                                        "No PIN entered. Try again from the tray menu.");
                                }
                            }
                        }
                        IDM_QUIT => {
                            let _ = shutdown_tx.send(true);
                            let _ = Shell_NotifyIconW(NIM_DELETE, &make_nid(hwnd));
                            return;
                        }
                        _ => {}
                    }
                }
                continue; // tray callbacks don't need TranslateMessage/Dispatch
            }

            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        // Handle commands from the rest of the process.
        match rx.try_recv() {
            Ok(TrayCmd::Notify(title, body)) => show_balloon(hwnd, &title, &body),
            Ok(TrayCmd::Quit) | Err(mpsc::TryRecvError::Disconnected) => {
                let _ = Shell_NotifyIconW(NIM_DELETE, &make_nid(hwnd));
                return;
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }

        std::thread::sleep(std::time::Duration::from_millis(16));
    }
}

// ── Context menu ───────────────────────────────────────────────────────────

/// Show the right-click context menu at `(x, y)` and return the selected
/// item's command ID, or 0 if the menu was dismissed without a selection.
/// Uses `TPM_RETURNCMD` so no WM_COMMAND handler is needed.
unsafe fn show_context_menu(hwnd: HWND, x: i32, y: i32) -> u32 {
    let menu = match CreatePopupMenu() {
        Ok(m) => m,
        Err(_) => return 0,
    };

    let _ = AppendMenuW(menu, MF_STRING, IDM_ENTER_PIN as usize,
                        PCWSTR(ENTER_PIN_LABEL_W.as_ptr()));
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR(std::ptr::null()));
    let _ = AppendMenuW(menu, MF_STRING, IDM_QUIT as usize,
                        PCWSTR(QUIT_LABEL_W.as_ptr()));

    // SetForegroundWindow is required so the menu dismisses when the user
    // clicks elsewhere (documented Win32 quirk for tray context menus).
    let _ = SetForegroundWindow(hwnd);

    let result = TrackPopupMenu(
        menu,
        TPM_RETURNCMD | TPM_BOTTOMALIGN | TPM_RIGHTALIGN,
        x, y, 0, hwnd, None,
    );

    let _ = DestroyMenu(menu);
    // Post WM_NULL to flush the internal menu state (another Win32 quirk).
    let _ = PostMessageW(hwnd, 0, WPARAM(0), LPARAM(0));

    result.0 as u32
}

// ── PIN input dialog ───────────────────────────────────────────────────────

/// Show a native Windows `InputBox` (via the VB runtime bundled with every
/// Windows install) and return the trimmed text, or `None` if the user
/// cancelled or entered something other than exactly 4 digits.
///
/// Runs PowerShell in a hidden window so no console flashes on screen.
/// The VB `InputBox` call blocks until the user clicks OK/Cancel, so this
/// function is intentionally synchronous and must only be called from the
/// dedicated tray OS thread.
fn prompt_pin() -> Option<String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let output = std::process::Command::new("powershell.exe")
        .creation_flags(CREATE_NO_WINDOW)
        .args([
            "-NonInteractive",
            "-Command",
            // Load the VB runtime (ships with every Windows), show InputBox.
            "[System.Reflection.Assembly]::LoadWithPartialName(\
                'Microsoft.VisualBasic') | Out-Null; \
             [Microsoft.VisualBasic.Interaction]::InputBox(\
                'Enter the 4-digit PIN shown on your Moonlight device.', \
                'Nova — Pairing', '')",
        ])
        .output()
        .ok()?;

    let raw = String::from_utf8_lossy(&output.stdout);
    let pin = raw.trim().to_string();

    if pin.len() == 4 && pin.chars().all(|c| c.is_ascii_digit()) {
        Some(pin)
    } else {
        None // cancelled (empty) or non-numeric
    }
}

// ── Balloon notifications ──────────────────────────────────────────────────

unsafe fn show_balloon(hwnd: HWND, title: &str, body: &str) {
    let mut nid = make_nid(hwnd);
    nid.uFlags = NIF_INFO;
    nid.dwInfoFlags = NIIF_INFO;
    copy_wide(&mut nid.szInfoTitle, title);
    copy_wide(&mut nid.szInfo, body);
    let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
}

// ── Helpers ────────────────────────────────────────────────────────────────

fn make_nid(hwnd: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ID,
        ..Default::default()
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM,
) -> LRESULT {
    if msg == WM_DESTROY { PostQuitMessage(0); }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

/// Copy a UTF-8 string into a fixed-width UTF-16 buffer, always NUL-terminating.
fn copy_wide(dest: &mut [u16], src: &str) {
    let wide: Vec<u16> = src.encode_utf16().collect();
    let n = wide.len().min(dest.len().saturating_sub(1));
    dest[..n].copy_from_slice(&wide[..n]);
    dest[n] = 0;
}

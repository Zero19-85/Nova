use std::sync::{mpsc, Arc};

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
    GetCursorPos, HICON, HMENU, HWND_MESSAGE, IDI_APPLICATION, LoadIconW, MF_STRING,
    MENU_ITEM_FLAGS, MSG, PeekMessageW, PostMessageW, PostQuitMessage, RegisterClassExW,
    SetForegroundWindow, TPM_BOTTOMALIGN, TPM_RETURNCMD, TPM_RIGHTALIGN, TRACK_POPUP_MENU_FLAGS,
    TrackPopupMenu, TranslateMessage, WM_CONTEXTMENU, WM_DESTROY, WM_RBUTTONUP, WM_USER,
    WINDOW_EX_STYLE, WNDCLASSEXW, WS_OVERLAPPED, PM_REMOVE,
};

/// Commands the pairing layer (or any other subsystem) can send to the tray.
pub enum TrayCmd {
    /// Show the 4-digit pairing PIN as a balloon notification.
    PairingPin(String),
    /// Generic balloon: (title, body).
    Notify(String, String),
    /// Tear down the tray icon and exit the thread.
    Quit,
}

/// Spawn the tray thread.
///
/// * `rx` — commands from the rest of the process (pairing PINs, etc.)
/// * `shutdown_tx` — sending `true` here breaks the main capture loop cleanly.
pub fn spawn(rx: mpsc::Receiver<TrayCmd>, shutdown_tx: Arc<watch::Sender<bool>>) {
    std::thread::Builder::new()
        .name("nova-tray".to_string())
        .spawn(move || unsafe { tray_main(rx, shutdown_tx) })
        .expect("failed to spawn tray thread");
}

// ── Constants ──────────────────────────────────────────────────────────────

const TRAY_ID: u32 = 1;
const WM_TRAYICON: u32 = WM_USER + 1;
const IDM_QUIT: usize = 1001;

// Static UTF-16 strings — avoids the windows::w! macro whose import path
// differs across crate versions.
const CLASS_NAME_W: &[u16] = &[
    b'N' as u16, b'o' as u16, b'v' as u16, b'a' as u16,
    b'T' as u16, b'r' as u16, b'a' as u16, b'y' as u16,
    b'W' as u16, b'n' as u16, b'd' as u16, 0,
];
const NOVA_W: &[u16] = &[b'N' as u16, b'o' as u16, b'v' as u16, b'a' as u16, 0];
const QUIT_LABEL_W: &[u16] = &[
    b'Q' as u16, b'u' as u16, b'i' as u16, b't' as u16, b' ' as u16,
    b'N' as u16, b'o' as u16, b'v' as u16, b'a' as u16, 0,
];

// ── Main tray thread ───────────────────────────────────────────────────────

unsafe fn tray_main(rx: mpsc::Receiver<TrayCmd>, shutdown_tx: Arc<watch::Sender<bool>>) {
    // GetModuleHandleW returns HMODULE; WNDCLASSEXW / CreateWindowExW /
    // LoadIconW need HINSTANCE.  Both are pointer-sized opaque handles.
    let hmodule = GetModuleHandleW(PCWSTR::null()).unwrap_or_default();
    let hinstance = HINSTANCE(hmodule.0);

    // Minimal hidden window class just to receive tray callback messages.
    let class_name = PCWSTR(CLASS_NAME_W.as_ptr());
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wnd_proc),
        hInstance: hinstance,
        lpszClassName: class_name,
        ..Default::default()
    };
    RegisterClassExW(&wc);

    // HWND_MESSAGE — message-only window, never shown on screen.
    let hwnd = match CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        class_name,
        PCWSTR(NOVA_W.as_ptr()),
        WS_OVERLAPPED,
        0, 0, 0, 0,
        HWND_MESSAGE,
        None,
        hinstance,
        None,
    ) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("tray: CreateWindowExW failed: {e}");
            return;
        }
    };

    // Load the embedded app icon (ID 1 compiled by build.rs) or fall back to
    // the generic Windows "application" icon for dev builds without assets/.
    let hicon = LoadIconW(hinstance, PCWSTR(TRAY_ID as *const u16))
        .unwrap_or_else(|_| LoadIconW(None, IDI_APPLICATION).unwrap_or_default());

    // Register the icon in the system tray.
    let mut base_nid = make_nid(hwnd);
    base_nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    base_nid.uCallbackMessage = WM_TRAYICON;
    base_nid.hIcon = hicon;
    copy_wide(&mut base_nid.szTip, "Nova Game Streaming");
    let _ = Shell_NotifyIconW(NIM_ADD, &base_nid);

    // NOTIFYICON_VERSION_4: modern balloon behaviour + richer wParam/lParam
    // encoding.  With this set: LOWORD(lParam) = notification event code,
    // HIWORD(lParam) = icon ID, LOWORD(wParam) = x, HIWORD(wParam) = y.
    let mut ver_nid = make_nid(hwnd);
    ver_nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
    let _ = Shell_NotifyIconW(NIM_SETVERSION, &ver_nid);

    let mut msg = MSG::default();
    loop {
        // Drain Win32 messages — intercept tray callbacks before dispatching.
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            if msg.message == 0x0012 {
                // WM_QUIT — tear down and exit.
                let _ = Shell_NotifyIconW(NIM_DELETE, &make_nid(hwnd));
                return;
            }

            // Tray icon callback (WM_USER+1).  With NOTIFYICON_VERSION_4,
            // LOWORD(lParam) carries the mouse/keyboard event that fired.
            if msg.message == WM_TRAYICON {
                let event = msg.lParam.0 as u32 & 0xFFFF;
                if event == WM_RBUTTONUP || event == WM_CONTEXTMENU {
                    let mut pt = POINT::default();
                    let _ = GetCursorPos(&mut pt);
                    if context_menu_quit_selected(hwnd, pt.x, pt.y) {
                        // Signal the main tokio loop to break cleanly.
                        let _ = shutdown_tx.send(true);
                        let _ = Shell_NotifyIconW(NIM_DELETE, &make_nid(hwnd));
                        return;
                    }
                }
                continue; // tray callbacks don't need TranslateMessage/Dispatch
            }

            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        // Handle commands from the rest of the process.
        match rx.try_recv() {
            Ok(TrayCmd::PairingPin(pin)) => {
                show_balloon(hwnd, "Nova — Pairing", &format!("Enter PIN in Moonlight: {pin}"));
            }
            Ok(TrayCmd::Notify(title, body)) => {
                show_balloon(hwnd, &title, &body);
            }
            Ok(TrayCmd::Quit) | Err(mpsc::TryRecvError::Disconnected) => {
                let _ = Shell_NotifyIconW(NIM_DELETE, &make_nid(hwnd));
                return;
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }

        // ~60 Hz polling — fast enough for UI feedback, negligible CPU cost.
        std::thread::sleep(std::time::Duration::from_millis(16));
    }
}

// ── Context menu ───────────────────────────────────────────────────────────

/// Show a right-click context menu at (x, y). Returns `true` if "Quit" was
/// selected.  Uses `TPM_RETURNCMD` so the selected item ID comes back as the
/// return value rather than via WM_COMMAND — no need for stateful wnd_proc.
unsafe fn context_menu_quit_selected(hwnd: HWND, x: i32, y: i32) -> bool {
    let menu = match CreatePopupMenu() {
        Ok(m) => m,
        Err(_) => return false,
    };

    let _ = AppendMenuW(menu, MF_STRING, IDM_QUIT, PCWSTR(QUIT_LABEL_W.as_ptr()));

    // SetForegroundWindow is required by Win32 so the menu dismisses when the
    // user clicks elsewhere (documented quirk for tray context menus).
    let _ = SetForegroundWindow(hwnd);

    let flags = TPM_RETURNCMD | TPM_BOTTOMALIGN | TPM_RIGHTALIGN;
    let result = TrackPopupMenu(menu, flags, x, y, 0, hwnd, None);

    let _ = DestroyMenu(menu);
    // Post WM_NULL to flush the internal menu state — another Win32 quirk.
    let _ = PostMessageW(hwnd, 0, WPARAM(0), LPARAM(0));

    result.0 as usize == IDM_QUIT
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
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_DESTROY {
        PostQuitMessage(0);
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

/// Copy a UTF-8 string into a fixed-width UTF-16 buffer, always NUL-terminating.
fn copy_wide(dest: &mut [u16], src: &str) {
    let wide: Vec<u16> = src.encode_utf16().collect();
    let n = wide.len().min(dest.len().saturating_sub(1));
    dest[..n].copy_from_slice(&wide[..n]);
    dest[n] = 0;
}

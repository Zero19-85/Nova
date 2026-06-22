use std::sync::mpsc;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NIM_SETVERSION, NOTIFYICONDATAW, NOTIFYICON_VERSION_4, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, HICON, HWND_MESSAGE, IDI_APPLICATION,
    LoadIconW, PeekMessageW, PostQuitMessage, RegisterClassExW, TranslateMessage, MSG, PM_REMOVE,
    WM_DESTROY, WM_USER, WNDCLASSEXW, WS_OVERLAPPED, WINDOW_EX_STYLE,
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

/// Spawn the tray thread. The caller owns the `SyncSender`; dropping it
/// causes the tray thread to exit cleanly on the next polling cycle.
pub fn spawn(rx: mpsc::Receiver<TrayCmd>) {
    std::thread::Builder::new()
        .name("nova-tray".to_string())
        .spawn(move || unsafe { tray_main(rx) })
        .expect("failed to spawn tray thread");
}

// ── Win32 implementation ───────────────────────────────────────────────────

const TRAY_ID: u32 = 1;
const WM_TRAYICON: u32 = WM_USER + 1;

// Static UTF-16 strings used as Win32 class / window names.
// Avoids the windows::w! macro path which differs between crate versions.
const CLASS_NAME_W: &[u16] = &[
    b'N' as u16, b'o' as u16, b'v' as u16, b'a' as u16,
    b'T' as u16, b'r' as u16, b'a' as u16, b'y' as u16,
    b'W' as u16, b'n' as u16, b'd' as u16, 0,
];
const NOVA_W: &[u16] = &[b'N' as u16, b'o' as u16, b'v' as u16, b'a' as u16, 0];

unsafe fn tray_main(rx: mpsc::Receiver<TrayCmd>) {
    // GetModuleHandleW returns HMODULE; window registration / icon loading
    // need HINSTANCE.  Both are pointer-sized opaque handles — cast directly.
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

    // Load the application icon from the embedded Win32 resource (ID 1,
    // compiled into the .exe by build.rs via rc.exe).  Falls back to the
    // generic application icon if the resource isn't present (dev builds).
    let hicon = LoadIconW(hinstance, PCWSTR(TRAY_ID as *const u16))
        .unwrap_or_else(|_| LoadIconW(None, IDI_APPLICATION).unwrap_or_default());

    // Register the icon in the system tray.
    let mut base_nid = make_nid(hwnd);
    base_nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    base_nid.uCallbackMessage = WM_TRAYICON;
    base_nid.hIcon = hicon;
    copy_wide(&mut base_nid.szTip, "Nova Game Streaming");
    let _ = Shell_NotifyIconW(NIM_ADD, &base_nid);

    // NOTIFYICON_VERSION_4: enables modern balloon behaviour.
    // Must be set immediately after NIM_ADD.
    let mut ver_nid = make_nid(hwnd);
    ver_nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
    let _ = Shell_NotifyIconW(NIM_SETVERSION, &ver_nid);

    println!("🖥️  Nova tray icon active");

    let mut msg = MSG::default();
    loop {
        // Drain all pending Win32 messages without blocking.
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            if msg.message == 0x0012 {
                // WM_QUIT — clean up and exit.
                let _ = Shell_NotifyIconW(NIM_DELETE, &make_nid(hwnd));
                return;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        // Check for commands from the rest of the process.
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

unsafe fn show_balloon(hwnd: HWND, title: &str, body: &str) {
    let mut nid = make_nid(hwnd);
    nid.uFlags = NIF_INFO;
    nid.dwInfoFlags = NIIF_INFO;
    copy_wide(&mut nid.szInfoTitle, title);
    copy_wide(&mut nid.szInfo, body);
    let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
}

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

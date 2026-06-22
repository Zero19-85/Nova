use std::sync::{mpsc, Arc, Mutex};

use tokio::sync::watch;
use tray_icon::{
    Icon, TrayIconBuilder, TrayIconEvent,
    menu::{Menu, MenuItem, MenuEvent, PredefinedMenuItem},
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, MSG, PeekMessageW, TranslateMessage, PM_REMOVE,
};

/// Commands the rest of the process can send to the tray thread.
pub enum TrayCmd {
    /// Update the tray tooltip to show a status string (e.g., "Pairing…").
    Notify(String, String),
    /// Force the tray thread to exit.
    Quit,
}

/// Spawn the dedicated tray OS thread.
///
/// * `rx`          — inbound commands from pairing / capture
/// * `shutdown_tx` — sending `true` here breaks the main capture loop
/// * `global_pin`  — shared slot the tray writes into when the user enters a PIN
pub fn spawn(
    rx: mpsc::Receiver<TrayCmd>,
    shutdown_tx: Arc<watch::Sender<bool>>,
    global_pin: Arc<Mutex<String>>,
) {
    std::thread::Builder::new()
        .name("nova-tray".to_string())
        .spawn(move || tray_main(rx, shutdown_tx, global_pin))
        .expect("failed to spawn tray thread");
}

// ── Tray thread ────────────────────────────────────────────────────────────

fn tray_main(
    rx: mpsc::Receiver<TrayCmd>,
    shutdown_tx: Arc<watch::Sender<bool>>,
    global_pin: Arc<Mutex<String>>,
) {
    // ── Build the right-click context menu ────────────────────────────────
    let pair_item = MenuItem::new("Pair Device", true, None);
    let quit_item = MenuItem::new("Quit Nova", true, None);

    let menu = Menu::new();
    let _ = menu.append(&pair_item);
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&quit_item);

    // Capture IDs now — the items will be moved into the menu.
    let pair_id = pair_item.id().clone();
    let quit_id = quit_item.id().clone();

    // ── Load the app icon ─────────────────────────────────────────────────
    // Try to load from the Win32 resource section (resource ID 1, compiled
    // into the .exe by build.rs via rc.exe).  Fall back to a plain blue
    // 16 × 16 RGBA square so the tray always shows *something*.
    let icon = Icon::from_resource(1, Some((32, 32))).unwrap_or_else(|_| {
        // RGBA: solid #0078D4 (Windows accent blue), fully opaque
        let px = [0u8, 120, 212, 255];
        Icon::from_rgba(px.repeat(16 * 16), 16, 16).expect("fallback tray icon")
    });

    // ── Create the tray icon ─────────────────────────────────────────────
    // tray-icon owns the hidden Win32 window, NOTIFYICONDATAW registration,
    // and SetForegroundWindow / TrackPopupMenu calls internally — all the
    // quirks our manual implementation was getting wrong.
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("Nova Game Streaming")
        .with_icon(icon)
        .build()
        .expect("failed to create system tray icon");

    // ── Event loop ────────────────────────────────────────────────────────
    let mut msg = MSG::default();
    loop {
        // Pump Win32 messages — required on Windows so tray-icon's hidden
        // window receives WM_TASKBARCREATED, tray callbacks, and menu WMs.
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        // ── Menu events (right-click context menu selections) ─────────────
        // MenuEvent::receiver() is a static channel populated by tray-icon's
        // internal window proc whenever a menu item is activated.
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            if event.id == pair_id {
                match prompt_for_pin() {
                    Some(pin) => {
                        *global_pin.lock().unwrap() = pin;
                        let _ = tray.set_tooltip(Some(
                            "Nova — PIN accepted, completing pairing…",
                        ));
                    }
                    None => {
                        let _ = tray.set_tooltip(Some("Nova Game Streaming"));
                    }
                }
            } else if event.id == quit_id {
                // Signal the main tokio capture loop to shut down cleanly.
                let _ = shutdown_tx.send(true);
                return; // exit the tray thread; TrayIcon drops and removes the icon
            }
        }

        // ── Tray icon events (left-click, double-click, balloon clicks) ───
        while let Ok(_event) = TrayIconEvent::receiver().try_recv() {
            // Nothing to do for now; could open the menu on left-click.
        }

        // ── Commands from the rest of the process ─────────────────────────
        match rx.try_recv() {
            Ok(TrayCmd::Notify(title, _body)) => {
                // No balloon API in tray-icon; update the tooltip instead so
                // hovering the icon shows the pairing status.
                let tip = format!("Nova — {title}");
                let _ = tray.set_tooltip(Some(&tip));
            }
            Ok(TrayCmd::Quit) | Err(mpsc::TryRecvError::Disconnected) => return,
            Err(mpsc::TryRecvError::Empty) => {}
        }

        std::thread::sleep(std::time::Duration::from_millis(16));
    }
}

// ── PIN input dialog ───────────────────────────────────────────────────────

/// Show a native Windows `InputBox` using the VisualBasic runtime (ships on
/// every Windows install).  Blocks until the user clicks OK or Cancel, then
/// returns the trimmed text.  Returns `None` if the user cancelled.
///
/// Uses `-WindowStyle Hidden` so no PowerShell console flashes on screen.
/// This function is intentionally synchronous and runs on the tray OS thread.
pub fn prompt_for_pin() -> Option<String> {
    let output = std::process::Command::new("powershell")
        .args(&[
            "-NoProfile",
            "-WindowStyle",
            "Hidden",
            "-Command",
            "Add-Type -AssemblyName Microsoft.VisualBasic; \
             [Microsoft.VisualBasic.Interaction]::InputBox(\
                'Enter the 4-digit PIN displayed on your Moonlight client:', \
                'Nova Device Pairing', '')",
        ])
        .output()
        .ok()?;

    let pin = String::from_utf8_lossy(&output.stdout).trim().to_string();

    if pin.is_empty() {
        None
    } else {
        Some(pin)
    }
}

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
/// * `global_pin`  — shared slot the tray writes `(pin, device_name)` into
///                   when the user pre-enters credentials via "Pair Device"
pub fn spawn(
    rx: mpsc::Receiver<TrayCmd>,
    shutdown_tx: Arc<watch::Sender<bool>>,
    global_pin: Arc<Mutex<(String, String)>>,
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
    global_pin: Arc<Mutex<(String, String)>>,
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
                match prompt_for_pin_and_name() {
                    Some((pin, name)) => {
                        *global_pin.lock().unwrap() = (pin, name);
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

// ── Pairing input dialog ───────────────────────────────────────────────────

/// Show two sequential native Windows `InputBox` dialogs using the
/// VisualBasic runtime (ships on every Windows install):
///   1. The 4-digit PIN shown on the Moonlight client.
///   2. A friendly device name to identify this client (e.g. "Xbox").
///
/// Returns `Some((pin, name))` on success, `None` if the user cancels
/// the PIN dialog.  Cancelling the name dialog is accepted — a default
/// name is generated.  Runs synchronously on the tray OS thread.
pub fn prompt_for_pin_and_name() -> Option<(String, String)> {
    let output = std::process::Command::new("powershell")
        .args(&[
            "-NoProfile",
            "-WindowStyle",
            "Hidden",
            "-Command",
            "Add-Type -AssemblyName Microsoft.VisualBasic; \
             $pin = [Microsoft.VisualBasic.Interaction]::InputBox(\
                'Enter the 4-digit PIN displayed on your Moonlight client:', \
                'Nova — Pair Device (1/2)', ''); \
             if ($pin -eq '') { exit 1 }; \
             $name = [Microsoft.VisualBasic.Interaction]::InputBox(\
                'Give this device a name (e.g. Xbox, Phone, TV):', \
                'Nova — Pair Device (2/2)', 'My Device'); \
             Write-Output \"$pin|$name\"",
        ])
        .output()
        .ok()?;

    if !output.status.success() {
        return None; // user cancelled the PIN dialog
    }

    let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if raw.is_empty() {
        return None;
    }

    let mut parts = raw.splitn(2, '|');
    let pin  = parts.next().unwrap_or("").trim().to_string();
    let name = parts.next().unwrap_or("").trim().to_string();

    if pin.is_empty() {
        return None;
    }

    let name = if name.is_empty() { "My Device".to_string() } else { name };
    Some((pin, name))
}

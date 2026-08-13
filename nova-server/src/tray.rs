//! System-tray UI: menu, pairing dialog, live Server Stats window, and the
//! streaming/idle icon state.
//!
//! ## Where this runs, and why that matters
//!
//! Under the Session-Survival Architecture this thread lives in the **Worker**
//! (`lib.rs::run_worker`) — the Master is a LocalSystem service in Session 0 and
//! cannot show UI at all. But the state three of these menu items act on lives
//! in the *Master*: the client session (`rtsp::ClientInfo`), the ENet control
//! peer, and pairing's in-memory trust store. So this module never performs
//! those actions itself; it raises a [`TrayAction`] and lets the owning process
//! decide. In the Worker that becomes a `ControlMsg` over `\\.\pipe\NovaControl`;
//! in the monolithic `run()` it is handled in-process. Doing the work here
//! instead would tear down capture while the Master happily kept the session
//! alive, and would "revoke" pairings that the Master still holds in memory and
//! would write straight back to disk on the next pair.
//!
//! Telemetry goes the other way and needs no IPC at all: the capture loop in
//! THIS process publishes to `crate::stats`, and the window below reads those
//! atomics directly — see that module's docs for why the numbers are labelled
//! `Encode` rather than throughput.
//!
//! ## One thread, one message pump
//!
//! Everything here shares the single `nova-tray` OS thread and the pump at the
//! bottom of [`tray_main`]. The Server Stats window is created on that thread,
//! so its `WM_PAINT`/`WM_CLOSE` are dispatched by the pump that already exists
//! for the tray icon — no second thread, no second pump, no UI framework.
//!
//! The known cost of that choice: [`prompt_for_pin_and_name`] blocks this
//! thread on a PowerShell dialog, so while a pairing prompt is open the stats
//! window stops repainting and menu clicks queue behind it. Pre-existing
//! behaviour, seconds long, and the price of not spawning a thread per surface.

use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::watch;
use tray_icon::{
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
    Icon, TrayIconBuilder, TrayIconEvent,
};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontW, CreateSolidBrush,
    DeleteDC, DeleteObject, EndPaint, FillRect, GetDC, GetDIBits, GetDeviceCaps,
    GetTextExtentPoint32W, InvalidateRect, ReleaseDC, SelectObject, SetBkMode, SetTextColor, TextOutW,
    BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP, HDC, HFONT, LOGPIXELSX,
    PAINTSTRUCT, SRCCOPY, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRect, CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyWindow,
    DispatchMessageW, GetIconInfo, IsWindow, LoadImageW, MessageBoxW, PeekMessageW,
    RegisterClassW, SetForegroundWindow, ShowWindow, SystemParametersInfoW, TranslateMessage,
    CS_HREDRAW, CS_VREDRAW, ICONINFO, IDYES, IMAGE_ICON, LR_DEFAULTCOLOR, MB_ICONQUESTION,
    MB_ICONWARNING, MB_YESNO, MSG, PM_REMOVE, SPI_GETWORKAREA, SW_SHOW,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, WM_CLOSE, WM_DESTROY, WM_ERASEBKGND,
    WM_PAINT, WNDCLASSW, WS_CAPTION, WS_EX_TOOLWINDOW, WS_POPUP, WS_SYSMENU,
};

/// Commands the rest of the process can send to the tray thread.
pub enum TrayCmd {
    /// Update the tray tooltip to show a status string (e.g., "Pairing…").
    Notify(String, String),
    /// Immediately open the PIN + device-name dialog (triggered by getservercert
    /// so the dialog is already showing before clientchallenge arrives — avoids
    /// any client-side HTTP response timeout).  No-op if a PIN is already waiting.
    OpenPairDialog,
    /// Force the tray thread to exit.
    Quit,
}

/// A user-initiated action from the tray menu, for the owning process to carry
/// out — see this module's header for why the tray never performs these itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    /// "End Stream": tear the live session down (physical monitor restored, DXGI
    /// and audio hooks released) while leaving the server listening for the next
    /// client. Already confirmed with the user by the time this is sent.
    EndStream,
    /// "Clear Paired Devices": drop every entry from the trust store, on disk
    /// and in memory. Already confirmed with the user by the time this is sent.
    ClearPairedDevices,
}

/// How long a pairing status message owns the tooltip before the live
/// stream summary takes it back. Long enough to read; short enough that a
/// finished pairing doesn't leave a stale "completing pairing…" forever.
const STATUS_TOOLTIP_HOLD: Duration = Duration::from_secs(15);

/// Repaint/refresh cadence for the stats window and tooltip. Twice a second
/// tracks a 1 Hz sampler without visible lag and costs nothing measurable.
const UI_REFRESH_INTERVAL: Duration = Duration::from_millis(500);

/// Spawn the dedicated tray OS thread.
///
/// * `rx`          — inbound commands from pairing / capture
/// * `shutdown_tx` — sending `true` here breaks the main capture loop
/// * `global_pin`  — shared slot the tray writes `(pin, device_name)` into
///                   when the user pre-enters credentials via "Pair Device"
/// * `action_tx`   — outbound menu actions (End Stream / Clear Paired Devices);
///                   a bounded channel, since these arrive at human cadence and
///                   a full queue means the consumer is wedged, not busy
pub fn spawn(
    rx: mpsc::Receiver<TrayCmd>,
    shutdown_tx: Arc<watch::Sender<bool>>,
    global_pin: Arc<Mutex<(String, String)>>,
    action_tx: mpsc::SyncSender<TrayAction>,
) {
    std::thread::Builder::new()
        .name("nova-tray".to_string())
        .spawn(move || tray_main(rx, shutdown_tx, global_pin, action_tx))
        .expect("failed to spawn tray thread");
}

// ── Tray thread ────────────────────────────────────────────────────────────

fn tray_main(
    rx: mpsc::Receiver<TrayCmd>,
    shutdown_tx: Arc<watch::Sender<bool>>,
    global_pin: Arc<Mutex<(String, String)>>,
    action_tx: mpsc::SyncSender<TrayAction>,
) {
    // Both icon states are built ONCE, here — the active one costs a GDI
    // round-trip through the resource icon's bitmap, which must never land on
    // the per-tick path that swaps between them.
    let (idle_icon, active_icon) = build_icons();

    // ── Create the tray icon (with pre-login retry) ──────────────────────
    // tray-icon owns the hidden Win32 window, NOTIFYICONDATAW registration,
    // and SetForegroundWindow / TrackPopupMenu calls internally — all the
    // quirks our manual implementation was getting wrong.
    //
    // Shell_NotifyIconW needs the shell (Explorer taskbar). The service-
    // launched host can start at the logon screen where no shell exists yet —
    // creation fails there. That must NOT panic (it used to, once per boot-
    // loop respawn): retry quietly until the user logs in and the taskbar
    // appears, then the icon shows up as normal. The menu/icon values are
    // consumed by the builder, so they are rebuilt on every attempt.
    let mut logged_wait = false;
    let (tray, ids, end_item) = loop {
        // Order puts the session actions first (what a user reaches for mid-
        // stream) and the destructive ones behind separators.
        let stats_item = MenuItem::new("Server Stats…", true, None);
        // Starts disabled: with no session there is nothing to end, and a
        // greyed item explains that better than an error dialog would.
        let end_item = MenuItem::new("End Stream", crate::stats::is_streaming(), None);
        let pair_item = MenuItem::new("Pair Device", true, None);
        let clear_item = MenuItem::new("Clear Paired Devices…", true, None);
        let quit_item = MenuItem::new("Quit Nova", true, None);

        let menu = Menu::new();
        let _ = menu.append(&stats_item);
        let _ = menu.append(&end_item);
        let _ = menu.append(&PredefinedMenuItem::separator());
        let _ = menu.append(&pair_item);
        let _ = menu.append(&clear_item);
        let _ = menu.append(&PredefinedMenuItem::separator());
        let _ = menu.append(&quit_item);

        // Capture IDs now — the items are borrowed into the menu above, but
        // `end_item` is also kept whole so its enabled state can be toggled as
        // sessions come and go.
        let ids = MenuIds {
            stats: stats_item.id().clone(),
            end: end_item.id().clone(),
            pair: pair_item.id().clone(),
            clear: clear_item.id().clone(),
            quit: quit_item.id().clone(),
        };

        match TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("Nova Game Streaming")
            .with_icon(idle_icon.clone())
            .build()
        {
            Ok(tray) => break (tray, ids, end_item),
            Err(e) => {
                if !logged_wait {
                    println!(
                        "ℹ️  Tray icon unavailable ({e}) — no shell yet \
                         (pre-login?). Retrying every 10 s until the taskbar \
                         exists."
                    );
                    logged_wait = true;
                }
                std::thread::sleep(std::time::Duration::from_secs(10));
            }
        }
    };
    if logged_wait {
        println!("✅ Tray icon created after shell became available");
    }

    // ── Event loop ────────────────────────────────────────────────────────
    let mut msg = MSG::default();
    // Icon/tooltip state, so neither is pushed to the shell unless it changed —
    // Shell_NotifyIconW is an IPC to Explorer, not a local write.
    let mut shown_streaming: Option<bool> = None;
    let mut shown_tooltip = String::new();
    // A pairing status message temporarily owns the tooltip; see STATUS_TOOLTIP_HOLD.
    let mut status_until: Option<Instant> = None;
    let mut last_refresh = Instant::now() - UI_REFRESH_INTERVAL;

    loop {
        // Pump Win32 messages — required on Windows so tray-icon's hidden
        // window receives WM_TASKBARCREATED, tray callbacks, and menu WMs.
        // The Server Stats window is created on this same thread, so this is
        // also what dispatches its WM_PAINT and WM_CLOSE.
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
            if event.id == ids.stats {
                open_or_focus_stats_window();
            } else if event.id == ids.end {
                // Confirmed rather than immediate: the click lands on a menu the
                // user opened for some other reason often enough, and this drops
                // a live session.
                if confirm(
                    "End the active streaming session?\n\n\
                     The virtual display is torn down, your physical monitor is \
                     restored, and the client is disconnected.\n\n\
                     Nova keeps running and stays ready for the next connection.",
                    "Nova — End Stream",
                    MB_ICONQUESTION,
                ) {
                    println!("🛑 Tray: \"End Stream\" — requesting session teardown");
                    if action_tx.try_send(TrayAction::EndStream).is_err() {
                        println!("⚠️  Tray: End Stream could not be delivered (queue full or receiver gone)");
                    }
                }
            } else if event.id == ids.pair {
                match prompt_for_pin_and_name() {
                    Some((pin, name)) => {
                        *global_pin.lock().unwrap() = (pin, name);
                        let _ = tray.set_tooltip(Some("Nova — PIN accepted, completing pairing…"));
                        status_until = Some(Instant::now() + STATUS_TOOLTIP_HOLD);
                    }
                    None => status_until = None,
                }
            } else if event.id == ids.clear {
                // Irreversible: every device must re-pair afterwards, so this
                // one gets the warning icon rather than the question icon.
                if confirm(
                    "Remove ALL paired devices?\n\n\
                     Every client will have to pair again with a new PIN before \
                     it can connect.\n\n\
                     A session that is already streaming keeps running until it \
                     ends — its connection was authorised when it started.",
                    "Nova — Clear Paired Devices",
                    MB_ICONWARNING,
                ) {
                    println!("🗑️  Tray: \"Clear Paired Devices\" — requesting trust-store wipe");
                    if action_tx.try_send(TrayAction::ClearPairedDevices).is_err() {
                        println!("⚠️  Tray: Clear Paired Devices could not be delivered (queue full or receiver gone)");
                    }
                }
            } else if event.id == ids.quit {
                // Signal the main capture loop to shut down cleanly.
                close_stats_window();
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
                shown_tooltip = tip;
                status_until = Some(Instant::now() + STATUS_TOOLTIP_HOLD);
            }
            Ok(TrayCmd::OpenPairDialog) => {
                // Only open if no PIN is already waiting (avoid double-prompt
                // when the user pre-entered via "Pair Device" before this arrives).
                let needs_input = global_pin.lock().unwrap().0.is_empty();
                if needs_input {
                    // No status hold to set alongside this one: the dialog below
                    // blocks this thread, so nothing can read (or overwrite) the
                    // tooltip until it returns and sets the real hold.
                    let _ = tray.set_tooltip(Some("Nova — Enter pairing PIN…"));
                    match prompt_for_pin_and_name() {
                        Some((pin, name)) => {
                            *global_pin.lock().unwrap() = (pin, name);
                            let _ =
                                tray.set_tooltip(Some("Nova — PIN accepted, completing pairing…"));
                            status_until = Some(Instant::now() + STATUS_TOOLTIP_HOLD);
                        }
                        None => status_until = None,
                    }
                }
            }
            Ok(TrayCmd::Quit) | Err(mpsc::TryRecvError::Disconnected) => {
                close_stats_window();
                return;
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }

        // ── Live state: icon, tooltip, menu enablement, stats repaint ─────
        if last_refresh.elapsed() >= UI_REFRESH_INTERVAL {
            last_refresh = Instant::now();
            let snap = crate::stats::snapshot();

            if shown_streaming != Some(snap.streaming) {
                shown_streaming = Some(snap.streaming);
                // Falls back to the idle icon if the badge composite failed at
                // startup — a missing state cue beats a missing tray icon.
                let icon = if snap.streaming {
                    active_icon.clone().unwrap_or_else(|| idle_icon.clone())
                } else {
                    idle_icon.clone()
                };
                let _ = tray.set_icon(Some(icon));
                end_item.set_enabled(snap.streaming);
            }

            let status_active = status_until.is_some_and(|t| Instant::now() < t);
            if status_active {
                // A pairing message owns the tooltip right now — don't clobber it.
            } else {
                status_until = None;
                let tip = snap.tooltip_text();
                if tip != shown_tooltip {
                    let _ = tray.set_tooltip(Some(&tip));
                    shown_tooltip = tip;
                }
            }

            refresh_stats_window();
        }

        std::thread::sleep(std::time::Duration::from_millis(16));
    }
}

/// Menu item IDs captured at build time (the items themselves are borrowed into
/// the `Menu`, which owns them from then on).
struct MenuIds {
    stats: tray_icon::menu::MenuId,
    end: tray_icon::menu::MenuId,
    pair: tray_icon::menu::MenuId,
    clear: tray_icon::menu::MenuId,
    quit: tray_icon::menu::MenuId,
}

// ── Server Stats window ────────────────────────────────────────────────────
//
// A plain Win32 window painted with GDI: no framework, no extra thread, and no
// dependency beyond the `windows` bindings already in the tree. The window
// belongs to the tray thread, so `tray_main`'s pump dispatches its messages and
// the 500 ms tick above drives its repaint.

/// Live stats window handle, or 0 when closed. The window proc is a bare
/// `extern "system"` fn with no user-data slot of its own, so the handle lives
/// in a static — same reason `capture::desktop_switch` keeps its state in
/// atomics. Only ever touched from the tray thread.
static STATS_HWND: AtomicIsize = AtomicIsize::new(0);

/// Logical (96-DPI) window metrics, scaled by the real DPI at paint time.
const STATS_W: i32 = 380;
const STATS_H: i32 = 300;

fn stats_hwnd() -> Option<HWND> {
    let raw = STATS_HWND.load(Ordering::Relaxed);
    if raw == 0 {
        return None;
    }
    let hwnd = HWND(raw as *mut core::ffi::c_void);
    // A window destroyed behind our back (shell restart) would otherwise leave
    // a dangling handle that InvalidateRect happily writes to.
    if unsafe { IsWindow(hwnd) }.as_bool() {
        Some(hwnd)
    } else {
        STATS_HWND.store(0, Ordering::Relaxed);
        None
    }
}

/// Open the stats window, or bring it forward if it is already up.
fn open_or_focus_stats_window() {
    if let Some(hwnd) = stats_hwnd() {
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = SetForegroundWindow(hwnd);
        }
        return;
    }

    unsafe {
        let hinstance = GetModuleHandleW(None).unwrap_or_default();
        let class_name = w!("NovaStatsWindow");
        // RegisterClassW failing with ERROR_CLASS_ALREADY_EXISTS is expected on
        // every reopen — the class outlives the window. CreateWindowExW below
        // is the real test, so the return value is deliberately not fatal here.
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(stats_wndproc),
            hInstance: hinstance.into(),
            lpszClassName: class_name,
            ..Default::default()
        };
        RegisterClassW(&wc);

        let dpi = system_dpi();
        let (w, h) = (scale(STATS_W, dpi), scale(STATS_H, dpi));
        let style = WS_POPUP | WS_CAPTION | WS_SYSMENU;

        // Size the CLIENT area to (w, h) — the caption is extra on top, so
        // without this the rows would be squeezed by its height.
        let mut rect = RECT { left: 0, top: 0, right: w, bottom: h };
        let _ = AdjustWindowRect(&mut rect, style, false);
        let (outer_w, outer_h) = (rect.right - rect.left, rect.bottom - rect.top);

        // Bottom-right of the WORK area (not the screen) so it never opens
        // underneath the taskbar.
        let mut work = RECT::default();
        let (x, y) = if SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut work as *mut RECT as *mut core::ffi::c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
        .is_ok()
        {
            let m = scale(16, dpi);
            (work.right - outer_w - m, work.bottom - outer_h - m)
        } else {
            (scale(80, dpi), scale(80, dpi))
        };

        match CreateWindowExW(
            // TOOLWINDOW keeps a diagnostics popup out of the taskbar and
            // Alt-Tab. Deliberately NOT topmost: this must not float over a
            // fullscreen game the user is streaming.
            WS_EX_TOOLWINDOW,
            class_name,
            w!("Nova — Server Stats"),
            style,
            x,
            y,
            outer_w,
            outer_h,
            None,
            None,
            hinstance,
            None,
        ) {
            Ok(hwnd) => {
                STATS_HWND.store(hwnd.0 as isize, Ordering::Relaxed);
                let _ = ShowWindow(hwnd, SW_SHOW);
                let _ = SetForegroundWindow(hwnd);
            }
            Err(e) => println!("⚠️  Tray: could not open the Server Stats window ({e})"),
        }
    }
}

/// Ask the stats window to repaint from the current snapshot (no-op when closed).
fn refresh_stats_window() {
    if let Some(hwnd) = stats_hwnd() {
        // erase=false: WM_ERASEBKGND is swallowed and the paint is fully
        // double-buffered, so an erase pass would only add a flash.
        unsafe {
            let _ = InvalidateRect(hwnd, None, false);
        }
    }
}

fn close_stats_window() {
    if let Some(hwnd) = stats_hwnd() {
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
        STATS_HWND.store(0, Ordering::Relaxed);
    }
}

unsafe extern "system" fn stats_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        // Swallowed: paint_stats fills every pixel from a memory DC, so letting
        // the system erase first would just flash the background colour twice a
        // second.
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            paint_stats(hwnd, hdc);
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            // Clear the handle from inside the destroy itself — the user can
            // close this window from its own title bar, which never routes
            // through close_stats_window().
            STATS_HWND.store(0, Ordering::Relaxed);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Fonts are rebuilt only when the DPI changes; creating them per paint would
/// churn GDI handles twice a second for the life of the window. Thread-local
/// because every paint happens on the tray thread and `HFONT` is not `Send`.
struct Fonts {
    dpi: i32,
    title: HFONT,
    label: HFONT,
    value: HFONT,
}

thread_local! {
    static FONTS: std::cell::RefCell<Option<Fonts>> = const { std::cell::RefCell::new(None) };
}

fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF(r as u32 | ((g as u32) << 8) | ((b as u32) << 16))
}

fn system_dpi() -> i32 {
    unsafe {
        let hdc = GetDC(None);
        if hdc.is_invalid() {
            return 96;
        }
        let dpi = GetDeviceCaps(hdc, LOGPIXELSX);
        ReleaseDC(None, hdc);
        if dpi <= 0 {
            96
        } else {
            dpi
        }
    }
}

fn scale(v: i32, dpi: i32) -> i32 {
    v * dpi / 96
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

fn wide_z(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe fn make_font(px: i32, bold: bool) -> HFONT {
    CreateFontW(
        px,
        0,
        0,
        0,
        if bold { 700 } else { 400 },
        0,
        0,
        0,
        1,  // DEFAULT_CHARSET
        0,  // OUT_DEFAULT_PRECIS
        0,  // CLIP_DEFAULT_PRECIS
        5,  // CLEARTYPE_QUALITY
        0,  // DEFAULT_PITCH | FF_DONTCARE
        w!("Segoe UI"),
    )
}

unsafe fn draw_text(hdc: HDC, x: i32, y: i32, text: &str, color: COLORREF, font: HFONT) {
    let old = SelectObject(hdc, font);
    SetTextColor(hdc, color);
    let s = wide(text);
    let _ = TextOutW(hdc, x, y, &s);
    SelectObject(hdc, old);
}

/// Right-aligned draw — values line up on the window's right margin regardless
/// of how wide "HEVC Main10 HDR" happens to render at the current DPI.
unsafe fn draw_text_right(
    hdc: HDC,
    right: i32,
    y: i32,
    text: &str,
    color: COLORREF,
    font: HFONT,
) {
    let old = SelectObject(hdc, font);
    SetTextColor(hdc, color);
    let s = wide(text);
    let mut size = SIZE::default();
    let _ = GetTextExtentPoint32W(hdc, &s, &mut size);
    let _ = TextOutW(hdc, right - size.cx, y, &s);
    SelectObject(hdc, old);
}

unsafe fn fill(hdc: HDC, rect: RECT, color: COLORREF) {
    let brush = CreateSolidBrush(color);
    FillRect(hdc, &rect, brush);
    let _ = DeleteObject(brush);
}

/// Paint the whole window into a memory DC and blit it in one go — at 2 Hz an
/// unbuffered paint is a visible flicker.
unsafe fn paint_stats(hwnd: HWND, hdc: HDC) {
    let mut client = RECT::default();
    if windows::Win32::UI::WindowsAndMessaging::GetClientRect(hwnd, &mut client).is_err() {
        return;
    }
    let (cw, ch) = (client.right - client.left, client.bottom - client.top);
    if cw <= 0 || ch <= 0 {
        return;
    }

    let mem_dc = CreateCompatibleDC(hdc);
    if mem_dc.is_invalid() {
        return;
    }
    let mem_bmp: HBITMAP = CreateCompatibleBitmap(hdc, cw, ch);
    let old_bmp = SelectObject(mem_dc, mem_bmp);
    SetBkMode(mem_dc, TRANSPARENT);

    let dpi = system_dpi();
    FONTS.with(|f| {
        let mut f = f.borrow_mut();
        let stale = f.as_ref().map(|x| x.dpi != dpi).unwrap_or(true);
        if stale {
            if let Some(old) = f.take() {
                let _ = DeleteObject(old.title);
                let _ = DeleteObject(old.label);
                let _ = DeleteObject(old.value);
            }
            *f = Some(Fonts {
                dpi,
                title: make_font(scale(19, dpi), true),
                label: make_font(scale(15, dpi), false),
                value: make_font(scale(15, dpi), true),
            });
        }
        let fonts = f.as_ref().unwrap();

        // Palette: fixed dark, not system-themed. A telemetry readout that
        // changes colour with the user's light/dark setting buys nothing, and
        // the window is often being viewed THROUGH the stream, where the host's
        // theme is irrelevant.
        let c_bg = rgb(0x1B, 0x1B, 0x1F);
        let c_title = rgb(0xFF, 0xFF, 0xFF);
        let c_label = rgb(0x9A, 0x9A, 0xA5);
        let c_value = rgb(0xF0, 0xF0, 0xF5);
        let c_rule = rgb(0x30, 0x30, 0x38);
        let c_live = rgb(0x2E, 0xCC, 0x71);
        let c_idle = rgb(0x6E, 0x6E, 0x78);

        fill(mem_dc, RECT { left: 0, top: 0, right: cw, bottom: ch }, c_bg);

        let snap = crate::stats::snapshot();
        let pad = scale(18, dpi);
        let mut y = scale(14, dpi);

        draw_text(mem_dc, pad, y, "Nova — Live Session", c_title, fonts.title);
        y += scale(30, dpi);
        fill(
            mem_dc,
            RECT { left: pad, top: y, right: cw - pad, bottom: y + 1 },
            c_rule,
        );
        y += scale(12, dpi);

        let rows: [(&str, String); 6] = [
            ("Resolution", snap.resolution_text()),
            ("Frame rate", snap.fps_text()),
            ("Codec", snap.codec.to_string()),
            // "Encode", not "Bitrate": this is NVENC's output, before RTP and
            // FEC overhead — see the stats module docs.
            ("Encode", crate::stats::Snapshot::rate_text(snap.measured_kbps)),
            ("Target (QoS)", crate::stats::Snapshot::rate_text(snap.target_kbps)),
            ("Ceiling", crate::stats::Snapshot::rate_text(snap.ceiling_kbps)),
        ];
        let row_h = scale(26, dpi);
        for (label, value) in rows.iter() {
            draw_text(mem_dc, pad, y, label, c_label, fonts.label);
            draw_text_right(mem_dc, cw - pad, y, value, c_value, fonts.value);
            y += row_h;
        }

        y += scale(6, dpi);
        fill(
            mem_dc,
            RECT { left: pad, top: y, right: cw - pad, bottom: y + 1 },
            c_rule,
        );
        y += scale(12, dpi);

        // Status dot drawn as a glyph rather than with a GDI pen+brush pair —
        // one TextOutW instead of four handle allocations per paint.
        let (dot, status, colour) = if snap.streaming {
            ("●", "Streaming", c_live)
        } else {
            ("○", "Idle — waiting for a client", c_idle)
        };
        draw_text(mem_dc, pad, y, dot, colour, fonts.label);
        draw_text(mem_dc, pad + scale(18, dpi), y, status, colour, fonts.label);
    });

    let _ = BitBlt(hdc, 0, 0, cw, ch, mem_dc, 0, 0, SRCCOPY);
    SelectObject(mem_dc, old_bmp);
    let _ = DeleteObject(mem_bmp);
    let _ = DeleteDC(mem_dc);
}

// ── Tray icon states ───────────────────────────────────────────────────────

/// Build the idle and streaming tray icons.
///
/// Idle is the app icon straight out of the resource section (ID 1, compiled in
/// by build.rs). Streaming is that same artwork with a green badge composited
/// into its lower-right corner — keeping Nova's icon recognisable instead of
/// swapping in an unrelated shape, and needing no second `.ico` on disk.
///
/// Returns `(idle, Some(active))`, or `(idle, None)` if the badge composite
/// failed — the caller then simply never changes the icon, which is a cosmetic
/// loss rather than a broken tray.
fn build_icons() -> (Icon, Option<Icon>) {
    const SIZE: u32 = 32;

    let idle = Icon::from_resource(1, Some((SIZE, SIZE))).unwrap_or_else(|_| {
        // RGBA: solid #0078D4 (Windows accent blue), fully opaque
        let px = [0u8, 120, 212, 255];
        Icon::from_rgba(px.repeat((SIZE * SIZE) as usize), SIZE, SIZE)
            .expect("fallback tray icon")
    });

    let active = match unsafe { resource_icon_rgba(1, SIZE as i32) } {
        Some(mut rgba) => {
            stamp_active_badge(&mut rgba, SIZE as i32);
            match Icon::from_rgba(rgba, SIZE, SIZE) {
                Ok(icon) => Some(icon),
                Err(e) => {
                    println!("ℹ️  Tray: streaming icon badge unavailable ({e}) — icon stays static");
                    None
                }
            }
        }
        None => {
            println!("ℹ️  Tray: could not read the app icon bitmap — streaming icon stays static");
            None
        }
    };

    (idle, active)
}

/// Read resource icon `id` as `size`×`size` straight RGBA.
///
/// Goes through `GetIconInfo` + `GetDIBits` rather than `DrawIconEx` into a DIB:
/// DrawIconEx composites against the destination and does not reliably write an
/// alpha channel, which would give the badge a black square to sit on.
unsafe fn resource_icon_rgba(id: u16, size: i32) -> Option<Vec<u8>> {
    let hinstance = GetModuleHandleW(None).ok()?;
    let handle = LoadImageW(
        hinstance,
        PCWSTR(id as usize as *const u16), // MAKEINTRESOURCE
        IMAGE_ICON,
        size,
        size,
        LR_DEFAULTCOLOR,
    )
    .ok()?;
    let hicon = windows::Win32::UI::WindowsAndMessaging::HICON(handle.0);

    let mut info = ICONINFO::default();
    let got_info = GetIconInfo(hicon, &mut info).is_ok();
    let result = (|| {
        if !got_info || info.hbmColor.is_invalid() {
            return None;
        }
        let hdc = GetDC(None);
        if hdc.is_invalid() {
            return None;
        }

        let mut bmi = BITMAPINFO::default();
        bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bmi.bmiHeader.biWidth = size;
        // Negative height = top-down rows, so the buffer is in the same order
        // Icon::from_rgba expects and no vertical flip is needed.
        bmi.bmiHeader.biHeight = -size;
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        bmi.bmiHeader.biCompression = BI_RGB.0;

        let mut buf = vec![0u8; (size * size * 4) as usize];
        let lines = GetDIBits(
            hdc,
            info.hbmColor,
            0,
            size as u32,
            Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
            &mut bmi,
            DIB_RGB_COLORS,
        );
        ReleaseDC(None, hdc);
        if lines == 0 {
            return None;
        }

        // GDI hands back BGRA. A pre-XP style icon carries no alpha at all
        // (every byte zero), which would render as fully transparent — treat
        // that as opaque instead of shipping an invisible icon.
        let opaque = buf.iter().skip(3).step_by(4).all(|&a| a == 0);
        for px in buf.chunks_exact_mut(4) {
            px.swap(0, 2); // BGRA -> RGBA
            if opaque {
                px[3] = 255;
            }
        }
        Some(buf)
    })();

    if !info.hbmColor.is_invalid() {
        let _ = DeleteObject(info.hbmColor);
    }
    if !info.hbmMask.is_invalid() {
        let _ = DeleteObject(info.hbmMask);
    }
    let _ = DestroyIcon(hicon);
    result
}

/// Composite the "streaming" badge: a green disc in the lower-right corner with
/// a dark ring so it stays legible against light and dark taskbars alike.
///
/// Coverage-based edge blending (no GDI+, no extra dependency) — at 32 px a
/// hard-edged circle looks obviously aliased next to the shell's own icons.
fn stamp_active_badge(rgba: &mut [u8], size: i32) {
    let r = (size as f32) * 0.28; // outer (ring) radius
    let cx = size as f32 - r - 1.0;
    let cy = size as f32 - r - 1.0;
    let ring = (0x10u8, 0x14u8, 0x18u8); // near-black outline
    let fill = (0x2Eu8, 0xCCu8, 0x71u8); // #2ECC71

    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 + 0.5 - cx;
            let dy = y as f32 + 0.5 - cy;
            let d = (dx * dx + dy * dy).sqrt();

            // Outer coverage fades the ring's edge; inner coverage fades the
            // green disc into the ring. Both clamp to [0,1].
            let outer = (r - d + 0.5).clamp(0.0, 1.0);
            if outer <= 0.0 {
                continue;
            }
            let inner = (r - 1.6 - d + 0.5).clamp(0.0, 1.0);

            let (br, bg, bb) = (
                ring.0 as f32 * (1.0 - inner) + fill.0 as f32 * inner,
                ring.1 as f32 * (1.0 - inner) + fill.1 as f32 * inner,
                ring.2 as f32 * (1.0 - inner) + fill.2 as f32 * inner,
            );

            let i = ((y * size + x) * 4) as usize;
            let dst = &mut rgba[i..i + 4];
            let a = outer;
            dst[0] = (dst[0] as f32 * (1.0 - a) + br * a) as u8;
            dst[1] = (dst[1] as f32 * (1.0 - a) + bg * a) as u8;
            dst[2] = (dst[2] as f32 * (1.0 - a) + bb * a) as u8;
            // The badge is opaque where it covers, so a transparent corner of
            // the source icon still shows a solid dot.
            dst[3] = dst[3].max((255.0 * a) as u8);
        }
    }
}

// ── Dialogs ────────────────────────────────────────────────────────────────

/// Modal Yes/No confirmation on the tray thread.
///
/// `HWND(null)` as the owner deliberately: the tray has no window the user
/// thinks of as "the app", and parenting to tray-icon's hidden window would let
/// a stuck dialog take the tray's message pump down with it.
fn confirm(
    text: &str,
    caption: &str,
    icon: windows::Win32::UI::WindowsAndMessaging::MESSAGEBOX_STYLE,
) -> bool {
    let text = wide_z(text);
    let caption = wide_z(caption);
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(text.as_ptr()),
            PCWSTR(caption.as_ptr()),
            MB_YESNO | icon,
        ) == IDYES
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn badge_marks_the_lower_right_and_leaves_the_upper_left_alone() {
        const S: i32 = 32;
        // Fully transparent source: whatever the badge writes is unambiguous.
        let mut rgba = vec![0u8; (S * S * 4) as usize];
        stamp_active_badge(&mut rgba, S);

        let px = |x: i32, y: i32| {
            let i = ((y * S + x) * 4) as usize;
            (rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3])
        };

        // Badge centre: opaque and green-dominant.
        let (r, g, b, a) = px(S - 9, S - 9);
        assert_eq!(a, 255, "badge centre must be opaque");
        assert!(g > r && g > b, "badge centre should be green, got {r},{g},{b}");

        // Opposite corner: untouched, so the app artwork still shows through.
        assert_eq!(px(0, 0), (0, 0, 0, 0));
        assert_eq!(px(2, S - 2), (0, 0, 0, 0));
    }

    #[test]
    fn badge_edge_is_antialiased_rather_than_hard_cut() {
        const S: i32 = 32;
        let mut rgba = vec![0u8; (S * S * 4) as usize];
        stamp_active_badge(&mut rgba, S);
        // Some pixel must land at partial coverage; an all-or-nothing alpha
        // channel would mean the coverage blend regressed to a hard circle.
        let partial = rgba
            .iter()
            .skip(3)
            .step_by(4)
            .any(|&a| a > 0 && a < 255);
        assert!(partial, "expected antialiased badge edge pixels");
    }

    #[test]
    fn wide_z_is_nul_terminated_for_pcwstr() {
        let v = wide_z("Nova");
        assert_eq!(v.last(), Some(&0));
        assert_eq!(v.len(), 5);
    }
}

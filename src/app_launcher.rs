//! Native app launching and box art for the Moonlight `/applist`, `/appasset`,
//! and `/launch` endpoints.
//!
//! Box art is premium, pre-made JPEG artwork baked into the binary via
//! `include_bytes!` — keeps the executable a single portable file while
//! looking right in Moonlight's 3:4 vertical tile UI (dynamically-extracted
//! Win32 icons were too small/square for that layout).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    MessageBoxW, PostMessageW, HWND_BROADCAST, IDYES, MB_ICONQUESTION, MB_SETFOREGROUND,
    MB_TOPMOST, MB_YESNO, SC_MONITORPOWER, WM_SYSCOMMAND,
};

pub const APP_ID_DESKTOP: u32 = 1;
pub const APP_ID_STEAM: u32 = 2;
pub const APP_ID_XBOX: u32 = 3;
pub const APP_ID_RETROARCH: u32 = 4;
pub const APP_ID_VIRTUAL_DESKTOP: u32 = 5;

const BOX_ART_DESKTOP: &[u8] = include_bytes!("../assets/desktop.jpg");
const BOX_ART_STEAM: &[u8] = include_bytes!("../assets/steam.jpg");
const BOX_ART_XBOX: &[u8] = include_bytes!("../assets/xbox.jpg");
const BOX_ART_RETROARCH: &[u8] = include_bytes!("../assets/retroarch.jpg");
const BOX_ART_VIRTUAL_DESKTOP: &[u8] = include_bytes!("../assets/virtual_desktop.jpg");

/// JPEG box art for `app_id`, for the `/appasset` response.
pub fn get_box_art(app_id: u32) -> &'static [u8] {
    match app_id {
        APP_ID_STEAM => BOX_ART_STEAM,
        APP_ID_XBOX => BOX_ART_XBOX,
        APP_ID_RETROARCH => BOX_ART_RETROARCH,
        APP_ID_VIRTUAL_DESKTOP => BOX_ART_VIRTUAL_DESKTOP,
        _ => BOX_ART_DESKTOP,
    }
}

/// Launch (or apply) the action backing `app_id`, as selected from the
/// Moonlight app list. Desktop, Virtual Desktop, and any unrecognized id are
/// a no-op here — the client just lands on the live desktop (already what's
/// streamed), and for Virtual Desktop the actual display-topology switch +
/// [`sleep_displays`] happen later, from `lib.rs`'s connect handler, only
/// once `VirtualDisplay::activate_for_stream` has succeeded and capture has
/// moved onto the virtual output. Calling `sleep_displays()` here — before
/// RTSP PLAY, while DXGI is still duplicating the physical display — powered
/// the physical panel down out from under the live capture (the "screen
/// blinks, no video" symptom).
pub fn launch_app(app_id: u32) {
    match app_id {
        APP_ID_STEAM => launch_steam_big_picture(),
        APP_ID_XBOX => launch_xbox_app(),
        APP_ID_RETROARCH => launch_retroarch(),
        _ => {}
    }
}

fn launch_steam_big_picture() {
    println!("🚀 Launching Steam in Big Picture mode");
    if let Err(e) = Command::new("cmd")
        .args(["/C", "start", "", "steam://open/bigpicture"])
        .spawn()
    {
        println!("⚠️  Failed to launch Steam: {}", e);
    }
}

/// Launch the Windows Xbox app (Microsoft.GamingApp / legacy Microsoft.XboxApp)
/// and then toggle it into the immersive "Fullscreen experience" shell.
///
/// As of mid-2026 there is no documented stable URI/command-line to jump
/// straight into that shell — it's an OS feature (Settings > Gaming >
/// Fullscreen experience) normally toggled with Win+F11 once enabled, with
/// no per-app launch API. So we resolve the app's AUMID dynamically (the
/// package family name has changed across Windows versions), launch it
/// normally via `explorer.exe shell:appsFolder\<AUMID>` (the same mechanism
/// the Start menu uses), then — once it's had a moment to come to the
/// foreground — replay Win+F11 via `input::send_win_f11` to flip it into
/// fullscreen/immersive mode.
fn launch_xbox_app() {
    println!("🚀 Launching Xbox app (immersive)");
    match resolve_xbox_aumid() {
        Some(aumid) => {
            let target = format!("shell:appsFolder\\{}", aumid);
            if let Err(e) = Command::new("explorer.exe").arg(target).spawn() {
                println!("⚠️  Failed to launch Xbox app: {}", e);
                return;
            }
            // Give the app a moment to launch and gain focus before sending
            // the Win+F11 fullscreen-toggle — run off-thread so /launch
            // returns to Moonlight immediately.
            std::thread::spawn(|| {
                std::thread::sleep(Duration::from_millis(2500));
                println!("🖥️  Sending Win+F11 to toggle Xbox app fullscreen mode");
                crate::input::send_win_f11();
            });
        }
        None => {
            println!("⚠️  Could not resolve Xbox app AUMID (Get-StartApps found no match) — is the Xbox app installed?");
        }
    }
}

fn resolve_xbox_aumid() -> Option<String> {
    let output = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "(Get-StartApps | Where-Object { $_.AppID -like '*GamingApp*' -or $_.AppID -like '*XboxApp*' } | Select-Object -First 1 -ExpandProperty AppID)",
        ])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

// ---------------------------------------------------------------------
// RetroArch — auto-download a portable build on first launch, then run it
// in fullscreen.
// ---------------------------------------------------------------------

/// Official RetroArch Windows x64 portable build (`.7z`).
///
/// libretro's buildbot serves stable builds per-version (no "latest" alias),
/// confirmed present at this exact path via the directory index at
/// https://buildbot.libretro.com/stable/ as of 2026. Bump the version
/// segment here when a newer stable release replaces 1.22.2.
const RETROARCH_DOWNLOAD_URL: &str =
    "https://buildbot.libretro.com/stable/1.22.2/windows/x86_64/RetroArch.7z";

/// `<exe_dir>/RetroArch` — where RetroArch is expected/installed.
fn retroarch_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("RetroArch")))
        .unwrap_or_else(|| PathBuf::from("RetroArch"))
}

/// Launch RetroArch in fullscreen, downloading the portable bundle first if
/// `retroarch.exe` isn't present yet. Runs off-thread since the download can
/// take a while and `/launch` must return to Moonlight immediately.
fn launch_retroarch() {
    std::thread::spawn(|| {
        let dir = retroarch_dir();

        let mut exe = find_retroarch_exe(&dir);

        if exe.is_none() {
            if !confirm_retroarch_install() {
                println!("🚫 RetroArch install declined — launch aborted");
                return;
            }
            println!("📦 RetroArch not found — downloading portable bundle to {}", dir.display());
            if !download_and_extract_retroarch(&dir) {
                println!("⚠️  RetroArch download/extract failed — launch aborted");
                return;
            }
            exe = find_retroarch_exe(&dir);
        }

        let Some(exe) = exe else {
            println!("⚠️  RetroArch still not found under {} after extraction — check the archive layout", dir.display());
            return;
        };

        println!("🚀 Launching RetroArch (fullscreen): {}", exe.display());
        let cwd = exe.parent().unwrap_or(&dir);
        if let Err(e) = Command::new(&exe).arg("-f").current_dir(cwd).spawn() {
            println!("⚠️  Failed to launch RetroArch: {}", e);
        }
    });
}

/// Shows a Yes/No prompt on the host's screen asking whether to download and
/// install RetroArch. Blocks (this runs off the main/HTTP thread) until the
/// person at the host responds — Nova won't silently pull ~200MB and run it.
fn confirm_retroarch_install() -> bool {
    let result = unsafe {
        MessageBoxW(
            HWND(std::ptr::null_mut()),
            w!("RetroArch wasn't found on this PC.\n\nDownload and install the official portable build (~200 MB) now?"),
            w!("Nova — RetroArch"),
            MB_YESNO | MB_ICONQUESTION | MB_TOPMOST | MB_SETFOREGROUND,
        )
    };
    result == IDYES
}

/// Searches `dir` (up to 3 levels deep) for `retroarch.exe` — the bundle's
/// internal folder layout isn't fixed, so we don't assume `dir/retroarch.exe`
/// directly.
fn find_retroarch_exe(dir: &Path) -> Option<PathBuf> {
    fn search(dir: &Path, depth: u32) -> Option<PathBuf> {
        if depth > 3 {
            return None;
        }
        let mut subdirs = Vec::new();
        for entry in std::fs::read_dir(dir).ok()?.flatten() {
            let path = entry.path();
            if path.is_dir() {
                subdirs.push(path);
            } else if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.eq_ignore_ascii_case("retroarch.exe"))
            {
                return Some(path);
            }
        }
        subdirs.into_iter().find_map(|sub| search(&sub, depth + 1))
    }
    search(dir, 0)
}

/// Downloads `RETROARCH_DOWNLOAD_URL` (a `.7z` archive) and extracts it into
/// `dir`. Windows has no built-in `.7z` support, so this tries, in order:
///
///  1. `tar` — bsdtar, built into Windows 10 1803+ (libarchive has 7z/LZMA2
///     read support), so this works with zero extra installs on a modern host.
///  2. `7z`/`7za` — picked up from PATH or the usual 7-Zip install locations,
///     if the user already has 7-Zip installed.
///
/// Nova won't fetch a third-party unzip tool from an unverified URL just to
/// bootstrap this — if neither is available, it logs instructions instead.
fn download_and_extract_retroarch(dir: &Path) -> bool {
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let archive_path = dir.join("RetroArch.7z");

    let download_ps = format!(
        "$ProgressPreference='SilentlyContinue'; Invoke-WebRequest -Uri '{url}' -OutFile '{archive}'",
        url = RETROARCH_DOWNLOAD_URL,
        archive = archive_path.display(),
    );
    match Command::new("powershell").args(["-NoProfile", "-Command", &download_ps]).status() {
        Ok(status) if status.success() => {}
        Ok(status) => {
            println!("⚠️  RetroArch download failed (powershell exit {:?})", status.code());
            return false;
        }
        Err(e) => {
            println!("⚠️  Failed to spawn powershell for RetroArch download: {}", e);
            return false;
        }
    }

    let extracted = extract_7z(&archive_path, dir);
    let _ = std::fs::remove_file(&archive_path);
    extracted
}

fn extract_7z(archive: &Path, dir: &Path) -> bool {
    // bsdtar (built into Windows 10 1803+) can read 7z/LZMA2 archives.
    if let Ok(status) = Command::new("tar")
        .args(["-xf", &archive.to_string_lossy(), "-C", &dir.to_string_lossy()])
        .status()
    {
        if status.success() {
            return true;
        }
    }

    // Fall back to an existing 7-Zip install, if any.
    for candidate in [
        "7z",
        r"C:\Program Files\7-Zip\7z.exe",
        r"C:\Program Files (x86)\7-Zip\7z.exe",
    ] {
        if let Ok(status) = Command::new(candidate)
            .args(["x", "-y", &format!("-o{}", dir.display()), &archive.to_string_lossy()])
            .status()
        {
            if status.success() {
                return true;
            }
        }
    }

    println!(
        "⚠️  Could not extract {} — install 7-Zip (https://www.7-zip.org/) or extract \
         it into {} manually",
        archive.display(),
        dir.display()
    );
    false
}

// ---------------------------------------------------------------------
// "Virtual Desktop" host blackout — the GPU/NVENC pipeline keeps capturing
// and streaming as normal, but the physical monitors are told to power off
// so the host doesn't light up a room while someone streams.
// ---------------------------------------------------------------------

static MONITORS_ASLEEP: AtomicBool = AtomicBool::new(false);

/// Put the physical displays to sleep via an `SC_MONITORPOWER` broadcast.
/// Called once the virtual display has become the desktop primary
/// ([`crate::virtual_display::VirtualDisplay::activate_for_stream`]) so the
/// stream continues uninterrupted while the physical panel goes dark.
/// [`wake_displays`] reverses this on stream teardown.
pub fn sleep_displays() {
    println!("🌙 Virtual Desktop: putting physical displays to sleep (stream continues)");
    unsafe {
        let _ = PostMessageW(
            HWND_BROADCAST,
            WM_SYSCOMMAND,
            WPARAM(SC_MONITORPOWER as usize),
            LPARAM(2), // 2 = off / low power
        );
    }
    MONITORS_ASLEEP.store(true, Ordering::SeqCst);
}

/// Wake the physical displays if [`sleep_displays`] put them to sleep.
/// Safe to call unconditionally on every stream teardown — a no-op if the
/// displays were never put to sleep.
pub fn wake_displays() {
    if MONITORS_ASLEEP.swap(false, Ordering::SeqCst) {
        println!("☀️  Virtual Desktop: waking physical displays");
        unsafe {
            let _ = PostMessageW(
                HWND_BROADCAST,
                WM_SYSCOMMAND,
                WPARAM(SC_MONITORPOWER as usize),
                LPARAM(-1), // -1 = on
            );
        }
    }
}

//! Native app launching and box art for the Moonlight `/applist`, `/appasset`,
//! and `/launch` endpoints.
//!
//! Box art is premium, pre-made JPEG artwork baked into the binary via
//! `include_bytes!` — keeps the executable a single portable file while
//! looking right in Moonlight's 3:4 vertical tile UI (dynamically-extracted
//! Win32 icons were too small/square for that layout).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use windows::core::w;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    MessageBoxW, IDYES, MB_ICONQUESTION, MB_SETFOREGROUND, MB_TOPMOST, MB_YESNO,
};

pub const APP_ID_DESKTOP: u32 = 1;
pub const APP_ID_STEAM: u32 = 2;
pub const APP_ID_XBOX: u32 = 3;
pub const APP_ID_RETROARCH: u32 = 4;
pub const APP_ID_VIRTUAL_DESKTOP: u32 = 5;

/// Universal VDD policy: every app routes through the Virtual Display Driver.
///
/// The VDD is the sole capture source regardless of which app the client
/// launches. Returning `true` here causes `lib.rs`'s connect handler to call
/// `activate_for_stream` (snapping the VDD to the client-negotiated resolution)
/// and `rebind_capture_and_encoder` (pointing DXGI + NVENC at the VDD output)
/// for every session — so SPS, capture rect, and NVENC surface all agree on
/// exactly the resolution the client requested.
pub fn uses_virtual_display(_app_id: u32) -> bool {
    true
}

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
/// streamed). For Virtual Desktop, the actual display-topology switch
/// happens later, from `lib.rs`'s connect handler, via
/// `VirtualDisplay::activate_for_stream` (gated on [`uses_virtual_display`])
/// once capture has moved onto the virtual output.
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

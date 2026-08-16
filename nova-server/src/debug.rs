/// Nova file logger.
///
/// Strategy:
///   Rust side  — `SetStdHandle(STD_OUTPUT/ERROR_HANDLE, log_file)` redirects
///                ALL subsequent `println!` / `eprintln!` in the entire process
///                to the log file.  Zero changes to existing call sites needed.
///
///   C shim side — The CRT's FILE* descriptors are independent of the Win32
///                 handle table, so `printf()` does NOT follow `SetStdHandle`.
///                 `InitShimLog` passes the log path to `shim.cpp` which opens
///                 the file itself, `_dup2`s the CRT stdout/stderr, and falls
///                 back to `WriteFile` for all `ShimLog()` calls.
///
/// Log location: `{exe_dir}\nova.log`  (same directory as the executable).
/// In a Windows Service the SCM sets CWD = System32, so a relative path would
/// silently write (or fail) there.  Anchoring to the exe directory keeps the
/// log next to the binary regardless of how the process was started.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_ALWAYS,
};
use windows::Win32::System::Console::{
    SetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
};

// ── Log path ─────────────────────────────────────────────────────────────────

fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn log_path() -> PathBuf {
    exe_dir().join("nova.log")
}

/// The launcher **service** logs to its own file. Critical: the service holds
/// its log handle open for its whole life, and the host it spawns opens ITS
/// log at startup. If both targeted `nova.log`, the host's open would collide
/// with the service's (even with FILE_SHARE_WRITE the two would interleave
/// unreadably). Separate files keep each process's log clean and, more
/// importantly, guarantee the host can always open its own.
pub fn service_log_path() -> PathBuf {
    exe_dir().join("nova-service.log")
}

/// The SYSTEM input helper (`--system-input-helper`) logs to its own file, for
/// the same reason the service does: the Worker holds `nova.log` open for its
/// whole life, and a helper spawns/dies inside that window. A third file keeps
/// the secure-desktop injection trail readable on its own.
pub fn input_helper_log_path() -> PathBuf {
    exe_dir().join("nova-input.log")
}

/// Log path encoded as a null-terminated UTF-16 string for the C shim.
pub fn log_path_wide() -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    log_path()
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0u16))
        .collect()
}

// ── Rotation ─────────────────────────────────────────────────────────────────

/// Rotate once a log passes this size. Nova logs roughly 2 lines/second while
/// streaming (`📊 RTP/s`), so an always-on host produced ~13 MB/day and grew
/// without bound — the log was 16 MB before this existed.
const MAX_LOG_BYTES: u64 = 16 * 1024 * 1024;

/// Rotated generations kept beside the live log (`nova.log.1`, `nova.log.2`).
/// Two is enough to still hold the *previous* session when a crash is found
/// after the fact, and bounds each log family at 48 MB.
const KEPT_GENERATIONS: usize = 2;

/// How often the watchdog re-checks the size. A process can stream for days,
/// so rotating only at startup would not actually bound anything.
const ROTATE_CHECK: std::time::Duration = std::time::Duration::from_secs(60);

/// Called after a rotation so a subsystem holding its own handle to the log can
/// reopen it. The C++ shim keeps a CRT handle that Win32 `SetStdHandle` does
/// not reach, so without this it would keep writing into the rotated-away file.
static REOPEN_HOOK: OnceLock<fn()> = OnceLock::new();

/// Register a reopen callback (see [`REOPEN_HOOK`]). First caller wins; the
/// shim registers itself when it initialises its log.
pub fn set_log_reopen_hook(hook: fn()) {
    let _ = REOPEN_HOOK.set(hook);
}

fn generation_path(path: &Path, n: usize) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(".{n}"));
    PathBuf::from(name)
}

/// Shift `log.1 → log.2`, drop the oldest, and move the live file aside.
///
/// Every step is best-effort: a rotation that cannot happen (file locked by a
/// viewer, permissions) must never stop the process from logging, so failures
/// leave the current file in place and the next check simply tries again.
fn rotate(path: &Path) {
    let _ = std::fs::remove_file(generation_path(path, KEPT_GENERATIONS));
    for n in (1..KEPT_GENERATIONS).rev() {
        let _ = std::fs::rename(generation_path(path, n), generation_path(path, n + 1));
    }
    let _ = std::fs::rename(path, generation_path(path, 1));
}

fn oversized(path: &Path) -> bool {
    std::fs::metadata(path).map(|m| m.len() > MAX_LOG_BYTES).unwrap_or(false)
}

/// Open (or reopen) the log and point the process-wide stdout/stderr at it.
///
/// `FILE_SHARE_DELETE` is what makes live rotation possible at all: without it
/// the rename below fails for as long as the process holds the handle, which is
/// its entire life.
fn open_and_redirect(path: &Path) -> Option<HANDLE> {
    let handle = unsafe {
        CreateFileW(
            &windows::core::HSTRING::from(path.as_os_str()),
            0x0004u32, // FILE_APPEND_DATA — CreateFileW takes raw u32, not FILE_ACCESS_RIGHTS
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_ALWAYS,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    };
    match handle {
        Ok(h) => {
            unsafe {
                let _ = SetStdHandle(STD_OUTPUT_HANDLE, h);
                let _ = SetStdHandle(STD_ERROR_HANDLE, h);
            }
            Some(h)
        }
        Err(e) => {
            eprintln!("[Nova] WARNING: cannot open log file {}: {:?}", path.display(), e);
            None
        }
    }
}

/// Watch the live log and rotate it when it outgrows [`MAX_LOG_BYTES`].
///
/// Rotation is rename-then-reopen rather than truncate-in-place: truncating
/// would discard the whole history at the instant it crossed the threshold,
/// which is reliably the moment before you needed it.
fn spawn_rotation_watchdog(path: PathBuf, initial: HANDLE) {
    // `HANDLE` wraps a raw pointer and so is not `Send`. The value itself is
    // just a kernel handle-table index, valid process-wide, so it moves across
    // the thread boundary as an integer and is rebuilt on the far side.
    let initial = initial.0 as isize;
    std::thread::Builder::new()
        .name("nova-log-rotate".into())
        .spawn(move || {
            let mut current = HANDLE(initial as *mut core::ffi::c_void);
            loop {
                std::thread::sleep(ROTATE_CHECK);
                if !oversized(&path) {
                    continue;
                }
                rotate(&path);
                // Reopen even if the rename failed — the handle still points at
                // the old file either way, and a fresh open is harmless.
                if let Some(fresh) = open_and_redirect(&path) {
                    // Only now is the old handle unreferenced by stdout/stderr.
                    unsafe { let _ = CloseHandle(current); }
                    current = fresh;
                    println!("🗂️  Log rotated at {} (cap {} MB, keeping {} generation(s))",
                        timestamp(), MAX_LOG_BYTES / (1024 * 1024), KEPT_GENERATIONS);
                    if let Some(hook) = REOPEN_HOOK.get() {
                        hook();
                    }
                }
            }
        })
        .ok();
}

// ── Initialisation ────────────────────────────────────────────────────────────

/// Call ONCE, as the very first line of `run()` / `service_main()`, BEFORE
/// any `println!`.  Opens the log file and redirects the process-wide Win32
/// stdout + stderr handles so that all subsequent `println!` / `eprintln!`
/// anywhere in the Rust code — including on spawned threads — write to the
/// log file instead of the (absent) console.
pub fn init_debug_logger() {
    init_logger_to(log_path());
}

/// Logger init for the launcher **service** (`--service`). Uses a separate file
/// ([`service_log_path`]) so it never collides with the host's `nova.log`.
pub fn init_service_logger() {
    init_logger_to(service_log_path());
}

/// Logger init for the SYSTEM input helper (`--system-input-helper`) — see
/// [`input_helper_log_path`].
pub fn init_input_helper_logger() {
    init_logger_to(input_helper_log_path());
}

fn init_logger_to(path: PathBuf) {
    // Ensure parent directory exists (it should — exe is already there).
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }

    // Rotate before opening, so a restart never inherits an already-huge file
    // and start-up cost stays bounded.
    if oversized(&path) {
        rotate(&path);
    }

    // Open in append mode so multiple restarts accumulate in one file.
    // FILE_SHARE_READ lets an external viewer (`tail -f`) follow the log live;
    // WRITE means opening can never sharing-violation just because another Nova
    // process (service ↔ host, or a lingering instance) still has it open — the
    // open always succeeds so a process is never left silently logless.
    match open_and_redirect(&path) {
        Some(h) => {
            // From this point on, println! writes to the log file.
            println!();
            println!("══════════════════════════════════════════════════════════");
            println!("  Nova  started at {}", timestamp());
            println!("  Log   {}", path.display());
            println!("  PID   {}", std::process::id());
            println!("  Cap   {} MB, {} rotated generation(s) kept",
                MAX_LOG_BYTES / (1024 * 1024), KEPT_GENERATIONS);
            println!("══════════════════════════════════════════════════════════");
            spawn_rotation_watchdog(path, h);
        }
        None => {
            eprintln!("[Nova] Service output will not be captured.");
        }
    }
}

// ── DLL path probe ────────────────────────────────────────────────────────────

/// Log the absolute on-disk path of nova_shim.dll and whether it actually
/// exists where we expect it.  This catches "stale DLL in System32" or
/// "wrong search path" issues immediately on service startup.
pub fn log_shim_dll_path() {
    let exe_dir = exe_dir();
    let expected = exe_dir.join("nova_shim.dll");

    println!("[Nova] Exe directory   : {}", exe_dir.display());
    println!("[Nova] nova_shim.dll   : {}", expected.display());

    if expected.exists() {
        // Read the file metadata so we can log size and modification time —
        // helps confirm "stale old DLL vs freshly compiled one" at a glance.
        match std::fs::metadata(&expected) {
            Ok(m) => {
                let size_kb = m.len() / 1024;
                let modified = m.modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| format_epoch(d.as_secs()))
                    .unwrap_or_else(|| "unknown".to_string());
                println!("[Nova]   ✅  exists  size={}KB  modified={}", size_kb, modified);
            }
            Err(e) => println!("[Nova]   ⚠️  exists but metadata failed: {}", e),
        }
    } else {
        println!("[Nova]   ❌  NOT FOUND — stream will fail to start");
        println!("[Nova]   Deploy nova_shim.dll alongside nova-server.exe");

        // Check if a copy is lurking somewhere on the DLL search path (System32 etc.)
        for dir in dll_search_dirs() {
            let candidate = dir.join("nova_shim.dll");
            if candidate.exists() {
                println!("[Nova]   ⚠️  Found stale copy at {} — this may be loaded instead!",
                    candidate.display());
            }
        }
    }
}

/// Common locations Windows searches for DLLs (simplified; the real search
/// order also includes the manifest redirects and SxS, which we skip here).
fn dll_search_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(sys) = std::env::var_os("SystemRoot") {
        dirs.push(Path::new(&sys).join("System32"));
        dirs.push(Path::new(&sys).join("SysWOW64"));
    }
    if let Ok(path_var) = std::env::var("PATH") {
        for p in std::env::split_paths(&path_var) {
            dirs.push(p);
        }
    }
    dirs
}

// ── Legacy helpers ────────────────────────────────────────────────────────────

/// Writes a timestamped line to the log.  With `SetStdHandle` active, plain
/// `println!` already goes to the log file, so this function is just a
/// convenience wrapper for code that wants explicit timestamps.
pub fn debug_log(msg: &str) {
    println!("[{}] {}", timestamp(), msg);
}

// ── Timestamp ─────────────────────────────────────────────────────────────────

fn timestamp() -> String {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format_epoch(d.as_secs()) + &format!(".{:03}", d.subsec_millis())
}

fn format_epoch(secs: u64) -> String {
    let (y, mo, dd, hh, mm, ss) = epoch_to_parts(secs);
    format!("{y}-{mo:02}-{dd:02} {hh:02}:{mm:02}:{ss:02} UTC")
}

fn epoch_to_parts(secs: u64) -> (u32, u32, u32, u32, u32, u32) {
    let ss = (secs % 60) as u32;
    let mm = ((secs / 60) % 60) as u32;
    let hh = ((secs / 3600) % 24) as u32;
    let mut days = secs / 86400;

    let mut y = 1970u32;
    loop {
        let in_year = if is_leap(y) { 366 } else { 365 };
        if days < in_year { break; }
        days -= in_year;
        y += 1;
    }
    let month_lens = [31u64, if is_leap(y) { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut mo = 0u32;
    let mut rem = days as u64;
    for &ml in &month_lens {
        if rem < ml { break; }
        rem -= ml;
        mo += 1;
    }
    (y, mo + 1, rem as u32 + 1, hh, mm, ss)
}

fn is_leap(y: u32) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

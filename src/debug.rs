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
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ,
    OPEN_ALWAYS,
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

/// Log path encoded as a null-terminated UTF-16 string for the C shim.
pub fn log_path_wide() -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    log_path()
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0u16))
        .collect()
}

// ── Initialisation ────────────────────────────────────────────────────────────

/// Call ONCE, as the very first line of `run()` / `service_main()`, BEFORE
/// any `println!`.  Opens the log file and redirects the process-wide Win32
/// stdout + stderr handles so that all subsequent `println!` / `eprintln!`
/// anywhere in the Rust code — including on spawned threads — write to the
/// log file instead of the (absent) console.
pub fn init_debug_logger() {
    let path = log_path();

    // Ensure parent directory exists (it should — exe is already there).
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }

    // Open in append mode so multiple service restarts accumulate in one file.
    // FILE_SHARE_READ lets an external viewer (`tail -f`) read the log live.
    let handle: windows::core::Result<HANDLE> = unsafe {
        CreateFileW(
            &windows::core::HSTRING::from(path.as_os_str()),
            0x0004u32, // FILE_APPEND_DATA — CreateFileW takes raw u32, not FILE_ACCESS_RIGHTS
            FILE_SHARE_READ,
            None,
            OPEN_ALWAYS,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    };

    match handle {
        Ok(h) => {
            unsafe {
                // Redirect Win32 stdout and stderr to the log file.
                // Rust's println! calls WriteFile(GetStdHandle(STD_OUTPUT_HANDLE)),
                // so this redirect covers ALL println!/eprintln! in the process.
                let _ = SetStdHandle(STD_OUTPUT_HANDLE, h);
                let _ = SetStdHandle(STD_ERROR_HANDLE, h);
            }
            // From this point on, println! writes to the log file.
            println!();
            println!("══════════════════════════════════════════════════════════");
            println!("  Nova  started at {}", timestamp());
            println!("  Log   {}", path.display());
            println!("  PID   {}", std::process::id());
            println!("══════════════════════════════════════════════════════════");
        }
        Err(e) => {
            // Can't redirect — fall back to stderr (visible in cargo run, lost in service).
            eprintln!("[Nova] WARNING: cannot open log file {}: {:?}", path.display(), e);
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

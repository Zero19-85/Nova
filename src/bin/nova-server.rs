// Nova Server — elevated tray-resident application.
//
// Deployment model (Task Scheduler, NOT Windows Service):
//   --install    → registers a "On Logon / Highest Privileges" scheduled task,
//                  cleans up any old SCM service, and runs the Ghost Protocol
//                  (removes stale nova_shim.dll copies from System32/SysWOW64).
//   --uninstall  → removes the scheduled task and kills any running instance.
//   (no flag)    → normal tray run; called by the scheduled task on logon.
//
// Why not a Windows Service?
//   Services run in Session 0 (isolated from the interactive desktop).
//   DXGI Desktop Duplication, the D3D11 Video Processor, and Windows Graphics
//   Capture ALL require the interactive session (Session 1+).  In Session 0
//   they return E_ACCESSDENIED and the encoder writes uninitialized texture
//   data → "half-green / half-smeared" stream corruption.
//   A scheduled task launched On Logon runs in the user's own session with
//   the same elevated token the UAC manifest requests, solving both problems.

#![windows_subsystem = "windows"]

fn main() {
    // ── DPI awareness ─────────────────────────────────────────────────────────
    // The manifest already declares PerMonitorV2 awareness (nova-server.manifest,
    // compiled in by build.rs via rc.exe), which fires before this line.  This
    // runtime call is belt-and-suspenders: it ensures DPI awareness is active
    // even if an older copy of the manifest was embedded, or if the exe is
    // launched in a way that bypasses the manifest (e.g. some debuggers).
    //
    // Why this matters:
    //   On a 4K display at 200% scaling, without DPI awareness Windows lies to
    //   GetMonitorInfoW and DXGI, reporting 1920×1080 instead of 3840×2160.
    //   The WGC frame pool is created at the scaled-down "logical" size while
    //   NVENC is initialised at the true physical size — the frame fills only
    //   the top-left quarter of the encode buffer; the rest is uninitialised
    //   memory, producing exactly the "top-right smear / bottom-half green"
    //   artefact observed when running as an installed Task Scheduler entry.
    //
    // SetProcessDpiAwarenessContext returns FALSE / ERROR_ACCESS_DENIED when
    // awareness is already set (e.g. from the manifest) — that's fine, ignore.
    unsafe {
        use windows::Win32::UI::HiDpi::{
            SetProcessDpiAwarenessContext,
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        };
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    let args: Vec<String> = std::env::args().collect();

    match args.get(1).map(String::as_str) {
        Some("--install") => {
            // Init the file logger so install output lands in nova.log even
            // though there is no console (windows_subsystem = "windows").
            nova_server::debug::init_debug_logger();
            println!("=== Nova Install ===");
            match install_task() {
                Ok(()) => std::process::exit(0),
                Err(e) => {
                    println!("❌ Install failed: {}", e);
                    std::process::exit(1);
                }
            }
        }

        Some("--uninstall") => {
            nova_server::debug::init_debug_logger();
            println!("=== Nova Uninstall ===");
            match uninstall_task() {
                Ok(()) => std::process::exit(0),
                Err(e) => {
                    println!("❌ Uninstall failed: {}", e);
                    std::process::exit(1);
                }
            }
        }

        _ => {
            // Normal run — launched by the scheduled task on logon, or manually.
            // (Also handles any stray --run-service invocations from old installs
            // by simply running the server, which is harmless.)
            tokio::runtime::Runtime::new()
                .expect("tokio runtime")
                .block_on(nova_server::run())
                .expect("nova_server::run failed");
        }
    }
}

// ── Scheduled-Task install ────────────────────────────────────────────────────

const TASK_NAME: &str = "Nova Game Streaming";

fn install_task() -> Result<(), String> {
    let exe = std::env::current_exe()
        .map_err(|e| format!("Cannot resolve exe path: {e}"))?;
    let exe_str = exe.to_string_lossy().into_owned();

    // ── Ghost Protocol ────────────────────────────────────────────────────────
    // Old SCM-based installs sometimes copied nova_shim.dll into System32 or
    // SysWOW64.  Windows searches those directories before the exe directory,
    // so the stale copy gets loaded instead of the freshly-built one — causing
    // "half-green / half-smeared" frames.  Nuke it before registering the task.
    ghost_protocol_purge_dll();

    // ── Retire the old SCM service (if present) ───────────────────────────────
    // Silently ignore errors — the service simply may not exist.
    println!("🔧 Cleaning up any old NovaServer SCM service...");
    let _ = run_silent("sc",    &["stop",   "NovaServer"]);
    let _ = run_silent("sc",    &["delete", "NovaServer"]);

    // ── Kill any running Nova instance so the new task starts fresh ───────────
    println!("🔧 Stopping any running Nova instance...");
    let _ = run_silent("taskkill", &["/F", "/IM", "nova-server.exe"]);

    // ── Register the Scheduled Task ───────────────────────────────────────────
    //   /sc ONLOGON   — trigger: fires when the current user logs on
    //   /rl HIGHEST   — run level: requests the elevated token (skips per-session UAC)
    //   /f            — force overwrite if the task already exists
    //   No /ru        — runs as the interactive user who called --install
    println!("📋 Registering scheduled task '{}'...", TASK_NAME);
    let status = std::process::Command::new("schtasks")
        .args([
            "/create",
            "/tn",  TASK_NAME,
            "/tr",  &format!("\"{}\"", exe_str),
            "/sc",  "ONLOGON",
            "/rl",  "HIGHEST",
            "/f",
        ])
        .status()
        .map_err(|e| format!("Failed to run schtasks: {e}"))?;

    if !status.success() {
        return Err(format!(
            "schtasks /create exited with code {:?}. \
             Check nova.log for details. \
             Ensure the installer is running with Administrator rights.",
            status.code()
        ));
    }

    println!("✅ Scheduled task '{}' registered.", TASK_NAME);
    println!("   Trigger : On Logon");
    println!("   Level   : Highest Privileges (no UAC prompt on startup)");
    println!("   Nova will start automatically next time you log on.");
    println!("   Uninstall: nova-server.exe --uninstall");
    Ok(())
}

// ── Ghost Protocol ─────────────────────────────────────────────────────────────
// Scans the locations where a misguided previous installer might have dropped
// nova_shim.dll and removes any copies found.  Runs with admin rights (the UAC
// manifest on the exe requests requireAdministrator) so the deletion succeeds.

fn ghost_protocol_purge_dll() {
    let win_root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string());
    let candidates = [
        format!(r"{}\System32\nova_shim.dll",  win_root),
        format!(r"{}\SysWOW64\nova_shim.dll",  win_root),
        // Belt-and-suspenders: also check the legacy SYSTEM path that old
        // schtasks / service installs occasionally wrote to.
        format!(r"{}\nova_shim.dll",            win_root),
    ];

    println!("👻 Ghost Protocol: scanning for stale nova_shim.dll copies...");
    let mut found = false;
    for path_str in &candidates {
        let p = std::path::Path::new(path_str);
        if p.exists() {
            found = true;
            match std::fs::remove_file(p) {
                Ok(()) => println!("   🗑  Deleted: {}", path_str),
                Err(e) => println!("   ⚠️  Could not delete {} — {}", path_str, e),
            }
        }
    }
    if !found {
        println!("   ✅ No stale DLL copies found.");
    }
}

// ── Scheduled-Task uninstall ──────────────────────────────────────────────────

fn uninstall_task() -> Result<(), String> {
    // Kill the running instance first so Inno Setup can delete the files.
    println!("🔧 Stopping Nova...");
    let _ = run_silent("taskkill", &["/F", "/IM", "nova-server.exe"]);

    // Remove the scheduled task.  schtasks exits 1 if the task doesn't exist —
    // that's fine, so we only error on codes > 1.
    println!("📋 Removing scheduled task '{}'...", TASK_NAME);
    let status = std::process::Command::new("schtasks")
        .args(["/delete", "/tn", TASK_NAME, "/f"])
        .status()
        .map_err(|e| format!("Failed to run schtasks: {e}"))?;

    let code = status.code().unwrap_or(0);
    if !status.success() && code != 1 {
        // code 1 = task not found; anything else is a real error
        println!("⚠️  schtasks /delete returned code {} — task may already be gone.", code);
    }

    // Also clean up any old SCM service that might still be lingering.
    let _ = run_silent("sc", &["stop",   "NovaServer"]);
    let _ = run_silent("sc", &["delete", "NovaServer"]);

    println!("✅ Nova uninstalled.");
    println!("   The scheduled task has been removed.");
    println!("   Log file (nova.log) is kept for reference.");
    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Run a command silently (inheriting the redirected log file as stdout/stderr).
/// Returns the exit status; callers decide whether to care about it.
fn run_silent(program: &str, args: &[&str]) -> std::io::Result<std::process::ExitStatus> {
    std::process::Command::new(program).args(args).status()
}

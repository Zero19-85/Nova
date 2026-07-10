// Nova Server — elevated tray-resident application.
//
// Deployment model (Task Scheduler, NOT Windows Service):
//   --install    → registers a "NovaServerBoot" At-Logon / Highest-Privileges
//                  scheduled task, migrates/removes any old task names, cleans
//                  up any SCM service remnant, and runs the Ghost Protocol
//                  (removes stale nova_shim.dll copies from System32/SysWOW64).
//   --uninstall  → removes the task and kills any running instance.
//   (no flag)    → normal tray run; called by the scheduled task on logon.
//
// Why not a Windows Service?
//   Services run in Session 0 (isolated from the interactive desktop).
//   DXGI Desktop Duplication, the D3D11 Video Processor, and Windows Graphics
//   Capture ALL require the interactive session (Session 1+).  In Session 0
//   they return E_ACCESSDENIED — the encoder writes uninitialized texture
//   data → "half-green / half-smeared" stream corruption.
//
// Why Task XML instead of schtasks /create flags?
//   `schtasks /create` without an explicit /ru resolves ambiguously on some
//   Windows 11 builds and can default to SYSTEM (Session 0).  The XML path
//   lets us set <LogonType>InteractiveToken</LogonType> — the exact COM-level
//   flag that forces the task into the user's interactive session — plus
//   <UserId> scoped to the installing account and a 5-second startup delay
//   so DWM/WGC have time to initialise before Nova binds its capture session.

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

        // ── SYSTEM launcher service (Phase 15.2c) ─────────────────────────────
        // The service runs as LocalSystem and spawns the host (the no-arg mode
        // above) into the interactive console session with an elevated token.
        // Deployment is not switched to it yet — these are opt-in.
        Some("--service") => {
            // Invoked by the SCM. Blocks in the dispatcher until the service
            // stops. Its OWN log file (nova-service.log) — never nova.log, which
            // the spawned host owns; a shared file would leave the host unable to
            // open its log (sharing) and its startup errors invisible.
            nova_server::debug::init_service_logger();
            if let Err(e) = nova_server::service::run_service_dispatcher() {
                println!("❌ Service dispatcher failed: {e}");
                std::process::exit(1);
            }
            std::process::exit(0);
        }

        Some("--install-service") => {
            nova_server::debug::init_debug_logger();
            println!("=== Nova Service Install ===");
            // Same stale-DLL cleanup the task installer does — a copy of
            // nova_shim.dll left in System32/SysWOW64 shadows the real one.
            ghost_protocol_purge_dll();
            let exe = std::env::current_exe()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            // Remove the scheduled task as part of install so the service and
            // task can never both spawn a host.
            let remove_task = || {
                let _ = uninstall_task();
            };
            match nova_server::service::install_service(&exe, remove_task) {
                Ok(()) => std::process::exit(0),
                Err(e) => {
                    println!("❌ Service install failed: {e}");
                    std::process::exit(1);
                }
            }
        }

        Some("--uninstall-service") => {
            nova_server::debug::init_debug_logger();
            println!("=== Nova Service Uninstall ===");
            match nova_server::service::uninstall_service() {
                Ok(()) => std::process::exit(0),
                Err(e) => {
                    println!("❌ Service uninstall failed: {e}");
                    std::process::exit(1);
                }
            }
        }

        // Host launched by the service WITH a SYSTEM impersonation token handle
        // (`--system-token <n>`). The handle was inherited at the same numeric
        // value; stash it (the DDA capture thread assumes it for secure-desktop
        // capture), then run the host exactly like the normal no-arg path.
        Some("--system-token") => {
            if let Some(raw) = args.get(2).and_then(|s| s.parse::<isize>().ok()) {
                nova_server::service::set_system_impersonation_token(raw);
            }
            run_host();
        }

        _ => {
            // Normal run — launched by the scheduled task on logon, or manually.
            run_host();
        }
    }
}

/// Runs the host (tokio runtime + `nova_server::run()`), exiting 1 on a clean
/// startup failure. Shared by the no-arg launch and the `--system-token` launch.
fn run_host() -> ! {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    match rt.block_on(nova_server::run()) {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            // run() returns Err when a critical resource is unavailable (no GPU,
            // no display, WGC unsupported, etc.). Log to nova.log (stdout is
            // already redirected there) and exit 1 — a clean failure, not a panic.
            println!("❌ Nova exited with error: {e:?}");
            std::process::exit(1);
        }
    }
}

// ── Task identity ─────────────────────────────────────────────────────────────

/// The canonical scheduled-task name registered by --install.
const TASK_NAME: &str = "NovaServerBoot";

/// Legacy names from previous install strategies.  Swept during both --install
/// (to avoid leaving a defunct ghost task) and --uninstall (belt-and-suspenders).
const TASK_NAMES_LEGACY: &[&str] = &[
    "Nova Game Streaming",
    "NovaServer",
    "Nova Server",
];

// ── Scheduled-task install ────────────────────────────────────────────────────

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
    println!("🔧 Cleaning up any old NovaServer SCM service...");
    let _ = run_hidden("sc", &["stop",   "NovaServer"]);
    let _ = run_hidden("sc", &["delete", "NovaServer"]);

    // ── Sweep legacy task names ───────────────────────────────────────────────
    // Removes stale entries from prior install strategies so Task Scheduler
    // stays clean and there is no ambiguity about which entry fires on logon.
    println!("🔧 Removing any prior task registrations...");
    for old_name in TASK_NAMES_LEGACY {
        let _ = run_hidden("schtasks", &["/delete", "/tn", old_name, "/f"]);
    }

    // ── Kill any running Nova instance ────────────────────────────────────────
    // Excludes THIS process — a blanket taskkill would terminate the installer.
    println!("🔧 Stopping any other running Nova instance...");
    kill_other_nova_instances();

    // ── Register NovaServerBoot via Task XML ──────────────────────────────────
    println!("📋 Registering scheduled task '{TASK_NAME}'...");
    register_task_xml(TASK_NAME, &exe_str)?;

    println!("✅ Scheduled task '{TASK_NAME}' registered.");
    println!("   Trigger : At Logon — current user only, 5-second startup delay");
    println!("   Session : InteractiveToken (Session 1+, full DWM/WGC access)");
    println!("   Level   : HighestAvailable (elevated token; no UAC prompt at boot)");
    println!("   Nova will start automatically next time you log on.");
    println!("   Uninstall: nova-server.exe --uninstall");
    Ok(())
}

/// Builds a Task Scheduler XML definition and registers it via `schtasks /create /xml`.
///
/// Using XML rather than `/sc /rl` flags gives us:
///   • `<LogonType>InteractiveToken</LogonType>` — the exact COM-level bit that
///     forces the task into the logged-on user's interactive session (Session 1+),
///     preventing the ambiguous-SYSTEM fallback that causes Session 0 binding.
///   • `<RunLevel>HighestAvailable</RunLevel>` — request the elevated token for
///     accounts in the Administrators group; no-op for standard accounts.
///   • `<UserId>` scoped to the installing account — trigger fires only for this
///     user, not any user, so a shared PC doesn't launch Nova on every logon.
///   • `<Delay>PT5S</Delay>` — 5-second grace period for DWM/WGC to settle
///     before Nova attempts to open a capture session.
///   • `<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>` — prevents
///     duplicate Nova processes if the task fires more than once.
///   • `<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>` — no automatic timeout;
///     Nova is a long-running tray process and must not be killed by the scheduler.
fn register_task_xml(task_name: &str, exe_path: &str) -> Result<(), String> {
    // Resolve the current user as DOMAIN\Username (works for local, MS-account,
    // and domain accounts — matches what Task Scheduler expects in <UserId>).
    let username   = std::env::var("USERNAME")
        .unwrap_or_else(|_| "User".to_string());
    let userdomain = std::env::var("USERDOMAIN")
        .unwrap_or_else(|_| std::env::var("COMPUTERNAME").unwrap_or_else(|_| ".".to_string()));
    let full_user = format!("{userdomain}\\{username}");

    // Escape XML special characters in path and user strings.
    let safe_exe  = xml_escape(exe_path);
    let safe_user = xml_escape(&full_user);

    let xml = format!(r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.4" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>Nova Game Streaming — auto-start at user logon</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{safe_user}</UserId>
      <Delay>PT5S</Delay>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{safe_user}</UserId>
      <!-- InteractiveToken: runs inside the user's interactive session (Session 1+).
           This is the flag that prevents Session 0 binding and gives the process
           access to the DWM compositor, WGC, and the D3D11 Video Processor. -->
      <LogonType>InteractiveToken</LogonType>
      <!-- HighestAvailable: request the elevated admin token when available.
           Needed for VDD devnode manipulation and ViGEmBus injection. -->
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <!-- PT0S = no execution time limit; Nova is a long-running tray process. -->
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>4</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{safe_exe}</Command>
    </Exec>
  </Actions>
</Task>"#);

    // schtasks /create /xml requires UTF-16 LE with BOM.
    let utf16: Vec<u16> = xml.encode_utf16().collect();
    let mut bytes = Vec::with_capacity(2 + utf16.len() * 2);
    bytes.extend_from_slice(&[0xFF, 0xFE]); // BOM
    for unit in &utf16 {
        bytes.push((*unit & 0xFF) as u8);
        bytes.push((*unit >> 8)   as u8);
    }

    let tmp_path = std::env::temp_dir().join("nova_task_reg.xml");
    std::fs::write(&tmp_path, &bytes)
        .map_err(|e| format!("Failed to write task XML to temp file: {e}"))?;

    let tmp_str = tmp_path.to_string_lossy().into_owned();
    let status = run_hidden("schtasks", &[
        "/create",
        "/tn",  task_name,
        "/xml", &tmp_str,
        "/f",
    ]).map_err(|e| format!("Failed to run schtasks: {e}"))?;

    // Clean up regardless of success.
    let _ = std::fs::remove_file(&tmp_path);

    if !status.success() {
        return Err(format!(
            "schtasks /create /xml exited with code {:?}. \
             Check nova.log for details. \
             Ensure --install is invoked with Administrator rights.",
            status.code()
        ));
    }

    println!("   Registered for user: {full_user}");
    Ok(())
}

/// Escapes the five XML predefined entities in a string.
fn xml_escape(s: &str) -> String {
    s.replace('&',  "&amp;")
     .replace('<',  "&lt;")
     .replace('>',  "&gt;")
     .replace('"',  "&quot;")
     .replace('\'', "&apos;")
}

// ── Ghost Protocol ─────────────────────────────────────────────────────────────
// Scans the locations where a misguided previous installer might have dropped
// nova_shim.dll and removes any copies found.  Runs with admin rights (the UAC
// manifest on the exe requests requireAdministrator) so the deletion succeeds.

fn ghost_protocol_purge_dll() {
    let win_root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string());
    let candidates = [
        format!(r"{win_root}\System32\nova_shim.dll"),
        format!(r"{win_root}\SysWOW64\nova_shim.dll"),
        format!(r"{win_root}\nova_shim.dll"),
    ];

    println!("👻 Ghost Protocol: scanning for stale nova_shim.dll copies...");
    let mut found = false;
    for path_str in &candidates {
        let p = std::path::Path::new(path_str);
        if p.exists() {
            found = true;
            match std::fs::remove_file(p) {
                Ok(()) => println!("   🗑  Deleted: {path_str}"),
                Err(e) => println!("   ⚠️  Could not delete {path_str} — {e}"),
            }
        }
    }
    if !found {
        println!("   ✅ No stale DLL copies found.");
    }
}

// ── Scheduled-task uninstall ──────────────────────────────────────────────────

fn uninstall_task() -> Result<(), String> {
    // Kill the running instance first so Inno Setup can delete the files.
    // Excludes THIS process (uninstall_task is also called from --install-service).
    println!("🔧 Stopping Nova...");
    kill_other_nova_instances();

    // Remove the canonical task name.
    println!("📋 Removing scheduled task '{TASK_NAME}'...");
    let _ = run_hidden("schtasks", &["/delete", "/tn", TASK_NAME, "/f"]);

    // Sweep all legacy names — belt-and-suspenders for partial / failed installs.
    for old_name in TASK_NAMES_LEGACY {
        let _ = run_hidden("schtasks", &["/delete", "/tn", old_name, "/f"]);
    }

    // Clean up any lingering SCM service from an even older install strategy.
    let _ = run_hidden("sc", &["stop",   "NovaServer"]);
    let _ = run_hidden("sc", &["delete", "NovaServer"]);

    println!("✅ Nova uninstalled.");
    println!("   Scheduled task removed; log file (nova.log) kept for reference.");
    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Force-terminate any OTHER running `nova-server.exe` — but never this
/// process. `--install`, `--uninstall`, and `--install-service` all run inside
/// a `nova-server.exe`, so a blanket `taskkill /F /IM nova-server.exe` would
/// terminate the very process doing the install (observed: the install self-
/// killed before `CreateServiceW`). The `PID ne <self>` filter excludes us.
fn kill_other_nova_instances() {
    let filter = format!("PID ne {}", std::process::id());
    let _ = run_hidden("taskkill", &["/F", "/IM", "nova-server.exe", "/FI", &filter]);
}

/// Run a command without creating a visible console window.
///
/// `nova-server.exe` is compiled with `#![windows_subsystem = "windows"]`, so
/// it has no console of its own.  Without `CREATE_NO_WINDOW`, Windows creates a
/// new console window for every console-subsystem child process (schtasks, sc,
/// taskkill, powershell) — visible as brief black flashes during install.
fn run_hidden(program: &str, args: &[&str]) -> std::io::Result<std::process::ExitStatus> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    std::process::Command::new(program)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .status()
}

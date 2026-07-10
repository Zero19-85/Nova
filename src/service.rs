//! Thin SYSTEM launcher service (Phase 15.2c).
//!
//! ## Why a service at all
//!
//! Nova's host (`nova-server.exe` with no args) must run in the interactive
//! console session (Session 1+) — DWM/WGC/D3D11-VP all fail in Session 0. Today
//! a `NovaServerBoot` scheduled task provides that. What the task *cannot*
//! provide is a token derived from `LocalSystem` with `SeTcbPrivilege`, which is
//! what lets the host attach a capture thread to the **secure desktop**
//! (`WinSta0\Winlogon`) via `SetThreadDesktop` so the Phase 15.2 DDA backend can
//! duplicate UAC / logon screens instead of showing a frozen frame.
//!
//! This service is that missing piece: it runs as `LocalSystem` in Session 0,
//! resolves the active console session's user token, promotes it to the
//! elevated linked token, and `CreateProcessAsUserW`-spawns the **unchanged**
//! host into the interactive session. The host keeps 100% of its current logic;
//! it simply now starts with a SYSTEM-derived elevated token.
//!
//! ```text
//!  SCM ─▶ nova-server.exe --service            (LocalSystem, Session 0)
//!           │  WTSGetActiveConsoleSessionId
//!           │  WTSQueryUserToken ─▶ user token (filtered under UAC)
//!           │  TokenLinkedToken   ─▶ full elevated token
//!           │  DuplicateTokenEx   ─▶ primary token
//!           └─ CreateProcessAsUserW ─▶ nova-server.exe   (user, Session 1, elevated)
//! ```
//!
//! ## Lifecycle & robustness
//!
//! The worker keeps exactly one host alive in the current console session:
//! - host exits (crash / user quit) ⇒ respawn after a short backoff;
//! - console session changes (fast user switching, RDP connect/disconnect) ⇒
//!   the `SERVICE_CONTROL_SESSIONCHANGE` handler nudges the worker, which kills
//!   the host bound to the old session and spawns a fresh one in the new one;
//! - `STOP` / `SHUTDOWN` ⇒ stop managing and terminate the host. (On OS
//!   shutdown the host also receives its own `WM_ENDSESSION` / console-ctrl
//!   signals, so its emergency display-restore still runs; see `shutdown.rs`.)
//!
//! ## Status (Phase 15.2c = launcher plumbing)
//!
//! The token/spawn path implemented here is the standard, well-documented
//! "spawn interactive-user process from a SYSTEM service" flow. Whether the
//! resulting token grants `SetThreadDesktop(Winlogon)` for real DDA capture of
//! the secure desktop is the item that needs live validation on target
//! hardware — if the elevated user token proves insufficient, the fallback is
//! to run the capture thread itself under the service's SYSTEM token (a later
//! refinement). Either way the launcher is the prerequisite and lands here.
//!
//! Deployment is NOT switched to the service yet: `--install` still registers
//! the scheduled task. `--install-service` is the opt-in path (and removes the
//! task first, so the two can never double-spawn the host). Installer migration
//! is the follow-up chunk.

use std::ffi::c_void;
use std::sync::atomic::{AtomicIsize, Ordering};

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, ERROR_ACCESS_DENIED};
use windows::Win32::Security::{
    DuplicateTokenEx, GetTokenInformation, SecurityIdentification, SecurityImpersonation,
    SetTokenInformation, TokenElevationType, TokenElevationTypeLimited, TokenLinkedToken,
    TokenPrimary, TokenSessionId, SECURITY_ATTRIBUTES, TOKEN_ALL_ACCESS, TOKEN_DUPLICATE,
    TOKEN_ELEVATION_TYPE, TOKEN_LINKED_TOKEN, TOKEN_QUERY,
};
use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows::Win32::System::RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken};
use windows::Win32::System::Services::{
    ChangeServiceConfigW, CreateServiceW, DeleteService, OpenSCManagerW, OpenServiceW,
    RegisterServiceCtrlHandlerExW, SetServiceStatus, StartServiceCtrlDispatcherW, ControlService,
    ENUM_SERVICE_TYPE, SC_HANDLE, SC_MANAGER_ALL_ACCESS, SERVICE_ACCEPT_SHUTDOWN,
    SERVICE_ACCEPT_STOP, SERVICE_ALL_ACCESS, SERVICE_AUTO_START, SERVICE_CHANGE_CONFIG,
    SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP, SERVICE_ERROR, SERVICE_ERROR_NORMAL,
    SERVICE_START_TYPE, SERVICE_STATUS, SERVICE_STATUS_HANDLE, SERVICE_TABLE_ENTRYW,
    SERVICE_WIN32_OWN_PROCESS, SERVICE_STOPPED, SERVICE_RUNNING, SERVICE_START_PENDING,
    SERVICE_STOP_PENDING, SERVICE_STATUS_CURRENT_STATE,
};
use windows::Win32::System::Threading::{
    CreateProcessAsUserW, CreateEventW, GetCurrentProcess, OpenProcessToken, SetEvent,
    WaitForMultipleObjects, WaitForSingleObject, CREATE_UNICODE_ENVIRONMENT,
    NORMAL_PRIORITY_CLASS, PROCESS_INFORMATION, STARTUPINFOW,
};

// ── SYSTEM impersonation token handed to the host ─────────────────────────────
//
// The host runs as the interactive USER (WGC needs a real user; it fails as
// SYSTEM with 0x80070424). But the secure desktop's ACL only admits SYSTEM, so
// the host needs a SYSTEM token to assume on its DDA capture thread. The service
// IS LocalSystem, so it duplicates its own token as an inheritable impersonation
// token and passes the handle value to the host via `--system-token <n>`; the
// child inherits the handle (same value) and assumes it only around the
// secure-desktop DDA calls. Stored process-globally in the HOST after parsing.

/// Set by the host's `--system-token` arg handler. The inherited handle value.
static SYSTEM_TOKEN: AtomicIsize = AtomicIsize::new(0);

/// (Host side) Record the SYSTEM impersonation token handle the service passed.
pub fn set_system_impersonation_token(raw_handle: isize) {
    SYSTEM_TOKEN.store(raw_handle, Ordering::Release);
}

/// (Host side) The SYSTEM impersonation token to assume for secure-desktop DDA,
/// or `None` when the host wasn't launched by the service (task/manual launch).
pub fn system_impersonation_token() -> Option<HANDLE> {
    let v = SYSTEM_TOKEN.load(Ordering::Acquire);
    if v == 0 {
        None
    } else {
        Some(HANDLE(v as *mut c_void))
    }
}

// ── Identity ──────────────────────────────────────────────────────────────────

/// Service name registered with the SCM (and passed to the dispatcher).
pub const SERVICE_NAME: &str = "NovaService";
const SERVICE_DISPLAY_NAME: &str = "Nova Game Streaming Launcher";

/// `SERVICE_CONTROL_SESSIONCHANGE` — not surfaced as a typed constant in
/// windows-rs 0.58's Services module.
const SERVICE_CONTROL_SESSIONCHANGE: u32 = 0x0000_000E;
/// `SERVICE_ACCEPT_SESSIONCHANGE` — ditto.
const SERVICE_ACCEPT_SESSIONCHANGE: u32 = 0x0000_0080;
/// `WTSGetActiveConsoleSessionId` sentinel meaning "no console session".
const NO_SESSION: u32 = 0xFFFF_FFFF;
/// `SERVICE_NO_CHANGE` — "leave this field as-is" for `ChangeServiceConfigW`.
const SERVICE_NO_CHANGE: u32 = 0xFFFF_FFFF;

// ── Global SCM state ──────────────────────────────────────────────────────────
//
// The SCM callbacks (`service_main`, `handler_ex`) are bare `extern "system"`
// function pointers with no user-data slot we control end-to-end, so the small
// amount of shared state lives in atomics. Handles are stored as `isize`.

static STATUS_HANDLE: AtomicIsize = AtomicIsize::new(0);
/// Manual-reset event: set once to ask the worker to stop.
static STOP_EVENT: AtomicIsize = AtomicIsize::new(0);
/// Auto-reset event: set to wake the worker early (session change).
static WAKE_EVENT: AtomicIsize = AtomicIsize::new(0);

fn load_handle(slot: &AtomicIsize) -> HANDLE {
    HANDLE(slot.load(Ordering::Acquire) as *mut c_void)
}

// ── Public entry points (called from bin/nova-server.rs) ──────────────────────

/// SCM entry (`nova-server.exe --service`). Blocks in the dispatcher until the
/// service stops. Returns `Err` only if the dispatcher itself fails to connect
/// (e.g. the exe was run with `--service` from a normal shell, not the SCM).
pub fn run_service_dispatcher() -> Result<(), String> {
    let mut name: Vec<u16> = SERVICE_NAME.encode_utf16().chain(std::iter::once(0)).collect();
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: PWSTR(name.as_mut_ptr()),
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW::default(), // NULL terminator
    ];
    unsafe {
        StartServiceCtrlDispatcherW(table.as_ptr())
            .map_err(|e| format!("StartServiceCtrlDispatcherW failed (run via the SCM, not directly): {e:?}"))
    }
}

/// Register `NovaService` with the SCM (`--install-service`). Removes the
/// scheduled task first so the two deployment models never both spawn a host.
pub fn install_service(exe_path: &str, remove_task: impl FnOnce()) -> Result<(), String> {
    println!("🔧 Removing the scheduled task first (service and task must not both launch the host)...");
    remove_task();

    let scm = open_scm(SC_MANAGER_ALL_ACCESS)?;
    let _guard = ScHandleGuard(scm);

    // Binary path = "<exe>" --service  (quoted; path may contain spaces).
    let bin_path = format!("\"{exe_path}\" --service");

    let name_w = wide(SERVICE_NAME);
    let display_w = wide(SERVICE_DISPLAY_NAME);
    let bin_w = wide(&bin_path);

    let svc = unsafe {
        CreateServiceW(
            scm,
            PCWSTR(name_w.as_ptr()),
            PCWSTR(display_w.as_ptr()),
            SERVICE_ALL_ACCESS,
            SERVICE_WIN32_OWN_PROCESS,
            SERVICE_AUTO_START,
            SERVICE_ERROR_NORMAL,
            PCWSTR(bin_w.as_ptr()),
            PCWSTR::null(),
            None,
            PCWSTR::null(),           // no dependencies
            PCWSTR::null(),           // lpServiceStartName = NULL ⇒ LocalSystem
            PCWSTR::null(),           // no password
        )
    };
    match svc {
        Ok(h) => {
            unsafe { let _ = windows::Win32::System::Services::CloseServiceHandle(h); }
            println!("✅ Service '{SERVICE_NAME}' installed (LocalSystem, auto-start).");
            println!("   It spawns the Nova host into the active console session on boot/logon.");
            Ok(())
        }
        Err(e) if e.code() == windows::core::HRESULT::from_win32(
            windows::Win32::Foundation::ERROR_SERVICE_EXISTS.0,
        ) => {
            // Upgrade / reinstall: the service is already registered. Update its
            // binary path in place (idempotent, race-free — no delete/recreate
            // churn) so a reinstall to a different directory still points here.
            update_service_binary_path(scm, &name_w, &bin_w)?;
            println!("✅ Service '{SERVICE_NAME}' already existed — binary path updated to \"{bin_path}\".");
            Ok(())
        }
        Err(e) => Err(format!("CreateServiceW failed: {e:?} (run --install-service elevated)")),
    }
}

/// Update an existing service's binary path (upgrade/reinstall path), leaving
/// every other field unchanged.
fn update_service_binary_path(scm: SC_HANDLE, name_w: &[u16], bin_w: &[u16]) -> Result<(), String> {
    unsafe {
        let svc = OpenServiceW(scm, PCWSTR(name_w.as_ptr()), SERVICE_CHANGE_CONFIG)
            .map_err(|e| format!("OpenServiceW(existing NovaService): {e:?}"))?;
        let _guard = ScHandleGuard(svc);
        ChangeServiceConfigW(
            svc,
            ENUM_SERVICE_TYPE(SERVICE_NO_CHANGE),
            SERVICE_START_TYPE(SERVICE_NO_CHANGE),
            SERVICE_ERROR(SERVICE_NO_CHANGE),
            PCWSTR(bin_w.as_ptr()),
            PCWSTR::null(),
            None,
            PCWSTR::null(),
            PCWSTR::null(),
            PCWSTR::null(),
            PCWSTR::null(),
        )
        .map_err(|e| format!("ChangeServiceConfigW: {e:?}"))
    }
}

/// Stop and delete `NovaService` (`--uninstall-service`). Idempotent.
pub fn uninstall_service() -> Result<(), String> {
    let scm = open_scm(SC_MANAGER_ALL_ACCESS)?;
    let _scm_guard = ScHandleGuard(scm);

    let name_w = wide(SERVICE_NAME);
    let svc = unsafe { OpenServiceW(scm, PCWSTR(name_w.as_ptr()), SERVICE_ALL_ACCESS) };
    let svc = match svc {
        Ok(h) => h,
        Err(_) => {
            println!("ℹ️  Service '{SERVICE_NAME}' is not installed — nothing to remove.");
            return Ok(());
        }
    };
    let _svc_guard = ScHandleGuard(svc);

    // Best-effort stop before delete.
    let mut status = SERVICE_STATUS::default();
    unsafe {
        let _ = ControlService(svc, SERVICE_CONTROL_STOP, &mut status);
        DeleteService(svc).map_err(|e| format!("DeleteService failed: {e:?}"))?;
    }
    println!("✅ Service '{SERVICE_NAME}' removed.");
    Ok(())
}

// ── SCM callbacks ─────────────────────────────────────────────────────────────

/// `ServiceMain`: register the control handler, create the stop/wake events,
/// report RUNNING, run the worker, report STOPPED.
unsafe extern "system" fn service_main(_argc: u32, _argv: *mut PWSTR) {
    let name_w = wide(SERVICE_NAME);
    let handle = match RegisterServiceCtrlHandlerExW(
        PCWSTR(name_w.as_ptr()),
        Some(handler_ex),
        None,
    ) {
        Ok(h) => h,
        Err(_) => return, // nothing we can report to without a status handle
    };
    STATUS_HANDLE.store(handle.0 as isize, Ordering::Release);

    // Stop = manual-reset (stays signalled); wake = auto-reset (one-shot nudge).
    let stop = CreateEventW(None, true, false, PCWSTR::null());
    let wake = CreateEventW(None, false, false, PCWSTR::null());
    let (Ok(stop), Ok(wake)) = (stop, wake) else {
        report_status(SERVICE_STOPPED, 0, 1);
        return;
    };
    STOP_EVENT.store(stop.0 as isize, Ordering::Release);
    WAKE_EVENT.store(wake.0 as isize, Ordering::Release);

    report_status(SERVICE_START_PENDING, accepted(), 0);
    report_status(SERVICE_RUNNING, accepted(), 0);

    service_worker(stop, wake);

    report_status(SERVICE_STOPPED, 0, 0);
    let _ = CloseHandle(stop);
    let _ = CloseHandle(wake);
}

/// Controls the service accepts while RUNNING.
fn accepted() -> u32 {
    SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN | SERVICE_ACCEPT_SESSIONCHANGE
}

/// SCM control handler. STOP/SHUTDOWN ⇒ signal stop; SESSIONCHANGE ⇒ wake the
/// worker to re-evaluate which session the host should be in.
unsafe extern "system" fn handler_ex(
    control: u32,
    _event_type: u32,
    _event_data: *mut c_void,
    _context: *mut c_void,
) -> u32 {
    match control {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
            report_status(SERVICE_STOP_PENDING, 0, 0);
            let _ = SetEvent(load_handle(&STOP_EVENT));
        }
        SERVICE_CONTROL_SESSIONCHANGE => {
            let _ = SetEvent(load_handle(&WAKE_EVENT));
        }
        _ => {}
    }
    0 // NO_ERROR
}

/// Report a service-status transition to the SCM.
fn report_status(state: SERVICE_STATUS_CURRENT_STATE, controls: u32, exit_code: u32) {
    let raw = STATUS_HANDLE.load(Ordering::Acquire);
    if raw == 0 {
        return;
    }
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: controls,
        dwWin32ExitCode: exit_code,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: 0,
        dwWaitHint: if state == SERVICE_START_PENDING || state == SERVICE_STOP_PENDING {
            3000
        } else {
            0
        },
    };
    unsafe {
        let _ = SetServiceStatus(SERVICE_STATUS_HANDLE(raw as *mut c_void), &status);
    }
}

// ── Worker: keep one host alive in the active console session ─────────────────

/// One managed host process, tagged with the console session it belongs to.
struct HostProcess {
    process: HANDLE,
    session_id: u32,
}

impl HostProcess {
    /// True if the process has exited (WAIT_OBJECT_0 = signalled = exited).
    fn has_exited(&self) -> bool {
        unsafe { WaitForSingleObject(self.process, 0).0 == 0 }
    }
    fn terminate(&self) {
        unsafe {
            let _ = windows::Win32::System::Threading::TerminateProcess(self.process, 0);
            let _ = CloseHandle(self.process);
        }
    }
}

fn service_worker(stop: HANDLE, wake: HANDLE) {
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();

    let mut host: Option<HostProcess> = None;
    let handles = [stop, wake];

    loop {
        // Reconcile: ensure a host is running in the CURRENT console session.
        let session = unsafe { WTSGetActiveConsoleSessionId() };
        if session != NO_SESSION {
            let need_spawn = match &host {
                None => true,
                Some(h) if h.has_exited() => {
                    let _ = unsafe { CloseHandle(h.process) };
                    true
                }
                Some(h) if h.session_id != session => {
                    // Console moved to another session (fast user switch / RDP).
                    println!("🔄 Console session {} → {} — respawning host", h.session_id, session);
                    h.terminate();
                    true
                }
                Some(_) => false,
            };
            if need_spawn {
                host = None;
                match spawn_host_in_session(&exe, session) {
                    Ok(h) => {
                        println!("✅ Host spawned in session {session} (pid handle live)");
                        host = Some(h);
                    }
                    Err(e) => {
                        // Transient at boot (no shell yet) or a real privilege
                        // problem — either way keep the service alive and retry.
                        println!("⚠️  Host spawn into session {session} failed: {e}");
                    }
                }
            }
        }

        // Wait for stop, a session-change wake, or a 2 s reconcile tick.
        let w = unsafe { WaitForMultipleObjects(&handles, false, 2000) };
        if w.0 == 0 {
            // WAIT_OBJECT_0 = stop event.
            break;
        }
        // WAIT_OBJECT_0+1 (wake) or WAIT_TIMEOUT ⇒ loop and reconcile.
    }

    // Service is stopping. The common stop cause is the user's tray "Quit",
    // which makes the host call `sc stop NovaService` and THEN run its own
    // graceful teardown (display + audio restore). Give it a grace period to
    // exit on its own before force-terminating, so that teardown completes.
    if let Some(h) = host.take() {
        unsafe {
            let _ = WaitForSingleObject(h.process, HOST_GRACEFUL_EXIT_MS);
        }
        h.terminate(); // no-op terminate if it already exited; also closes the handle
    }
}

/// Grace period the service gives the host to exit on its own (finishing its
/// display/audio teardown) after a stop, before force-terminating it.
const HOST_GRACEFUL_EXIT_MS: u32 = 6000;

/// Ask the SCM to stop `NovaService` (best-effort). The host's tray "Quit"
/// calls this so the service does not immediately respawn the exiting host —
/// without it, "Quit" just triggers a relaunch. A no-op in effect when the host
/// was not launched by the service (task/manual launch): `sc` returns an error
/// which we ignore. Uses `.status()` so the stop request is delivered to the
/// SCM before the host proceeds to tear down and exit.
pub fn request_service_stop() {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let _ = std::process::Command::new("sc")
        .args(["stop", SERVICE_NAME])
        .creation_flags(CREATE_NO_WINDOW)
        .status();
}

// ── Token acquisition + host spawn ────────────────────────────────────────────

/// `CreateProcessAsUserW`-spawn the host into `session_id` as the interactive
/// user's **elevated** token, and hand it a SYSTEM impersonation token for
/// secure-desktop DDA.
///
/// Why the user token and not SYSTEM (validated live 2026-07-10): running the
/// host as SYSTEM breaks WGC — Nova's primary, HDR-capable capture backend —
/// with `0x80070424` (WGC's WinRT/broker infra requires a real user session).
/// So the host runs as the elevated user (WGC/HDR/audio all work), and the
/// SYSTEM identity the secure desktop's ACL demands is supplied as a separate
/// **impersonation token** that the host assumes only on its DDA capture thread
/// (`dda::begin_system_impersonation`). Sunshine can run wholesale as SYSTEM
/// because its primary backend is DDA, not WGC.
fn spawn_host_in_session(exe: &str, session_id: u32) -> Result<HostProcess, String> {
    unsafe {
        // 1. The console user's token (filtered/limited under UAC).
        let mut user_token = HANDLE::default();
        WTSQueryUserToken(session_id, &mut user_token)
            .map_err(|e| format!("WTSQueryUserToken(session {session_id}): {e:?}"))?;
        let _user_guard = HandleGuard(user_token);

        // 2. Promote to the full elevated linked token when UAC filtered it, and
        //    duplicate to a primary token for CreateProcessAsUser.
        let elevated = elevated_token(user_token);
        let elevated_ref = elevated.as_ref().map(|g| g.0).unwrap_or(user_token);
        let mut primary = HANDLE::default();
        DuplicateTokenEx(
            elevated_ref,
            TOKEN_ALL_ACCESS,
            None,
            SecurityIdentification,
            TokenPrimary,
            &mut primary,
        )
        .map_err(|e| format!("DuplicateTokenEx(user): {e:?}"))?;
        let _primary_guard = HandleGuard(primary);

        // 3. SYSTEM impersonation token for secure-desktop DDA — inheritable, so
        //    the child gets a handle at the same value; passed by value on the
        //    command line. Retargeted to the console session so a thread
        //    impersonating it is "SYSTEM in session N" (what the secure desktop's
        //    ACL requires). Best-effort: without it, DDA secure-desktop capture
        //    simply stays denied (graceful, same as no service).
        let sys_token = create_inheritable_system_token(session_id);

        // 4. User environment block.
        let mut env: *mut c_void = std::ptr::null_mut();
        let have_env = CreateEnvironmentBlock(&mut env, primary, false).is_ok();

        // 5. Command line: exe (quoted) + the inherited SYSTEM token handle. The
        //    working dir is the exe folder (a LocalSystem service's CWD is
        //    System32; the host resolves paths from current_exe() but this
        //    removes any relative-path ambiguity).
        let cmdline = match sys_token {
            Some(h) => format!("\"{exe}\" --system-token {}", h.0 as isize),
            None => format!("\"{exe}\""),
        };
        let mut cmdline_w: Vec<u16> = cmdline.encode_utf16().chain(std::iter::once(0)).collect();
        let inherit_handles = sys_token.is_some();
        let workdir_w = std::path::Path::new(exe)
            .parent()
            .map(|d| wide(&d.to_string_lossy()));
        let mut desktop_w: Vec<u16> = "WinSta0\\Default".encode_utf16().chain(std::iter::once(0)).collect();
        let mut si = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            lpDesktop: PWSTR(desktop_w.as_mut_ptr()),
            ..Default::default()
        };
        let mut pi = PROCESS_INFORMATION::default();

        let result = CreateProcessAsUserW(
            primary,
            PCWSTR::null(),                    // app name parsed from the command line
            PWSTR(cmdline_w.as_mut_ptr()),
            None,
            None,
            inherit_handles,                   // inherit the SYSTEM token handle
            CREATE_UNICODE_ENVIRONMENT | NORMAL_PRIORITY_CLASS,
            if have_env { Some(env) } else { None },
            workdir_w
                .as_ref()
                .map_or(PCWSTR::null(), |w| PCWSTR(w.as_ptr())),
            &si,
            &mut pi,
        );

        if have_env && !env.is_null() {
            let _ = DestroyEnvironmentBlock(env);
        }
        // Close the service's copy of the SYSTEM token — the child inherited its
        // own handle at the same value; closing ours does not affect the child's.
        if let Some(h) = sys_token {
            let _ = CloseHandle(h);
        }
        // Keep `si` alive until after the call (lpDesktop points into desktop_w).
        let _ = &mut si;

        result.map_err(|e| {
            if e.code() == windows::core::HRESULT::from_win32(ERROR_ACCESS_DENIED.0) {
                format!("CreateProcessAsUserW denied ({e:?}) — the service must run as LocalSystem")
            } else {
                format!("CreateProcessAsUserW: {e:?}")
            }
        })?;

        // We manage the process; the thread handle is not needed.
        let _ = CloseHandle(pi.hThread);
        Ok(HostProcess { process: pi.hProcess, session_id })
    }
}

/// Duplicate the service's own (LocalSystem) token as an **inheritable SYSTEM
/// token retargeted to `session_id`**, to hand to the host. A thread that
/// impersonates it becomes "SYSTEM in session N", which is what the secure
/// (Winlogon) desktop's ACL admits. Returns the handle (valid in this process;
/// the child inherits an equal-valued copy). `None` on failure — DDA
/// secure-desktop capture then stays gracefully denied.
fn create_inheritable_system_token(session_id: u32) -> Option<HANDLE> {
    unsafe {
        let mut current = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_DUPLICATE | TOKEN_QUERY, &mut current).ok()?;
        let _guard = HandleGuard(current);

        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            bInheritHandle: true.into(),
            ..Default::default()
        };
        // Primary token, full access — ImpersonateLoggedOnUser accepts a primary
        // token, and full access is needed to retarget the session id below.
        let mut dup = HANDLE::default();
        DuplicateTokenEx(
            current,
            TOKEN_ALL_ACCESS,
            Some(&sa),
            SecurityImpersonation,
            TokenPrimary,
            &mut dup,
        )
        .ok()?;

        // Retarget to the interactive console session (SeTcbPrivilege — which
        // LocalSystem has). Without this the token is SYSTEM-in-session-0 and a
        // thread impersonating it cannot open session N's Winlogon desktop.
        if let Err(e) = SetTokenInformation(
            dup,
            TokenSessionId,
            &session_id as *const u32 as *const c_void,
            std::mem::size_of::<u32>() as u32,
        ) {
            println!("⚠️  Service: could not retarget SYSTEM token to session {session_id}: {e:?}");
        }
        Some(dup)
    }
}

/// Returns the full elevated linked token when `user_token` is a UAC-filtered
/// (limited) token, else `None` (UAC off, or already full — use the original).
fn elevated_token(user_token: HANDLE) -> Option<HandleGuard> {
    unsafe {
        let mut elev_type = TOKEN_ELEVATION_TYPE::default();
        let mut ret_len = 0u32;
        let ok = GetTokenInformation(
            user_token,
            TokenElevationType,
            Some(&mut elev_type as *mut _ as *mut c_void),
            std::mem::size_of::<TOKEN_ELEVATION_TYPE>() as u32,
            &mut ret_len,
        )
        .is_ok();
        if !ok || elev_type != TokenElevationTypeLimited {
            return None;
        }
        let mut linked = TOKEN_LINKED_TOKEN::default();
        let ok = GetTokenInformation(
            user_token,
            TokenLinkedToken,
            Some(&mut linked as *mut _ as *mut c_void),
            std::mem::size_of::<TOKEN_LINKED_TOKEN>() as u32,
            &mut ret_len,
        )
        .is_ok();
        if ok && !linked.LinkedToken.is_invalid() {
            Some(HandleGuard(linked.LinkedToken))
        } else {
            None
        }
    }
}

// ── Small RAII helpers ────────────────────────────────────────────────────────

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn open_scm(access: u32) -> Result<windows::Win32::System::Services::SC_HANDLE, String> {
    unsafe {
        OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), access)
            .map_err(|e| format!("OpenSCManagerW failed: {e:?} (run elevated)"))
    }
}

/// Closes a kernel `HANDLE` on drop.
struct HandleGuard(HANDLE);
impl Drop for HandleGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe { let _ = CloseHandle(self.0); }
        }
    }
}

/// Closes an SCM handle on drop.
struct ScHandleGuard(windows::Win32::System::Services::SC_HANDLE);
impl Drop for ScHandleGuard {
    fn drop(&mut self) {
        unsafe { let _ = windows::Win32::System::Services::CloseServiceHandle(self.0); }
    }
}

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
//! - `STOP` / `SHUTDOWN` ⇒ signal the host's named shutdown event
//!   (`Global\NovaHostShutdown`) so it runs its full graceful teardown, wait
//!   the grace period, then `TerminateProcess` as the backstop. (On OS
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
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS,
    ERROR_ELEVATION_REQUIRED, HANDLE,
};
use windows::Win32::Security::{
    DuplicateTokenEx, GetTokenInformation, SecurityIdentification, SecurityImpersonation,
    SetTokenInformation, TokenElevationType, TokenElevationTypeLimited, TokenLinkedToken,
    TokenPrimary, TokenSessionId, SECURITY_ATTRIBUTES, TOKEN_ALL_ACCESS, TOKEN_DUPLICATE,
    TOKEN_ELEVATION_TYPE, TOKEN_LINKED_TOKEN, TOKEN_QUERY,
};
use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows::Win32::System::RemoteDesktop::{
    WTSFreeMemory, WTSGetActiveConsoleSessionId, WTSQuerySessionInformationW, WTSQueryUserToken,
    WTSSessionInfoEx, WTSINFOEXW, WTS_SESSIONSTATE_UNLOCK,
};
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
    CreateEventW, CreateMutexW, CreateProcessAsUserW, GetCurrentProcess, OpenEventW,
    OpenProcessToken, ResetEvent, SetEvent, WaitForMultipleObjects, WaitForSingleObject,
    CREATE_UNICODE_ENVIRONMENT, EVENT_MODIFY_STATE, INFINITE, NORMAL_PRIORITY_CLASS,
    PROCESS_INFORMATION, STARTUPINFOW,
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

// ── Pre-login SYSTEM-fallback marker ───────────────────────────────────────────
//
// `WTSQueryUserToken` only returns a token for a session with an authenticated
// user — a bare logon screen (nobody has typed credentials yet, e.g. right
// after boot or a full sign-out) has none at all, so the normal "duplicate the
// user's elevated token" spawn is impossible. `spawn_host_as_system_fallback`
// spawns the host directly under the service's own SYSTEM-in-session token
// instead — degraded (no WGC/HDR/full audio, same as the existing
// `--system-token` impersonation case) but enough to show the logon screen via
// DDA and accept remote input (see `input.rs`'s secure-desktop guard) before
// anyone has signed in. `--system-fallback` marks the host so it can skip
// diagnostics that assume a normal interactive-user identity.

static RUNNING_AS_SYSTEM_FALLBACK: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// (Host side) Record that this process IS the SYSTEM-in-session identity
/// itself (not an elevated user with a separate impersonation token).
pub fn set_running_as_system_fallback() {
    RUNNING_AS_SYSTEM_FALLBACK.store(true, Ordering::Release);
}

/// (Host side) True when this host was spawned as the pre-login SYSTEM
/// fallback (see module note above) rather than as the interactive user.
pub fn is_system_fallback() -> bool {
    RUNNING_AS_SYSTEM_FALLBACK.load(Ordering::Acquire)
}

// ── Once-per-boot VDD devnode cycle ────────────────────────────────────────────
//
// `ensure_enabled_at_boot` (virtual_display.rs) briefly enables the VDD
// devnode at host startup to refresh its mode table — a real, visible
// topology change (the physical monitor gets displaced and restored). That
// was fine when one boot spawned exactly one host. The SYSTEM-fallback
// upgrade path means a single boot/sign-out can now spawn TWO hosts in
// well under a second (fallback, then the interactive upgrade once a real
// user token appears) — confirmed live (2026-07-20): reported as "the
// monitor is being hijacked on install", which was actually the SECOND
// devnode cycle from the upgrade respawn, not anything install-specific.
// This service-lifetime flag makes only the FIRST host spawned per service
// run do the cycle; every subsequent spawn (upgrade, session change, crash
// respawn) is told to skip it via `--skip-vdd-cycle`.

static VDD_CYCLED_THIS_BOOT: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// (Service side) True for the first caller this service run, false for
/// every subsequent one. Call once per host spawn attempt.
fn claim_vdd_cycle_slot() -> bool {
    !VDD_CYCLED_THIS_BOOT.swap(true, Ordering::AcqRel)
}

/// Set by the host's `--skip-vdd-cycle` arg handler.
static SKIP_VDD_CYCLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// (Host side) Record that a sibling host already did this boot's VDD
/// devnode refresh cycle.
pub fn set_skip_vdd_cycle() {
    SKIP_VDD_CYCLE.store(true, Ordering::Release);
}

/// (Host side) True when `ensure_enabled_at_boot` should skip the visible
/// devnode enable/disable cycle — a sibling host already did it this boot.
pub fn should_skip_vdd_cycle() -> bool {
    SKIP_VDD_CYCLE.load(Ordering::Acquire)
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

// ── Session-Survival Architecture rollout switch (approved plan,
//    transient-snuggling-cosmos.md) ─────────────────────────────────────────
//
// Phase 2 (2026-07-20): flipped to `true` for live end-to-end testing of the
// full Master/Worker split (RTSP/control/pairing/mDNS/RtpSender in this
// --service process; VDD/capture/NVENC/WASAPI/input in a spawned `--worker`
// process, driven by IPC). The monolithic `run()` path is untouched and
// still fully intact — flip this back to `false` and rebuild to fall back to
// it immediately if the split doesn't hold up under a real test pass.
//
// Known Phase 2 scope gaps, not yet built (tracked, not silently missing):
//   - ~~New-device pairing~~ — CLOSED 2026-08-06: pairing's PIN dialog
//     request now travels Master→Worker as ControlMsg::OpenPairDialog and
//     the entered PIN returns as ControlMsg::PinRelay into pairing's
//     global_pin (see start_master_network's pair-dialog forwarder).
//   - No pre-activation latency hiding (every session pays full VDD-
//     activation latency at PLAY time) — see session_watcher's doc comment.
//   - One ConfigureStart per Worker process lifetime — no in-place
//     suspend/resume; a session ending currently has no explicit "tear down
//     and wait for the next one" signal, so a fresh session after this one
//     ends will need a freshly-spawned Worker (Phase 4 scope).
//   - Audio stays fully in-Worker on its own UDP socket (unchanged from the
//     monolithic path) — MediaMsg::AudioFrame is defined but unused; the
//     audio IPC extraction is Phase 3.
const WORKER_SPLIT_ENABLED: bool = true;

// ── Worker: keep one host alive in the active console session ─────────────────

/// One managed host process, tagged with the console session it belongs to.
struct HostProcess {
    process: HANDLE,
    session_id: u32,
    /// True when this host was spawned via [`spawn_host_as_system_fallback`]
    /// (no user token was available yet) rather than as the interactive user.
    is_system_fallback: bool,
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

    // Session-Survival Architecture (Phase 1, WORKER_SPLIT_ENABLED): async
    // IPC pipe creation/accept needs a tokio runtime, but service_worker
    // itself stays a plain synchronous reconcile loop (the SCM control
    // handler and this worker loop run on separate threads regardless — see
    // service_main — so there's no async/sync conflict to bridge beyond this
    // one runtime). Only built when the switch is on, so the unmodified path
    // pays nothing for it.
    let ipc_runtime = if WORKER_SPLIT_ENABLED {
        Some(tokio::runtime::Runtime::new().expect("tokio runtime for worker-split IPC"))
    } else {
        None
    };
    // Master's network stack (RTSP/control/pairing/mDNS/RtpSender + the
    // control/media/session-watcher supervisor tasks) starts ONCE, here —
    // not per-worker-spawn. It runs on ipc_runtime's own worker threads for
    // the rest of this function's (i.e. the service's) lifetime.
    let master: Option<crate::MasterHandles> = ipc_runtime
        .as_ref()
        .map(|rt| rt.block_on(crate::start_master_network()));

    // Crash-loop damper. A host that dies within HEALTHY_UPTIME of its spawn
    // is treated as crash-looping and each successive respawn is delayed
    // (4 s, 8 s, 16 s, … capped at 60 s). Without this, a host that crashes
    // during startup gets respawned every reconcile tick — and because every
    // host start cycles the VDD devnode, a pre-login crash loop played the
    // Windows device connect/disconnect chime endlessly (the "boot loop"
    // sound). A host that stays up past HEALTHY_UPTIME resets the damper.
    const HEALTHY_UPTIME: std::time::Duration = std::time::Duration::from_secs(30);
    // A real user token can appear within a second of the fallback host
    // starting (confirmed live 2026-07-20: token available ~0.5s after
    // spawn). Upgrading immediately tore down the host mid RTSP-handshake
    // for a client that had just started connecting to the lock screen —
    // reported as "failed to start streaming at the login screen when
    // connecting". Require the fallback host to have been up this long
    // before it's upgrade-eligible, so an in-flight connection attempt (or
    // someone mid-PIN-entry over remote input) has room to land first.
    const MIN_FALLBACK_UPTIME_BEFORE_UPGRADE: std::time::Duration = std::time::Duration::from_secs(5);
    let mut last_spawn: Option<std::time::Instant> = None;
    let mut fast_exits: u32 = 0;
    let mut respawn_not_before: Option<std::time::Instant> = None;
    // Suppress the fallback→interactive UPGRADE while the interactive spawn is
    // demonstrably failing. At a LOCKED session a user token exists
    // (session_has_user_token == true) but CreateProcessAsUserW into the locked
    // console can't produce a working interactive host, so the spawn falls back
    // to SYSTEM again. Without this guard the upgrade fired every
    // MIN_FALLBACK_UPTIME_BEFORE_UPGRADE seconds, and each attempt killed the
    // running SYSTEM-fallback host — dropping the remote client that was
    // connected to the lock screen (live 2026-08-09: ~23 rapid respawns, the
    // client "kicked" every few seconds and unable to reach the PIN screen).
    // We re-test at most once per this interval; between tests the lock-screen
    // host stays STABLE so the client can connect and type its PIN. A real
    // sign-in makes the interactive spawn succeed (not a fallback), which never
    // arms this — so the happy path is unchanged.
    const INTERACTIVE_RETRY_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(45);
    let mut interactive_retry_not_before: Option<std::time::Instant> = None;
    // True while the spawn on this tick was triggered by the fallback→
    // interactive upgrade branch. If that spawn lands back on a SYSTEM
    // fallback, the cooldown must arm UNCONDITIONALLY — re-probing the token
    // at spawn-result time (the old behavior) raced the OS's own login-state
    // transitions and could skip arming, letting the upgrade thrash every
    // MIN_FALLBACK_UPTIME seconds (confirmed live 2026-08-10).
    let mut upgrade_attempted = false;
    // Set when the interactive spawn failed with ERROR_ELEVATION_REQUIRED: the
    // signed-in user is a STANDARD (non-admin) account, so there is no linked
    // elevated token and `nova-server.exe` (requireAdministrator) can never be
    // launched as them. Retrying is futile AND destructive — every attempt
    // stops the running fallback host first, killing the client's stream
    // (confirmed live 2026-08-11: 309 such cycles on a standard-user sign-in,
    // reported as "signing in loads a dead screen"). Suppressed until the
    // console session changes, i.e. until somebody else signs in.
    let mut interactive_impossible_for_session: Option<u32> = None;

    loop {
        // Reconcile: ensure a host is running in the CURRENT console session.
        let session = unsafe { WTSGetActiveConsoleSessionId() };
        if session != NO_SESSION {
            let need_spawn = match &host {
                None => true,
                Some(h) if h.has_exited() => {
                    let _ = unsafe { CloseHandle(h.process) };
                    match last_spawn {
                        Some(t) if t.elapsed() < HEALTHY_UPTIME => {
                            fast_exits += 1;
                            let delay_s = (2u64 << fast_exits.min(5)).min(60);
                            respawn_not_before = Some(
                                std::time::Instant::now()
                                    + std::time::Duration::from_secs(delay_s),
                            );
                            println!(
                                "⚠️  Host exited {:.1}s after spawn (fast exit #{fast_exits}) — \
                                 backing off {delay_s}s before respawn",
                                t.elapsed().as_secs_f32()
                            );
                        }
                        _ => {
                            fast_exits = 0;
                            respawn_not_before = None;
                        }
                    }
                    true
                }
                Some(h) if h.session_id != session => {
                    // Console moved to another session (fast user switch / RDP).
                    // Graceful first: the old session's host restores its VDD
                    // topology and audio endpoint before the new session's host
                    // starts from a clean slate.
                    println!("🔄 Console session {} → {} — respawning host", h.session_id, session);
                    stop_host(h);
                    // A session change is a fresh environment — don't carry a
                    // crash-loop backoff or upgrade-suppression into it. The
                    // non-admin verdict is per-session and keyed by id, so it
                    // needs no explicit reset here.
                    fast_exits = 0;
                    respawn_not_before = None;
                    interactive_retry_not_before = None;
                    true
                }
                Some(h) if h.is_system_fallback
                    && last_spawn.is_some_and(|t| t.elapsed() >= MIN_FALLBACK_UPTIME_BEFORE_UPGRADE)
                    && session_has_user_token(session)
                    // The load-bearing gate: only upgrade when the session is
                    // genuinely UNLOCKED. A token alone also exists at an
                    // ARSO-locked lock screen (overnight-update reboot), where
                    // the interactive spawn can't produce a usable host — the
                    // old token-only gate killed a healthy lock-screen host on
                    // every cooldown expiry (confirmed live 2026-08-10).
                    && session_is_unlocked(session)
                    // A standard (non-admin) user can never host the elevated
                    // Worker — see interactive_impossible_for_session.
                    && interactive_impossible_for_session != Some(session)
                    // Don't re-attempt the upgrade while the interactive spawn
                    // is known to be failing — see the cooldown declaration.
                    && interactive_retry_not_before.is_none_or(|t| std::time::Instant::now() >= t) =>
                {
                    // Someone finished signing in (token + UNLOCK) — replace
                    // the degraded SYSTEM-fallback host (DDA-only, no WGC/HDR/
                    // VDD) with a proper interactive-user one. Graceful stop
                    // first, same as a session change: the fallback host hands
                    // back its VDD/audio state before the upgraded host starts.
                    //
                    // Deliberately NOT gated on "no client connected" anymore:
                    // this handoff IS how a client that just signed in over
                    // Moonlight gets its full-quality stream back. Master keeps
                    // the client's RTSP/control session alive across the swap,
                    // replays ConfigureStart to the new Worker, and stamps
                    // start_frame_index so the client's frame timeline
                    // continues — a ~2-5 s freeze, not a disconnect. (The old
                    // client-connected event gate was blind in the split
                    // deployment anyway: only the monolithic path ever set it.)
                    println!("🔑 Session {session} is signed in + unlocked — \
                        upgrading from SYSTEM fallback to interactive host");
                    upgrade_attempted = true;
                    stop_host(h);
                    fast_exits = 0;
                    respawn_not_before = None;
                    true
                }
                Some(_) => false,
            };
            let backoff_active = respawn_not_before
                .is_some_and(|t| std::time::Instant::now() < t);
            if need_spawn {
                // Drop the dead entry immediately — its handle is already
                // closed; keeping it would re-wait/re-close a stale HANDLE on
                // the next tick while a backoff delays the respawn.
                host = None;
            }
            if need_spawn && !backoff_active {
                // No user token yet (bare logon screen) falls back to spawning
                // under the service's own SYSTEM-in-session token instead of
                // just retrying forever — see spawn_host_as_system_fallback's
                // doc comment. A real privilege problem (not "no token yet")
                // fails both attempts and surfaces the combined error below.
                let result = if let (Some(rt), Some(m)) = (ipc_runtime.as_ref(), master.as_ref()) {
                    // A panic anywhere in the IPC spawn path (named-pipe/OS-token
                    // FFI-heavy code) must not take this reconcile loop's thread
                    // down with it — that previously meant "no host ever spawns
                    // again for the rest of the service's life, no crash-loop
                    // damper catches it, silent" (confirmed live: the reactor-
                    // context panic below did exactly this before the fix).
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        spawn_host_with_ipc(&exe, session, rt, m)
                    }))
                    .unwrap_or_else(|_| Err("spawn_host_with_ipc panicked".to_string()))
                } else {
                    spawn_host_in_session(&exe, session, false).or_else(|user_err| {
                        println!("   ↳ interactive spawn failed ({user_err}) — trying SYSTEM fallback");
                        spawn_host_as_system_fallback(&exe, session, false)
                            .map_err(|sys_err| format!("{user_err}; SYSTEM-fallback also failed: {sys_err}"))
                    })
                };
                match result {
                    Ok(h) => {
                        let mode = if h.is_system_fallback {
                            " as SYSTEM (pre-login fallback — no user signed in yet)"
                        } else {
                            ""
                        };
                        println!("✅ Host spawned in session {session}{mode} (pid handle live)");
                        // Arm/clear the upgrade cooldown. A SYSTEM fallback
                        // landing where an interactive host was expected —
                        // after an explicit upgrade attempt, or with a user
                        // token present — means the interactive spawn is
                        // failing; don't let the upgrade thrash, re-test only
                        // after the cooldown. (`upgrade_attempted` matters:
                        // re-probing the token here raced login-state
                        // transitions and skipping the arm re-enabled the
                        // 5-second thrash loop.) A real interactive host means
                        // the session is usable — clear any suppression.
                        interactive_retry_not_before = if h.is_system_fallback
                            && (upgrade_attempted || session_has_user_token(session))
                        {
                            if INTERACTIVE_NEEDS_ELEVATION.load(Ordering::Acquire) {
                                // Permanent for this session — retrying would
                                // just kill this host again every cooldown.
                                if interactive_impossible_for_session != Some(session) {
                                    interactive_impossible_for_session = Some(session);
                                    println!("   ↳ the user signed into session {session} is NOT an \
                                        administrator, so the elevated Worker cannot be started as \
                                        them. Staying on the SYSTEM host (it can still capture and \
                                        drive the virtual display); no further upgrade attempts for \
                                        this session.");
                                }
                                None
                            } else {
                                println!("   ↳ interactive spawn unavailable (session locked?) — \
                                    holding this lock-screen host stable for {}s before re-testing",
                                    INTERACTIVE_RETRY_COOLDOWN.as_secs());
                                Some(std::time::Instant::now() + INTERACTIVE_RETRY_COOLDOWN)
                            }
                        } else {
                            None
                        };
                        host = Some(h);
                        last_spawn = Some(std::time::Instant::now());
                    }
                    Err(e) => {
                        // Transient at boot (no shell yet) or a real privilege
                        // problem — either way keep the service alive and retry.
                        println!("⚠️  Host spawn into session {session} failed: {e}");
                        // A failed upgrade spawn (no host at all came up) must
                        // also arm the cooldown, or the next tick re-kills
                        // whatever respawns and tries again immediately.
                        if upgrade_attempted {
                            interactive_retry_not_before =
                                Some(std::time::Instant::now() + INTERACTIVE_RETRY_COOLDOWN);
                        }
                    }
                }
                upgrade_attempted = false;
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

    // Service is stopping — from the tray "Quit" (host already exiting), an
    // installer upgrade's `sc stop`, a manual stop, or OS shutdown. stop_host
    // signals the host's named shutdown event so its graceful teardown
    // (display + audio restore) runs even when the host did NOT initiate the
    // stop, then grace-waits before the TerminateProcess backstop.
    if let Some(h) = host.take() {
        stop_host(&h);
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

// ── Cross-process graceful stop (service → host) ─────────────────────────────
//
// TerminateProcess skips every Rust destructor and console handler in the
// host, so a service-initiated stop (installer upgrade `sc stop`, manual stop,
// session change) used to leave the VDD headless topology and the virtual
// audio sink engaged until the next boot's healing pass. The fix is a named
// kernel event: the HOST creates and waits on it; the SERVICE signals it and
// only falls back to TerminateProcess after the grace period. This funnels a
// service stop into the exact same graceful teardown path as the tray "Quit".

/// Named manual-reset event bridging service (Session 0, SYSTEM) and host
/// (Session 1+, elevated user). `Global\` namespace so it crosses sessions;
/// the elevated host has `SeCreateGlobalPrivilege`, and SYSTEM can open
/// anything the host creates with a default security descriptor.
const HOST_SHUTDOWN_EVENT: &str = "Global\\NovaHostShutdown";

/// (Host side) Create the named shutdown event and watch it from a background
/// thread; `on_signal` runs once when the service asks this host to exit.
///
/// Manual-reset + reset-on-create: if a previous host crashed without
/// consuming a signal, the stale set state would instantly kill the next
/// host — so every new host clears the event before waiting. (Manual-reset
/// rather than auto-reset so the signal is level-triggered: a host mid-wait
/// always sees it even if a new host resets the event moments later.)
pub fn spawn_host_shutdown_watcher(on_signal: impl FnOnce() + Send + 'static) {
    let _ = std::thread::Builder::new()
        .name("nova-host-shutdown".into())
        .spawn(move || unsafe {
            let name_w = wide(HOST_SHUTDOWN_EVENT);
            let event = match CreateEventW(None, true, false, PCWSTR(name_w.as_ptr())) {
                Ok(h) => h,
                Err(e) => {
                    println!("⚠️  Host shutdown event unavailable ({e:?}) — a service STOP \
                        will fall back to TerminateProcess (display/audio restored at next boot)");
                    return;
                }
            };
            let _ = ResetEvent(event);
            let _ = WaitForSingleObject(event, INFINITE);
            println!("🛑 Service requested shutdown — starting graceful teardown");
            on_signal();
            let _ = CloseHandle(event);
        });
}

/// (Service side) Signal the host's named shutdown event. `false` when the
/// event doesn't exist (no host running, or a pre-15.5 host build) — callers
/// then rely on the TerminateProcess backstop alone.
fn signal_host_shutdown() -> bool {
    unsafe {
        let name_w = wide(HOST_SHUTDOWN_EVENT);
        match OpenEventW(EVENT_MODIFY_STATE, false, PCWSTR(name_w.as_ptr())) {
            Ok(ev) => {
                let ok = SetEvent(ev).is_ok();
                let _ = CloseHandle(ev);
                ok
            }
            Err(_) => false,
        }
    }
}

// ── Client-connected signal (host → service) ──────────────────────────────────
//
// Historical: the upgrade gate used to defer the fallback→interactive swap
// while this event was set, so a client watching the lock screen wasn't cut
// off mid-stream. Two problems killed that design (2026-08-10): the split
// deployment's Master never set the event (only the monolithic path did — the
// gate was permanently open), and deferring the upgrade is the OPPOSITE of
// what a just-signed-in client needs — the swap is precisely how it gets its
// full-quality (WGC/HDR/VDD) stream back, survivable now that Master replays
// ConfigureStart with a continued frame timeline. The upgrade gate now keys
// on session UNLOCK instead (see `session_is_unlocked`); the event remains
// only as a monolithic-path breadcrumb with no service-side reader.

const CLIENT_CONNECTED_EVENT: &str = "Global\\NovaClientConnected";

/// (Host side, monolithic path) Record whether a Moonlight client is
/// currently connected. No service-side consumer since the UNLOCK-keyed
/// upgrade gate — kept as a cheap externally-observable signal (e.g. for
/// diagnosing with `handle.exe`), and so the monolithic path stays unchanged.
pub fn set_client_connected(connected: bool) {
    unsafe {
        let name_w = wide(CLIENT_CONNECTED_EVENT);
        if let Ok(ev) = CreateEventW(None, true, false, PCWSTR(name_w.as_ptr())) {
            if connected {
                let _ = SetEvent(ev);
            } else {
                let _ = ResetEvent(ev);
            }
            let _ = CloseHandle(ev);
        }
    }
}

/// Stop a managed host with dignity: signal the named shutdown event so the
/// host runs its graceful display/audio teardown, give it
/// `HOST_GRACEFUL_EXIT_MS` to exit on its own, then force-terminate as the
/// backstop (a no-op if it already exited; also closes the handle).
fn stop_host(h: &HostProcess) {
    if signal_host_shutdown() {
        println!("🤝 Host (session {}) asked to exit gracefully", h.session_id);
    }
    unsafe {
        let _ = WaitForSingleObject(h.process, HOST_GRACEFUL_EXIT_MS);
    }
    h.terminate();
}

// ── Machine-wide single-instance guard (host side) ────────────────────────────
//
// The scheduled-task fallback, a manual launch, and the NovaService deployment
// must never run two hosts at once: the second instance would cycle the VDD
// devnode, fight over the GameStream ports, and duel for the WGC session.
// Ports failing to bind used to be the only (crashy) backstop; the named mutex
// makes the second instance exit cleanly before touching ANY system state.

const HOST_SINGLETON_MUTEX: &str = "Global\\NovaServerHostSingleton";

/// Held for the host's entire lifetime; released automatically at process
/// exit (kernel closes the handle), letting the next host start.
pub struct HostSingleton(#[allow(dead_code)] Option<HandleGuard>);

/// (Host side) Claim the machine-wide host mutex.
///
/// `Err(reason)` = another Nova host already holds it — the caller must exit
/// WITHOUT touching system state (VDD devnode, ports, audio sink). Failing to
/// CREATE the mutex at all (e.g. unelevated: no `SeCreateGlobalPrivilege` for
/// the `Global\` namespace) is non-fatal: proceed unguarded with a warning —
/// the port-bind conflicts still backstop a true double-launch.
pub fn acquire_host_singleton() -> Result<HostSingleton, String> {
    unsafe {
        let name_w = wide(HOST_SINGLETON_MUTEX);
        match CreateMutexW(None, false, PCWSTR(name_w.as_ptr())) {
            Ok(h) => {
                if GetLastError() == ERROR_ALREADY_EXISTS {
                    let _ = CloseHandle(h);
                    Err(format!(
                        "Another Nova host is already running ({HOST_SINGLETON_MUTEX} held) \
                         — exiting; the existing instance keeps serving"
                    ))
                } else {
                    Ok(HostSingleton(Some(HandleGuard(h))))
                }
            }
            Err(e) => {
                println!("⚠️  Could not create single-instance mutex ({e:?}) — continuing unguarded");
                Ok(HostSingleton(None))
            }
        }
    }
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
/// Set by [`spawn_host_in_session`] when `CreateProcessAsUserW` reports
/// ERROR_ELEVATION_REQUIRED — i.e. the console user is a standard account with
/// no admin rights to derive an elevated token from. Read (and cleared) by the
/// reconcile loop, which then stops attempting the upgrade for that session.
static INTERACTIVE_NEEDS_ELEVATION: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn spawn_host_in_session(exe: &str, session_id: u32, worker_mode: bool) -> Result<HostProcess, String> {
    INTERACTIVE_NEEDS_ELEVATION.store(false, Ordering::Release);
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

        // 5. Command line: exe (quoted) + the inherited SYSTEM token handle,
        //    + --skip-vdd-cycle for every spawn after the first this service
        //    run (see claim_vdd_cycle_slot's doc comment), + --worker when
        //    spawning under the Session-Survival Architecture (see
        //    WORKER_SPLIT_ENABLED's doc comment — off by default). The
        //    working dir is the exe folder (a LocalSystem service's CWD is
        //    System32; the host resolves paths from current_exe() but this
        //    removes any relative-path ambiguity).
        let skip_cycle = if claim_vdd_cycle_slot() { "" } else { " --skip-vdd-cycle" };
        let worker_flag = if worker_mode { " --worker" } else { "" };
        let cmdline = match sys_token {
            Some(h) => format!("\"{exe}\" --system-token {}{skip_cycle}{worker_flag}", h.0 as isize),
            None => format!("\"{exe}\"{skip_cycle}{worker_flag}"),
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
            } else if e.code() == windows::core::HRESULT::from_win32(ERROR_ELEVATION_REQUIRED.0) {
                // The signed-in user is a STANDARD account: no linked elevated
                // token exists, so a requireAdministrator binary cannot be
                // launched as them at all. Flagged so the reconcile loop stops
                // retrying — see interactive_impossible_for_session.
                INTERACTIVE_NEEDS_ELEVATION.store(true, Ordering::Release);
                format!("CreateProcessAsUserW: {e:?} — the signed-in user is not an administrator")
            } else {
                format!("CreateProcessAsUserW: {e:?}")
            }
        })?;

        // We manage the process; the thread handle is not needed.
        let _ = CloseHandle(pi.hThread);
        Ok(HostProcess {
            process: pi.hProcess,
            session_id,
            is_system_fallback: false,
        })
    }
}

/// Cheap probe used only while a SYSTEM-fallback host is running: does
/// `session_id` now have an authenticated user (i.e. has login completed)?
/// Opens and immediately closes the token — this is a existence check, not a
/// spawn.
fn session_has_user_token(session_id: u32) -> bool {
    unsafe {
        let mut tok = HANDLE::default();
        match WTSQueryUserToken(session_id, &mut tok) {
            Ok(()) => {
                let _ = CloseHandle(tok);
                true
            }
            Err(_) => false,
        }
    }
}

/// True when `session_id` is genuinely UNLOCKED (a user is signed in AND at
/// their desktop), per `WTSQuerySessionInformationW(WTSSessionInfoEx)`.
///
/// Why this exists (confirmed live 2026-08-10): "a user token exists" is NOT
/// the same as "a user is signed in and usable". Windows 11's automatic
/// restart sign-on (ARSO — overnight update reboots) signs the user in and
/// then LOCKS the session, so `session_has_user_token` reads true at what
/// looks like a plain lock screen. The upgrade gate keyed on the token alone
/// therefore killed a healthy lock-screen fallback host on every cooldown
/// expiry (45 s), all morning, kicking any connected client with it. LOCK vs
/// UNLOCK is the signal that actually distinguishes "mid-PIN-entry /parked at
/// the lock screen" from "signed in for real".
///
/// Fail-open (`true`) when the query itself errors: the 45 s retry cooldown
/// still bounds any resulting thrash, which is the pre-existing behavior.
fn session_is_unlocked(session_id: u32) -> bool {
    unsafe {
        let mut buf = windows::core::PWSTR::null();
        let mut len = 0u32;
        match WTSQuerySessionInformationW(
            HANDLE::default(), // WTS_CURRENT_SERVER_HANDLE
            session_id,
            WTSSessionInfoEx,
            &mut buf,
            &mut len,
        ) {
            Ok(()) => {
                let mut unlocked = true;
                if !buf.is_null() && len as usize >= std::mem::size_of::<WTSINFOEXW>() {
                    let info = &*(buf.as_ptr() as *const WTSINFOEXW);
                    if info.Level == 1 {
                        // Win8+ semantics: LOCK = 0, UNLOCK = 1 (the swapped
                        // values only afflicted Win7/2008R2 — not a target).
                        let flags = info.Data.WTSInfoExLevel1.SessionFlags;
                        unlocked = flags == WTS_SESSIONSTATE_UNLOCK as i32;
                    }
                }
                WTSFreeMemory(buf.as_ptr() as *mut c_void);
                unlocked
            }
            Err(e) => {
                println!("⚠️  WTSSessionInfoEx query for session {session_id} failed ({e:?}) — \
                    treating as unlocked");
                true
            }
        }
    }
}

/// Pre-login fallback for [`spawn_host_in_session`]: spawns the host directly
/// under a SYSTEM-in-session primary token (the same token
/// [`create_inheritable_system_token`] builds for the impersonation handoff,
/// used here AS the process token instead) when `WTSQueryUserToken` has
/// nothing to duplicate — a bare logon screen with no one signed in yet.
///
/// Degraded relative to the normal path: WGC fails under SYSTEM (falls back
/// to DDA, same as the existing secure-desktop case — see `capture/dda.rs`
/// and lib.rs's `DesktopManager::new_wgc` DDA-first startup fallback), and
/// there is no real user profile so HDR/audio/env are best-effort. Good
/// enough to show the logon screen over DDA and accept remote keyboard/mouse
/// (`input.rs`) so a user can authenticate from the Moonlight client itself.
/// The reconcile loop upgrades to a full interactive-user host the moment
/// [`session_has_user_token`] reports a real login has completed.
fn spawn_host_as_system_fallback(exe: &str, session_id: u32, worker_mode: bool) -> Result<HostProcess, String> {
    unsafe {
        let sys_token = create_inheritable_system_token(session_id)
            .ok_or_else(|| "could not build a SYSTEM-in-session token".to_string())?;
        let _token_guard = HandleGuard(sys_token);

        let mut env: *mut c_void = std::ptr::null_mut();
        let have_env = CreateEnvironmentBlock(&mut env, sys_token, false).is_ok();

        let skip_cycle = if claim_vdd_cycle_slot() { "" } else { " --skip-vdd-cycle" };
        let worker_flag = if worker_mode { " --worker" } else { "" };
        let cmdline = format!("\"{exe}\" --system-fallback{skip_cycle}{worker_flag}");
        let mut cmdline_w: Vec<u16> = cmdline.encode_utf16().chain(std::iter::once(0)).collect();
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
            sys_token,
            PCWSTR::null(),
            PWSTR(cmdline_w.as_mut_ptr()),
            None,
            None,
            false, // nothing to inherit — no separate impersonation handle this time
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
        let _ = &mut si;

        result.map_err(|e| format!("CreateProcessAsUserW(system-fallback): {e:?}"))?;

        let _ = CloseHandle(pi.hThread);
        Ok(HostProcess {
            process: pi.hProcess,
            session_id,
            is_system_fallback: true,
        })
    }
}

/// A running `--system-input-helper` process: SYSTEM primary token, console
/// session. Exists only while the secure desktop is up.
pub struct InputHelper {
    process: HANDLE,
}

// A process HANDLE is a process-wide kernel handle value, not thread-affine
// state — moving ownership between threads is safe, and the only operation
// here (`terminate`, which consumes self) can therefore run from wherever the
// value ended up. Needed because `lib.rs`'s `input_helper_supervisor` holds an
// `InputHelper` across `.await` points inside a `tokio::spawn`ed task.
unsafe impl Send for InputHelper {}

impl InputHelper {
    /// Kill the helper and release its handle. Safe to call unconditionally —
    /// the helper owns no display/audio/VDD state, so there is nothing to
    /// unwind gracefully (unlike the host, which is why `stop_host` has a
    /// whole grace-period dance and this does not).
    pub fn terminate(self) {
        unsafe {
            let _ = windows::Win32::System::Threading::TerminateProcess(self.process, 0);
            let _ = CloseHandle(self.process);
        }
    }
}

/// Spawn `nova-server.exe --system-input-helper` as **SYSTEM with a SYSTEM
/// PRIMARY token, inside the active console session**.
///
/// This exists for one reason: UIPI evaluates the injecting process's PRIMARY
/// token, not a thread's impersonation token, when deciding whether injected
/// input may reach the Winlogon/credential-provider desktop. The Worker's
/// primary token is the interactive (High-integrity) user, so its `SendInput`
/// is accepted at the API boundary and then silently discarded — see
/// `input.rs`'s UIPI note. A process whose primary token is SYSTEM and which
/// lives in the console session clears that bar, and it is exactly how
/// Sunshine gets PIN-screen input (its host process runs SYSTEM-in-session).
///
/// Nova cannot simply run the whole Worker that way: host-as-SYSTEM breaks WGC
/// (`0x80070424` — WinRT's capture broker needs a real interactive user;
/// confirmed live 2026-07-10, see CLAUDE.md Phase 15.2c). Hence a *separate*,
/// minimal helper — WGC/HDR/audio stay with the interactive Worker, and only
/// the ~200 bytes/packet injection path moves to SYSTEM for the duration of
/// the secure-desktop interlude.
pub fn spawn_input_helper() -> Result<InputHelper, String> {
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .map_err(|e| format!("current_exe: {e}"))?;
    let session_id = unsafe { WTSGetActiveConsoleSessionId() };
    if session_id == NO_SESSION {
        return Err("no active console session".to_string());
    }

    unsafe {
        let sys_token = create_inheritable_system_token(session_id)
            .ok_or_else(|| "could not build a SYSTEM-in-session token".to_string())?;
        let _token_guard = HandleGuard(sys_token);

        let mut env: *mut c_void = std::ptr::null_mut();
        let have_env = CreateEnvironmentBlock(&mut env, sys_token, false).is_ok();

        let cmdline = format!("\"{exe}\" --system-input-helper");
        let mut cmdline_w: Vec<u16> = cmdline.encode_utf16().chain(std::iter::once(0)).collect();
        let workdir_w = std::path::Path::new(&exe)
            .parent()
            .map(|d| wide(&d.to_string_lossy()));
        // Start on Default and let the injection thread attach to whatever
        // desktop currently has input focus (input.rs's always-follow mode) —
        // Sunshine's `syncThreadDesktop` model. Naming Winlogon here instead
        // would bind the process to the specific secure desktop live at spawn
        // time, which is exactly the stale-desktop-object failure the
        // reactive re-attach exists to survive.
        let mut desktop_w: Vec<u16> = "WinSta0\\Default".encode_utf16().chain(std::iter::once(0)).collect();
        let mut si = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            lpDesktop: PWSTR(desktop_w.as_mut_ptr()),
            ..Default::default()
        };
        let mut pi = PROCESS_INFORMATION::default();

        let result = CreateProcessAsUserW(
            sys_token,
            PCWSTR::null(),
            PWSTR(cmdline_w.as_mut_ptr()),
            None,
            None,
            false,
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
        let _ = &mut si;

        result.map_err(|e| format!("CreateProcessAsUserW(system-input-helper): {e:?}"))?;
        let _ = CloseHandle(pi.hThread);
        Ok(InputHelper { process: pi.hProcess })
    }
}

/// Session-Survival Architecture (Phase 2, only reachable when
/// `WORKER_SPLIT_ENABLED`): create Master's IPC pipes, spawn a `--worker`
/// process into `session_id` (same token machinery as the normal path, just
/// with the extra flag), and block until it connects to both pipes and sends
/// `ControlMsg::WorkerReady`. On any failure the half-spawned Worker (if any)
/// is terminated rather than left running with no working IPC — a Worker
/// that can't reach Master is useless and would otherwise linger as an
/// unmanaged zombie process.
///
/// On success, hands the now-verified-live pipes to `handles` — Master's
/// long-lived `control_supervisor`/`media_supervisor` tasks (spawned once by
/// `crate::start_master_network`) take over all ONGOING traffic; this
/// function's own accept+handshake is a one-shot liveness check, not the
/// pipes' permanent owner.
fn spawn_host_with_ipc(
    exe: &str,
    session_id: u32,
    rt: &tokio::runtime::Runtime,
    handles: &crate::MasterHandles,
) -> Result<HostProcess, String> {
    // create_master_pipes() constructs tokio::net::windows::named_pipe types,
    // which register with the tokio reactor at creation time and therefore
    // panic ("there is no reactor running") unless called from inside this
    // runtime's context. This function's caller (service_worker's reconcile
    // loop) runs on a plain sync thread, not inside `rt.block_on`, so the
    // call must be wrapped in `rt.enter()` explicitly rather than relying on
    // the later `rt.block_on` below.
    let (mut control, media) = {
        let _guard = rt.enter();
        crate::ipc::create_master_pipes().map_err(|e| format!("create_master_pipes: {e}"))?
    };

    let h = spawn_host_in_session(exe, session_id, true).or_else(|user_err| {
        // Say WHY the interactive spawn failed before falling back — this was
        // silent (the error only surfaced if the fallback ALSO failed), which
        // made every "spawned as SYSTEM when a user looked signed-in" episode
        // undiagnosable from the logs (2026-08-10).
        println!("   ↳ interactive spawn failed ({user_err}) — trying SYSTEM fallback");
        spawn_host_as_system_fallback(exe, session_id, true)
            .map_err(|sys_err| format!("{user_err}; SYSTEM-fallback also failed: {sys_err}"))
    })?;

    let accept_result = rt.block_on(async {
        crate::ipc::accept_worker(&control, &media).await?;
        crate::ipc::recv_control(&mut control).await
    });

    match accept_result {
        Ok(crate::ipc::ControlMsg::WorkerReady) => {
            handles.adopt_worker_pipes(control, media);
            Ok(h)
        }
        Ok(other) => {
            h.terminate();
            Err(format!("worker's first IPC message was {other:?}, expected WorkerReady"))
        }
        Err(e) => {
            h.terminate();
            Err(format!("worker did not complete the IPC handshake: {e}"))
        }
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

// Silent background app — no console window on launch.
// --install / --uninstall allocate a temporary console for user feedback.
#![windows_subsystem = "windows"]

// Windows service plumbing — define_windows_service! must live in the binary
// crate (it generates an extern "system" fn entry point used by the SCM).
use windows_service::{
    define_windows_service,
    service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState,
        ServiceStatus, ServiceType,
    },
    service_control_handler::{self, ServiceControlHandlerResult},
    service_dispatcher,
};

define_windows_service!(ffi_service_main, service_main);

fn service_main(_arguments: Vec<std::ffi::OsString>) {
    let event_handler = move |control_event| -> ServiceControlHandlerResult {
        match control_event {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                // TODO: signal the tokio runtime to shut down gracefully.
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };

    let status_handle =
        service_control_handler::register("NovaServer", event_handler).unwrap();

    status_handle
        .set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::Running,
            controls_accepted: ServiceControlAccept::STOP,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: std::time::Duration::default(),
            process_id: None,
        })
        .unwrap();

    // Run the full Nova server on a dedicated tokio runtime (service_main is sync).
    tokio::runtime::Runtime::new()
        .expect("tokio runtime")
        .block_on(nova_server::run())
        .ok();

    status_handle
        .set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::Stopped,
            controls_accepted: ServiceControlAccept::empty(),
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: std::time::Duration::default(),
            process_id: None,
        })
        .unwrap();
}

// ── SCM install / uninstall (uses the windows crate already linked) ────────

// Static UTF-16 strings for the SCM APIs (avoids the windows::w! macro which
// changed paths across crate versions).
const SVC_NAME_W: &[u16] = &[
    b'N' as u16, b'o' as u16, b'v' as u16, b'a' as u16,
    b'S' as u16, b'e' as u16, b'r' as u16, b'v' as u16, b'e' as u16, b'r' as u16, 0,
];
const SVC_DISPLAY_W: &[u16] = &[
    b'N' as u16, b'o' as u16, b'v' as u16, b'a' as u16, b' ' as u16,
    b'G' as u16, b'a' as u16, b'm' as u16, b'e' as u16, b' ' as u16,
    b'S' as u16, b't' as u16, b'r' as u16, b'e' as u16, b'a' as u16,
    b'm' as u16, b'i' as u16, b'n' as u16, b'g' as u16, 0,
];

fn install_service() -> windows::core::Result<()> {
    use windows::Win32::System::Services::*;
    use windows::core::PCWSTR;

    let exe = std::env::current_exe()
        .expect("cannot resolve exe path")
        .to_string_lossy()
        .into_owned();
    // Append --run-service so the SCM-started process enters the service path.
    let cmd = format!("\"{}\" --run-service", exe);
    let cmd_wide: Vec<u16> = cmd.encode_utf16().chain([0]).collect();

    unsafe {
        let scm = OpenSCManagerW(None, None, SC_MANAGER_CREATE_SERVICE)?;

        let svc = CreateServiceW(
            scm,
            PCWSTR(SVC_NAME_W.as_ptr()),
            PCWSTR(SVC_DISPLAY_W.as_ptr()),
            SERVICE_ALL_ACCESS,
            SERVICE_WIN32_OWN_PROCESS,
            SERVICE_AUTO_START,
            SERVICE_ERROR_NORMAL,
            PCWSTR(cmd_wide.as_ptr()),
            None, // load-order group
            None, // tag id out-param
            None, // dependencies
            None, // account (LocalSystem)
            None, // password
        )?;

        println!("✅ Nova registered as Windows service 'NovaServer' (auto-start).");
        println!("   Start with:   sc start NovaServer");
        println!("   Uninstall:    nova-server.exe --uninstall");

        let _ = CloseServiceHandle(svc);
        let _ = CloseServiceHandle(scm);
    }
    Ok(())
}

fn uninstall_service() -> windows::core::Result<()> {
    use windows::Win32::System::Services::*;
    use windows::core::PCWSTR;

    unsafe {
        let scm = OpenSCManagerW(None, None, SC_MANAGER_CONNECT)?;
        let svc = OpenServiceW(scm, PCWSTR(SVC_NAME_W.as_ptr()), SERVICE_ALL_ACCESS)?;
        DeleteService(svc)?;
        let _ = CloseServiceHandle(svc);
        let _ = CloseServiceHandle(scm);
    }
    println!("✅ Nova service 'NovaServer' marked for deletion.");
    Ok(())
}

// ── Entry point ────────────────────────────────────────────────────────────

fn main() {
    let args: Vec<String> = std::env::args().collect();

    match args.get(1).map(String::as_str) {
        Some("--install") => {
            install_service().unwrap_or_else(|e| eprintln!("Install failed: {e}"));
        }
        Some("--uninstall") => {
            uninstall_service().unwrap_or_else(|e| eprintln!("Uninstall failed: {e}"));
        }
        Some("--run-service") => {
            // Launched by the SCM — hand off to the windows-service dispatcher.
            // Blocks until the service stops.
            service_dispatcher::start("NovaServer", ffi_service_main)
                .expect("service_dispatcher::start failed");
        }
        _ => {
            // Normal interactive / tray run.
            tokio::runtime::Runtime::new()
                .expect("tokio runtime")
                .block_on(nova_server::run())
                .expect("nova_server::run failed");
        }
    }
}

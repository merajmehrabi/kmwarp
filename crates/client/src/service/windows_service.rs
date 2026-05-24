//! Windows-service registration + session-0 helper-spawn (M10).
//!
//! ## Why this is two-process
//!
//! Per spec gotcha "UAC": `SendInput` from a `LocalSystem` service in
//! session 0 *cannot* reach a user-session desktop. A service that does
//! the input injection itself silently swallows every event. The
//! workaround — established Windows pattern, documented in MSDN's "Using
//! Services" — is:
//!
//! 1. The service registers with the SCM and runs in session 0 as
//!    `LocalSystem`.
//! 2. On start it calls `WTSGetActiveConsoleSessionId` to find whoever
//!    is logged in at the physical console, fetches that user's token via
//!    `WTSQueryUserToken`, duplicates it to a primary token, and
//!    `CreateProcessAsUserW`s a fresh `kmwarp-client.exe run-as-helper`
//!    into that session.
//! 3. The helper inherits the user's desktop access; its `SendInput` calls
//!    reach the actual desktop.
//! 4. The service waits on the helper process handle. On SCM stop /
//!    shutdown, the event handler `TerminateProcess`s the helper, which
//!    unblocks the wait and lets the service report `Stopped`.
//!
//! ## Caveats the user should know
//!
//! - **No active session = no helper.** If no user is logged in at the
//!   console (e.g. on a Server SKU between RDP sessions), `WTSQueryUserToken`
//!   fails and the service exits cleanly. The SCM will restart it per
//!   `failure-actions` policy (we don't configure that explicitly; default
//!   is "no restart"); for v1 the user just logs in and re-`Start-Service`s.
//! - **Session change events.** If the active user logs out and a different
//!   user logs in, the original helper is still bound to the previous
//!   token. v1 doesn't track session-change notifications (`WM_WTSSESSION_CHANGE`);
//!   the helper will eventually crash or sit idle. A v1.1 enhancement is
//!   to register for session-change events and re-spawn.
//! - **Codesigning matters.** An unsigned service binary works for the
//!   developer but Windows Defender / corporate AV will flag it on other
//!   machines. The `scripts/build-windows.ps1` template wires Authenticode
//!   signing; production deployment should use it.
//!
//! ## Why no tokio in this file
//!
//! Service main is pure-sync. The helper child process is where tokio runs
//! (`main.rs` -> `app::run_client`). Keeping the service path tokio-free
//! avoids a runtime that the SCM teardown timing wouldn't respect.

use std::ffi::OsString;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use thiserror::Error;
use tracing::{debug, error, info, warn};
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, BOOL, HANDLE};
use windows::Win32::Security::{
    DuplicateTokenEx, SecurityImpersonation, TokenPrimary, SECURITY_ATTRIBUTES, TOKEN_ALL_ACCESS,
};
use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows::Win32::System::RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken};
use windows::Win32::System::Threading::{
    CreateProcessAsUserW, TerminateProcess, WaitForSingleObject, CREATE_NO_WINDOW,
    CREATE_UNICODE_ENVIRONMENT, INFINITE, PROCESS_INFORMATION, STARTUPINFOW,
};
use windows_service::service::{
    ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode,
    ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_service::{define_windows_service, service_dispatcher};

/// Service name in the SCM. Lowercased; users type this into `sc.exe`
/// and `Get-Service`.
pub const SERVICE_NAME: &str = "kmwarp-client";

/// Friendly name shown in Services.msc.
pub const DISPLAY_NAME: &str = "kmwarp Windows client";

/// Service type: own-process, not a shared service host.
const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;

/// Single, atomic slot holding the active helper process HANDLE so the
/// stop event handler can `TerminateProcess` it. HANDLE is a `*mut c_void`
/// at the FFI layer — we store its bits in `AtomicUsize` and reconstruct
/// the pointer on read.
///
/// `OnceLock` so we initialize on first set without locking on read.
static HELPER_PROCESS_HANDLE: OnceLock<AtomicUsize> = OnceLock::new();

/// Errors surfaced to the install / uninstall / run paths.
#[derive(Debug, Error)]
pub enum ServiceError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("windows-service crate error: {0}")]
    WindowsService(#[from] windows_service::Error),

    #[error("Win32 error: {0}")]
    Win32(#[from] windows::core::Error),

    #[error("no active console session; is a user logged in?")]
    NoActiveSession,

    #[error("the SCM did not accept the service control dispatcher start: {0}")]
    DispatcherFailed(String),
}

// ──────────────────────────────────────────────────────────────────────
// Install / uninstall
// ──────────────────────────────────────────────────────────────────────

/// Register the kmwarp-client binary as an auto-start Windows service.
///
/// Must be run from an elevated (Administrator) PowerShell or cmd. The
/// launch argument is `run-as-service`, which `main.rs` routes to
/// [`run_as_service`].
pub fn install_service() -> Result<(), ServiceError> {
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )?;

    let service_binary_path = std::env::current_exe()?;
    let service_info = ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from(DISPLAY_NAME),
        service_type: SERVICE_TYPE,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: service_binary_path,
        launch_arguments: vec![OsString::from("run-as-service")],
        dependencies: vec![],
        account_name: None, // LocalSystem
        account_password: None,
    };

    let svc = manager.create_service(
        &service_info,
        ServiceAccess::CHANGE_CONFIG | ServiceAccess::START,
    )?;
    svc.set_description("kmwarp Windows side of the cross-platform KVM")?;
    svc.start::<&str>(&[])?;
    info!(name = SERVICE_NAME, "service installed and started");
    Ok(())
}

/// Stop the service if running, then delete its SCM entry.
///
/// Must be elevated. Idempotent except for the delete itself.
pub fn uninstall_service() -> Result<(), ServiceError> {
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::ENUMERATE_SERVICE,
    )?;
    let svc = manager.open_service(
        SERVICE_NAME,
        ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
    )?;
    // Best-effort stop. The service may already be Stopped, in which case
    // this errors with ERROR_SERVICE_NOT_ACTIVE — fine to ignore.
    if let Err(e) = svc.stop() {
        debug!(error = %e, "stop returned error (probably already stopped)");
    }
    svc.delete()?;
    info!(name = SERVICE_NAME, "service deleted");
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────
// Service dispatcher entry point
// ──────────────────────────────────────────────────────────────────────

define_windows_service!(ffi_service_main, service_main);

/// Entry point invoked when the binary is launched as `run-as-service`.
/// Hands control to the SCM dispatcher; returns when the service stops.
pub fn run_as_service() -> Result<(), ServiceError> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)
        .map_err(|e| ServiceError::DispatcherFailed(e.to_string()))
}

/// Service main thread, called by `windows-service` via `define_windows_service!`.
/// Errors here can't be surfaced to the SCM beyond the exit code, so we log.
fn service_main(_args: Vec<OsString>) {
    if let Err(e) = run_service_inner() {
        error!(error = %e, "service exited with error");
    }
}

fn run_service_inner() -> Result<(), ServiceError> {
    let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)?;

    status_handle.set_service_status(ServiceStatus {
        service_type: SERVICE_TYPE,
        current_state: ServiceState::Running,
        controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::ZERO,
        process_id: None,
    })?;

    // Spawn the user-session helper and wait for it to exit. This blocks
    // until either the helper terminates naturally or the event handler
    // calls TerminateProcess in response to an SCM stop.
    if let Err(e) = spawn_user_session_helper_and_wait() {
        warn!(error = %e, "user-session helper spawn or wait failed");
    }

    status_handle.set_service_status(ServiceStatus {
        service_type: SERVICE_TYPE,
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::ZERO,
        process_id: None,
    })?;
    Ok(())
}

/// SCM event handler. Runs on a thread the SCM owns; must return quickly.
fn event_handler(control_event: ServiceControl) -> ServiceControlHandlerResult {
    match control_event {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            info!(?control_event, "received stop/shutdown; terminating helper");
            terminate_helper_if_running();
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    }
}

/// `TerminateProcess` the helper if we have a stored handle. Best-effort.
fn terminate_helper_if_running() {
    let Some(slot) = HELPER_PROCESS_HANDLE.get() else {
        return;
    };
    let raw = slot.swap(0, Ordering::AcqRel);
    if raw == 0 {
        return;
    }
    // SAFETY: We stored a HANDLE created by `CreateProcessAsUserW`. We swap
    // it out atomically so a concurrent terminate cannot double-free.
    let handle = HANDLE(raw as *mut std::ffi::c_void);
    unsafe {
        if let Err(e) = TerminateProcess(handle, 1) {
            warn!(error = %e, "TerminateProcess on helper failed");
        }
        let _ = CloseHandle(handle);
    }
}

// ──────────────────────────────────────────────────────────────────────
// Session-0 helper spawn
// ──────────────────────────────────────────────────────────────────────

/// Spawn `kmwarp-client.exe run-as-helper` into the active user session
/// and block until it exits.
fn spawn_user_session_helper_and_wait() -> Result<(), ServiceError> {
    // SAFETY: pure FFI; returns 0xFFFFFFFF on "no console session".
    let session_id = unsafe { WTSGetActiveConsoleSessionId() };
    if session_id == 0xFFFF_FFFF {
        return Err(ServiceError::NoActiveSession);
    }
    info!(session_id, "found active console session");

    // Get the user's token for that session.
    let mut user_token = HANDLE::default();
    // SAFETY: `&mut user_token` is valid for the duration of the call.
    unsafe { WTSQueryUserToken(session_id, &mut user_token) }?;

    // Duplicate to a primary token suitable for CreateProcessAsUserW.
    let mut primary_token = HANDLE::default();
    let attrs: Option<*const SECURITY_ATTRIBUTES> = None;
    // SAFETY: pure FFI; out-param pointer valid; user_token is valid from
    // the just-checked WTSQueryUserToken call.
    if let Err(e) = unsafe {
        DuplicateTokenEx(
            user_token,
            TOKEN_ALL_ACCESS,
            attrs,
            SecurityImpersonation,
            TokenPrimary,
            &mut primary_token,
        )
    } {
        // SAFETY: close the leaked user token before bubbling.
        unsafe {
            let _ = CloseHandle(user_token);
        }
        return Err(ServiceError::Win32(e));
    }
    // We hold `primary_token` now; the original `user_token` can go.
    // SAFETY: paired with the successful WTSQueryUserToken open.
    unsafe {
        let _ = CloseHandle(user_token);
    }

    // Build an environment block for the user session. Without this the
    // helper process inherits the SYSTEM environment, which can break
    // apps that read e.g. APPDATA.
    let mut env_block: *mut std::ffi::c_void = std::ptr::null_mut();
    // SAFETY: out-param + valid token.
    if let Err(e) = unsafe { CreateEnvironmentBlock(&mut env_block, Some(primary_token), false) } {
        unsafe {
            let _ = CloseHandle(primary_token);
        }
        return Err(ServiceError::Win32(e));
    }

    // Build the command line. CreateProcessAsUserW wants a writable
    // UTF-16 buffer for `lpcommandline`; we own it for the duration of
    // the call.
    let exe = std::env::current_exe()?;
    let cmdline_str = format!("\"{}\" run-as-helper", exe.display());
    let mut cmdline_w: Vec<u16> = cmdline_str
        .encode_utf16()
        .chain(std::iter::once(0u16))
        .collect();

    let startup_info = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    let mut process_info = PROCESS_INFORMATION::default();
    let app_name: Option<PCWSTR> = None;
    let cwd: Option<PCWSTR> = None;

    // SAFETY: all pointers live for the call; `cmdline_w` is mutable and
    // null-terminated as required.
    let create_result = unsafe {
        CreateProcessAsUserW(
            Some(primary_token),
            app_name,
            Some(PWSTR(cmdline_w.as_mut_ptr())),
            None,
            None,
            false,
            CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW,
            Some(env_block),
            cwd,
            &startup_info,
            &mut process_info,
        )
    };

    // Whatever happens next, the primary token + env block are no longer
    // needed by us. The child has its own handle to its security context.
    // SAFETY: paired with their respective creates.
    unsafe {
        let _ = DestroyEnvironmentBlock(env_block);
        let _ = CloseHandle(primary_token);
    }

    create_result?;
    info!(
        process_id = process_info.dwProcessId,
        "helper process launched into user session"
    );

    // Stash the process handle so the stop event handler can terminate it.
    let slot = HELPER_PROCESS_HANDLE.get_or_init(|| AtomicUsize::new(0));
    slot.store(process_info.hProcess.0 as usize, Ordering::Release);

    // Block waiting for the helper to exit. WaitForSingleObject returns
    // either when the process exits naturally or when the event handler
    // TerminateProcesses it.
    // SAFETY: process_info.hProcess is valid (CreateProcess succeeded).
    let wait = unsafe { WaitForSingleObject(process_info.hProcess, INFINITE) };
    debug!(?wait, "helper wait returned");

    // Clean up. terminate_helper_if_running may already have closed the
    // process handle on the stop path — we use an atomic swap to avoid
    // a double close.
    let leftover = slot.swap(0, Ordering::AcqRel);
    if leftover != 0 {
        // SAFETY: still our handle; matches the value we just swapped out.
        unsafe {
            let _ = CloseHandle(HANDLE(leftover as *mut std::ffi::c_void));
        }
    }
    // The thread handle is always ours to close.
    // SAFETY: process_info.hThread is valid from the same successful create.
    unsafe {
        let _ = CloseHandle(process_info.hThread);
    }

    Ok(())
}

// Silence the "BOOL is unused if we add no #[link]" warning that surfaces
// if a future refactor drops the BOOL-typed APIs above. The import stays
// because the WTS / Security APIs accept BOOL parameters under the hood.
#[allow(dead_code)]
fn _bool_marker(_: BOOL) {}

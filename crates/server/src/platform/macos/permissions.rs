//! macOS privacy-permission probes + Settings deep-links.
//!
//! `kmwarp-server` needs **two** TCC permissions to capture global input:
//!
//! 1. **Accessibility** (`AXIsProcessTrusted`) — required to use `CGEventTap`
//!    at the HID location with default options.
//! 2. **Input Monitoring** (`CGPreflightListenEventAccess`) — required on
//!    macOS 10.15+ to receive keyboard *content* through the tap. Without it,
//!    the tap installs but keystrokes are blanked. Mouse-only flows mostly
//!    work without Input Monitoring, but we treat it as required so M5 is
//!    not blocked when the user re-runs.
//!
//! The CGS / HIServices entry points used below are stable but the Rust
//! `core-graphics` / `objc2-app-kit` crates don't expose them, so we declare
//! the FFI directly. They're part of frameworks already linked transitively
//! by `core-graphics`.

use std::process::Command;

use tracing::warn;

/// Aggregated permission state. `NeedsAccessibility` shadows
/// `NeedsInputMonitoring` because the user must fix Accessibility first
/// (Input Monitoring's TCC prompt is only triggered by an active tap, and
/// the tap can't run at all without Accessibility).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermStatus {
    Granted,
    NeedsAccessibility,
    NeedsInputMonitoring,
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    /// Returns `true` iff the running process holds Input Monitoring permission.
    /// Available since macOS 10.15.
    fn CGPreflightListenEventAccess() -> bool;
}

/// Inspect the two TCC permissions kmwarp-server needs. Logs a clear hint
/// (with the parent-app remediation suggestion) when either is missing.
pub fn check_permissions() -> PermStatus {
    // SAFETY: zero-arg C call into a system framework.
    let ax = unsafe { AXIsProcessTrusted() };
    if !ax {
        warn!(
            "Accessibility permission missing. Open System Settings → \
             Privacy & Security → Accessibility and enable the *parent app* \
             of this process (Terminal.app / iTerm / VS Code embedded shell). \
             You can deep-link with `open_accessibility_pane()`."
        );
        return PermStatus::NeedsAccessibility;
    }
    // SAFETY: zero-arg C call into a system framework.
    let im = unsafe { CGPreflightListenEventAccess() };
    if !im {
        warn!(
            "Input Monitoring permission missing. Open System Settings → \
             Privacy & Security → Input Monitoring and enable the *parent app* \
             of this process. You can deep-link with \
             `open_input_monitoring_pane()`."
        );
        return PermStatus::NeedsInputMonitoring;
    }
    PermStatus::Granted
}

/// Launch System Settings on the Accessibility pane.
pub fn open_accessibility_pane() {
    spawn_open("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility");
}

/// Launch System Settings on the Input Monitoring pane. The pane id is
/// `Privacy_ListenEvent` since macOS 10.15.
pub fn open_input_monitoring_pane() {
    spawn_open("x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent");
}

fn spawn_open(url: &str) {
    match Command::new("open").arg(url).spawn() {
        Ok(_) => {}
        Err(e) => warn!(error = %e, url, "failed to launch `open` for Settings deep-link"),
    }
}

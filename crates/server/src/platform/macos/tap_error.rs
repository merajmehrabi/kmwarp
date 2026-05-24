//! Errors raised while installing or running the macOS event tap.

use thiserror::Error;

/// Anything that can fail when bringing up the `CGEventTap`.
#[derive(Debug, Error)]
pub enum TapError {
    /// `CGEventTapCreate` returned NULL. The most common cause is missing
    /// Input Monitoring permission on the running binary's parent app.
    #[error("CGEventTapCreate failed (Accessibility/Input Monitoring likely denied)")]
    TapCreateFailed,

    /// TCC denied the tap before it could run. Distinct from
    /// `TapCreateFailed` so callers can prompt the user with a different
    /// remediation hint (deep-link to Input Monitoring vs. Accessibility).
    #[error("not permitted to install event tap — grant Accessibility + Input Monitoring")]
    NotPermitted,

    /// Could not attach the tap's mach port to a CFRunLoopSource, or the
    /// dedicated run-loop thread could not be spawned.
    #[error("failed to wire CGEventTap into a CFRunLoop")]
    RunLoopFailed,
}

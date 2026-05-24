//! Per-monitor DPI awareness + virtual-desktop metrics.
//!
//! ## Why this exists
//!
//! Spec gotcha "Coordinate spaces on Windows": without explicit per-monitor
//! DPI awareness, `SendInput`, `SetCursorPos`, and the `GetSystemMetrics`
//! virtual-screen constants all report values DPI-scaled by the system, so
//! a 2560×1440 display at 150% scaling reports as 1707×960. Pixel math
//! across multiple monitors then drifts in ways that look like "the cursor
//! lags behind the edge by 33%" — exactly the kind of bug that wastes an
//! afternoon to diagnose.
//!
//! The fix is one call to [`set_per_monitor_dpi_aware`] at process start,
//! before any cursor or screen API. We use `PER_MONITOR_AWARE_V2` (the
//! Windows 10 1703+ flavor): it gives us physical pixels and also handles
//! per-monitor DPI changes via `WM_DPICHANGED` without the v1 quirks.
//!
//! ## Why these helpers and not inline calls
//!
//! [`virtual_screen_size`] is what M4's wire-format conversion + M6's
//! absolute-positioning helper both need. Centralising the `GetSystemMetrics`
//! constants here keeps the `SM_*` magic numbers out of the inject path.
//!
//! The whole module only compiles on Windows — its parent
//! (`platform/mod.rs`) cfg-gates the `pub mod windows;` declaration.

use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXSCREEN, SM_CXVIRTUALSCREEN, SM_CYSCREEN, SM_CYVIRTUALSCREEN,
};

use crate::platform::windows::inject_error::DpiError;

/// Pin per-monitor DPI awareness V2 for the process.
///
/// Must be called exactly once at process start, before any cursor or
/// screen-related API. Calling twice (or after another component already
/// pinned a different awareness level) returns `E_ACCESSDENIED`, which we
/// surface as [`DpiError::SetAwareness`].
///
/// Without this, `SendInput` coords are skewed on HiDPI displays.
pub fn set_per_monitor_dpi_aware() -> Result<(), DpiError> {
    // SAFETY: pure FFI call; no aliasing concerns, no callback registration.
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)? };
    Ok(())
}

/// Bounding box of the virtual desktop (union of every monitor) in physical
/// pixels, `(width, height)`.
///
/// Used by M6's absolute-positioning helper to convert pixel coordinates
/// into the 0..65535 normalized range that `MOUSEEVENTF_ABSOLUTE +
/// MOUSEEVENTF_VIRTUALDESK` expects.
pub fn virtual_screen_size() -> (i32, i32) {
    // SAFETY: `GetSystemMetrics` is a pure read of a system constant.
    unsafe { (GetSystemMetrics(SM_CXVIRTUALSCREEN), GetSystemMetrics(SM_CYVIRTUALSCREEN)) }
}

/// Primary-monitor size in physical pixels, `(width, height)`.
pub fn primary_screen_size() -> (i32, i32) {
    // SAFETY: `GetSystemMetrics` is a pure read of a system constant.
    unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) }
}

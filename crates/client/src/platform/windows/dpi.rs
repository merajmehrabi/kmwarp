//! Per-monitor DPI awareness + virtual-screen metrics.
//!
//! Stubbed in commit 1 to let the platform skeleton land; filled in by
//! the next commit ("add DPI awareness and screen-size helpers").

use crate::platform::windows::inject_error::DpiError;

/// Placeholder; real implementation lands in the next commit.
pub fn set_per_monitor_dpi_aware() -> Result<(), DpiError> {
    Ok(())
}

/// Placeholder; real implementation lands in the next commit.
pub fn virtual_screen_size() -> (i32, i32) {
    (0, 0)
}

/// Placeholder; real implementation lands in the next commit.
pub fn primary_screen_size() -> (i32, i32) {
    (0, 0)
}

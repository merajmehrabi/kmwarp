//! Pluggable [`InputSink`] for the client.
//!
//! On Windows the real sink is [`crate::platform::WinInputSink`]
//! (`SendInput` under the hood). On any other host — macOS dev box, Linux
//! CI — we substitute a [`NoOpSink`] so the wire-pump plumbing is still
//! exercised end-to-end (frames decode, dispatch lands at a real
//! `InputSink` implementor) without trying to drive a `SendInput` that
//! doesn't exist. This lets the developer smoke-test the M4 pipe on a
//! single Mac with `kmwarp-server` and `kmwarp-client` both running
//! locally.
//!
//! [`NoOpSink`] only logs at `trace`/`debug` so it doesn't drown
//! production logs at a 60 Hz mouse rate; a developer can flip the level
//! to `kmwarp_client=trace` to see every dispatch.

use kmwarp_core::{InputSink, KeyState, ModMask, MouseButton};
use tracing::{debug, trace};

/// Type alias selecting the real sink on Windows and the no-op sink on
/// other targets. Used by `app::run_client` so the rest of the wire-pump
/// code is platform-agnostic.
#[cfg(target_os = "windows")]
pub type DefaultSink = crate::platform::WinInputSink;

#[cfg(not(target_os = "windows"))]
pub type DefaultSink = NoOpSink;

/// Construct the default sink for this target. Returns an error only if
/// the real Windows sink fails to pin per-monitor DPI awareness.
#[cfg(target_os = "windows")]
pub fn build_default_sink() -> anyhow::Result<DefaultSink> {
    Ok(crate::platform::WinInputSink::new()?)
}

#[cfg(not(target_os = "windows"))]
pub fn build_default_sink() -> anyhow::Result<DefaultSink> {
    Ok(NoOpSink::default())
}

/// Logs-only [`InputSink`]. Lets the client run on non-Windows hosts
/// without requiring a real injection backend.
#[derive(Default)]
pub struct NoOpSink {
    rel_count: u64,
    button_count: u64,
    wheel_count: u64,
}

impl InputSink for NoOpSink {
    fn inject_mouse_rel(&mut self, dx: i32, dy: i32) {
        self.rel_count = self.rel_count.saturating_add(1);
        // Log first few then every 60th so a 60 Hz stream is visible at
        // ~1 Hz in `debug` level. `trace` shows every event.
        if self.rel_count <= 8 || self.rel_count % 60 == 0 {
            debug!(
                dx,
                dy,
                total = self.rel_count,
                "noop sink: would inject_mouse_rel"
            );
        } else {
            trace!(dx, dy, "noop sink: inject_mouse_rel");
        }
    }

    fn inject_mouse_button(&mut self, btn: MouseButton, state: KeyState) {
        self.button_count = self.button_count.saturating_add(1);
        debug!(
            ?btn,
            ?state,
            total = self.button_count,
            "noop sink: would inject_mouse_button"
        );
    }

    fn inject_mouse_wheel(&mut self, dx: i16, dy: i16) {
        self.wheel_count = self.wheel_count.saturating_add(1);
        debug!(
            dx,
            dy,
            total = self.wheel_count,
            "noop sink: would inject_mouse_wheel"
        );
    }

    fn inject_key(&mut self, hid: u16, state: KeyState, mods: ModMask) {
        trace!(hid, ?state, mods = mods.0, "noop sink: inject_key");
    }

    fn warp_cursor_abs(&mut self, x: i32, y: i32) {
        debug!(x, y, "noop sink: warp_cursor_abs");
    }

    fn hide_cursor(&mut self) {
        debug!("noop sink: hide_cursor");
    }

    fn show_cursor(&mut self) {
        debug!("noop sink: show_cursor");
    }
}

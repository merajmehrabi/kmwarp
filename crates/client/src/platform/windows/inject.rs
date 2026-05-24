//! `WinInputSink` skeleton.
//!
//! Stubbed in commit 1 so the platform module re-exports compile; full
//! `SendInput` body lands in the "implement WinInputSink with SendInput"
//! commit. The struct exists here so [`crate::platform::windows::mod`] can
//! re-export the type name without forward references.

use kmwarp_core::{InputSink, KeyState, ModMask, MouseButton};

use crate::platform::windows::dpi;
use crate::platform::windows::inject_error::DpiError;

/// Windows side `core::InputSink` implementation backed by `SendInput`.
///
/// Construction is fallible because it pins per-monitor DPI awareness,
/// which must happen exactly once and before any cursor or screen API.
pub struct WinInputSink {
    _private: (),
}

impl WinInputSink {
    /// Initialize per-monitor DPI awareness and return a fresh sink.
    pub fn new() -> Result<Self, DpiError> {
        dpi::set_per_monitor_dpi_aware()?;
        Ok(Self { _private: () })
    }
}

impl InputSink for WinInputSink {
    fn inject_mouse_rel(&mut self, _dx: i32, _dy: i32) {
        // Filled in by the SendInput commit.
    }

    fn inject_mouse_button(&mut self, _btn: MouseButton, _state: KeyState) {
        // Filled in by the SendInput commit.
    }

    fn inject_mouse_wheel(&mut self, _dx: i16, _dy: i16) {
        // Filled in by the SendInput commit.
    }

    fn inject_key(&mut self, _hid: u16, _state: KeyState, _mods: ModMask) {
        // M5 fills this in.
    }

    fn warp_cursor_abs(&mut self, _x: i32, _y: i32) {
        // Filled in by the SendInput commit (SetCursorPos).
    }

    fn hide_cursor(&mut self) {
        // M6 server-side concern; client stub.
    }

    fn show_cursor(&mut self) {
        // M6 server-side concern; client stub.
    }
}

//! `WinInputSink` — `core::InputSink` backed by `SendInput`.
//!
//! ## What this is
//!
//! The Windows side of M3. Translates the source-shaped mouse calls
//! delivered by `core::InputSink` (relative deltas, button up/down, scroll,
//! absolute warp) into one or two `SendInput` calls.
//!
//! Keyboard, hide/show cursor are stubbed for M3 — M5 fills keyboard, M6
//! puts hide/show on the macOS server side.
//!
//! ## SendInput semantics worth remembering
//!
//! - `MOUSEEVENTF_MOVE` alone = relative; `+ MOUSEEVENTF_ABSOLUTE` = `dx`/`dy`
//!   are normalized 0..65535 across either the primary screen or the entire
//!   virtual desktop depending on `MOUSEEVENTF_VIRTUALDESK`. The relative
//!   path is what runs steady-state; absolute is only for `TakeControl` in
//!   M6.
//! - X-button up/down sets `MOUSEEVENTF_XDOWN` / `XUP` with `mouseData =
//!   XBUTTON1` (= 1) or `XBUTTON2` (= 2). It is *not* shifted into the
//!   high word despite what some snippets suggest — MSDN says it's a flag
//!   value occupying the low bits.
//! - `MOUSEEVENTF_WHEEL` / `MOUSEEVENTF_HWHEEL` need `mouseData = clicks *
//!   WHEEL_DELTA (= 120)`. We accept logical clicks in the wire i16 and
//!   multiply here.
//!
//! ## Why injects don't propagate errors
//!
//! `core::InputSink`'s methods return `()`. `SendInput` *can* fail (returns
//! 0 on failure) but the trait doesn't permit surfacing it. We log a `warn!`
//! and drop the event — a dropped delta is far less harmful than panicking
//! the injector task, which would tear the whole client session down.

use std::mem::size_of;

use kmwarp_core::{InputSink, KeyState, ModMask, MouseButton};
use tracing::warn;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK,
    MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, MOUSE_EVENT_FLAGS,
};
use windows::Win32::UI::WindowsAndMessaging::SetCursorPos;

use crate::platform::windows::dpi;
use crate::platform::windows::inject_error::DpiError;

/// Magnitude of one notched scroll click in `mouseData` units (Win32
/// `WHEEL_DELTA`, hard-coded so we don't depend on the constant living in
/// a particular `windows` crate feature).
const WHEEL_DELTA: i32 = 120;

/// Normalization range for `MOUSEEVENTF_ABSOLUTE` (Win32 fixed constant).
const ABSOLUTE_AXIS_MAX: i32 = 65_535;

/// `XBUTTON1` flag value carried in `MOUSEINPUT::mouseData` when sending
/// `MOUSEEVENTF_XDOWN/XUP`. Hard-coded so we don't depend on whether the
/// `windows` crate exposes it as a bare `u32`, a newtype, or otherwise.
const XBUTTON1_FLAG: u32 = 0x0001;
/// `XBUTTON2` flag value. See [`XBUTTON1_FLAG`].
const XBUTTON2_FLAG: u32 = 0x0002;

/// Windows `core::InputSink` implementation backed by `SendInput`.
///
/// Construction is fallible because it pins per-monitor DPI awareness,
/// which must happen exactly once and before any cursor or screen API.
pub struct WinInputSink {
    /// Cached virtual-desktop size so `move_cursor_norm` doesn't re-query
    /// `GetSystemMetrics` per call. Refreshed only on resolution change,
    /// which we'll wire to `WM_DISPLAYCHANGE` if it ever matters; for now
    /// the constructor's snapshot is fine — Windows desktop resizing
    /// during a session is extremely rare.
    virtual_w: i32,
    virtual_h: i32,
}

impl WinInputSink {
    /// Initialize per-monitor DPI awareness and cache the virtual-desktop
    /// bounds. Call exactly once at process start, before any cursor API.
    pub fn new() -> Result<Self, DpiError> {
        dpi::set_per_monitor_dpi_aware()?;
        let (virtual_w, virtual_h) = dpi::virtual_screen_size();
        Ok(Self {
            virtual_w,
            virtual_h,
        })
    }

    /// Absolute cursor positioning across the virtual desktop, only used
    /// by M6's `TakeControl` to place the cursor at the entry point.
    ///
    /// Inputs are in physical pixels of the virtual desktop. We convert to
    /// the 0..65535 normalized range that `MOUSEEVENTF_ABSOLUTE +
    /// MOUSEEVENTF_VIRTUALDESK` requires. (Spec gotcha: absolute mode is
    /// *not* in pixels.)
    pub fn move_cursor_norm(&mut self, x_pixel: i32, y_pixel: i32) {
        // Guard against div-by-zero if the virtual screen reports degenerate.
        let denom_x = (self.virtual_w - 1).max(1);
        let denom_y = (self.virtual_h - 1).max(1);
        let nx = (x_pixel.saturating_mul(ABSOLUTE_AXIS_MAX)) / denom_x;
        let ny = (y_pixel.saturating_mul(ABSOLUTE_AXIS_MAX)) / denom_y;

        let input = mouse_input(MOUSEINPUT {
            dx: nx,
            dy: ny,
            mouseData: 0,
            dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
            time: 0,
            dwExtraInfo: 0,
        });
        send_one(&input, "move_cursor_norm");
    }
}

impl InputSink for WinInputSink {
    fn inject_mouse_rel(&mut self, dx: i32, dy: i32) {
        // i32 → LONG is the same on 64-bit Windows; no clamping required.
        // Wire deltas are i16 upstream, so this can never overflow.
        let input = mouse_input(MOUSEINPUT {
            dx,
            dy,
            mouseData: 0,
            dwFlags: MOUSEEVENTF_MOVE,
            time: 0,
            dwExtraInfo: 0,
        });
        send_one(&input, "inject_mouse_rel");
    }

    fn inject_mouse_button(&mut self, btn: MouseButton, state: KeyState) {
        let (flag, data) = button_flags(btn, state);
        let input = mouse_input(MOUSEINPUT {
            dx: 0,
            dy: 0,
            mouseData: data,
            dwFlags: flag,
            time: 0,
            dwExtraInfo: 0,
        });
        send_one(&input, "inject_mouse_button");
    }

    fn inject_mouse_wheel(&mut self, dx: i16, dy: i16) {
        // Two separate SendInput calls; Win32 doesn't combine wheel axes.
        if dy != 0 {
            let amount = i32::from(dy).saturating_mul(WHEEL_DELTA);
            let input = mouse_input(MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: amount as u32,
                dwFlags: MOUSEEVENTF_WHEEL,
                time: 0,
                dwExtraInfo: 0,
            });
            send_one(&input, "inject_mouse_wheel (vertical)");
        }
        if dx != 0 {
            let amount = i32::from(dx).saturating_mul(WHEEL_DELTA);
            let input = mouse_input(MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: amount as u32,
                dwFlags: MOUSEEVENTF_HWHEEL,
                time: 0,
                dwExtraInfo: 0,
            });
            send_one(&input, "inject_mouse_wheel (horizontal)");
        }
    }

    fn inject_key(&mut self, _hid: u16, _state: KeyState, _mods: ModMask) {
        // M5 fills this in. Warn (don't panic) so a stray Key event during
        // M3/M4 wiring doesn't crash the injector task.
        warn!("inject_key called before M5 wiring; ignoring");
    }

    fn warp_cursor_abs(&mut self, x: i32, y: i32) {
        // SAFETY: pure FFI; per-monitor DPI awareness was pinned in `new`.
        if let Err(e) = unsafe { SetCursorPos(x, y) } {
            warn!(error = %e, x, y, "SetCursorPos failed");
        }
    }

    fn hide_cursor(&mut self) {
        // M6 handles cursor hiding on the *server* side (macOS
        // `CGDisplayHideCursor`); the client never hides its local cursor
        // in v1. Stub kept so the trait surface is complete.
        warn!("WinInputSink::hide_cursor called; no-op on client side");
    }

    fn show_cursor(&mut self) {
        warn!("WinInputSink::show_cursor called; no-op on client side");
    }
}

/// Build an `INPUT` union holding a `MOUSEINPUT`. Keeps the unsafe union
/// construction in one place.
fn mouse_input(mi: MOUSEINPUT) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi },
    }
}

/// Send one `INPUT` and log a warning if Win32 reports it was blocked.
fn send_one(input: &INPUT, label: &str) {
    let slice = std::slice::from_ref(input);
    // SAFETY: `SendInput` only reads from `slice` for the duration of the
    // call; `cbSize` is the exact size of the `INPUT` union.
    let sent = unsafe { SendInput(slice, size_of::<INPUT>() as i32) };
    if sent as usize != slice.len() {
        let err = windows::core::Error::from_win32();
        warn!(
            label,
            err = %err,
            expected = slice.len(),
            sent,
            "SendInput rejected event"
        );
    }
}

/// Translate a `(button, state)` pair into the `(dwFlags, mouseData)` SendInput
/// expects. X-buttons carry their index in `mouseData`; main buttons leave it
/// zero.
fn button_flags(btn: MouseButton, state: KeyState) -> (MOUSE_EVENT_FLAGS, u32) {
    let down = matches!(state, KeyState::Down);
    match btn {
        MouseButton::Left => (
            if down {
                MOUSEEVENTF_LEFTDOWN
            } else {
                MOUSEEVENTF_LEFTUP
            },
            0,
        ),
        MouseButton::Right => (
            if down {
                MOUSEEVENTF_RIGHTDOWN
            } else {
                MOUSEEVENTF_RIGHTUP
            },
            0,
        ),
        MouseButton::Middle => (
            if down {
                MOUSEEVENTF_MIDDLEDOWN
            } else {
                MOUSEEVENTF_MIDDLEUP
            },
            0,
        ),
        MouseButton::X1 => (
            if down {
                MOUSEEVENTF_XDOWN
            } else {
                MOUSEEVENTF_XUP
            },
            XBUTTON1_FLAG,
        ),
        MouseButton::X2 => (
            if down {
                MOUSEEVENTF_XDOWN
            } else {
                MOUSEEVENTF_XUP
            },
            XBUTTON2_FLAG,
        ),
    }
}

//! Server-side `InputSink` implementation.
//!
//! The macOS server never *injects* mouse/key events — it captures and
//! forwards. It does, however, need the cursor-control half of
//! [`InputSink`] in M6: `warp_cursor_abs` for the 5-px back-warp and the
//! return-to-edge warp, and `hide_cursor` / `show_cursor` so the local
//! pointer disappears while the user controls the Windows peer.
//!
//! The `inject_*` methods are stubbed (warn-and-no-op) so accidental
//! mis-wiring of an `Action::Forward*` against the wrong sink is loud
//! but non-fatal.

use core_graphics::display::CGDisplay;
use core_graphics::geometry::CGPoint;
use kmwarp_core::platform::{InputSink, KeyState, ModMask, MouseButton};
use tracing::{trace, warn};

/// Owns no state — every method is a single Quartz call. Construct once
/// per server session and pass through to the edge brain.
#[derive(Debug, Default)]
pub struct MacInputSink;

impl MacInputSink {
    pub fn new() -> Self {
        Self
    }
}

impl InputSink for MacInputSink {
    fn inject_mouse_rel(&mut self, dx: i32, dy: i32) {
        warn!(
            dx,
            dy, "MacInputSink::inject_mouse_rel called on the server; this is a bug — server never injects"
        );
    }

    fn inject_mouse_button(&mut self, btn: MouseButton, state: KeyState) {
        warn!(
            ?btn,
            ?state,
            "MacInputSink::inject_mouse_button called on the server; this is a bug"
        );
    }

    fn inject_mouse_wheel(&mut self, dx: i16, dy: i16) {
        warn!(
            dx,
            dy, "MacInputSink::inject_mouse_wheel called on the server; this is a bug"
        );
    }

    fn inject_key(&mut self, hid: u16, state: KeyState, mods: ModMask) {
        warn!(
            hid,
            ?state,
            mods = format!("0x{:02X}", mods.0),
            "MacInputSink::inject_key called on the server; this is a bug"
        );
    }

    fn warp_cursor_abs(&mut self, x: i32, y: i32) {
        let point = CGPoint::new(f64::from(x), f64::from(y));
        // `warp_mouse_cursor_position` returns `Result<(), CGError>` —
        // CGError is i32; non-zero is failure. Log and continue: a
        // failed warp is annoying but not fatal (next motion will
        // resync the cursor).
        if let Err(e) = CGDisplay::warp_mouse_cursor_position(point) {
            warn!(x, y, cg_error = e, "CGWarpMouseCursorPosition failed");
            return;
        }
        // After a warp, macOS by default suppresses cursor-position
        // events for ~250 ms (it assumes app code is initiating a
        // drag). For the edge back-warp + return-to-edge use case we
        // want the cursor to immediately respond to user motion again,
        // so re-associate.
        if let Err(e) = CGDisplay::associate_mouse_and_mouse_cursor_position(true) {
            warn!(
                cg_error = e,
                "CGAssociateMouseAndMouseCursorPosition(true) failed after warp"
            );
        }
        trace!(x, y, "warped cursor + re-associated mouse");
    }

    fn hide_cursor(&mut self) {
        // Hide the visual cursor AND decouple it from physical mouse motion.
        // Without `associate(false)`, hiding alone leaves the (invisible)
        // cursor following the user's hand — so when control transfers to
        // Windows, the Mac cursor still drifts and re-crosses the edge
        // immediately. Per M6, RemoteActive means the cursor stays put.
        let main = CGDisplay::main();
        if let Err(e) = main.hide_cursor() {
            warn!(cg_error = e, "CGDisplayHideCursor failed");
        }
        if let Err(e) = CGDisplay::associate_mouse_and_mouse_cursor_position(false) {
            warn!(
                cg_error = e,
                "CGAssociateMouseAndMouseCursorPosition(false) failed; cursor will drift"
            );
        } else {
            trace!("cursor hidden + decoupled from physical motion");
        }
    }

    fn show_cursor(&mut self) {
        // Re-couple BEFORE showing so the cursor is responsive the instant
        // it reappears. Order matters: associate(true) first, then unhide.
        if let Err(e) = CGDisplay::associate_mouse_and_mouse_cursor_position(true) {
            warn!(
                cg_error = e,
                "CGAssociateMouseAndMouseCursorPosition(true) failed; cursor may not respond"
            );
        }
        let main = CGDisplay::main();
        if let Err(e) = main.show_cursor() {
            warn!(cg_error = e, "CGDisplayShowCursor failed");
        } else {
            trace!("cursor re-coupled + shown");
        }
    }
}

//! Background poller that detects when the cursor leaves the Windows
//! screen and tells the server to take control back.
//!
//! ## Edge convention (v1)
//!
//! v1 hardcodes a single edge crossing per spec §M6: **the Mac sits to
//! the left of the Windows screen**, so:
//!
//! - The cursor enters Windows from the **left edge** (server emits
//!   `TakeControl`; client warps to `(0, entry_y)`).
//! - The cursor leaves Windows back to the Mac through the **left edge**
//!   too (this watcher detects `x <= 0` and emits `ReleaseControl`).
//!
//! When v1.1 grows configurable topology this watcher gets parameterized
//! over which edge is the "exit"; right now the constant is baked in.
//!
//! ## Why polling and not a hook
//!
//! Windows offers low-level mouse hooks (`SetWindowsHookEx(WH_MOUSE_LL,
//! …)`) that fire per-event, but they require a running message loop on
//! the thread that installed them — which means a dedicated OS thread,
//! not a tokio task. Polling `GetCursorPos` at 60 Hz costs effectively
//! nothing (the call is a single kernel transition into a cached
//! position) and stays inside the tokio runtime, which is much simpler.
//!
//! ## Anti-thrash warp
//!
//! After detecting the leave, we `SetCursorPos` the Windows cursor to a
//! "safe" interior point so the next tick doesn't immediately re-fire
//! `ReleaseControl` while the user is still moving leftward. The
//! `active` flag also gets cleared; M6 server-side will set it again on
//! the next `TakeControl`.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use kmwarp_core::wire::Message;
use tokio::sync::{mpsc, watch};
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, SetCursorPos};

use crate::app::ClientStatus;

/// Cursor-poll cadence. 60 Hz matches the mouse-move event rate the
/// server feeds in, so the watcher reacts within one frame of the user
/// crossing the edge.
const POLL_PERIOD: Duration = Duration::from_millis(16);

/// Edge threshold. We treat `pos.x <= LEFT_EDGE_THRESHOLD` as "leaving"
/// rather than strict `== 0` because virtual-desktop layouts on multi-
/// monitor setups can have slight negative-x neighbour offsets and DPI
/// rounding can land us at `-1` momentarily.
const LEFT_EDGE_THRESHOLD: i32 = 0;

/// Long-running task that watches the Windows cursor while the client is
/// "active" (i.e. the server told us to take control). Emits a
/// `Message::ReleaseControl { exit_y }` into `out` when the cursor
/// crosses the left edge, then clears `active` and warps the cursor
/// inward by `safe_warp_x` pixels to prevent immediate re-fire.
///
/// Cancellation: the task is meant to be aborted by the session JoinSet;
/// it has no explicit shutdown signal. Sending `out` is `try_send` so a
/// full or closed outbound channel just logs a `warn!` and continues.
pub async fn cursor_leave_watcher(
    out: mpsc::Sender<Message>,
    active: Arc<AtomicBool>,
    safe_warp_x: i32,
    status_tx: Option<watch::Sender<ClientStatus>>,
    peer_addr: SocketAddr,
) {
    let mut ticker = tokio::time::interval(POLL_PERIOD);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    tracing::debug!(safe_warp_x, "cursor_leave_watcher entered");

    loop {
        ticker.tick().await;
        if !active.load(Ordering::Relaxed) {
            continue;
        }
        let mut pos = POINT::default();
        // SAFETY: pure FFI; pointer is valid for the duration of the call.
        if unsafe { GetCursorPos(&mut pos) }.is_err() {
            tracing::trace!("GetCursorPos failed; skipping tick");
            continue;
        }
        if pos.x > LEFT_EDGE_THRESHOLD {
            continue;
        }

        // Cursor crossed. Pack exit_y into u16 per wire format (the spec
        // declares it as u16) — clamp into range rather than panic on
        // odd-resolution displays.
        let exit_y = pos.y.clamp(0, u16::MAX as i32) as u16;
        tracing::info!(
            exit_x = pos.x,
            exit_y,
            "cursor left Windows; releasing control"
        );

        match out.try_send(Message::ReleaseControl { exit_y }) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::warn!("outbound full; dropping ReleaseControl");
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                tracing::debug!("outbound closed; cursor watcher exiting");
                return;
            }
        }

        active.store(false, Ordering::Relaxed);
        // Mirror the active=false flip into the tray: the Mac is no
        // longer driving this box, we're back in `Connected`.
        if let Some(tx) = status_tx.as_ref() {
            let _ = tx.send(ClientStatus::Connected {
                peer: peer_addr.to_string(),
            });
        }

        // Anti-thrash warp: park cursor at the interior X so the user's
        // continued leftward motion doesn't immediately re-trigger.
        // SAFETY: pure FFI; per-monitor DPI awareness was pinned at
        // `WinInputSink::new()` earlier in the session.
        if let Err(e) = unsafe { SetCursorPos(safe_warp_x, pos.y) } {
            tracing::warn!(error = %e, safe_warp_x, "SetCursorPos failed after release");
        }
    }
}

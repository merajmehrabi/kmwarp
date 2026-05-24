//! Client-side encoder + injector pumps (M4).
//!
//! - [`encoder_loop`] is the mirror of the server's — it owns the
//!   [`FrameWriter`] and drains a bounded `mpsc::Receiver<Message>` of
//!   outbound frames (heartbeats, `EchoPong` responses, future
//!   `ReleaseControl` from M6, etc.).
//! - [`injector_loop`] owns the [`FrameReader`] and an [`InputSink`]; for
//!   every incoming frame it pulses the deadline `Notify`, routes mouse
//!   variants to the sink, and responds to `EchoPing` by enqueuing a
//!   matching `EchoPong` into the outbound channel.
//!
//! Wire-byte → enum conversion goes through `core::wire::convert` so
//! server and client cannot drift on `MouseButton.button` / `.state`
//! dictionary values.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use kmwarp_core::wire::{apply_key_to_sink, apply_mouse_to_sink, Message};
use kmwarp_core::InputSink;
use tokio::sync::{mpsc, Notify};
use tracing::{debug, info, trace, warn};

use crate::error::ClientError;
use crate::net::{FrameReader, FrameWriter};

/// Drain `rx` into `writer` until the channel closes or the socket dies.
pub async fn encoder_loop(
    mut rx: mpsc::Receiver<Message>,
    mut writer: FrameWriter,
) -> Result<(), ClientError> {
    debug!("encoder_loop entered");
    while let Some(msg) = rx.recv().await {
        trace!(?msg, "encoder_loop writing frame");
        writer.write_frame(&msg).await?;
    }
    debug!("encoder_loop: outbound channel closed; exiting");
    Ok(())
}

/// Continuously decode frames from `reader`, pulse `notify`, dispatch
/// mouse/key to `sink`, bounce `EchoPing` back as `EchoPong` via
/// `tx_out`, and toggle `active` on `TakeControl` / `ReleaseControl` so
/// the M6 cursor-leave watcher (Windows-only) knows when to poll.
///
/// Generic over the sink so the same loop drives a real
/// [`crate::platform::WinInputSink`] on Windows and the
/// [`crate::sink::NoOpSink`] on other targets.
///
/// Returns `Err` on socket / wire failure (treat as session-fatal); on a
/// peer `Bye` it returns `Ok(())` for a clean session end.
pub async fn injector_loop<S: InputSink + Send>(
    mut reader: FrameReader,
    mut sink: S,
    notify: Arc<Notify>,
    tx_out: mpsc::Sender<Message>,
    active: Arc<AtomicBool>,
) -> Result<(), ClientError> {
    loop {
        let msg = reader.read_frame().await?;
        notify.notify_one();
        // Mouse-first because mouse events dominate the steady-state
        // packet rate (cursor motion is continuous; keypresses are rare).
        // The shared dispatch helpers in `core::wire::convert` keep the
        // byte conventions identical to the server's encode path.
        if apply_mouse_to_sink(&msg, &mut sink) {
            continue;
        }
        if apply_key_to_sink(&msg, &mut sink) {
            continue;
        }
        match msg {
            Message::Heartbeat { seq } => {
                trace!(seq, "received Heartbeat");
            }
            Message::EchoPing { ts_ns } => {
                let response = Message::EchoPong { ts_ns };
                if let Err(e) = tx_out.try_send(response) {
                    warn!(error = ?e, "failed to enqueue EchoPong");
                }
            }
            Message::EchoPong { .. } => {
                // The client never sends EchoPing in M4, so we shouldn't
                // see pongs. Log at trace in case a future server-side
                // bug bounces them at us.
                trace!(?msg, "received unsolicited EchoPong; ignoring");
            }
            Message::Bye { reason_code } => {
                info!(reason_code, "peer sent Bye; ending session");
                return Ok(());
            }
            Message::Hello { .. } | Message::HelloAck { .. } => {
                warn!(?msg, "unexpected post-handshake control frame");
            }
            Message::TakeControl { entry_y } => {
                // Spec §M6: warp our cursor to the left edge at the y
                // the server reported, then arm the leave-watcher. We
                // go through the sink's `warp_cursor_abs` so the
                // non-Windows NoOpSink doesn't try to call `SetCursorPos`.
                info!(entry_y, "received TakeControl; activating");
                sink.warp_cursor_abs(0, i32::from(entry_y));
                active.store(true, Ordering::Relaxed);
            }
            Message::ReleaseControl { .. } => {
                // Defensive: v1 wire convention is client→server only
                // for ReleaseControl. Log and ignore rather than treat
                // as a fault — a future bidirectional version may use
                // this arm legitimately.
                trace!(
                    ?msg,
                    "received ReleaseControl from server (unusual); ignoring"
                );
            }
            Message::ClipboardText { .. } => {
                trace!(?msg, "received frame; M8 will handle");
            }
            // `apply_mouse_to_sink` / `apply_key_to_sink` covered these
            // above; the compiler can't tell, so fall through.
            Message::MouseMoveRel { .. }
            | Message::MouseButton { .. }
            | Message::MouseWheel { .. } => unreachable!("handled by apply_mouse_to_sink"),
            Message::KeyEvent { .. } => unreachable!("handled by apply_key_to_sink"),
        }
    }
}

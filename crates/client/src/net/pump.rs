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

use std::sync::Arc;

use kmwarp_core::wire::{apply_mouse_to_sink, Message};
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
/// mouse to `sink`, and bounce `EchoPing` back as `EchoPong` via `tx_out`.
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
) -> Result<(), ClientError> {
    loop {
        let msg = reader.read_frame().await?;
        notify.notify_one();
        if apply_mouse_to_sink(&msg, &mut sink) {
            continue;
        }
        match msg {
            Message::Heartbeat { seq } => {
                trace!(seq, "received Heartbeat");
            }
            Message::KeyEvent { .. } => {
                // M5 wires this in; for M4 we drop silently to avoid noise.
                trace!(?msg, "received KeyEvent (M5 territory; ignoring)");
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
            Message::ClipboardText { .. }
            | Message::TakeControl { .. }
            | Message::ReleaseControl { .. } => {
                trace!(?msg, "received frame; M6/M8 will handle");
            }
            // `apply_mouse_to_sink` covered MouseMoveRel / MouseButton /
            // MouseWheel above; the compiler can't tell, so fall through.
            Message::MouseMoveRel { .. }
            | Message::MouseButton { .. }
            | Message::MouseWheel { .. } => unreachable!("handled by apply_mouse_to_sink"),
        }
    }
}

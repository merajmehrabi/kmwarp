//! Outbound encoder pump (M4).
//!
//! Producers (heartbeat ticker, mouse pump, echo responder, future
//! keyboard / clipboard pumps) push `Message`s into a bounded
//! `mpsc::Receiver<Message>`. [`encoder_loop`] is the only task that owns
//! the [`FrameWriter`] half of the connection; it drains the receiver and
//! writes each frame to the socket.
//!
//! This is the natural backpressure point of the input path. If the socket
//! stalls, the channel fills, and producers see `try_send` errors —
//! per PLAN.md §Async channel topology, the encoder is where backpressure
//! lives, not deep inside the platform layers.
//!
//! The loop exits cleanly when all senders are dropped (`recv` returns
//! `None`) or with `ServerError` on a write failure (peer dead).

use kmwarp_core::wire::Message;
use tokio::sync::mpsc;
use tracing::{debug, trace};

use crate::error::ServerError;
use crate::net::FrameWriter;

/// Drain `rx` into `writer` until the channel closes or the socket dies.
///
/// On socket failure the writer's error is returned so the per-peer
/// session can log it and tear down. On `rx` close (graceful shutdown), we
/// return `Ok(())`.
pub async fn encoder_loop(
    mut rx: mpsc::Receiver<Message>,
    mut writer: FrameWriter,
) -> Result<(), ServerError> {
    debug!("encoder_loop entered");
    while let Some(msg) = rx.recv().await {
        trace!(?msg, "encoder_loop writing frame");
        writer.write_frame(&msg).await?;
    }
    debug!("encoder_loop: outbound channel closed; exiting");
    Ok(())
}

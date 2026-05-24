//! Client-side error type.
//!
//! Mirrors [`kmwarp_server::error::ServerError`] in shape so the two binaries
//! present a consistent error vocabulary. Bonus variant `HandshakeRejected`
//! distinguishes a peer that talks the protocol correctly but explicitly
//! refused us (`HelloAck { accepted: false }`).

use kmwarp_core::WireError;
use thiserror::Error;

/// Anything that can go wrong inside the client library half.
#[derive(Debug, Error)]
pub enum ClientError {
    /// Underlying I/O failure (connect, socket read/write).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// Wire-format violation observed while encoding or decoding a frame.
    #[error("wire protocol error: {0}")]
    Wire(#[from] WireError),

    /// Peer closed the TCP stream (EOF) while we were waiting for a frame.
    #[error("peer disconnected")]
    Disconnected,

    /// Server returned `HelloAck { accepted: false }`. The client should
    /// surface this to the operator rather than retrying blindly.
    #[error("server rejected handshake")]
    HandshakeRejected,

    /// Server sent something other than `HelloAck` in response to our
    /// `Hello`. Treat as a protocol-level fatal mismatch.
    #[error("server sent unexpected frame during handshake")]
    UnexpectedHandshakeFrame,
}

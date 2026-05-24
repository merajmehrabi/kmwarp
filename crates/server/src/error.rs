//! Server-side error type.
//!
//! Wraps the protocol-level [`WireError`](kmwarp_core::WireError) and
//! [`std::io::Error`] without leaking either into the library's public API
//! shape, and adds a distinct `Disconnected` variant so callers can pattern
//! match on "peer closed cleanly" without grovelling through `io::ErrorKind`.

use kmwarp_core::WireError;
use thiserror::Error;

/// Anything that can go wrong inside the server library half.
#[derive(Debug, Error)]
pub enum ServerError {
    /// Underlying I/O failure (socket read/write, bind, accept).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// Wire-format violation observed while encoding or decoding a frame.
    #[error("wire protocol error: {0}")]
    Wire(#[from] WireError),

    /// Peer closed the TCP stream (EOF) while we were waiting for a frame.
    /// Distinct from `Io` so callers can log the loss without inspecting
    /// `ErrorKind::UnexpectedEof`.
    #[error("peer disconnected")]
    Disconnected,
}

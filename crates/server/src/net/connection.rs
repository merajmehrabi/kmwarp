//! Framed wire-protocol I/O over `tokio::net::TcpStream`.
//!
//! The [`Connection`] is the M1 handshake-time view: one struct owns the
//! whole stream plus a single read-side buffer, exposing `read_frame` and
//! `write_frame` for the brief Hello / HelloAck exchange.
//!
//! For the steady-state phase (concurrent heartbeat writer + frame reader)
//! callers split the connection via [`Connection::into_split`] which hands
//! back independent [`FrameReader`] and [`FrameWriter`] halves backed by
//! `tokio`'s `OwnedReadHalf` / `OwnedWriteHalf`. Splitting preserves any
//! bytes already buffered on the read side so frames straddling the
//! handshake boundary are not lost.
//!
//! All I/O paths are pure — `read_frame` and `write_frame` do not impose
//! timeouts. Deadline enforcement (2 s of silence ⇒ peer dead) is the
//! responsibility of the orchestrating task in `app.rs`.

use bytes::BytesMut;
use kmwarp_core::wire::{decode_frame, encode_frame, Message};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;

use crate::error::ServerError;

/// Initial read buffer size. 4 KiB comfortably holds dozens of heartbeats /
/// input events without ever reallocating in steady state.
const READ_BUF_CAPACITY: usize = 4096;

/// Owns one peer's `TcpStream` plus its read-side accumulation buffer.
///
/// Constructed via [`Connection::new`], which sets `TCP_NODELAY` so input
/// events never sit in Nagle's coalescing window (PLAN.md cross-cutting
/// gotcha; don't defer).
pub struct Connection {
    stream: TcpStream,
    read_buf: BytesMut,
}

impl Connection {
    /// Wrap an accepted/connected `TcpStream`. Enables `TCP_NODELAY` on the
    /// underlying socket so latency-sensitive single-message writes flush
    /// immediately.
    pub fn new(stream: TcpStream) -> std::io::Result<Self> {
        stream.set_nodelay(true)?;
        Ok(Self {
            stream,
            read_buf: BytesMut::with_capacity(READ_BUF_CAPACITY),
        })
    }

    /// Encode `msg` into a scratch buffer and `write_all` it to the socket.
    ///
    /// Allocates one `BytesMut` per call; M1 traffic is heartbeats + a single
    /// handshake so this is fine. The hot mouse/keyboard path in M4/M5 will
    /// reuse a writer-owned scratch buffer instead — see [`FrameWriter`].
    pub async fn write_frame(&mut self, msg: &Message) -> Result<(), ServerError> {
        let mut scratch = BytesMut::with_capacity(64);
        encode_frame(msg, &mut scratch)?;
        self.stream.write_all(&scratch).await?;
        Ok(())
    }

    /// Read frames from the socket, returning the next complete one.
    ///
    /// - `Ok(msg)`: a frame decoded cleanly.
    /// - `Err(ServerError::Disconnected)`: the peer closed the stream (EOF)
    ///   before we could finish reading a frame.
    /// - `Err(ServerError::Wire(_))`: protocol violation — caller should
    ///   treat the connection as dead.
    /// - `Err(ServerError::Io(_))`: socket-level failure.
    pub async fn read_frame(&mut self) -> Result<Message, ServerError> {
        loop {
            if let Some(msg) = decode_frame(&mut self.read_buf)? {
                return Ok(msg);
            }
            let n = self.stream.read_buf(&mut self.read_buf).await?;
            if n == 0 {
                // Two cases for a zero-byte read: a clean EOF mid-frame
                // (Disconnected) or an EOF on a clean frame boundary (also
                // Disconnected from our perspective; if we had a partial
                // frame the buffer would be non-empty here).
                return Err(ServerError::Disconnected);
            }
        }
    }

    /// Split into independent reader and writer halves for concurrent task
    /// use (heartbeat sender + frame reader). The existing read buffer is
    /// carried into the reader so any bytes already accumulated are not
    /// lost.
    pub fn into_split(self) -> (FrameReader, FrameWriter) {
        let (read_half, write_half) = self.stream.into_split();
        (
            FrameReader {
                read_half,
                read_buf: self.read_buf,
            },
            FrameWriter {
                write_half,
                scratch: BytesMut::with_capacity(64),
            },
        )
    }
}

/// Read half of a split [`Connection`].
pub struct FrameReader {
    read_half: OwnedReadHalf,
    read_buf: BytesMut,
}

impl FrameReader {
    /// See [`Connection::read_frame`].
    pub async fn read_frame(&mut self) -> Result<Message, ServerError> {
        loop {
            if let Some(msg) = decode_frame(&mut self.read_buf)? {
                return Ok(msg);
            }
            let n = self.read_half.read_buf(&mut self.read_buf).await?;
            if n == 0 {
                return Err(ServerError::Disconnected);
            }
        }
    }
}

/// Write half of a split [`Connection`].
pub struct FrameWriter {
    write_half: OwnedWriteHalf,
    scratch: BytesMut,
}

impl FrameWriter {
    /// See [`Connection::write_frame`]. Reuses an owned scratch buffer so
    /// the steady-state heartbeat loop is allocation-free.
    pub async fn write_frame(&mut self, msg: &Message) -> Result<(), ServerError> {
        self.scratch.clear();
        encode_frame(msg, &mut self.scratch)?;
        self.write_half.write_all(&self.scratch).await?;
        Ok(())
    }
}

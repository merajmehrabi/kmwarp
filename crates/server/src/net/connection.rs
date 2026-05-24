//! Framed wire-protocol I/O over an async byte stream.
//!
//! Pre-M9 this was concrete over `tokio::net::TcpStream`. M9 wraps every
//! peer connection in a `tokio_rustls::server::TlsStream`, which has a
//! different concrete type but the same `AsyncRead + AsyncWrite` shape.
//! Rather than make every caller carry a type parameter, the
//! [`Connection`] / [`FrameReader`] / [`FrameWriter`] structs hold boxed
//! trait objects — the dynamic-dispatch overhead is microscopic compared
//! to a TLS record decrypt or a TCP syscall, and the call-site churn
//! goes from "every signature" to zero.
//!
//! The [`Connection`] is the handshake-time view: one struct owns the
//! whole stream plus a single read-side buffer, exposing `read_frame` /
//! `write_frame` for the brief Hello / HelloAck exchange. After
//! handshake (and after the M9 pairing flow if needed) callers split
//! via [`Connection::into_split`] for the steady-state writer +
//! reader tasks.
//!
//! Splitting preserves any bytes already buffered on the read side so
//! frames straddling the handshake boundary are not lost.
//!
//! [`FrameReader`] / [`FrameWriter`] also expose `read_raw` / `write_raw`
//! — length-prefixed `[u32 LE][bytes]` framings used exclusively by the
//! M9 pairing flow for the SPAKE2 and HMAC blobs (which aren't `Message`
//! variants because they only ever fly once, pre-handshake).

use bytes::BytesMut;
use kmwarp_core::wire::{decode_frame, encode_frame, Message};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::error::ServerError;

/// Initial read buffer size. 4 KiB comfortably holds dozens of heartbeats /
/// input events without ever reallocating in steady state.
const READ_BUF_CAPACITY: usize = 4096;

/// Hard cap on the size of a raw pairing frame. Pairing payloads are
/// SPAKE2 elements (~32 bytes) and HMAC-authed cert frames (~1 KiB for a
/// self-signed ed25519 cert). 64 KiB is plenty of headroom and far below
/// anything malicious would need to OOM us.
const MAX_RAW_FRAME: usize = 64 * 1024;

/// Type alias for an owned, `Send`-able, generic async byte stream. Used
/// to type-erase `TcpStream` vs `TlsStream<TcpStream>`.
type DynRead = Box<dyn AsyncRead + Unpin + Send>;
type DynWrite = Box<dyn AsyncWrite + Unpin + Send>;

/// Owns one peer's async byte stream plus its read-side accumulation
/// buffer.
pub struct Connection {
    stream_read: DynRead,
    stream_write: DynWrite,
    read_buf: BytesMut,
}

impl Connection {
    /// Wrap an accepted plain `TcpStream`. Enables `TCP_NODELAY` so input
    /// events never sit in Nagle's coalescing window (PLAN.md cross-cutting
    /// gotcha; don't defer). Used pre-M9 and inside M9 for the brief plain-
    /// TCP window before the TLS handshake (which itself sets nodelay on
    /// the underlying socket since we hand it off after construction).
    pub fn new(stream: TcpStream) -> std::io::Result<Self> {
        stream.set_nodelay(true)?;
        Ok(Self::from_io(stream))
    }

    /// Wrap an arbitrary async byte stream (e.g. a `tokio_rustls`
    /// `TlsStream<TcpStream>` post-handshake). Caller is responsible for
    /// any socket-level options like `TCP_NODELAY` — for TLS that means
    /// setting it on the underlying `TcpStream` before the `accept` /
    /// `connect` handshake.
    pub fn from_io<S>(stream: S) -> Self
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (r, w) = tokio::io::split(stream);
        Self {
            stream_read: Box::new(r),
            stream_write: Box::new(w),
            read_buf: BytesMut::with_capacity(READ_BUF_CAPACITY),
        }
    }

    /// Encode `msg` and `write_all` it to the socket.
    pub async fn write_frame(&mut self, msg: &Message) -> Result<(), ServerError> {
        let mut scratch = BytesMut::with_capacity(64);
        encode_frame(msg, &mut scratch)?;
        self.stream_write.write_all(&scratch).await?;
        Ok(())
    }

    /// Read frames from the socket, returning the next complete one.
    ///
    /// - `Ok(msg)`: a frame decoded cleanly.
    /// - `Err(ServerError::Disconnected)`: peer EOF before a full frame
    ///   landed.
    /// - `Err(ServerError::Wire(_))`: protocol violation.
    /// - `Err(ServerError::Io(_))`: socket / TLS failure.
    pub async fn read_frame(&mut self) -> Result<Message, ServerError> {
        loop {
            if let Some(msg) = decode_frame(&mut self.read_buf)? {
                return Ok(msg);
            }
            let n = self.stream_read.read_buf(&mut self.read_buf).await?;
            if n == 0 {
                return Err(ServerError::Disconnected);
            }
        }
    }

    /// Write a raw length-prefixed `[u32 LE][payload]` blob. Used by the
    /// M9 pairing flow to ship SPAKE2 elements and HMAC-authed cert
    /// frames pre-Hello.
    pub async fn write_raw(&mut self, payload: &[u8]) -> Result<(), ServerError> {
        write_raw_to(&mut self.stream_write, payload).await
    }

    /// Read a raw length-prefixed `[u32 LE][payload]` blob. Caps at
    /// [`MAX_RAW_FRAME`] bytes; anything larger is treated as a wire
    /// violation.
    pub async fn read_raw(&mut self) -> Result<Vec<u8>, ServerError> {
        read_raw_from(&mut self.stream_read, &mut self.read_buf).await
    }

    /// Split into independent reader and writer halves. The buffer is
    /// carried into the reader so any bytes already accumulated during
    /// the handshake aren't lost.
    pub fn into_split(self) -> (FrameReader, FrameWriter) {
        (
            FrameReader {
                read_half: self.stream_read,
                read_buf: self.read_buf,
            },
            FrameWriter {
                write_half: self.stream_write,
                scratch: BytesMut::with_capacity(64),
            },
        )
    }
}

/// Read half of a split [`Connection`].
pub struct FrameReader {
    read_half: DynRead,
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
    write_half: DynWrite,
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

/// Shared raw-write implementation used by both `Connection::write_raw`
/// and any future caller. Length prefix is `u32 LE` so it covers payloads
/// larger than the `Message` 16-bit length field, with a hard cap at
/// [`MAX_RAW_FRAME`].
async fn write_raw_to(w: &mut DynWrite, payload: &[u8]) -> Result<(), ServerError> {
    if payload.len() > MAX_RAW_FRAME {
        return Err(ServerError::Wire(
            kmwarp_core::WireError::InvalidPayload("raw frame exceeds 64 KiB cap"),
        ));
    }
    let len = u32::try_from(payload.len()).expect("bounds-checked above");
    w.write_all(&len.to_le_bytes()).await?;
    w.write_all(payload).await?;
    Ok(())
}

/// Shared raw-read implementation. Buffer-reuse strategy matches
/// `read_frame`: reads into a `BytesMut`, returns a fresh `Vec<u8>` for
/// the caller (pairing payloads are one-shot, so the alloc cost is
/// negligible vs. the SPAKE2 / HMAC math).
async fn read_raw_from(r: &mut DynRead, buf: &mut BytesMut) -> Result<Vec<u8>, ServerError> {
    // First read enough for the 4-byte length prefix.
    while buf.len() < 4 {
        let n = r.read_buf(buf).await?;
        if n == 0 {
            return Err(ServerError::Disconnected);
        }
    }
    let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if len > MAX_RAW_FRAME {
        return Err(ServerError::Wire(kmwarp_core::WireError::InvalidPayload(
            "raw frame exceeds 64 KiB cap",
        )));
    }
    // Then fill until we have all len payload bytes.
    while buf.len() < 4 + len {
        let n = r.read_buf(buf).await?;
        if n == 0 {
            return Err(ServerError::Disconnected);
        }
    }
    // Split off the prefix + payload from the buffer.
    let _prefix = buf.split_to(4);
    let payload = buf.split_to(len);
    Ok(payload.to_vec())
}

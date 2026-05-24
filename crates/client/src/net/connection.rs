//! Framed wire-protocol I/O over an async byte stream (client side).
//!
//! Mirror of `kmwarp_server::net::connection`. The two are intentionally
//! near-identical — only error types differ. See the server-side file for
//! the design rationale around the boxed `DynRead` / `DynWrite` trait
//! objects (M9 added TLS-wrapped variants of the underlying stream).

use bytes::BytesMut;
use kmwarp_core::wire::{decode_frame, encode_frame, Message};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::error::ClientError;

const READ_BUF_CAPACITY: usize = 4096;
const MAX_RAW_FRAME: usize = 64 * 1024;

type DynRead = Box<dyn AsyncRead + Unpin + Send>;
type DynWrite = Box<dyn AsyncWrite + Unpin + Send>;

/// Owns one connected async byte stream plus its read-side accumulation
/// buffer.
pub struct Connection {
    stream_read: DynRead,
    stream_write: DynWrite,
    read_buf: BytesMut,
}

impl Connection {
    /// Wrap a connected plain `TcpStream`. Enables `TCP_NODELAY` so input
    /// events never sit in Nagle's coalescing window.
    pub fn new(stream: TcpStream) -> std::io::Result<Self> {
        stream.set_nodelay(true)?;
        Ok(Self::from_io(stream))
    }

    /// Wrap an arbitrary async byte stream (e.g. a `tokio_rustls`
    /// `TlsStream<TcpStream>` post-handshake).
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
    pub async fn write_frame(&mut self, msg: &Message) -> Result<(), ClientError> {
        let mut scratch = BytesMut::with_capacity(64);
        encode_frame(msg, &mut scratch)?;
        self.stream_write.write_all(&scratch).await?;
        Ok(())
    }

    /// Read frames from the socket, returning the next complete one.
    pub async fn read_frame(&mut self) -> Result<Message, ClientError> {
        loop {
            if let Some(msg) = decode_frame(&mut self.read_buf)? {
                return Ok(msg);
            }
            let n = self.stream_read.read_buf(&mut self.read_buf).await?;
            if n == 0 {
                return Err(ClientError::Disconnected);
            }
        }
    }

    /// Write a raw length-prefixed `[u32 LE][payload]` blob. Used by the
    /// M9 pairing flow to ship SPAKE2 elements and HMAC-authed cert frames
    /// pre-Hello.
    pub async fn write_raw(&mut self, payload: &[u8]) -> Result<(), ClientError> {
        write_raw_to(&mut self.stream_write, payload).await
    }

    /// Read a raw length-prefixed `[u32 LE][payload]` blob. Caps at 64 KiB.
    pub async fn read_raw(&mut self) -> Result<Vec<u8>, ClientError> {
        read_raw_from(&mut self.stream_read, &mut self.read_buf).await
    }

    /// Split into independent reader and writer halves.
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
    pub async fn read_frame(&mut self) -> Result<Message, ClientError> {
        loop {
            if let Some(msg) = decode_frame(&mut self.read_buf)? {
                return Ok(msg);
            }
            let n = self.read_half.read_buf(&mut self.read_buf).await?;
            if n == 0 {
                return Err(ClientError::Disconnected);
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
    pub async fn write_frame(&mut self, msg: &Message) -> Result<(), ClientError> {
        self.scratch.clear();
        encode_frame(msg, &mut self.scratch)?;
        self.write_half.write_all(&self.scratch).await?;
        Ok(())
    }
}

async fn write_raw_to(w: &mut DynWrite, payload: &[u8]) -> Result<(), ClientError> {
    if payload.len() > MAX_RAW_FRAME {
        return Err(ClientError::Wire(
            kmwarp_core::WireError::InvalidPayload("raw frame exceeds 64 KiB cap"),
        ));
    }
    let len = u32::try_from(payload.len()).expect("bounds-checked above");
    w.write_all(&len.to_le_bytes()).await?;
    w.write_all(payload).await?;
    Ok(())
}

async fn read_raw_from(r: &mut DynRead, buf: &mut BytesMut) -> Result<Vec<u8>, ClientError> {
    while buf.len() < 4 {
        let n = r.read_buf(buf).await?;
        if n == 0 {
            return Err(ClientError::Disconnected);
        }
    }
    let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if len > MAX_RAW_FRAME {
        return Err(ClientError::Wire(kmwarp_core::WireError::InvalidPayload(
            "raw frame exceeds 64 KiB cap",
        )));
    }
    while buf.len() < 4 + len {
        let n = r.read_buf(buf).await?;
        if n == 0 {
            return Err(ClientError::Disconnected);
        }
    }
    let _prefix = buf.split_to(4);
    let payload = buf.split_to(len);
    Ok(payload.to_vec())
}

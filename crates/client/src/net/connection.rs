//! Framed wire-protocol I/O over `tokio::net::TcpStream` (client side).
//!
//! Mirror of `kmwarp_server::net::connection` — see that file for the design
//! commentary. The only differences are the error type (`ClientError`) and
//! that there is no accept-side counterpart; the client constructs a
//! `Connection` after `TcpStream::connect` succeeds.

use bytes::BytesMut;
use kmwarp_core::wire::{decode_frame, encode_frame, Message};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;

use crate::error::ClientError;

const READ_BUF_CAPACITY: usize = 4096;

/// Owns one connected `TcpStream` plus its read-side accumulation buffer.
pub struct Connection {
    stream: TcpStream,
    read_buf: BytesMut,
}

impl Connection {
    /// Wrap a connected `TcpStream`. Enables `TCP_NODELAY` so input events
    /// never sit in Nagle's coalescing window.
    pub fn new(stream: TcpStream) -> std::io::Result<Self> {
        stream.set_nodelay(true)?;
        Ok(Self {
            stream,
            read_buf: BytesMut::with_capacity(READ_BUF_CAPACITY),
        })
    }

    /// Encode `msg` into a scratch buffer and `write_all` it to the socket.
    pub async fn write_frame(&mut self, msg: &Message) -> Result<(), ClientError> {
        let mut scratch = BytesMut::with_capacity(64);
        encode_frame(msg, &mut scratch)?;
        self.stream.write_all(&scratch).await?;
        Ok(())
    }

    /// Read frames from the socket, returning the next complete one. See the
    /// server-side mirror for error-case discussion.
    pub async fn read_frame(&mut self) -> Result<Message, ClientError> {
        loop {
            if let Some(msg) = decode_frame(&mut self.read_buf)? {
                return Ok(msg);
            }
            let n = self.stream.read_buf(&mut self.read_buf).await?;
            if n == 0 {
                return Err(ClientError::Disconnected);
            }
        }
    }

    /// Split into independent reader and writer halves for concurrent task
    /// use. The existing read buffer carries into the reader.
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
    write_half: OwnedWriteHalf,
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

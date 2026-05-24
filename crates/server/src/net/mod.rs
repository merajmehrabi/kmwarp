//! Networking primitives for the server.
//!
//! [`connection`] owns the framed wire-protocol I/O on top of a `TcpStream`.
//! Higher-level orchestration (accept loop, per-peer task management) lives in
//! the parent `app` module.

pub mod connection;

pub use connection::{Connection, FrameReader, FrameWriter};

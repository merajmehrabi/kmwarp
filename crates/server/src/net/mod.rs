//! Networking primitives for the server.
//!
//! [`connection`] owns the framed wire-protocol I/O on top of a `TcpStream`.
//! Higher-level orchestration (accept loop, per-peer task management) lives in
//! the parent `app` module.

pub mod connection;
pub mod pairing;
pub mod pump;

pub use connection::{Connection, FrameReader, FrameWriter};
pub use pairing::{run_server_pairing_flow, ServerPairingError};
pub use pump::encoder_loop;

//! Networking primitives for the client.
//!
//! [`connection`] mirrors the server's framed wire-protocol I/O. The two
//! eventually share the same `core::wire` codec — only error types differ.

pub mod connection;
pub mod pump;

pub use connection::{Connection, FrameReader, FrameWriter};
#[cfg(target_os = "windows")]
pub use pump::clipboard_out_task;
pub use pump::{encoder_loop, injector_loop, injector_loop_with_source, FrameSource};

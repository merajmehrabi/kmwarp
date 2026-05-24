//! kmwarp-core: platform-agnostic protocol, state machine, and traits.
//!
//! This crate intentionally has no platform dependencies; the macOS server
//! (`kmwarp-server`) and Windows client (`kmwarp-client`) bring those in
//! and implement the traits exposed here.

pub mod clipboard;
pub mod config;
pub mod edge;
pub mod error;
pub mod hid;
pub mod modmap;
pub mod pairing;
pub mod platform;
pub mod stuck_keys;
pub mod wire;

pub use clipboard::{ChunkFlags, Chunker, EchoGuard, ReassembleError, Reassembler};
pub use error::{ConfigError, PairingError, StateError, WireError};
pub use platform::{
    Clipboard, ClipboardEvent, InputSink, InputSource, KeyState, ModMask, MouseButton, SourceEvent,
};

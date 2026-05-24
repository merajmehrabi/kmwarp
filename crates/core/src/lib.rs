//! kmwarp-core: platform-agnostic protocol, state machine, and traits.
//!
//! This crate intentionally has no platform dependencies; the macOS server
//! (`kmwarp-server`) and Windows client (`kmwarp-client`) bring those in
//! and implement the traits exposed here.

pub mod error;
pub mod hid;
pub mod platform;
pub mod stuck_keys;
pub mod wire;

pub use error::{StateError, WireError};
pub use platform::{
    Clipboard, ClipboardEvent, InputSink, InputSource, KeyState, ModMask, MouseButton, SourceEvent,
};

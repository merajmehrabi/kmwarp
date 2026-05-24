//! Windows platform layer for the client.
//!
//! Owns three concerns:
//!
//! - [`dpi`] — per-monitor DPI awareness and virtual-desktop size lookup.
//!   The spec calls this out as a foot-gun ("Coordinate spaces on Windows"
//!   gotcha): without per-monitor V2 awareness, `SendInput` and
//!   `SetCursorPos` give skewed coordinates on HiDPI displays. Must run
//!   before any cursor or screen API.
//! - [`inject`] — [`WinInputSink`], the `core::InputSink` implementation
//!   that translates `SourceEvent`-shaped calls into `SendInput` /
//!   `SetCursorPos` calls.
//! - [`inject_error`] — [`InjectError`] / [`DpiError`] for fallible
//!   initialization paths. The injection methods themselves can't return
//!   errors (the trait isn't fallible) so they log via `tracing` instead.
//!
//! The module is only compiled on Windows; `crate::platform::mod.rs` cfg-
//! gates the `pub mod windows;`. On macOS this entire subtree is dead and
//! never reached by the compiler.

pub mod dpi;
pub mod inject;
pub mod inject_error;

pub use dpi::{primary_screen_size, set_per_monitor_dpi_aware, virtual_screen_size};
pub use inject::WinInputSink;
pub use inject_error::{DpiError, InjectError};

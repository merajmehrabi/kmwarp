//! Platform-specific adapters for the client.
//!
//! The whole subtree is cfg-gated to `target_os = "windows"` so the macOS
//! development host can still `cargo build --workspace` cleanly while
//! Windows-only code (the `windows` crate, `SendInput`, DPI awareness) is
//! compiled out. CI (`windows-latest` runner) is the source of truth for
//! actually building this layer.
//!
//! Re-exports here keep the public surface flat so consumers can write
//! `kmwarp_client::platform::WinInputSink` without threading nested module
//! paths.

#[cfg(target_os = "windows")]
pub mod windows;

#[cfg(target_os = "windows")]
pub use self::windows::{DpiError, InjectError, WinInputSink};

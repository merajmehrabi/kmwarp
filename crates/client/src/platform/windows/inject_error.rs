//! Error types for the Windows platform layer.
//!
//! Two distinct paths can fail:
//!
//! - [`DpiError`] — `SetProcessDpiAwarenessContext` returns `E_ACCESSDENIED`
//!   if called more than once per process, or if a manifest already pinned
//!   a different awareness level. We surface this so `WinInputSink::new`
//!   can return it rather than swallowing.
//! - [`InjectError`] — reserved for future fallible inject paths. The
//!   `core::InputSink` trait methods themselves return `()`, so at present
//!   we only use `InjectError` from helpers that aren't trait-bound (e.g.
//!   M6's `move_cursor_norm` once it grows error handling).

use thiserror::Error;

/// Failure modes for DPI-awareness setup.
#[derive(Debug, Error)]
pub enum DpiError {
    /// The Win32 call to set per-monitor DPI awareness failed. Wraps the
    /// raw `windows::core::Error` so callers keep the HRESULT for logging.
    #[cfg(target_os = "windows")]
    #[error("SetProcessDpiAwarenessContext failed: {0}")]
    SetAwareness(#[from] windows::core::Error),
}

/// Failure modes for input injection.
///
/// Stub for M3 — fleshed out in M5/M6 once we have paths that legitimately
/// fail (e.g. clipboard reads, scancode mapping misses).
#[derive(Debug, Error)]
pub enum InjectError {
    /// Generic placeholder so the variant set isn't empty (would otherwise
    /// trip `unreachable_patterns` on consumers' `match`).
    #[error("input injection failed: {0}")]
    Other(String),
}

/// Failure modes for clipboard install / read / write.
///
/// Install can fail if `RegisterClassW` / `CreateWindowExW` /
/// `AddClipboardFormatListener` return errors; read/write usually fail
/// only because another process holds the global clipboard lock — we
/// retry briefly and then surface this rather than spin forever.
#[derive(Debug, Error)]
pub enum ClipboardError {
    /// Could not install the message-only window listener.
    #[error("clipboard listener install failed: {0}")]
    Init(String),

    /// Could not open / read the global clipboard.
    #[error("clipboard read failed: {0}")]
    Read(String),

    /// Could not open / write the global clipboard.
    #[error("clipboard write failed: {0}")]
    Write(String),
}

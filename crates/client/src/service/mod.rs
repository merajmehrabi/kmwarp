//! Service / daemon integration for the client.
//!
//! M10's Windows side: register `kmwarp-client` with the Service Control
//! Manager so it auto-starts on boot, plus a session-0-isolation
//! workaround that re-spawns the actual input-injection logic into the
//! active user session via `WTSQueryUserToken` + `CreateProcessAsUser`.
//!
//! The whole subtree is cfg-gated to `target_os = "windows"` at the
//! `pub mod` declarations below so the macOS development host can still
//! `cargo build --workspace` without windows-rs in scope.

#[cfg(target_os = "windows")]
pub mod windows_service;

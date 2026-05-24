//! Platform-specific input adapters for the server.
//!
//! Only one OS is meaningful for a `kmwarp-server` binary at runtime (macOS),
//! but the module is cfg-gated so the workspace still type-checks on Windows
//! CI (`cargo check --workspace`). Anything else that consumes a platform
//! adapter (e.g. the future M4 wire pump) must cfg-gate its calls in the
//! same way.

#[cfg(target_os = "macos")]
pub mod macos;

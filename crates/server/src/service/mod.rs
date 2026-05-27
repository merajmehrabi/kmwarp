//! Background-service install/uninstall (M10) + menu bar surface (v1.1).
//!
//! macOS only for v1.0 — the platform-specific entry point is
//! [`launchagent`], which renders a launchd plist into
//! `~/Library/LaunchAgents/com.kmwarp.server.plist` and loads it.
//!
//! v1.0 shipped headless; v1.1 adds [`menubar`], an `NSStatusItem`-
//! backed status surface that mirrors [`crate::app::ServerStatus`] in
//! the system menu bar and offers a Quit menu item. The runtime
//! publishes status via a `tokio::sync::watch` channel handed to
//! `run_server`; the menubar polls that receiver from inside the
//! NSApp run loop.

#[cfg(target_os = "macos")]
pub mod launchagent;

#[cfg(target_os = "macos")]
pub mod menubar;

#[cfg(target_os = "macos")]
pub use launchagent::{install_launch_agent, launch_agent_path, uninstall_launch_agent};

use thiserror::Error;

/// Anything that can go wrong while installing / uninstalling the
/// background service.
#[derive(Debug, Error)]
pub enum ServiceError {
    /// Filesystem I/O failure (writing the plist, creating
    /// `~/Library/LaunchAgents`, etc.).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// `directories::BaseDirs::new()` returned `None` (no `$HOME`).
    /// Extremely rare; sandboxed contexts only.
    #[error("could not resolve user home directory")]
    NoHomeDir,

    /// `launchctl` exited non-zero. The plist may have been written but
    /// not loaded. The string carries the captured stderr.
    #[error("launchctl failed: {0}")]
    LaunchctlFailed(String),

    /// The binary's own path was not resolvable
    /// (`std::env::current_exe` failed). Rare; happens if the binary
    /// was deleted after launch.
    #[error("could not resolve current_exe: {0}")]
    NoCurrentExe(std::io::Error),
}

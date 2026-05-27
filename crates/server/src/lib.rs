//! kmwarp-server library half.
//!
//! `main.rs` is a thin shim that parses environment configuration, initializes
//! tracing, and hands off to [`app::run_server`]. Everything testable lives
//! inside the library so future integration tests and (eventually) the M10
//! LaunchAgent helper can link against it without rebuilding a binary.

pub mod app;
pub mod discovery;
pub mod error;
pub mod net;
pub mod platform;
pub mod service;
pub mod tls;

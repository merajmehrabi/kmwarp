//! kmwarp-client library half.
//!
//! `main.rs` is a thin shim that parses environment configuration, initializes
//! tracing, and hands off to [`app::run_client`]. Everything testable lives
//! inside the library so the M10 Windows-service split can call into it
//! without rebuilding the binary.

pub mod app;
pub mod error;
pub mod net;
pub mod platform;
pub mod service;
pub mod sink;
pub mod tls;

//! kmwarp-server entry point.
//!
//! Reads bind address and peer name from the environment, initializes
//! `tracing`, and hands off to [`kmwarp_server::app::run_server`].
//!
//! Environment:
//!   * `KMWARP_BIND` — listen address (default `0.0.0.0:51423`).
//!   * `KMWARP_PEER_NAME` — name advertised to peers (default `kmwarp-server`).
//!   * `KMWARP_M2_DEMO=1` (macOS only) — bypass the normal server and run
//!     the M2 CGEventTap acceptance harness instead.
//!   * `RUST_LOG` — standard tracing filter (default `kmwarp=info`).

use std::env;
use std::net::SocketAddr;

use anyhow::{Context, Result};
use kmwarp_server::app::run_server;
use tracing_subscriber::EnvFilter;

const DEFAULT_BIND: &str = "0.0.0.0:51423";
const DEFAULT_PEER_NAME: &str = "kmwarp-server";

#[tokio::main]
async fn main() -> Result<()> {
    // RUST_LOG (if set) wins; otherwise default to `kmwarp=info`. We
    // intentionally don't `.add_directive("kmwarp=info")` on top of the env
    // filter, because EnvFilter replaces same-target directives, which would
    // demote any `RUST_LOG=kmwarp=debug` back to `info`.
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("kmwarp=info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    // M2 acceptance demo: short-circuit the normal server when the hook is
    // set. macOS-only because it depends on `CGEventTap`. On other platforms
    // the env var is ignored.
    #[cfg(target_os = "macos")]
    if env::var("KMWARP_M2_DEMO").ok().as_deref() == Some("1") {
        return kmwarp_server::platform::macos::m2_demo::run().await;
    }

    let bind_str = env::var("KMWARP_BIND").unwrap_or_else(|_| DEFAULT_BIND.to_string());
    let bind: SocketAddr = bind_str
        .parse()
        .with_context(|| format!("parsing KMWARP_BIND={bind_str:?}"))?;
    let peer_name = env::var("KMWARP_PEER_NAME").unwrap_or_else(|_| DEFAULT_PEER_NAME.to_string());

    run_server(bind, &peer_name).await
}

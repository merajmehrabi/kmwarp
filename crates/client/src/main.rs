//! kmwarp-client entry point.
//!
//! Reads connect address and peer name from the environment, initializes
//! `tracing`, and hands off to [`kmwarp_client::app::run_client`].
//!
//! Environment:
//!   * `KMWARP_CONNECT` — server address to connect to (default `127.0.0.1:51423`).
//!   * `KMWARP_PEER_NAME` — name advertised to the server (default `kmwarp-client`).
//!   * `RUST_LOG` — standard tracing filter (default `kmwarp=info`).

use std::env;
use std::net::SocketAddr;

use anyhow::{Context, Result};
use kmwarp_client::app::run_client;
use tracing_subscriber::EnvFilter;

const DEFAULT_CONNECT: &str = "127.0.0.1:51423";
const DEFAULT_PEER_NAME: &str = "kmwarp-client";

#[tokio::main]
async fn main() -> Result<()> {
    // RUST_LOG (if set) wins; otherwise default to `kmwarp=info`. See the
    // server `main.rs` for why we don't layer `.add_directive` on top.
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("kmwarp=info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let connect_str = env::var("KMWARP_CONNECT").unwrap_or_else(|_| DEFAULT_CONNECT.to_string());
    let connect: SocketAddr = connect_str
        .parse()
        .with_context(|| format!("parsing KMWARP_CONNECT={connect_str:?}"))?;
    let peer_name = env::var("KMWARP_PEER_NAME").unwrap_or_else(|_| DEFAULT_PEER_NAME.to_string());

    run_client(connect, &peer_name).await
}

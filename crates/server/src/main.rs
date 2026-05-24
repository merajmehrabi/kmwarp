//! kmwarp-server entry point (macOS).
//!
//! M0 placeholder: initialize tracing and log a startup line.

use tracing_subscriber::EnvFilter;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    tracing::info!("hello from kmwarp-server");
}

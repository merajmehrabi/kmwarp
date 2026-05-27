//! kmwarp-server entry point.
//!
//! ## Subcommands
//!
//! - `kmwarp-server run` (default) — initialize tracing, read
//!   environment, and call [`app::run_server`]. This is what launchd
//!   invokes via the installed `~/Library/LaunchAgents/com.kmwarp.server.plist`.
//! - `kmwarp-server install` (macOS) — write the LaunchAgent plist
//!   and `launchctl load -w` it. Idempotent.
//! - `kmwarp-server uninstall` (macOS) — `launchctl unload -w` and
//!   remove the plist. Idempotent.
//!
//! `kmwarp-server` with no subcommand defaults to `run` so existing
//! direct invocations (including the existing CI / smoke-test
//! patterns) keep working unchanged. The LaunchAgent's
//! `ProgramArguments` always passes `run` explicitly, so its
//! semantics don't drift if the default ever changes.
//!
//! ## Environment (read by the `run` path)
//!
//! - `KMWARP_BIND` — listen address (default `0.0.0.0:51423`).
//! - `KMWARP_PEER_NAME` — name advertised to peers (default `kmwarp-server`).
//! - `KMWARP_M2_DEMO=1` (macOS only) — bypass the normal server and
//!   run the M2 mouse-capture acceptance harness instead.
//! - `KMWARP_M5_DEMO=1` (macOS only) — bypass the normal server and
//!   run the M5 keyboard-capture acceptance harness instead.
//! - `RUST_LOG` — standard tracing filter (default `kmwarp=info`).

use std::env;
use std::net::SocketAddr;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use kmwarp_server::app::run_server;
use tracing_subscriber::EnvFilter;

const DEFAULT_BIND: &str = "0.0.0.0:51423";
const DEFAULT_PEER_NAME: &str = "kmwarp-server";

/// Top-level CLI.
#[derive(Debug, Parser)]
#[command(
    name = "kmwarp-server",
    version,
    about = "kmwarp macOS server — captures input, forwards to a paired Windows client",
    long_about = None
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the server in the foreground (default behavior when no
    /// subcommand is given). This is what the installed LaunchAgent
    /// invokes.
    Run,

    /// Install the launchd agent at
    /// `~/Library/LaunchAgents/com.kmwarp.server.plist` and load it
    /// via `launchctl`. macOS only.
    Install,

    /// `launchctl unload -w` and remove the plist. macOS only.
    /// Idempotent — succeeds even if the agent was never installed.
    Uninstall,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing();

    match cli.command.unwrap_or(Command::Run) {
        Command::Run => run().await,
        Command::Install => cmd_install(),
        Command::Uninstall => cmd_uninstall(),
    }
}

fn init_tracing() {
    // RUST_LOG (if set) wins; otherwise default to `kmwarp=info`. We
    // intentionally don't `.add_directive("kmwarp=info")` on top of the
    // env filter, because EnvFilter replaces same-target directives,
    // which would demote any `RUST_LOG=kmwarp=debug` back to `info`.
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("kmwarp=info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

async fn run() -> Result<()> {
    // M2 / M5 acceptance demos: short-circuit the normal server when
    // the hook is set. macOS-only because they depend on `CGEventTap`.
    // On other platforms the env vars are ignored.
    #[cfg(target_os = "macos")]
    if env::var("KMWARP_M2_DEMO").ok().as_deref() == Some("1") {
        return kmwarp_server::platform::macos::m2_demo::run().await;
    }
    #[cfg(target_os = "macos")]
    if env::var("KMWARP_M5_DEMO").ok().as_deref() == Some("1") {
        return kmwarp_server::platform::macos::m5_demo::run().await;
    }

    let bind_str = env::var("KMWARP_BIND").unwrap_or_else(|_| DEFAULT_BIND.to_string());
    let bind: SocketAddr = bind_str
        .parse()
        .with_context(|| format!("parsing KMWARP_BIND={bind_str:?}"))?;
    let peer_name = env::var("KMWARP_PEER_NAME").unwrap_or_else(|_| DEFAULT_PEER_NAME.to_string());

    run_server(bind, &peer_name, None).await
}

#[cfg(target_os = "macos")]
fn cmd_install() -> Result<()> {
    kmwarp_server::service::install_launch_agent().context("installing LaunchAgent")?;
    let path = kmwarp_server::service::launch_agent_path()
        .context("resolving LaunchAgent path post-install")?;
    println!(
        "kmwarp-server installed.\n  plist : {}\n  logs  : /tmp/kmwarp-server.log (stdout), /tmp/kmwarp-server.err (stderr)\n  status: launchctl list | grep kmwarp",
        path.display()
    );
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn cmd_install() -> Result<()> {
    anyhow::bail!("`install` is macOS-only; this binary was built for a different OS")
}

#[cfg(target_os = "macos")]
fn cmd_uninstall() -> Result<()> {
    kmwarp_server::service::uninstall_launch_agent().context("uninstalling LaunchAgent")?;
    println!("kmwarp-server uninstalled.");
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn cmd_uninstall() -> Result<()> {
    anyhow::bail!("`uninstall` is macOS-only; this binary was built for a different OS")
}

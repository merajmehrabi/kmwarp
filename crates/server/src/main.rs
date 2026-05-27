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
//! ## v1.1 runtime topology (macOS)
//!
//! v1.0 ran the server via `#[tokio::main]` — tokio owned the main
//! thread. v1.1 adds a menu bar status item ([`service::menubar`])
//! which requires NSApp on the main thread, so the topology flips:
//!
//! ```text
//!   main thread:   NSApp.run() — owns the AppKit run loop forever
//!   worker thread: tokio runtime + run_server(...) task graph
//! ```
//!
//! Status updates flow worker → main via
//! `tokio::sync::watch<ServerStatus>`; the menu bar's NSTimer polls
//! the receiver at 4 Hz. Quit clicks flow main → process exit (the
//! `on_quit` callback `std::process::exit(0)`s after a short grace
//! sleep, which gives tokio a chance to flush logs).
//!
//! The `KMWARP_HEADLESS=1` env var bypasses the menu bar entirely
//! and re-runs the v1.0 `tokio::main`-style entry — useful for
//! launchd contexts where the menu bar would be invisible anyway
//! (LaunchAgents run in the user session but headlessly is just
//! simpler), and for any CI smoke tests that don't want an NSApp
//! sitting in the foreground.
//!
//! ## Environment (read by the `run` path)
//!
//! - `KMWARP_BIND` — listen address (default `0.0.0.0:51423`).
//! - `KMWARP_PEER_NAME` — name advertised to peers (default `kmwarp-server`).
//! - `KMWARP_HEADLESS=1` — skip the macOS menu bar; run tokio on main.
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

fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing();

    match cli.command.unwrap_or(Command::Run) {
        Command::Run => run(),
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

/// Top-level run dispatch. Routes to:
///   * the M2 / M5 demos if the matching env var is set
///   * the headless path (tokio on main) if `KMWARP_HEADLESS=1` or
///     the build target isn't macOS
///   * the menubar-hosted path (tokio on a worker, NSApp on main)
///     on macOS by default
fn run() -> Result<()> {
    // M2 / M5 acceptance demos — macOS-only. These don't run the
    // normal server, so they don't need the menubar surface.
    #[cfg(target_os = "macos")]
    if env::var("KMWARP_M2_DEMO").ok().as_deref() == Some("1") {
        return run_blocking_on_tokio(async {
            kmwarp_server::platform::macos::m2_demo::run().await
        });
    }
    #[cfg(target_os = "macos")]
    if env::var("KMWARP_M5_DEMO").ok().as_deref() == Some("1") {
        return run_blocking_on_tokio(async {
            kmwarp_server::platform::macos::m5_demo::run().await
        });
    }

    let bind = parse_bind()?;
    let peer_name = env::var("KMWARP_PEER_NAME").unwrap_or_else(|_| DEFAULT_PEER_NAME.to_string());

    // Headless override OR non-macOS: just run the server on a single
    // tokio runtime hosted on main, the v1.0 shape. No NSApp, no
    // menu bar.
    if env::var("KMWARP_HEADLESS").ok().as_deref() == Some("1") {
        tracing::info!("KMWARP_HEADLESS=1 set; skipping menu bar");
        return run_blocking_on_tokio(async move { run_server(bind, &peer_name, None).await });
    }
    #[cfg(not(target_os = "macos"))]
    {
        return run_blocking_on_tokio(async move { run_server(bind, &peer_name, None).await });
    }

    // Default macOS path: tokio on a worker thread, NSApp on main.
    #[cfg(target_os = "macos")]
    {
        run_with_menubar(bind, peer_name)
    }
}

/// Build a multi-thread tokio runtime and `block_on` the supplied
/// future. Used by the headless path and the demo routes.
fn run_blocking_on_tokio<F>(fut: F) -> Result<()>
where
    F: std::future::Future<Output = Result<()>>,
{
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")?;
    rt.block_on(fut)
}

/// Parse `KMWARP_BIND` (or default) into a `SocketAddr`.
fn parse_bind() -> Result<SocketAddr> {
    let bind_str = env::var("KMWARP_BIND").unwrap_or_else(|_| DEFAULT_BIND.to_string());
    bind_str
        .parse()
        .with_context(|| format!("parsing KMWARP_BIND={bind_str:?}"))
}

/// macOS default-mode entry: spin up tokio on a worker thread and
/// hand main to the AppKit run loop hosting our `NSStatusItem`.
///
/// **Quit shape (deliberately simple).** When the menu bar's
/// "Quit kmwarp" item fires, `on_quit` runs on the main thread. It
/// sleeps 500 ms (to let any in-flight tracing writes flush) and
/// then calls `std::process::exit(0)`. The runtime thread is
/// abandoned mid-tick; for a server whose only external state is a
/// pin file written atomically that's acceptable. A cleaner approach
/// would be a tokio oneshot wired into a `tokio::select!` inside
/// `run_server`'s accept loop — we picked the exit() path because
/// (a) the spec OK's it and (b) the run_server loop has no easy
/// shutdown seam today.
///
/// This function does not return — `run_on_main_thread` enters
/// `NSApp.run()` which blocks until `terminate:` calls `exit(0)`.
#[cfg(target_os = "macos")]
fn run_with_menubar(bind: SocketAddr, peer_name: String) -> Result<()> {
    use kmwarp_server::app::ServerStatus;
    use kmwarp_server::service::menubar;
    use tokio::sync::watch;

    let (status_tx, status_rx) = watch::channel(ServerStatus::Idle);

    // Build the runtime BEFORE spawning the worker thread so a build
    // failure propagates as a normal Result to the caller (rather
    // than panicking on the worker).
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building tokio runtime for menubar mode")?;

    // Worker thread owns the runtime + the server future. We don't
    // join this thread — Quit calls process::exit, which tears down
    // every thread atomically.
    std::thread::Builder::new()
        .name("kmwarp-runtime".into())
        .spawn(move || {
            let result = rt.block_on(run_server(bind, &peer_name, Some(status_tx)));
            if let Err(e) = result {
                tracing::error!(error = %e, "run_server exited with error");
                // The menubar tick keeps spinning even after the
                // server dies; exit so the operator notices.
                std::process::exit(1);
            }
            // run_server never returns Ok in practice (it's a `loop`
            // around accept) — but if it ever does, treat it as
            // clean shutdown.
            std::process::exit(0);
        })
        .context("spawning kmwarp-runtime thread")?;

    // Quit handler: best-effort log flush, then exit. Runs on the
    // main thread because the menu bar dispatches it there.
    let on_quit: Box<dyn FnOnce() + Send + 'static> = Box::new(|| {
        tracing::info!("Quit clicked; exiting in 500ms");
        std::thread::sleep(std::time::Duration::from_millis(500));
        std::process::exit(0);
    });

    // Hands main thread to NSApp; never returns.
    menubar::run_on_main_thread(status_rx, on_quit);
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

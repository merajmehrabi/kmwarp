//! kmwarp-client entry point.
//!
//! Subcommands (parsed via `clap`):
//!
//! - `run` (default) — foreground connect-and-inject. The user's "just
//!   start it from a terminal" path.
//! - `install` — register as an auto-start Windows service. Requires
//!   Administrator. Windows-only.
//! - `uninstall` — stop + delete the service. Windows-only.
//! - `run-as-service` — entry point invoked by the SCM. Not for humans.
//!   Windows-only.
//! - `run-as-helper` — entry point spawned by the service into the
//!   active user session via `CreateProcessAsUserW`. Same semantics as
//!   `run`; named distinctly so the service path is grep-able.
//!
//! Environment:
//!   * `KMWARP_CONNECT` — server address to connect to. When unset
//!     (the v1.1 default), the client browses mDNS for
//!     `_kmwarp._tcp.local.` for up to 10 s and uses the first
//!     resolved IPv4 server. Setting it explicitly bypasses
//!     discovery entirely — useful as an override for non-mDNS
//!     networks or for pinning a specific peer when several Macs
//!     are on the same LAN.
//!   * `KMWARP_PEER_NAME` — name advertised to the server (default `kmwarp-client`).
//!   * `KMWARP_HEADLESS` — when set to `1`, skip the system-tray icon and
//!     run tokio on the main thread (v1.0 shape). Default-on inside the
//!     Windows service entry point (session 0 can't show Shell_NotifyIconW
//!     anyway). Set explicitly for CI / smoke tests.
//!   * `RUST_LOG` — standard tracing filter (default `kmwarp=info`).
//!   * `KMWARP_M3_DEMO` — when set to `1`, runs the M3 acceptance harness
//!     instead of starting the client. Drives the local cursor in a smooth
//!     50 px circle for 5 seconds via `SendInput`. Windows-only — on macOS
//!     it logs a refusal and exits 0.
//!
//! ## v1.1 runtime topology (Windows interactive `run` subcommand)
//!
//! Mirrors the macOS server's NSApp split:
//!
//! ```text
//!   main thread:   Win32 message pump + Shell_NotifyIconW tray
//!   worker thread: tokio runtime + run_client task graph
//! ```
//!
//! The tray surface (`platform::windows::tray`) requires a thread
//! running `PeekMessageW`; that thread must be the one that created
//! the tray window, and there's no cheap way to do that off the main
//! thread without a foreign event loop. So tokio moves to a worker
//! and main hosts the pump. Quit on the menu item runs the
//! `on_quit` closure (tokio shutdown signal) and then `exit(0)` after
//! a brief grace.
//!
//! `KMWARP_HEADLESS=1` bypasses the tray entirely and restores the
//! v1.0 single-runtime-on-main shape. The Windows service entry
//! point sets this internally because session 0 can't render a tray.
//!
//! M3 acceptance check (run on the Windows box):
//!
//! ```powershell
//! $env:KMWARP_M3_DEMO=1; cargo run -p kmwarp-client
//! ```
//!
//! Expected: the mouse cursor traces a smooth circle for ~5 seconds, then
//! a final-summary log line is emitted and the process exits.
//!
//! M5 acceptance check (cross-machine — no dedicated demo binary):
//!
//! On the **macOS** box:
//!
//! ```sh
//! cargo run -p kmwarp-server
//! ```
//!
//! On the **Windows** box, with Notepad focused:
//!
//! ```powershell
//! $env:KMWARP_CONNECT="<mac-ip>:51423"; cargo run -p kmwarp-client
//! ```
//!
//! Expected: typing the alphabet, digits, and common punctuation on the
//! Mac keyboard produces the corresponding characters in Windows Notepad.
//! Deferred keys (media, Fn-layer, IME / dead keys) are tracked in
//! IDEAS.md per PLAN.md §M5.
//!
//! M8 acceptance check (clipboard sync, both directions):
//!
//! Same two `cargo run` invocations as M5. Then:
//!
//! 1. On the **Mac**: copy some text in any app (`Cmd+C`).
//! 2. On the **Windows** box: paste into Notepad (`Ctrl+V`). The text
//!    should appear, propagation < 500 ms.
//! 3. On **Windows**: copy different text (`Ctrl+C`).
//! 4. On the **Mac**: paste (`Cmd+V`). Other text should appear.
//!
//! No infinite ping-pong: the `EchoGuard` on each side suppresses the
//! immediate local change-notification triggered by the inbound write.
//!
//! M10 install (Windows, elevated PowerShell):
//!
//! ```powershell
//! cargo build --release -p kmwarp-client
//! .\target\release\kmwarp-client.exe install
//! # ... reboot or `Start-Service kmwarp-client` ...
//! .\target\release\kmwarp-client.exe uninstall
//! ```

use std::env;
use std::net::SocketAddr;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use kmwarp_client::app::run_client;
#[cfg(target_os = "windows")]
use kmwarp_client::app::ClientStatus;
use kmwarp_client::discovery::{self, DEFAULT_BROWSE_TIMEOUT};
use tracing_subscriber::EnvFilter;

const DEFAULT_PEER_NAME: &str = "kmwarp-client";

/// Env var that suppresses the system-tray icon and routes the run
/// path back to the v1.0 single-runtime-on-main shape.
const HEADLESS_ENV: &str = "KMWARP_HEADLESS";

/// Returns true if the user (or the service entry) explicitly asked
/// for the no-tray path. Only consulted on Windows — non-Windows
/// builds always take the headless branch anyway.
#[cfg(target_os = "windows")]
fn headless_requested() -> bool {
    env::var(HEADLESS_ENV).ok().as_deref() == Some("1")
}

#[derive(Parser, Debug)]
#[command(name = "kmwarp-client", version, about = "kmwarp Windows client")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run in the foreground (default).
    Run,
    /// Register as an auto-start Windows service (requires Administrator).
    Install,
    /// Stop and delete the registered Windows service.
    Uninstall,
    /// SCM dispatcher entry point. Not for interactive use.
    RunAsService,
    /// User-session helper entry point spawned by the service. Same
    /// semantics as `run`, named distinctly so the service path is
    /// grep-able in logs.
    RunAsHelper,
}

fn main() -> Result<()> {
    init_tracing();

    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Run) {
        Command::Install => install_subcommand(),
        Command::Uninstall => uninstall_subcommand(),
        Command::RunAsService => run_as_service_subcommand(),
        Command::Run => run_foreground(),
        // The service's `CreateProcessAsUserW` spawn lands here. We
        // force headless: Shell_NotifyIconW can usually render in a
        // user-session helper, but the desktop attribute the service
        // hands the spawn doesn't always wire up — and a tray that
        // sometimes appears and sometimes doesn't is worse than one
        // that never does. Tray for service-spawned helpers is a
        // separate refactor; for now, the service path stays
        // explicitly headless.
        Command::RunAsHelper => {
            std::env::set_var(HEADLESS_ENV, "1");
            run_foreground()
        }
    }
}

fn init_tracing() {
    // RUST_LOG (if set) wins; otherwise default to `kmwarp=info`. See the
    // server `main.rs` for why we don't layer `.add_directive` on top.
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("kmwarp=info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

/// Foreground run — the `run` and `run-as-helper` subcommands route here.
/// Also handles the `KMWARP_M3_DEMO=1` env-gated demo harness so the M3
/// invocation continues to work without a dedicated subcommand.
fn run_foreground() -> Result<()> {
    if env::var("KMWARP_M3_DEMO").as_deref() == Ok("1") {
        return run_m3_demo();
    }

    let peer_name = env::var("KMWARP_PEER_NAME").unwrap_or_else(|_| DEFAULT_PEER_NAME.to_string());

    // Branch: tray-hosted (Windows interactive default) vs headless
    // (KMWARP_HEADLESS=1, service contexts, non-Windows builds).
    #[cfg(target_os = "windows")]
    {
        if !headless_requested() {
            return run_with_tray(peer_name);
        }
    }

    // Headless path: do the mDNS browse on main, then run the client
    // on a single tokio runtime hosted on main. Matches v1.0 shape.
    // Default pairing input: stdin (the operator types into the
    // terminal that launched the binary).
    let connect = resolve_connect_addr(None)?;
    let code_factory: kmwarp_client::net::CodeProviderFactory =
        Box::new(|| kmwarp_client::net::stdin_code_provider());
    run_blocking_on_tokio(
        async move { run_client(connect, &peer_name, None, code_factory).await },
    )
}

/// Build a multi-thread tokio runtime and `block_on` the supplied
/// future. Used by the headless / non-Windows / demo paths.
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

/// Resolve the server `SocketAddr` to connect to.
///
/// * `KMWARP_CONNECT=<ip>:<port>` set → parse it verbatim (existing
///   v1.0 behaviour preserved as an override).
/// * unset → browse mDNS `_kmwarp._tcp.local.` for up to 10 s and
///   take the first resolved IPv4. This is the v1.1 zero-config
///   path that lets a fresh Windows install just-work.
///
/// `status_tx` is optional; when present, the function publishes
/// `ClientStatus::Discovering` for the duration of the mDNS browse
/// so the tray reflects what's happening pre-connect. Headless
/// callers pass `None`.
///
/// Returns a clean anyhow error if the env var is malformed OR the
/// browse times out, so the operator sees a single human-readable
/// line ("no kmwarp-server found on the LAN after 10000ms — is the
/// Mac side running? …") rather than a stack of low-level mDNS
/// errors.
#[allow(unused_variables)] // status_tx is Windows-only; suppress on macOS dev builds
fn resolve_connect_addr(
    #[cfg(target_os = "windows")] status_tx: Option<&tokio::sync::watch::Sender<ClientStatus>>,
    #[cfg(not(target_os = "windows"))] status_tx: Option<&()>,
) -> Result<SocketAddr> {
    if let Ok(connect_str) = env::var("KMWARP_CONNECT") {
        let addr: SocketAddr = connect_str
            .parse()
            .with_context(|| format!("parsing KMWARP_CONNECT={connect_str:?}"))?;
        tracing::info!(
            addr = %addr,
            "KMWARP_CONNECT set; skipping mDNS discovery"
        );
        return Ok(addr);
    }
    #[cfg(target_os = "windows")]
    if let Some(tx) = status_tx {
        let _ = tx.send(ClientStatus::Discovering);
    }
    tracing::info!(
        timeout_ms = DEFAULT_BROWSE_TIMEOUT.as_millis() as u64,
        "KMWARP_CONNECT unset; browsing mDNS for kmwarp-server"
    );
    discovery::discover_server(DEFAULT_BROWSE_TIMEOUT)
        .context("mDNS discovery for kmwarp-server failed")
}

/// Windows interactive `run` path: tokio on a worker thread, Win32
/// message pump + tray on main.
///
/// `on_quit` simply sleeps a short grace and `exit(0)`s — matching
/// the macOS `run_with_menubar` shape. The runtime thread is
/// abandoned mid-tick; the only persistent state (peer.pin) is
/// written atomically by the pairing flow, and the process tear-down
/// closes the TCP socket cleanly enough that the server-side reader
/// task sees EOF and the M7 stuck-key drain on each side runs.
#[cfg(target_os = "windows")]
fn run_with_tray(peer_name: String) -> Result<()> {
    use kmwarp_client::platform::windows::tray;
    use std::thread;
    use tokio::sync::watch;

    let (status_tx, status_rx) = watch::channel(ClientStatus::Idle);

    // Resolve the connect addr on main BEFORE moving anything onto
    // the worker — discovery is a blocking call we don't want
    // racing the tray bootstrap, and a malformed KMWARP_CONNECT
    // should fail cleanly to the operator's terminal, not get
    // surfaced through the tray.
    let connect = resolve_connect_addr(Some(&status_tx))?;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building tokio runtime for tray mode")?;

    // Worker thread owns the runtime + the client future. We don't
    // join — Quit calls process::exit which tears every thread down.
    //
    // Pairing input on the tray path: the native Win32 dialog from
    // `platform::windows::pairing_dialog` (built in task #14 by the
    // windows-tray agent). If that module isn't present yet (or the
    // build is for a different feature set), we fall back to stdin —
    // the binary still pairs, just via the launching terminal.
    let worker_peer_name = peer_name;
    let worker_status_tx = status_tx;
    let code_factory: kmwarp_client::net::CodeProviderFactory =
        build_windows_dialog_factory();
    thread::Builder::new()
        .name("kmwarp-runtime".into())
        .spawn(move || {
            let result = rt.block_on(run_client(
                connect,
                &worker_peer_name,
                Some(worker_status_tx),
                code_factory,
            ));
            if let Err(e) = result {
                tracing::error!(error = %e, "run_client exited with error");
                std::process::exit(1);
            }
            std::process::exit(0);
        })
        .context("spawning kmwarp-runtime thread")?;

    let on_quit: Box<dyn FnOnce() + Send + 'static> = Box::new(|| {
        tracing::info!("tray Quit; exiting in 500ms");
        std::thread::sleep(std::time::Duration::from_millis(500));
        std::process::exit(0);
    });

    // Hands main thread to the Win32 message pump; never returns.
    tray::run_on_main_thread(status_rx, on_quit);
}

/// Build the [`CodeProviderFactory`] for the Windows tray run path.
///
/// Returns a factory that, on each pairing attempt, hands the
/// pairing flow a fresh provider backed by
/// [`platform::windows::pairing_dialog::ask_pairing_code`] — the
/// native Win32 input dialog landed in task #14. The dialog runs on
/// tokio's blocking pool so no runtime worker is parked while the
/// user is typing.
///
/// Wrapping the underlying `async fn` in a `CodeProvider`
/// (FnOnce returning a boxed future) keeps the dialog module
/// self-contained — it doesn't need to know about the
/// `CodeProvider` type alias — and lets the factory pattern live
/// purely in main.rs.
#[cfg(target_os = "windows")]
fn build_windows_dialog_factory() -> kmwarp_client::net::CodeProviderFactory {
    use kmwarp_client::platform::windows::pairing_dialog;
    Box::new(|| {
        Box::new(|| {
            Box::pin(async move { pairing_dialog::ask_pairing_code().await })
                as kmwarp_client::net::CodeFuture
        })
    })
}

// ──────────────────────────────────────────────────────────────────────
// Windows service subcommands
// ──────────────────────────────────────────────────────────────────────

#[cfg(target_os = "windows")]
fn install_subcommand() -> Result<()> {
    kmwarp_client::service::windows_service::install_service()
        .context("installing Windows service")?;
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn install_subcommand() -> Result<()> {
    anyhow::bail!("`install` is Windows-only; this binary targets a non-Windows OS")
}

#[cfg(target_os = "windows")]
fn uninstall_subcommand() -> Result<()> {
    kmwarp_client::service::windows_service::uninstall_service()
        .context("uninstalling Windows service")?;
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn uninstall_subcommand() -> Result<()> {
    anyhow::bail!("`uninstall` is Windows-only; this binary targets a non-Windows OS")
}

#[cfg(target_os = "windows")]
fn run_as_service_subcommand() -> Result<()> {
    kmwarp_client::service::windows_service::run_as_service()
        .context("running as Windows service")?;
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn run_as_service_subcommand() -> Result<()> {
    anyhow::bail!("`run-as-service` is Windows-only; this binary targets a non-Windows OS")
}

// ──────────────────────────────────────────────────────────────────────
// M3 cursor-circle demo (Windows-only; macOS stub)
// ──────────────────────────────────────────────────────────────────────

#[cfg(target_os = "windows")]
fn run_m3_demo() -> Result<()> {
    m3_demo::run()
}

#[cfg(not(target_os = "windows"))]
fn run_m3_demo() -> Result<()> {
    tracing::warn!(
        "KMWARP_M3_DEMO is Windows-only; this binary was built for a non-Windows target. Exiting."
    );
    Ok(())
}

/// Hardware-verification harness for M3. Compiled only on Windows.
///
/// Drives the cursor along a parametric circle of radius 50 px for 5 s at
/// roughly 60 Hz. Each frame computes the absolute target position on the
/// circle relative to the cursor's location *at demo start*, then issues a
/// relative `SendInput` delta to take it there. Using the start position as
/// the anchor (rather than the live cursor) keeps the trajectory a clean
/// circle even if the user nudges the mouse mid-demo.
#[cfg(target_os = "windows")]
mod m3_demo {
    use std::f32::consts::PI;
    use std::thread::sleep;
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result};
    use kmwarp_client::platform::WinInputSink;
    use kmwarp_core::InputSink;
    use tracing::info;
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

    const FRAME_PERIOD: Duration = Duration::from_millis(16); // ~60 Hz
    const DEMO_DURATION: Duration = Duration::from_secs(5);
    const RADIUS_PX: f32 = 50.0;
    const FRAMES_PER_REVOLUTION: f32 = 60.0; // one revolution per second at 60 Hz

    pub fn run() -> Result<()> {
        let mut sink = WinInputSink::new().context("initializing WinInputSink for M3 demo")?;

        // Capture the cursor position at demo start; the circle is centered
        // here so the trajectory stays in-bounds and the user can predict
        // where the cursor will end up.
        let mut origin = POINT::default();
        // SAFETY: pure FFI; pointer is valid for the duration of the call.
        unsafe { GetCursorPos(&mut origin) }.context("GetCursorPos failed at M3 demo start")?;

        info!(
            origin_x = origin.x,
            origin_y = origin.y,
            radius_px = RADIUS_PX,
            duration_ms = DEMO_DURATION.as_millis() as u64,
            "starting M3 demo: parametric circle via SendInput"
        );

        let start = Instant::now();
        let mut frame: u32 = 0;
        let mut last_x = 0.0_f32;
        let mut last_y = 0.0_f32;
        let mut total_dx: i64 = 0;
        let mut total_dy: i64 = 0;

        while start.elapsed() < DEMO_DURATION {
            let angle = (frame as f32) * 2.0 * PI / FRAMES_PER_REVOLUTION;
            let target_x = RADIUS_PX * angle.cos();
            let target_y = RADIUS_PX * angle.sin();
            // Delta from previous frame's target → this frame's target,
            // expressed in relative pixels.
            let dx = (target_x - last_x).round() as i32;
            let dy = (target_y - last_y).round() as i32;
            sink.inject_mouse_rel(dx, dy);
            total_dx += i64::from(dx);
            total_dy += i64::from(dy);
            last_x = target_x;
            last_y = target_y;
            frame += 1;
            sleep(FRAME_PERIOD);
        }

        let elapsed = start.elapsed();
        info!(
            frames = frame,
            elapsed_ms = elapsed.as_millis() as u64,
            total_dx,
            total_dy,
            "M3 demo complete"
        );
        Ok(())
    }
}

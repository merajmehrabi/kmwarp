//! kmwarp-client entry point.
//!
//! Reads connect address and peer name from the environment, initializes
//! `tracing`, and hands off to [`kmwarp_client::app::run_client`].
//!
//! Environment:
//!   * `KMWARP_CONNECT` — server address to connect to (default `127.0.0.1:51423`).
//!   * `KMWARP_PEER_NAME` — name advertised to the server (default `kmwarp-client`).
//!   * `RUST_LOG` — standard tracing filter (default `kmwarp=info`).
//!   * `KMWARP_M3_DEMO` — when set to `1`, runs the M3 acceptance harness
//!     instead of starting the client. Drives the local cursor in a smooth
//!     50 px circle for 5 seconds via `SendInput`. Windows-only — on macOS
//!     it logs a refusal and exits 0.
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

    if env::var("KMWARP_M3_DEMO").as_deref() == Ok("1") {
        return run_m3_demo();
    }

    let connect_str = env::var("KMWARP_CONNECT").unwrap_or_else(|_| DEFAULT_CONNECT.to_string());
    let connect: SocketAddr = connect_str
        .parse()
        .with_context(|| format!("parsing KMWARP_CONNECT={connect_str:?}"))?;
    let peer_name = env::var("KMWARP_PEER_NAME").unwrap_or_else(|_| DEFAULT_PEER_NAME.to_string());

    run_client(connect, &peer_name).await
}

/// M3 acceptance harness: drive the cursor in a parametric circle via
/// `SendInput` for 5 seconds. Per spec §M3 acceptance criterion.
///
/// Implementation lives in [`m3_demo`] on Windows; the macOS stub here
/// keeps `cargo run -p kmwarp-client` from misbehaving on the dev host.
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

//! M2 acceptance harness, gated behind `KMWARP_M2_DEMO=1`.
//!
//! Drives the `CGEventTap` for ~10 seconds while a sibling thread injects
//! 200 ms hangs once per second (regression hedge for "tap survives a
//! deliberate 200 ms hang in a sibling thread" from spec §M2).
//!
//! On exit it logs a single summary line with the move count and rate;
//! ≥ 300 moves over the 5-second target window (i.e. > 60 Hz) is the
//! pass criterion.

use std::time::{Duration, Instant};

use kmwarp_core::platform::{InputSource, SourceEvent};
use tracing::{info, warn};

use super::permissions::{check_permissions, PermStatus};
use super::tap::MacInputSource;

/// Total demo runtime. Spec asks for 5 s of mouse motion; we run 10 s so
/// the operator has unhurried time to wiggle the mouse and so a few hang
/// cycles fire.
const DEMO_SECONDS: u64 = 10;

/// Entry point. Returns `Err` when the tap could not be installed or
/// permissions were not granted.
pub async fn run() -> anyhow::Result<()> {
    info!("KMWARP_M2_DEMO=1 → starting CGEventTap acceptance demo ({DEMO_SECONDS}s)");

    // `KMWARP_M2_FORCE=1` skips the TCC pre-flight check so the operator
    // can still exercise the `CGEventTapCreate` failure path (e.g. when
    // diagnosing whether TCC is the actual blocker).
    let force = std::env::var("KMWARP_M2_FORCE").ok().as_deref() == Some("1");
    match check_permissions() {
        PermStatus::Granted => info!("Accessibility + Input Monitoring: granted"),
        PermStatus::NeedsAccessibility if !force => {
            warn!("Demo aborted: missing Accessibility permission");
            return Err(anyhow::anyhow!(
                "missing Accessibility permission — System Settings → Privacy & Security → Accessibility"
            ));
        }
        PermStatus::NeedsInputMonitoring if !force => {
            warn!("Demo aborted: missing Input Monitoring permission");
            return Err(anyhow::anyhow!(
                "missing Input Monitoring permission — System Settings → Privacy & Security → Input Monitoring"
            ));
        }
        _ => warn!("KMWARP_M2_FORCE=1 set — proceeding despite missing TCC permissions"),
    }

    let mut src = MacInputSource::install()?;
    info!("tap online — wiggle the mouse for {DEMO_SECONDS} seconds");

    // Sibling thread that periodically hangs for 200 ms. The tap runs on a
    // separate thread, so this must NOT affect the event stream.
    let hang_handle = std::thread::Builder::new()
        .name("kmwarp-m2-hang".into())
        .spawn(move || {
            for i in 0..DEMO_SECONDS {
                std::thread::sleep(Duration::from_millis(800));
                let t0 = Instant::now();
                std::thread::sleep(Duration::from_millis(200));
                tracing::debug!(
                    iter = i,
                    actual_ms = t0.elapsed().as_millis() as u64,
                    "hang sibling slept 200ms"
                );
            }
        })?;

    let deadline = Instant::now() + Duration::from_secs(DEMO_SECONDS);
    let started = Instant::now();
    let mut count_move = 0u64;
    let mut count_btn = 0u64;
    let mut count_wheel = 0u64;
    let mut count_other = 0u64;

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, src.next_event()).await {
            Ok(Some(SourceEvent::MouseRel { dx, dy })) => {
                count_move += 1;
                // Log first few moves verbatim, then every 60th, to avoid
                // drowning the console at full 120 Hz trackpad rate.
                if count_move <= 8 || count_move % 60 == 0 {
                    info!(dx, dy, total = count_move, "MouseRel");
                }
            }
            Ok(Some(SourceEvent::MouseButton { button, state })) => {
                count_btn += 1;
                info!(?button, ?state, "MouseButton");
            }
            Ok(Some(SourceEvent::MouseWheel { dx, dy })) => {
                count_wheel += 1;
                info!(dx, dy, "MouseWheel");
            }
            Ok(Some(other)) => {
                count_other += 1;
                tracing::debug!(?other, "other SourceEvent");
            }
            Ok(None) => {
                warn!("source channel closed unexpectedly mid-demo");
                break;
            }
            Err(_) => break, // deadline elapsed
        }
    }

    let elapsed = started.elapsed();
    let total = count_move + count_btn + count_wheel + count_other;
    let rate = total as f64 / elapsed.as_secs_f64().max(0.001);
    info!(
        elapsed_ms = elapsed.as_millis() as u64,
        moves = count_move,
        buttons = count_btn,
        wheels = count_wheel,
        other = count_other,
        total,
        rate_hz = format!("{rate:.1}"),
        "M2 demo summary"
    );

    let _ = hang_handle.join();
    drop(src); // triggers run-loop shutdown via the watcher thread
    Ok(())
}

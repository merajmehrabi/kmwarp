//! M5 keyboard-capture acceptance harness, gated behind `KMWARP_M5_DEMO=1`.
//!
//! Installs the same `MacInputSource` the real server uses, then pulls
//! events for ~15 seconds and `info!`s every `SourceEvent::Key` it sees.
//! Mouse events are tallied but silenced so the log stays readable while
//! the operator types into the launching terminal.
//!
//! Hardware acceptance (per spec §M5) is "typing alphabet+numbers+
//! common punctuation on Mac produces correct characters in Windows
//! Notepad" — that needs the full cross-machine pipe. This demo lets the
//! operator sanity-check the *capture* side in isolation first.
//!
//! Pass criteria for the standalone run:
//!   - alphabet keys produce `Key { hid_usage: 0x04..=0x1D, … }` with
//!     `state: Down` then `state: Up` per physical press;
//!   - Shift, Ctrl, Cmd, Alt produce `Key { hid_usage: 0xE0..=0xE7, … }`
//!     pairs with the aggregate `mods` field reflecting whatever is
//!     held at the moment;
//!   - holding a letter does NOT spam `Down` events (autorepeat filter
//!     working) — exactly one `Down`, then one `Up` on release.

use std::time::{Duration, Instant};

use kmwarp_core::platform::{InputSource, SourceEvent};
use tracing::{info, warn};

use super::permissions::{check_permissions, PermStatus};
use super::tap::MacInputSource;

/// Total demo runtime. Spec doesn't quantify; 15 s is enough time to
/// type "The quick brown fox jumps over the lazy dog 1234567890" plus
/// a few modifier chords.
const DEMO_SECONDS: u64 = 15;

/// Entry point. Returns `Err` when the tap could not be installed or
/// permissions were not granted.
pub async fn run() -> anyhow::Result<()> {
    info!("KMWARP_M5_DEMO=1 → starting keyboard-capture acceptance demo ({DEMO_SECONDS}s)");

    // Same `KMWARP_M5_FORCE=1` escape hatch as the M2 demo — lets the
    // operator exercise the `CGEventTapCreate` failure path even when
    // TCC is the suspected blocker.
    let force = std::env::var("KMWARP_M5_FORCE").ok().as_deref() == Some("1");
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
        _ => warn!("KMWARP_M5_FORCE=1 set — proceeding despite missing TCC permissions"),
    }

    let mut src = MacInputSource::install()?;
    info!("tap online — type into the launching terminal for {DEMO_SECONDS} seconds");

    let deadline = Instant::now() + Duration::from_secs(DEMO_SECONDS);
    let started = Instant::now();
    let mut count_key_down = 0u64;
    let mut count_key_up = 0u64;
    let mut count_mouse = 0u64;

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, src.next_event()).await {
            Ok(Some(SourceEvent::Key {
                hid_usage,
                state,
                mods,
            })) => {
                match state {
                    kmwarp_core::KeyState::Down => count_key_down += 1,
                    kmwarp_core::KeyState::Up => count_key_up += 1,
                }
                info!(
                    hid = format!("0x{hid_usage:02X}"),
                    ?state,
                    mods = format!("0x{:02X}", mods.0),
                    "Key"
                );
            }
            Ok(Some(SourceEvent::MouseRel { .. }))
            | Ok(Some(SourceEvent::MouseButton { .. }))
            | Ok(Some(SourceEvent::MouseWheel { .. })) => {
                count_mouse += 1;
                // Stay silent: the M5 demo is for keyboard observation
                // and the operator will likely move the cursor while
                // reaching for keys. M2's demo covers the mouse side.
            }
            Ok(Some(other)) => {
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
    info!(
        elapsed_ms = elapsed.as_millis() as u64,
        key_down = count_key_down,
        key_up = count_key_up,
        mouse_events_suppressed = count_mouse,
        "M5 demo summary"
    );

    drop(src); // triggers run-loop shutdown via the watcher thread
    Ok(())
}

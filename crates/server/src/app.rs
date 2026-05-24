//! Top-level server runtime.
//!
//! ## Per-peer task topology (M4)
//!
//! After the handshake, the per-peer session spins up the following tasks,
//! all managed by a single [`JoinSet`]. First exit aborts the rest so logs
//! land coherently.
//!
//! ```text
//!  ┌───────────────────────────┐   tx_out (mpsc 256, bounded)
//!  │ heartbeat_producer        │ ─────────────┐
//!  └───────────────────────────┘              │
//!  ┌───────────────────────────┐              ▼
//!  │ mouse_pump (cfg macos)    │ ─────► ┌─────────────────┐
//!  │  MacInputSource → Message │        │ encoder_loop    │ ─────► socket
//!  └───────────────────────────┘        │ (owns FrameWri.) │
//!  ┌───────────────────────────┐        └─────────────────┘
//!  │ latency_prober (cfg feat) │ ────────────┘
//!  └───────────────────────────┘
//!
//!  socket ─────► ┌─────────────────────┐
//!                │ reader_task         │ ─ notify (Notify) ─► deadline_watcher
//!                │  decode + dispatch  │
//!                │  EchoPing → EchoPong│ ── tx_out (cloned) ──┐
//!                │  EchoPong → RTT     │                       │
//!                └─────────────────────┘                       ▼
//!                                                       encoder_loop
//! ```
//!
//! The bounded `mpsc::channel(256)` is the natural backpressure point per
//! PLAN.md §Async channel topology. Producers `try_send` and warn-and-drop
//! on full; the encoder's socket write is the throttle.
//!
//! ## CGEventTap ownership
//!
//! [`MacInputSource::install`] is called per accepted peer. v1 is
//! single-peer, so contention is theoretical; if two peers happen to
//! connect, both install taps and both receive every event independently
//! (the OS happily multiplexes). That's not desirable long-term but it
//! does not corrupt state, and a single config-locked peer is M9's
//! enforcement story. Install runs via [`tokio::task::spawn_blocking`]
//! because it briefly blocks on a `std::sync::mpsc` handshake with the
//! run-loop thread.
//!
//! ## Latency probe (`--features latency-probe`)
//!
//! When the feature is on, a [`latency_prober`] task fires `EchoPing` at
//! [`LATENCY_PROBE_PERIOD`]; the reader records the inferred RTT into a
//! shared `Histogram` and prints p50/p95/p99 every
//! [`LATENCY_PROBE_SAMPLES_PER_REPORT`] samples. The wire variants exist
//! unconditionally so a probe-enabled server can converse with a stock
//! client (the client always echoes).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use kmwarp_core::wire::Message;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Notify};
use tokio::task::JoinSet;
use tokio::time::{interval, timeout, MissedTickBehavior};
use tracing::{debug, error, info, trace, warn};

use crate::error::ServerError;
use crate::net::{encoder_loop, Connection, FrameReader};

/// Heartbeat cadence; spec §M1 mandates 500 ms.
const HEARTBEAT_PERIOD: Duration = Duration::from_millis(500);

/// Silence budget before declaring the peer dead; spec §M1 mandates 2 s.
const SILENCE_DEADLINE: Duration = Duration::from_secs(2);

/// Outbound mpsc bound. Encoder is the natural backpressure point per
/// PLAN.md §Async channel topology; producers `try_send` and drop with
/// `warn!` if this fills (a flatlined socket).
const OUTBOUND_CHANNEL_BOUND: usize = 256;

/// Placeholder server-screen size returned in `HelloAck`. The real value
/// comes from `core-graphics` in M2 and from `Config` in M6.
const PLACEHOLDER_SCREEN_PX: (u16, u16) = (1920, 1080);

/// Mouse pump → outbound coalescing threshold. When the bounded outbound
/// channel is full, a `MouseMoveRel` `try_send` failure triggers an
/// in-place coalesce: the next dropped move's delta is merged into the
/// most recent buffered move. PLAN.md §Async channel topology calls out
/// "coalesces consecutive `MouseMoveRel` if backlog > 100" — we keep the
/// rule lighter here (drop with warn on overflow) since the worst-case
/// CGEventTap rate (~240 Hz) is well below the encoder's drain rate; this
/// constant exists to make the drop-rate noticeable in logs.
const MOUSE_DROP_LOG_EVERY: u64 = 100;

/// Latency-probe send cadence. 10 ms gives ~100 samples per second; a
/// 10-second run yields ~1000 samples, matching the
/// "report every 1000 samples" cadence below for a clean per-run
/// histogram print.
#[cfg(feature = "latency-probe")]
const LATENCY_PROBE_PERIOD: Duration = Duration::from_millis(10);

/// Print histogram percentiles every N RTT samples.
#[cfg(feature = "latency-probe")]
const LATENCY_PROBE_SAMPLES_PER_REPORT: u64 = 1000;

/// Bind, accept connections forever, and run the M4 input pipe per peer.
///
/// Returns `Err` only if the initial bind fails. Per-peer failures are
/// logged inside the spawned task and isolated to that peer.
pub async fn run_server(bind: SocketAddr, peer_name: &str) -> anyhow::Result<()> {
    let listener = TcpListener::bind(bind)
        .await
        .with_context(|| format!("binding kmwarp-server to {bind}"))?;
    let local_addr = listener.local_addr().unwrap_or(bind);
    info!(addr = %local_addr, "kmwarp-server listening on {local_addr}");

    let peer_name: Arc<str> = Arc::from(peer_name);

    loop {
        let (stream, remote) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                error!(error = %e, "accept failed; continuing");
                continue;
            }
        };
        let peer_name = Arc::clone(&peer_name);
        tokio::spawn(async move {
            if let Err(e) = handle_peer(stream, remote, peer_name).await {
                warn!(peer = %remote, error = %e, "peer session ended with error");
            }
        });
    }
}

/// Per-peer session: handshake, install platform source, run the M4 task
/// graph.
async fn handle_peer(
    stream: TcpStream,
    remote: SocketAddr,
    server_peer_name: Arc<str>,
) -> Result<(), ServerError> {
    info!(peer = %remote, "peer connected");

    let mut conn = Connection::new(stream)?;

    let hello = conn.read_frame().await?;
    match &hello {
        Message::Hello {
            proto_version,
            peer_name,
        } => {
            info!(
                peer = %remote,
                proto_version,
                peer_name = %peer_name,
                "received Hello"
            );
        }
        other => {
            warn!(peer = %remote, ?other, "expected Hello, got something else; closing");
            return Ok(());
        }
    }

    conn.write_frame(&Message::HelloAck {
        accepted: true,
        server_screen_px: PLACEHOLDER_SCREEN_PX,
    })
    .await?;
    debug!(
        peer = %remote,
        "sent HelloAck (server={}, screen={:?})",
        server_peer_name, PLACEHOLDER_SCREEN_PX
    );

    let (reader, writer) = conn.into_split();
    let (tx_out, rx_out) = mpsc::channel::<Message>(OUTBOUND_CHANNEL_BOUND);

    let notify = Arc::new(Notify::new());
    let start = Arc::new(Instant::now());

    #[cfg(feature = "latency-probe")]
    let latency_state = Arc::new(LatencyState::new());

    let mut set: JoinSet<TaskExit> = JoinSet::new();

    // Encoder owns the writer; drains tx_out / rx_out forever.
    set.spawn(spawn_encoder(remote, rx_out, writer));

    // Heartbeat producer: pushes Heartbeat into tx_out every 500 ms.
    set.spawn(heartbeat_producer(remote, tx_out.clone()));

    // Reader: decode + dispatch. Pulses notify on every successful read,
    // routes EchoPing → EchoPong, optionally records EchoPong RTT.
    set.spawn(reader_task(
        remote,
        reader,
        Arc::clone(&notify),
        tx_out.clone(),
        Arc::clone(&start),
        #[cfg(feature = "latency-probe")]
        Arc::clone(&latency_state),
    ));

    // Deadline watcher: 2 s silence budget.
    set.spawn(deadline_watcher(remote, notify));

    // Mouse pump (cfg macOS only) — installs CGEventTap and forwards.
    #[cfg(target_os = "macos")]
    spawn_mouse_pump(&mut set, remote, tx_out.clone()).await;

    // Latency probe sender — only with feature, runs alongside.
    #[cfg(feature = "latency-probe")]
    set.spawn(latency_prober(
        remote,
        tx_out.clone(),
        Arc::clone(&start),
        Arc::clone(&latency_state),
    ));

    // Drop the original sender so the channel can close when all task-held
    // clones are dropped (graceful drain on shutdown).
    drop(tx_out);

    run_until_first_exit(remote, set).await;
    info!(peer = %remote, "peer session ended");
    Ok(())
}

/// Wait for any task to exit, then abort + drain siblings so logs land
/// in a coherent order before returning.
async fn run_until_first_exit(remote: SocketAddr, mut set: JoinSet<TaskExit>) {
    if let Some(joined) = set.join_next().await {
        let exit = joined.unwrap_or(TaskExit::JoinError);
        log_exit(remote, exit);
    }
    set.abort_all();
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok(exit) => debug!(peer = %remote, ?exit, "sibling task drained"),
            Err(e) if e.is_cancelled() => {}
            Err(e) => debug!(peer = %remote, error = %e, "sibling task join error"),
        }
    }
}

/// Discriminant for which sub-task exited and why.
#[derive(Debug)]
enum TaskExit {
    EncoderFailed(String),
    EncoderClosed,
    HeartbeatFailed(String),
    ReaderFailed(String),
    DeadlineExpired,
    MousePumpFailed(String),
    #[cfg(feature = "latency-probe")]
    LatencyProberFailed(String),
    JoinError,
}

fn log_exit(remote: SocketAddr, exit: TaskExit) {
    match exit {
        TaskExit::EncoderFailed(reason) => {
            warn!(peer = %remote, reason, "encoder failed; tearing down")
        }
        TaskExit::EncoderClosed => {
            debug!(peer = %remote, "encoder channel closed; tearing down")
        }
        TaskExit::HeartbeatFailed(reason) => {
            warn!(peer = %remote, reason, "heartbeat producer failed; tearing down")
        }
        TaskExit::ReaderFailed(reason) => {
            warn!(peer = %remote, reason, "reader task failed; tearing down")
        }
        TaskExit::DeadlineExpired => {
            // Already logged inside the watcher.
            debug!(peer = %remote, "deadline watcher fired");
        }
        TaskExit::MousePumpFailed(reason) => {
            warn!(peer = %remote, reason, "mouse pump exited; tearing down")
        }
        #[cfg(feature = "latency-probe")]
        TaskExit::LatencyProberFailed(reason) => {
            warn!(peer = %remote, reason, "latency prober exited; tearing down")
        }
        TaskExit::JoinError => warn!(peer = %remote, "task join error"),
    }
}

/// Encoder wrapper that maps the lifecycle into a [`TaskExit`] for the
/// `JoinSet`'s uniform handling.
async fn spawn_encoder(
    _remote: SocketAddr,
    rx: mpsc::Receiver<Message>,
    writer: crate::net::FrameWriter,
) -> TaskExit {
    match encoder_loop(rx, writer).await {
        Ok(()) => TaskExit::EncoderClosed,
        Err(e) => TaskExit::EncoderFailed(e.to_string()),
    }
}

/// Push a `Heartbeat { seq }` into `tx` every [`HEARTBEAT_PERIOD`].
///
/// Uses `try_send` because heartbeats are recovery probes — losing one
/// because the encoder is wedged is acceptable; blocking on a full
/// outbound channel would defeat the deadline watcher's purpose.
async fn heartbeat_producer(remote: SocketAddr, tx: mpsc::Sender<Message>) -> TaskExit {
    let mut ticker = interval(HEARTBEAT_PERIOD);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut seq: u32 = 0;
    loop {
        ticker.tick().await;
        let msg = Message::Heartbeat { seq };
        if tx.is_closed() {
            return TaskExit::HeartbeatFailed("outbound channel closed".into());
        }
        match tx.try_send(msg) {
            Ok(()) => {
                trace!(peer = %remote, seq, "queued Heartbeat");
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                warn!(peer = %remote, seq, "outbound full; dropping Heartbeat")
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                return TaskExit::HeartbeatFailed("outbound channel closed".into())
            }
        }
        seq = seq.wrapping_add(1);
    }
}

/// Continuously decode frames, pulse the deadline notifier, route
/// `EchoPing` → `EchoPong`, and (with `latency-probe`) feed `EchoPong`
/// RTTs into the histogram.
async fn reader_task(
    remote: SocketAddr,
    mut reader: FrameReader,
    notify: Arc<Notify>,
    tx_out: mpsc::Sender<Message>,
    start: Arc<Instant>,
    #[cfg(feature = "latency-probe")] latency: Arc<LatencyState>,
) -> TaskExit {
    loop {
        match reader.read_frame().await {
            Ok(msg) => {
                notify.notify_one();
                trace!(peer = %remote, ?msg, "received frame");
                match msg {
                    Message::EchoPing { ts_ns } => {
                        let response = Message::EchoPong { ts_ns };
                        if let Err(e) = tx_out.try_send(response) {
                            warn!(peer = %remote, error = ?e, "failed to enqueue EchoPong");
                        }
                    }
                    #[cfg(feature = "latency-probe")]
                    Message::EchoPong { ts_ns } => {
                        let now_ns = start.elapsed().as_nanos() as u64;
                        let rtt_ns = now_ns.saturating_sub(ts_ns);
                        latency.record(rtt_ns, remote);
                    }
                    #[cfg(not(feature = "latency-probe"))]
                    Message::EchoPong { .. } => {
                        // Without the prober we don't expect pongs; ignore.
                    }
                    Message::Bye { reason_code } => {
                        info!(peer = %remote, reason_code, "peer sent Bye");
                        return TaskExit::ReaderFailed("peer Bye".into());
                    }
                    // Heartbeat / Mouse / Key / Clipboard / TakeControl /
                    // ReleaseControl from the client: deadline reset is the
                    // only handling needed in M4. M6 reacts to ReleaseControl;
                    // M8 to ClipboardText; etc.
                    _ => {}
                }
                // start is read on the latency-probe path; mark it unused
                // here so the compiler doesn't warn when the feature is off.
                #[cfg(not(feature = "latency-probe"))]
                let _ = &start;
            }
            Err(e) => return TaskExit::ReaderFailed(e.to_string()),
        }
    }
}

async fn deadline_watcher(remote: SocketAddr, notify: Arc<Notify>) -> TaskExit {
    loop {
        match timeout(SILENCE_DEADLINE, notify.notified()).await {
            Ok(()) => continue,
            Err(_) => {
                warn!(peer = %remote, "peer silent for 2s; declaring dead");
                return TaskExit::DeadlineExpired;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// macOS mouse pump
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
async fn spawn_mouse_pump(
    set: &mut JoinSet<TaskExit>,
    remote: SocketAddr,
    tx_out: mpsc::Sender<Message>,
) {
    use crate::platform::macos::MacInputSource;

    // `install` does brief blocking work (std mpsc handshake with the
    // CFRunLoop thread); spawn_blocking keeps the tokio worker free.
    let install_result = tokio::task::spawn_blocking(MacInputSource::install).await;
    match install_result {
        Ok(Ok(source)) => {
            info!(peer = %remote, "CGEventTap installed for mouse forwarding");
            set.spawn(mouse_pump_task(remote, source, tx_out));
        }
        Ok(Err(e)) => {
            warn!(
                peer = %remote,
                error = %e,
                "failed to install CGEventTap; continuing with heartbeats only"
            );
        }
        Err(e) => {
            error!(peer = %remote, error = %e, "spawn_blocking for tap install panicked");
        }
    }
}

#[cfg(target_os = "macos")]
async fn mouse_pump_task(
    remote: SocketAddr,
    mut source: crate::platform::macos::MacInputSource,
    tx_out: mpsc::Sender<Message>,
) -> TaskExit {
    use kmwarp_core::platform::InputSource;
    use kmwarp_core::wire::source_event_to_message;

    let mut drops: u64 = 0;
    loop {
        let ev = match source.next_event().await {
            Some(e) => e,
            None => return TaskExit::MousePumpFailed("MacInputSource channel closed".into()),
        };
        let Some(msg) = source_event_to_message(ev) else {
            continue;
        };
        match tx_out.try_send(msg) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(dropped)) => {
                drops = drops.saturating_add(1);
                if drops == 1 || drops % MOUSE_DROP_LOG_EVERY == 0 {
                    warn!(
                        peer = %remote,
                        ?dropped,
                        drops,
                        "outbound full; dropping mouse frame"
                    );
                }
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                return TaskExit::MousePumpFailed("outbound channel closed".into())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Latency probe
// ---------------------------------------------------------------------------

#[cfg(feature = "latency-probe")]
struct LatencyState {
    inner: std::sync::Mutex<LatencyStateInner>,
}

#[cfg(feature = "latency-probe")]
struct LatencyStateInner {
    /// 1 ns → 60 s window, 3 significant digits. 60s headroom covers any
    /// pathological RTT without saturating the histogram.
    hist: hdrhistogram::Histogram<u64>,
    samples_since_report: u64,
    total_samples: u64,
}

#[cfg(feature = "latency-probe")]
impl LatencyState {
    fn new() -> Self {
        let hist = hdrhistogram::Histogram::<u64>::new_with_bounds(1, 60 * 1_000_000_000, 3)
            .expect("hdrhistogram bounds are valid");
        Self {
            inner: std::sync::Mutex::new(LatencyStateInner {
                hist,
                samples_since_report: 0,
                total_samples: 0,
            }),
        }
    }

    fn record(&self, rtt_ns: u64, peer: std::net::SocketAddr) {
        let Ok(mut g) = self.inner.lock() else {
            return;
        };
        // hdrhistogram returns Err only if value exceeds the configured
        // upper bound; saturate at the bound and continue.
        let v = rtt_ns.min(60 * 1_000_000_000);
        if g.hist.record(v).is_err() {
            warn!(rtt_ns, "latency probe sample exceeded histogram bound");
        }
        g.samples_since_report += 1;
        g.total_samples += 1;
        if g.samples_since_report >= LATENCY_PROBE_SAMPLES_PER_REPORT {
            let p50 = g.hist.value_at_quantile(0.50);
            let p95 = g.hist.value_at_quantile(0.95);
            let p99 = g.hist.value_at_quantile(0.99);
            let max = g.hist.max();
            info!(
                peer = %peer,
                samples = g.total_samples,
                p50_us = p50 / 1000,
                p95_us = p95 / 1000,
                p99_us = p99 / 1000,
                max_us = max / 1000,
                "latency-probe rtt percentiles"
            );
            g.samples_since_report = 0;
        }
    }
}

#[cfg(feature = "latency-probe")]
async fn latency_prober(
    remote: SocketAddr,
    tx_out: mpsc::Sender<Message>,
    start: Arc<Instant>,
    _state: Arc<LatencyState>,
) -> TaskExit {
    let mut ticker = interval(LATENCY_PROBE_PERIOD);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        let ts_ns = start.elapsed().as_nanos() as u64;
        let msg = Message::EchoPing { ts_ns };
        match tx_out.try_send(msg) {
            Ok(()) => trace!(peer = %remote, ts_ns, "queued EchoPing"),
            Err(mpsc::error::TrySendError::Full(_)) => {
                warn!(peer = %remote, "outbound full; dropping EchoPing")
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                return TaskExit::LatencyProberFailed("outbound channel closed".into())
            }
        }
    }
}

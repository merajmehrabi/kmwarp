//! Top-level server runtime.
//!
//! ## Per-peer task topology (M6)
//!
//! After the handshake, the per-peer session spins up the following tasks,
//! all managed by a single [`JoinSet`]. First exit aborts the rest so logs
//! land coherently.
//!
//! ```text
//!  ┌───────────────────────────┐   tx_out (mpsc 256, bounded)
//!  │ heartbeat_producer        │ ─────────────┐
//!  └───────────────────────────┘              │
//!  ┌───────────────────────────┐              │
//!  │ source_forwarder (macos)  │              │
//!  │  MacInputSource           │              │
//!  └─────────────┬─────────────┘              │
//!                │ brain_tx (mpsc 256)        │
//!                ▼                            ▼
//!  ┌───────────────────────────┐    ┌─────────────────┐
//!  │ edge_brain (macos)        │ ─► │ encoder_loop    │ ─► socket
//!  │  StateMachine + sink      │    │ (owns FrameWri.) │
//!  └───────────────────────────┘    └─────────────────┘
//!  ┌───────────────────────────┐              ▲
//!  │ latency_prober (cfg feat) │ ─────────────┘
//!  └───────────────────────────┘
//!
//!  socket ─► ┌─────────────────────────┐
//!            │ reader_task             │ ─ notify ─► deadline_watcher
//!            │  decode + dispatch      │
//!            │  EchoPing → EchoPong    │ ── tx_out (cloned) ──┐
//!            │  EchoPong → RTT         │                       │
//!            │  ReleaseControl → brain │ ── brain_tx (cloned) ─┘
//!            └─────────────────────────┘
//! ```
//!
//! The bounded `mpsc::channel(256)` is the natural backpressure point per
//! PLAN.md §Async channel topology. Producers `try_send` and warn-and-drop
//! on full; the encoder's socket write is the throttle.
//!
//! ## M6 acceptance test (manual, real hardware)
//!
//! 1. Start `kmwarp-server` on the Mac and `kmwarp-client` on the Windows
//!    PC; both processes have the relevant TCC / Windows permissions.
//! 2. Move the Mac cursor to the right edge of the screen — the Mac
//!    cursor disappears (CGDisplayHideCursor + warp 5 px back), and the
//!    Windows cursor begins moving in response to physical mouse motion.
//! 3. From Windows, move the cursor to the left edge — control returns
//!    to the Mac (the client sends `ReleaseControl { exit_y }`); the Mac
//!    cursor reappears 5 px in from the right edge at the reported `y`.
//! 4. Hold Shift across a crossing — Shift release is synthesized on the
//!    Windows side as part of the `on_release_control` drain; nothing
//!    sticks.
//!
//! ## M8 acceptance test (manual, real hardware)
//!
//! 1. With server + client running and connected:
//!    Copy "hello clipboard" on the Mac (`Cmd+C` after selecting). Within
//!    ~500 ms, paste on Windows Notepad — text appears.
//! 2. Copy "world from win" on Windows. Within ~500 ms, paste on Mac
//!    (`Cmd+V`) — text appears.
//! 3. Verify no echo loop: repeat the Mac copy and confirm Windows shows
//!    the text only once (the SHA-256 `EchoGuard` suppresses the
//!    post-write watcher tick).
//! 4. Copy a long string (> 4 KiB) on the Mac — chunked frames assemble
//!    cleanly on the Windows side without truncation.
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
use kmwarp_core::tls::{cert, PinStore};
use kmwarp_core::wire::Message;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Notify};
use tokio::task::JoinSet;
use tokio::time::{interval, timeout, MissedTickBehavior};
use tokio_rustls::TlsAcceptor;
use tracing::{debug, error, info, trace, warn};

#[cfg(target_os = "macos")]
use std::sync::atomic::AtomicBool;

use crate::error::ServerError;
use crate::net::{encoder_loop, run_server_pairing_flow, Connection, FrameReader};
use crate::tls::{build_server_config, default_config_dir, init_crypto_provider, pin_path};

/// Edge-state-machine input. Fed by the source forwarder (`Source`
/// variant) and by `reader_task` (`WireMessage` variant — currently
/// only `Message::ReleaseControl` is acted on, per `StateMachine::
/// on_wire_message`). Single consumer: `edge_brain`. macOS-only
/// because the SM is currently wired exclusively to the macOS
/// source/sink pair.
#[cfg(target_os = "macos")]
#[derive(Debug)]
enum BrainInput {
    Source(kmwarp_core::SourceEvent),
    WireMessage(Message),
}

/// Heartbeat cadence; spec §M1 mandates 500 ms.
const HEARTBEAT_PERIOD: Duration = Duration::from_millis(500);

/// Silence budget before declaring the peer dead; spec §M1 mandates 2 s.
const SILENCE_DEADLINE: Duration = Duration::from_secs(2);

/// Outbound mpsc bound. Encoder is the natural backpressure point per
/// PLAN.md §Async channel topology; producers `try_send` and drop with
/// `warn!` if this fills (a flatlined socket).
const OUTBOUND_CHANNEL_BOUND: usize = 256;

/// Fallback server-screen size returned in `HelloAck` when CG queries
/// aren't available (non-macOS build). macOS reads the real values from
/// `CGDisplay::main()` inside `query_screen_px`.
#[cfg(not(target_os = "macos"))]
const FALLBACK_SCREEN_PX: (u16, u16) = (1920, 1080);

/// Brain channel bound. Same shape as the outbound channel — bounded
/// `try_send`, warn-and-drop on overflow.
#[cfg(target_os = "macos")]
const BRAIN_CHANNEL_BOUND: usize = 256;

/// M7 graceful-disconnect drain: per-send timeout when pushing a
/// synthesized `KeyEvent { Up }` into the outbound channel. Each held
/// HID gets one send attempt; the overall disconnect path is bounded
/// at roughly `held.len() * this`.
#[cfg(target_os = "macos")]
const GRACEFUL_DRAIN_PER_SEND_TIMEOUT: Duration = Duration::from_millis(100);

/// M7 graceful-disconnect drain: best-effort wait after queueing the
/// releases for the encoder to flush them to the peer. We can't
/// directly observe "encoder flushed N messages" — this is a heuristic
/// based on the encoder having consumed at least one slot.
#[cfg(target_os = "macos")]
const GRACEFUL_DRAIN_FLUSH_DEADLINE: Duration = Duration::from_millis(200);

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
    // M9 bootstrap: install the rustls crypto provider, resolve the
    // config dir, load or generate the self-signed cert + key, build a
    // PinStore. `KMWARP_REPAIR=1` deletes any existing pin so the next
    // connect re-enters pairing mode.
    init_crypto_provider();
    let config_dir = default_config_dir()
        .context("could not resolve OS-conventional config directory for TLS materials")?;
    let (cert_path, key_path) = cert::default_paths_in(&config_dir);
    let cert_bundle = if cert_path.exists() && key_path.exists() {
        cert::load_from_disk(&cert_path, &key_path).context("loading cert / key from disk")?
    } else {
        info!(
            ?cert_path,
            ?key_path,
            "no cert / key on disk; generating self-signed pair"
        );
        let bundle = cert::generate_self_signed().context("generating self-signed cert")?;
        cert::save_to_disk(&bundle, &cert_path, &key_path).context("saving cert / key to disk")?;
        bundle
    };
    let pin_store = Arc::new(PinStore::new(pin_path(&config_dir)));
    if std::env::var("KMWARP_REPAIR").as_deref() == Ok("1") {
        info!(
            path = %pin_store.path().display(),
            "KMWARP_REPAIR=1 set; deleting existing peer.pin"
        );
        if let Err(e) = pin_store.forget() {
            warn!(error = %e, "could not delete peer.pin; continuing");
        }
    }

    let listener = TcpListener::bind(bind)
        .await
        .with_context(|| format!("binding kmwarp-server to {bind}"))?;
    let local_addr = listener.local_addr().unwrap_or(bind);
    info!(addr = %local_addr, "kmwarp-server listening on {local_addr}");

    let peer_name: Arc<str> = Arc::from(peer_name);
    let mod_remap: Arc<kmwarp_core::modmap::ModRemap> = Arc::new(load_mod_remap_or_default());
    let cert_bundle = Arc::new(cert_bundle);

    loop {
        let (stream, remote) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                error!(error = %e, "accept failed; continuing");
                continue;
            }
        };
        let peer_name = Arc::clone(&peer_name);
        let mod_remap = Arc::clone(&mod_remap);
        let cert_bundle = Arc::clone(&cert_bundle);
        let pin_store = Arc::clone(&pin_store);
        tokio::spawn(async move {
            if let Err(e) =
                handle_peer(stream, remote, peer_name, mod_remap, cert_bundle, pin_store).await
            {
                warn!(peer = %remote, error = %e, "peer session ended with error");
            }
        });
    }
}

/// Load `~/.config/kmwarp/config.toml` and return the configured
/// `[modifiers]` section. Falls back to `ModRemap::default()`
/// (Cmd→Ctrl, Option→Alt) on any error — missing file, parse failure,
/// unresolvable home directory — and logs a `warn!` so the operator
/// can see why their custom config didn't apply.
///
/// Loaded once at server startup (not per-peer) because v1 is single-
/// peer and config reload at runtime is a v1.1 concern. The returned
/// `ModRemap` is wrapped in an `Arc` by the caller for the per-peer
/// spawn.
fn load_mod_remap_or_default() -> kmwarp_core::modmap::ModRemap {
    use kmwarp_core::config::Config;
    match Config::load_default() {
        Ok(cfg) => {
            info!(
                cmd = ?cfg.modifiers.cmd,
                option = ?cfg.modifiers.option,
                path = ?Config::default_config_path(),
                "loaded [modifiers] from config"
            );
            cfg.modifiers
        }
        Err(e) => {
            warn!(
                error = %e,
                path = ?Config::default_config_path(),
                "failed to load config; using default ModRemap (Cmd→Ctrl, Option→Alt)"
            );
            kmwarp_core::modmap::ModRemap::default()
        }
    }
}

/// Per-peer session: handshake, install platform source, run the M4 task
/// graph.
async fn handle_peer(
    stream: TcpStream,
    remote: SocketAddr,
    server_peer_name: Arc<str>,
    mod_remap: Arc<kmwarp_core::modmap::ModRemap>,
    cert_bundle: Arc<cert::CertBundle>,
    pin_store: Arc<PinStore>,
) -> Result<(), ServerError> {
    info!(peer = %remote, "peer connected");

    // Set TCP_NODELAY on the underlying socket BEFORE the TLS
    // handshake. `Connection::from_io` doesn't touch socket options
    // because the stream type is opaque at that level.
    stream.set_nodelay(true)?;

    // M9: load the current pin state from disk per-accept (so a deleted
    // pin file mid-run re-enters pairing on the next accept without a
    // restart). Build the rustls ServerConfig accordingly.
    let pinned = match pin_store.load() {
        Ok(p) => p,
        Err(e) => {
            error!(
                peer = %remote,
                error = %e,
                "pin file corrupt; refusing connection (delete peer.pin and re-pair)"
            );
            return Ok(());
        }
    };
    let server_config =
        match build_server_config(&cert_bundle.cert_der, &cert_bundle.private_key_der, pinned) {
            Ok(c) => c,
            Err(e) => {
                error!(peer = %remote, error = %e, "could not build TLS server config");
                return Ok(());
            }
        };
    let acceptor = TlsAcceptor::from(server_config);
    let tls = match acceptor.accept(stream).await {
        Ok(t) => t,
        Err(e) => {
            warn!(peer = %remote, error = %e, "TLS handshake failed (pin mismatch?)");
            return Ok(());
        }
    };
    info!(
        peer = %remote,
        mode = if pinned.is_some() { "pin" } else { "pairing" },
        "TLS handshake complete"
    );
    let mut conn = Connection::from_io(tls);

    // M9 pairing: if no pin on disk, run the in-stream pairing flow
    // BEFORE the normal Hello/HelloAck. Succeeds → pin written → subsequent
    // connects use pin mode. Fails → drop the session; the client will
    // reconnect and re-pair.
    if pinned.is_none() {
        match run_server_pairing_flow(&mut conn, &cert_bundle.cert_der, &pin_store).await {
            Ok(()) => info!(peer = %remote, "pairing succeeded"),
            Err(e) => {
                warn!(peer = %remote, error = %e, "pairing failed; closing connection");
                return Ok(());
            }
        }
    }

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

    let screen_px = query_screen_px();
    conn.write_frame(&Message::HelloAck {
        accepted: true,
        server_screen_px: screen_px,
    })
    .await?;
    debug!(
        peer = %remote,
        "sent HelloAck (server={}, screen={:?})",
        server_peer_name, screen_px
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

    // M7 stuck-key tracker, shared between the brain (writer) and
    // `handle_peer`'s post-shutdown drain (reader). The brain task is
    // the only writer; the disconnect-drain path is the only other
    // reader.
    #[cfg(target_os = "macos")]
    let held: std::sync::Arc<std::sync::Mutex<kmwarp_core::stuck_keys::HeldKeys>> =
        std::sync::Arc::new(std::sync::Mutex::new(
            kmwarp_core::stuck_keys::HeldKeys::new(),
        ));

    // Mouse + edge brain (macOS only). Installs the tap, takes the
    // swallow handle, spawns source-forwarder + edge-brain tasks, and
    // returns a `brain_tx` clone so the reader can dispatch
    // `Message::ReleaseControl` into the SM.
    #[cfg(target_os = "macos")]
    let brain_tx = spawn_input_brain(
        &mut set,
        remote,
        tx_out.clone(),
        screen_px.0,
        std::sync::Arc::clone(&held),
        Arc::clone(&mod_remap),
    )
    .await;

    // M8 clipboard sync (macOS only). Install the NSPasteboard watcher;
    // if it fails we log and continue — the peer session is still
    // useful without clipboard sync. Returns:
    //   - `clip_in_tx`: incoming-frame channel into which reader_task
    //     pushes `Message::ClipboardText` for reassembly + local write;
    //   - `echo_guard`: shared SHA-256 guard so the post-write watcher
    //     event doesn't bounce back to the peer.
    #[cfg(target_os = "macos")]
    let (clip_in_tx, echo_guard) = spawn_clipboard_tasks(&mut set, remote, tx_out.clone()).await;

    // Reader: decode + dispatch. Pulses notify on every successful read,
    // routes EchoPing → EchoPong, ReleaseControl → brain,
    // ClipboardText → clipboard_in_task, optionally records EchoPong RTT.
    set.spawn(reader_task(
        remote,
        reader,
        Arc::clone(&notify),
        tx_out.clone(),
        #[cfg(target_os = "macos")]
        brain_tx.clone(),
        #[cfg(target_os = "macos")]
        clip_in_tx.clone(),
        Arc::clone(&start),
        #[cfg(feature = "latency-probe")]
        Arc::clone(&latency_state),
    ));

    // Deadline watcher: 2 s silence budget.
    set.spawn(deadline_watcher(remote, notify));

    // Latency probe sender — only with feature, runs alongside.
    #[cfg(feature = "latency-probe")]
    set.spawn(latency_prober(
        remote,
        tx_out.clone(),
        Arc::clone(&start),
        Arc::clone(&latency_state),
    ));

    // Drop the brain + clipboard-in channel senders so those tasks can
    // shut down cleanly when their producers exit. tx_out is
    // intentionally kept alive in *this* scope so the post-disconnect
    // M7 drain can still push synthesized `KeyEvent { Up }` releases
    // into the encoder before we abort siblings.
    #[cfg(target_os = "macos")]
    drop(brain_tx);
    #[cfg(target_os = "macos")]
    drop(clip_in_tx);

    // Wait for the first task to exit, then run the M7 graceful-
    // disconnect drain: synthesize Up events for every key still held
    // and give the encoder a brief window to flush them to the peer
    // before tearing down. SIGKILL paths bypass this — the client is
    // responsible for synthesizing local releases on EOF (the spec's
    // "Windows side sees a clean release" invariant is satisfied by
    // either side; the graceful path is just lower-friction).
    let first_exit = wait_for_first_exit(remote, &mut set).await;
    #[cfg(target_os = "macos")]
    drain_held_on_disconnect(remote, &tx_out, &held).await;
    // M8: clear the echo guard so a legitimate post-reconnect local copy
    // isn't suppressed by a stale hash from before disconnect.
    #[cfg(target_os = "macos")]
    {
        if let Some(g) = echo_guard.as_ref() {
            g.lock().await.clear();
        }
    }
    drop(tx_out);
    drain_remaining_tasks(remote, set).await;
    debug!(peer = %remote, ?first_exit, "handle_peer exiting");
    info!(peer = %remote, "peer session ended");
    Ok(())
}

/// Wait for the first sub-task to exit. Returns a flag for the caller
/// to log. The remaining tasks are still running on return; the caller
/// should run the M7 drain (if applicable) and then call
/// [`drain_remaining_tasks`].
async fn wait_for_first_exit(remote: SocketAddr, set: &mut JoinSet<TaskExit>) -> TaskExit {
    let exit = match set.join_next().await {
        Some(joined) => joined.unwrap_or(TaskExit::JoinError),
        None => TaskExit::JoinError,
    };
    log_exit(remote, exit_kind(&exit));
    exit
}

/// Abort the surviving sub-tasks and drain the `JoinSet` so logs land
/// in a coherent order before `handle_peer` returns.
async fn drain_remaining_tasks(remote: SocketAddr, mut set: JoinSet<TaskExit>) {
    set.abort_all();
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok(exit) => debug!(peer = %remote, ?exit, "sibling task drained"),
            Err(e) if e.is_cancelled() => {}
            Err(e) => debug!(peer = %remote, error = %e, "sibling task join error"),
        }
    }
}

/// Helper for cloning a TaskExit into the variant log_exit prefers
/// (which takes ownership). Cheap: variants are small enums + Strings;
/// we duplicate the String for the log.
fn exit_kind(exit: &TaskExit) -> TaskExit {
    match exit {
        TaskExit::EncoderFailed(s) => TaskExit::EncoderFailed(s.clone()),
        TaskExit::EncoderClosed => TaskExit::EncoderClosed,
        TaskExit::HeartbeatFailed(s) => TaskExit::HeartbeatFailed(s.clone()),
        TaskExit::ReaderFailed(s) => TaskExit::ReaderFailed(s.clone()),
        TaskExit::DeadlineExpired => TaskExit::DeadlineExpired,
        #[cfg(target_os = "macos")]
        TaskExit::SourceForwarderFailed(s) => TaskExit::SourceForwarderFailed(s.clone()),
        #[cfg(target_os = "macos")]
        TaskExit::EdgeBrainFailed(s) => TaskExit::EdgeBrainFailed(s.clone()),
        #[cfg(target_os = "macos")]
        TaskExit::ClipboardOutFailed(s) => TaskExit::ClipboardOutFailed(s.clone()),
        #[cfg(target_os = "macos")]
        TaskExit::ClipboardInFailed(s) => TaskExit::ClipboardInFailed(s.clone()),
        #[cfg(feature = "latency-probe")]
        TaskExit::LatencyProberFailed(s) => TaskExit::LatencyProberFailed(s.clone()),
        TaskExit::JoinError => TaskExit::JoinError,
    }
}

/// **M7 graceful-disconnect drain.** After the first sub-task has
/// exited (peer Bye, deadline timeout, reader EOF, encoder failure)
/// but BEFORE we abort the siblings, snapshot the held-keys tracker
/// and push a `KeyEvent { state: Up, modifiers: 0 }` for every still-
/// held HID into the outbound queue. Then wait up to
/// [`GRACEFUL_DRAIN_DEADLINE`] for the encoder to flush the releases
/// to the peer (whichever happens first: flush completes or deadline
/// fires).
///
/// Why best-effort: if the encoder is what exited first (its socket
/// died), the tx_out queue will accept the messages but they'll never
/// reach the peer. That's fine — the SIGKILL/RST path is windows-dev's
/// problem to handle on the client side (the client synthesizes local
/// releases on TCP EOF). The graceful path is just the lower-friction
/// way when the server *can* still write.
///
/// This function is `async` because the drain wait uses
/// `tx_out.reserve()` and `tokio::time::timeout`. The lock window is
/// brief — we hold it only long enough to drain the tracker into a
/// local `Vec<u16>`.
#[cfg(target_os = "macos")]
async fn drain_held_on_disconnect(
    remote: SocketAddr,
    tx_out: &mpsc::Sender<Message>,
    held: &std::sync::Arc<std::sync::Mutex<kmwarp_core::stuck_keys::HeldKeys>>,
) {
    use kmwarp_core::wire::key_state_code;

    let hids: Vec<u16> = match held.lock() {
        Ok(mut h) => h.drain(),
        Err(poisoned) => {
            warn!(peer = %remote, "HeldKeys mutex poisoned on disconnect; recovering");
            poisoned.into_inner().drain()
        }
    };
    if hids.is_empty() {
        return;
    }
    warn!(
        peer = %remote,
        count = hids.len(),
        "M7: draining held keys on graceful disconnect"
    );

    // Use the `await` variant of send so we don't lose releases when
    // tx_out has filled up — this path is rare (only fires once per
    // disconnect) and correctness matters more than rate here. Each
    // send gets its own short timeout so a wedged encoder can't hang
    // the disconnect path indefinitely.
    for hid in hids {
        let msg = Message::KeyEvent {
            hid_usage: hid,
            state: key_state_code::UP,
            modifiers: 0,
        };
        match tokio::time::timeout(GRACEFUL_DRAIN_PER_SEND_TIMEOUT, tx_out.send(msg)).await {
            Ok(Ok(())) => trace!(peer = %remote, hid, "queued disconnect-drain release"),
            Ok(Err(_)) => {
                debug!(
                    peer = %remote,
                    hid,
                    "outbound closed during disconnect drain — peer unreachable"
                );
                return;
            }
            Err(_) => {
                warn!(
                    peer = %remote,
                    hid,
                    "disconnect-drain send timed out; encoder likely wedged"
                );
                return;
            }
        }
    }

    // Best-effort flush wait: give the encoder up to
    // GRACEFUL_DRAIN_FLUSH_DEADLINE to process the messages we just
    // queued. We can't directly observe "encoder flushed N
    // messages"; the proxy here is `tx_out.reserve()`, which awaits
    // until at least one slot is free — which only happens after the
    // encoder drains at least one message past the high-water mark.
    // It's a heuristic, not a guarantee.
    let _ = tokio::time::timeout(GRACEFUL_DRAIN_FLUSH_DEADLINE, tx_out.reserve()).await;
}

/// Discriminant for which sub-task exited and why.
#[derive(Debug)]
enum TaskExit {
    EncoderFailed(String),
    EncoderClosed,
    HeartbeatFailed(String),
    ReaderFailed(String),
    DeadlineExpired,
    /// Source forwarder (macOS) exited — either the tap channel closed
    /// (run-loop exit) or the brain channel closed (brain went away).
    #[cfg(target_os = "macos")]
    SourceForwarderFailed(String),
    /// Edge brain exited — typically because the brain channel closed.
    #[cfg(target_os = "macos")]
    EdgeBrainFailed(String),
    /// Clipboard outbound watcher exited — the NSPasteboard watcher
    /// thread sent EOF or the outbound channel closed.
    #[cfg(target_os = "macos")]
    ClipboardOutFailed(String),
    /// Clipboard inbound reassembler exited — its frame channel closed.
    #[cfg(target_os = "macos")]
    ClipboardInFailed(String),
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
        #[cfg(target_os = "macos")]
        TaskExit::SourceForwarderFailed(reason) => {
            warn!(peer = %remote, reason, "source forwarder exited; tearing down")
        }
        #[cfg(target_os = "macos")]
        TaskExit::EdgeBrainFailed(reason) => {
            warn!(peer = %remote, reason, "edge brain exited; tearing down")
        }
        #[cfg(target_os = "macos")]
        TaskExit::ClipboardOutFailed(reason) => {
            warn!(peer = %remote, reason, "clipboard outbound exited; tearing down")
        }
        #[cfg(target_os = "macos")]
        TaskExit::ClipboardInFailed(reason) => {
            warn!(peer = %remote, reason, "clipboard inbound exited; tearing down")
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
/// `EchoPing` → `EchoPong`, `ReleaseControl` → brain (macOS), and
/// (with `latency-probe`) feed `EchoPong` RTTs into the histogram.
async fn reader_task(
    remote: SocketAddr,
    mut reader: FrameReader,
    notify: Arc<Notify>,
    tx_out: mpsc::Sender<Message>,
    #[cfg(target_os = "macos")] brain_tx: Option<mpsc::Sender<BrainInput>>,
    #[cfg(target_os = "macos")] clip_in_tx: Option<mpsc::Sender<Message>>,
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
                    #[cfg(target_os = "macos")]
                    ref m @ Message::ReleaseControl { exit_y } => {
                        if let Some(ref bt) = brain_tx {
                            // try_send because reader is on the deadline-
                            // notifier hot path; a brief brain stall must
                            // not also stall the deadline pulses.
                            if let Err(e) = bt.try_send(BrainInput::WireMessage(m.clone())) {
                                warn!(
                                    peer = %remote,
                                    error = ?e,
                                    exit_y,
                                    "failed to enqueue ReleaseControl into brain"
                                );
                            }
                        } else {
                            debug!(
                                peer = %remote,
                                exit_y,
                                "ReleaseControl received but no brain (tap install failed?)"
                            );
                        }
                    }
                    #[cfg(target_os = "macos")]
                    ref m @ Message::ClipboardText { ref bytes, .. } => {
                        if let Some(ref ct) = clip_in_tx {
                            if let Err(e) = ct.try_send(m.clone()) {
                                warn!(
                                    peer = %remote,
                                    error = ?e,
                                    chunk_len = bytes.len(),
                                    "failed to enqueue ClipboardText into clipboard_in_task"
                                );
                            }
                        } else {
                            trace!(
                                peer = %remote,
                                chunk_len = bytes.len(),
                                "ClipboardText received but no clipboard task (install failed?)"
                            );
                        }
                    }
                    // Heartbeat / Mouse / Key / TakeControl from the
                    // client: deadline reset is the only handling
                    // needed today.
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
// Screen size query
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
fn query_screen_px() -> (u16, u16) {
    use core_graphics::display::CGDisplay;
    let main = CGDisplay::main();
    // `pixels_wide` / `pixels_high` return `u64`; the wire field is u16
    // (and HelloAck uses u16). Saturate at u16::MAX — anyone running
    // a >65535-px wide display is outside v1's tested envelope.
    let w = main.pixels_wide().min(u64::from(u16::MAX)) as u16;
    let h = main.pixels_high().min(u64::from(u16::MAX)) as u16;
    (w, h)
}

#[cfg(not(target_os = "macos"))]
fn query_screen_px() -> (u16, u16) {
    FALLBACK_SCREEN_PX
}

// ---------------------------------------------------------------------------
// macOS source + edge brain
// ---------------------------------------------------------------------------

/// Install the macOS tap, spawn the source-forwarder + edge-brain
/// tasks, and return the `brain_tx` so the reader can dispatch
/// `Message::ReleaseControl` into the same brain.
///
/// Returns `None` when the tap install fails — in that case the server
/// still runs heartbeats + handshake (preserving M1/M4 fallback
/// behavior), but no input is forwarded.
#[cfg(target_os = "macos")]
async fn spawn_input_brain(
    set: &mut JoinSet<TaskExit>,
    remote: SocketAddr,
    tx_out: mpsc::Sender<Message>,
    screen_w_px: u16,
    held: std::sync::Arc<std::sync::Mutex<kmwarp_core::stuck_keys::HeldKeys>>,
    mod_remap: Arc<kmwarp_core::modmap::ModRemap>,
) -> Option<mpsc::Sender<BrainInput>> {
    use crate::platform::macos::MacInputSource;

    // `install` does brief blocking work (std mpsc handshake with the
    // CFRunLoop thread); spawn_blocking keeps the tokio worker free.
    let install_result = tokio::task::spawn_blocking(MacInputSource::install).await;
    let source = match install_result {
        Ok(Ok(source)) => {
            info!(peer = %remote, "CGEventTap installed; edge brain online");
            source
        }
        Ok(Err(e)) => {
            warn!(
                peer = %remote,
                error = %e,
                "failed to install CGEventTap; continuing with heartbeats only \
                 (no input forwarding, no edge state machine)"
            );
            return None;
        }
        Err(e) => {
            error!(peer = %remote, error = %e, "spawn_blocking for tap install panicked");
            return None;
        }
    };

    let swallow = source.swallow_handle();
    let (brain_tx, brain_rx) = mpsc::channel::<BrainInput>(BRAIN_CHANNEL_BOUND);

    set.spawn(source_forwarder_task(remote, source, brain_tx.clone()));
    set.spawn(edge_brain_task(
        remote,
        brain_rx,
        swallow,
        tx_out,
        screen_w_px,
        held,
        mod_remap,
    ));

    Some(brain_tx)
}

/// Drain the `MacInputSource` and shovel every `SourceEvent` into the
/// brain channel. Use `try_send` + warn-and-drop on brain backlog;
/// blocking the source consumer would push back-pressure into the
/// CFRunLoop callback's unbounded mpsc, which would just grow
/// unbounded.
#[cfg(target_os = "macos")]
async fn source_forwarder_task(
    remote: SocketAddr,
    mut source: crate::platform::macos::MacInputSource,
    brain_tx: mpsc::Sender<BrainInput>,
) -> TaskExit {
    use kmwarp_core::platform::InputSource;

    let mut drops: u64 = 0;
    loop {
        let ev = match source.next_event().await {
            Some(e) => e,
            None => return TaskExit::SourceForwarderFailed("MacInputSource channel closed".into()),
        };
        match brain_tx.try_send(BrainInput::Source(ev)) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(dropped)) => {
                drops = drops.saturating_add(1);
                if drops == 1 || drops % MOUSE_DROP_LOG_EVERY == 0 {
                    warn!(
                        peer = %remote,
                        ?dropped,
                        drops,
                        "brain channel full; dropping SourceEvent"
                    );
                }
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                return TaskExit::SourceForwarderFailed("brain channel closed".into())
            }
        }
    }
}

/// Edge-state-machine driver. Owns the SM + `MacInputSink` (cursor
/// warp/hide). On every `BrainInput`, asks the SM for its action list
/// and runs each action through `execute_action`.
///
/// `screen_w_px` is the *local* (Mac) screen width — the SM detects
/// right-edge crossings at `x >= local_screen_w`. Local height is
/// stored only for the SM's `last_cursor` default and isn't used for
/// any decisions today; we query it once for completeness.
#[cfg(target_os = "macos")]
async fn edge_brain_task(
    remote: SocketAddr,
    mut brain_rx: mpsc::Receiver<BrainInput>,
    swallow: std::sync::Arc<AtomicBool>,
    tx_out: mpsc::Sender<Message>,
    screen_w_px: u16,
    held: std::sync::Arc<std::sync::Mutex<kmwarp_core::stuck_keys::HeldKeys>>,
    mod_remap: Arc<kmwarp_core::modmap::ModRemap>,
) -> TaskExit {
    use kmwarp_core::edge::{EdgeConfig, StateMachine};

    let (w, h) = query_screen_px();
    debug_assert_eq!(
        w, screen_w_px,
        "screen-w mismatch between HelloAck and brain"
    );
    let cfg = EdgeConfig {
        local_screen_w: u32::from(w),
        local_screen_h: u32::from(h),
        ..EdgeConfig::default()
    };
    let mut sm = StateMachine::new(cfg);
    let mut sink = crate::platform::macos::inject::MacInputSink::new();

    let epoch = Instant::now();
    debug!(peer = %remote, screen_w_px, "edge_brain online (LocalActive)");

    while let Some(input) = brain_rx.recv().await {
        let now_ms = epoch.elapsed().as_millis() as u64;
        let actions = match input {
            BrainInput::Source(ev) => sm.on_local_event(ev, now_ms),
            BrainInput::WireMessage(msg) => {
                if matches!(msg, Message::ReleaseControl { .. }) {
                    debug!(peer = %remote, ?msg, "brain: wire message");
                }
                sm.on_wire_message(&msg, now_ms)
            }
        };
        for action in actions {
            execute_action(
                remote, action, &mut sink, &tx_out, &swallow, &mod_remap, &held,
            );
        }
    }
    TaskExit::EdgeBrainFailed("brain input channel closed".into())
}

/// Synchronous action executor. The wire-bound actions go through
/// `tx_out.try_send` (warn-and-drop on full, same backpressure rule as
/// the rest of the M4 pump). Cursor-control actions are direct CG calls
/// on the sink. Swallow flips a single atomic.
///
/// Every `SendMessage(KeyEvent)` is run through [`remap_key_message`]
/// before enqueue: the modifier byte goes through `ModRemap::apply`,
/// and the modifier-key HID itself is rewritten via
/// [`remap_modifier_hid`] so e.g. a Cmd keypress (HID 0xE3 LeftGUI)
/// arrives on the wire as LeftCtrl (HID 0xE0) under the default
/// Cmd→Ctrl mapping. Non-key messages pass through unchanged.
#[cfg(target_os = "macos")]
fn execute_action(
    remote: SocketAddr,
    action: kmwarp_core::edge::Action,
    sink: &mut crate::platform::macos::inject::MacInputSink,
    tx_out: &mpsc::Sender<Message>,
    swallow: &std::sync::Arc<AtomicBool>,
    mod_remap: &kmwarp_core::modmap::ModRemap,
    held: &std::sync::Arc<std::sync::Mutex<kmwarp_core::stuck_keys::HeldKeys>>,
) {
    use kmwarp_core::edge::Action;
    use kmwarp_core::platform::InputSink;
    use kmwarp_core::wire::key_state_code;
    use std::sync::atomic::Ordering;

    match action {
        Action::SendTakeControl { entry_y } => {
            enqueue(remote, tx_out, Message::TakeControl { entry_y })
        }
        Action::SendReleaseControl { exit_y } => {
            enqueue(remote, tx_out, Message::ReleaseControl { exit_y })
        }
        Action::SendMessage(msg) => {
            let remapped = remap_key_message(msg, mod_remap);
            // Track held keys *after* remap — the destination side
            // sees the remapped HID, so the drained Up event must
            // carry the same HID.
            if let Message::KeyEvent {
                hid_usage, state, ..
            } = &remapped
            {
                if let Ok(mut h) = held.lock() {
                    if *state == key_state_code::DOWN {
                        h.insert(*hid_usage);
                    } else if *state == key_state_code::UP {
                        h.remove(*hid_usage);
                    }
                }
            }
            enqueue(remote, tx_out, remapped);
        }
        Action::WarpLocalCursor { x, y } => sink.warp_cursor_abs(x, y),
        Action::HideLocalCursor => sink.hide_cursor(),
        Action::ShowLocalCursor => sink.show_cursor(),
        Action::StartSwallow => {
            debug!(peer = %remote, "brain: StartSwallow (RemoteActive)");
            // Spec: "never enter RemoteActive with held keys on the
            // local side". Defensive drain — should be a no-op since
            // LocalActive doesn't forward keys.
            drain_held_to_peer(remote, tx_out, held, /* warn_if_nonempty */ true);
            swallow.store(true, Ordering::Relaxed);
        }
        Action::StopSwallow => {
            debug!(peer = %remote, "brain: StopSwallow (LocalActive)");
            // Drain whatever the user was holding during RemoteActive
            // so the Windows side sees a clean release for every key.
            // No-op if held is empty.
            drain_held_to_peer(remote, tx_out, held, /* warn_if_nonempty */ false);
            swallow.store(false, Ordering::Relaxed);
        }
    }
}

/// Drain the shared `HeldKeys` tracker into `Up`-state `KeyEvent`
/// messages enqueued through `tx_out`. Used by `StartSwallow`
/// (defensive — should be empty) and `StopSwallow` (the spec-mandated
/// release synthesis on every edge transition).
#[cfg(target_os = "macos")]
fn drain_held_to_peer(
    remote: SocketAddr,
    tx_out: &mpsc::Sender<Message>,
    held: &std::sync::Arc<std::sync::Mutex<kmwarp_core::stuck_keys::HeldKeys>>,
    warn_if_nonempty: bool,
) {
    use kmwarp_core::wire::key_state_code;

    let hids: Vec<u16> = match held.lock() {
        Ok(mut h) => h.drain(),
        Err(poisoned) => {
            // A poisoned mutex means a prior holder panicked. The
            // tracker is still recoverable via `into_inner`, but for
            // M7's purposes we'd rather warn loudly and continue with
            // whatever state we can read than panic this brain task.
            warn!(peer = %remote, "HeldKeys mutex poisoned; recovering");
            poisoned.into_inner().drain()
        }
    };

    if hids.is_empty() {
        return;
    }
    if warn_if_nonempty {
        warn!(
            peer = %remote,
            count = hids.len(),
            "held keys non-empty on StartSwallow — draining defensively"
        );
    } else {
        debug!(
            peer = %remote,
            count = hids.len(),
            "draining held keys on StopSwallow"
        );
    }
    for hid in hids {
        enqueue(
            remote,
            tx_out,
            Message::KeyEvent {
                hid_usage: hid,
                state: key_state_code::UP,
                modifiers: 0,
            },
        );
    }
}

/// Apply the configured [`ModRemap`] to a `Message::KeyEvent` (both the
/// modifier byte and, if the event itself is a modifier key, the HID
/// code). Other message variants pass through unchanged.
///
/// **Why both layers.** Without HID remap the Cmd KEY itself goes to
/// the Windows side as LeftGUI (the Win key), so even though the
/// modifier byte on the C event correctly says "Ctrl", the Win key is
/// physically held — the user gets Win+C, not Ctrl+C.
/// `ModRemap::apply_to_hid` + `ModRemap::apply_to_modmask` together
/// fix both layers.
#[cfg(target_os = "macos")]
fn remap_key_message(msg: Message, remap: &kmwarp_core::modmap::ModRemap) -> Message {
    use kmwarp_core::platform::ModMask;
    match msg {
        Message::KeyEvent {
            hid_usage,
            state,
            modifiers,
        } => Message::KeyEvent {
            hid_usage: remap.apply_to_hid(hid_usage),
            state,
            modifiers: remap
                .apply_to_modmask(ModMask::from_wire(modifiers))
                .to_wire(),
        },
        other => other,
    }
}

#[cfg(target_os = "macos")]
fn enqueue(remote: SocketAddr, tx_out: &mpsc::Sender<Message>, msg: Message) {
    match tx_out.try_send(msg) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(dropped)) => {
            warn!(peer = %remote, ?dropped, "outbound full; dropping action-emitted message");
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            debug!(peer = %remote, "outbound closed; action enqueue dropped");
        }
    }
}

// ---------------------------------------------------------------------------
// M8 clipboard sync
// ---------------------------------------------------------------------------

/// Per-peer clipboard sync bound: incoming `Message::ClipboardText`
/// frames are queued from reader_task into the inbound reassembler
/// task. Small bound: chunks arrive at human-clipboard rate, not
/// keyboard rate.
#[cfg(target_os = "macos")]
const CLIPBOARD_IN_CHANNEL_BOUND: usize = 64;

/// Install the NSPasteboard watcher and spawn the two clipboard
/// tasks (outbound watcher → wire, inbound wire → local pasteboard).
///
/// Returns `(clip_in_tx, echo_guard)` even when the install fails:
///   - `clip_in_tx` is `None` if the install failed (reader_task will
///     drop incoming `ClipboardText` frames rather than panic).
///   - `echo_guard` is `None` in the same case (the disconnect-clear
///     path skips its `.lock().clear()`).
///
/// Returning a tuple of `Option<Sender>` + `Option<Arc<...>>` rather
/// than failing the whole peer session matches the rest of the M2/M4
/// fallback pattern: input forwarding can keep running without
/// clipboard sync; the peer session is still useful.
#[cfg(target_os = "macos")]
async fn spawn_clipboard_tasks(
    set: &mut JoinSet<TaskExit>,
    remote: SocketAddr,
    tx_out: mpsc::Sender<Message>,
) -> (
    Option<mpsc::Sender<Message>>,
    Option<std::sync::Arc<tokio::sync::Mutex<kmwarp_core::clipboard::EchoGuard>>>,
) {
    use crate::platform::macos::clipboard::NsPasteboardClipboard;

    // Install is synchronous (just spawns a thread); no spawn_blocking
    // needed. Wrap in spawn_blocking anyway for symmetry with the M2
    // tap install, which DOES block.
    let install_result = tokio::task::spawn_blocking(NsPasteboardClipboard::install).await;
    let clipboard = match install_result {
        Ok(Ok(c)) => {
            info!(peer = %remote, "NSPasteboard watcher installed; M8 clipboard sync online");
            c
        }
        Ok(Err(e)) => {
            warn!(
                peer = %remote,
                error = %e,
                "failed to install NSPasteboard watcher; continuing without clipboard sync"
            );
            return (None, None);
        }
        Err(e) => {
            error!(peer = %remote, error = %e, "spawn_blocking for clipboard install panicked");
            return (None, None);
        }
    };

    let echo_guard = std::sync::Arc::new(tokio::sync::Mutex::new(
        kmwarp_core::clipboard::EchoGuard::new(),
    ));
    let (clip_in_tx, clip_in_rx) = mpsc::channel::<Message>(CLIPBOARD_IN_CHANNEL_BOUND);

    set.spawn(clipboard_out_task(
        remote,
        clipboard,
        std::sync::Arc::clone(&echo_guard),
        tx_out,
    ));
    set.spawn(clipboard_in_task(
        remote,
        clip_in_rx,
        std::sync::Arc::clone(&echo_guard),
    ));

    (Some(clip_in_tx), Some(echo_guard))
}

/// Outbound clipboard task: drain `clipboard.next_change()` and split
/// each text into wire `Message::ClipboardText` frames via
/// `Chunker::split`. Pre-filtered against `EchoGuard` so a change we
/// just wrote in response to a remote frame doesn't bounce back.
#[cfg(target_os = "macos")]
async fn clipboard_out_task(
    remote: SocketAddr,
    mut clipboard: crate::platform::macos::clipboard::NsPasteboardClipboard,
    echo_guard: std::sync::Arc<tokio::sync::Mutex<kmwarp_core::clipboard::EchoGuard>>,
    tx_out: mpsc::Sender<Message>,
) -> TaskExit {
    use kmwarp_core::clipboard::Chunker;
    use kmwarp_core::platform::{Clipboard, ClipboardEvent};

    loop {
        let ev = match clipboard.next_change().await {
            Some(e) => e,
            None => {
                return TaskExit::ClipboardOutFailed("NSPasteboard watcher channel closed".into())
            }
        };
        let ClipboardEvent::TextChanged(text) = ev;
        // SHA-256 echo guard: suppress changes that match the last
        // text we wrote in response to a remote frame.
        if echo_guard.lock().await.is_echo_of_remote(&text) {
            trace!(
                peer = %remote,
                len = text.len(),
                "suppressing clipboard change matching last remote write"
            );
            continue;
        }
        let chunks = Chunker::split(&text);
        debug!(
            peer = %remote,
            len = text.len(),
            chunks = chunks.len(),
            "forwarding clipboard change to peer"
        );
        for msg in chunks {
            match tx_out.try_send(msg) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    warn!(
                        peer = %remote,
                        "outbound full; dropping ClipboardText chunk (peer may see truncated paste)"
                    );
                    // Don't try to send remaining chunks — they'd be a
                    // dangling tail with no FIRST flag.
                    break;
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    return TaskExit::ClipboardOutFailed("outbound channel closed".into())
                }
            }
        }
    }
}

/// Inbound clipboard task: feed `Message::ClipboardText` frames from
/// reader_task into a `Reassembler`. On a complete payload, write to
/// `NSPasteboard` and register the SHA-256 in the echo guard so the
/// next watcher tick doesn't re-forward what we just wrote.
#[cfg(target_os = "macos")]
async fn clipboard_in_task(
    remote: SocketAddr,
    mut clip_in_rx: mpsc::Receiver<Message>,
    echo_guard: std::sync::Arc<tokio::sync::Mutex<kmwarp_core::clipboard::EchoGuard>>,
) -> TaskExit {
    use crate::platform::macos::clipboard::pasteboard_write;
    use kmwarp_core::clipboard::Reassembler;

    let mut reassembler = Reassembler::new();
    while let Some(msg) = clip_in_rx.recv().await {
        match reassembler.ingest(&msg) {
            Ok(Some(text)) => {
                debug!(peer = %remote, len = text.len(), "writing peer clipboard to NSPasteboard");
                // Order: register the hash FIRST, then write — the
                // watcher could fire between the two calls, and we
                // don't want to forward our own write to the peer.
                echo_guard.lock().await.remember_remote_write(&text);
                // pasteboard_write is fast (~µs) but not async; this
                // is fine because clipboard frames arrive at human
                // rate. If we ever need to free the runtime worker we
                // can spawn_blocking it.
                pasteboard_write(&text);
            }
            Ok(None) => {
                // Mid-stream chunk; keep collecting.
                trace!(peer = %remote, "clipboard chunk accumulated");
            }
            Err(e) => {
                warn!(peer = %remote, error = %e, "clipboard reassembly failed; resetting");
                // Reassembler resets its own buffer on FIRST flag; an
                // explicit reset isn't needed.
            }
        }
    }
    TaskExit::ClipboardInFailed("clipboard in channel closed".into())
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

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use kmwarp_core::hid::usage;
    use kmwarp_core::modmap::{ModRemap, ModTarget};
    use kmwarp_core::platform::ModMask;
    use kmwarp_core::wire::key_state_code;

    #[test]
    fn remap_key_message_cmd_c_to_ctrl_c() {
        let r = ModRemap::default();
        let msg = Message::KeyEvent {
            hid_usage: usage::C,
            state: key_state_code::DOWN,
            modifiers: ModMask::META.to_wire(),
        };
        let out = remap_key_message(msg, &r);
        match out {
            Message::KeyEvent {
                hid_usage,
                state,
                modifiers,
            } => {
                // HID for 'C' is unchanged (not a modifier key).
                assert_eq!(hid_usage, usage::C);
                assert_eq!(state, key_state_code::DOWN);
                // Modifier byte: META bit cleared, CTRL bit set.
                assert_eq!(modifiers, ModMask::CTRL.to_wire());
            }
            other => panic!("expected KeyEvent, got {other:?}"),
        }
    }

    #[test]
    fn remap_key_message_cmd_keypress_itself_becomes_lctrl() {
        let r = ModRemap::default();
        // User physically presses Cmd: FlagsChanged → SourceEvent::Key
        // { hid: LeftGUI, mods: META }. SM forwards as KeyEvent. We
        // remap to LeftCtrl on the wire so Windows actually sees Ctrl.
        let msg = Message::KeyEvent {
            hid_usage: usage::LEFT_GUI,
            state: key_state_code::DOWN,
            modifiers: ModMask::META.to_wire(),
        };
        let out = remap_key_message(msg, &r);
        match out {
            Message::KeyEvent {
                hid_usage,
                modifiers,
                ..
            } => {
                assert_eq!(hid_usage, usage::LEFT_CTRL);
                assert_eq!(modifiers, ModMask::CTRL.to_wire());
            }
            other => panic!("expected KeyEvent, got {other:?}"),
        }
    }

    #[test]
    fn remap_key_message_right_cmd_to_right_ctrl() {
        let r = ModRemap::default();
        let msg = Message::KeyEvent {
            hid_usage: usage::RIGHT_GUI,
            state: key_state_code::UP,
            modifiers: 0,
        };
        let out = remap_key_message(msg, &r);
        if let Message::KeyEvent { hid_usage, .. } = out {
            assert_eq!(hid_usage, usage::RIGHT_CTRL);
        } else {
            panic!("expected KeyEvent");
        }
    }

    #[test]
    fn remap_key_message_passes_through_non_modifier_hid() {
        let r = ModRemap::default();
        for hid in [usage::A, usage::Z, usage::D1, usage::SPACE, usage::F1] {
            let msg = Message::KeyEvent {
                hid_usage: hid,
                state: key_state_code::DOWN,
                modifiers: 0,
            };
            let out = remap_key_message(msg, &r);
            if let Message::KeyEvent { hid_usage, .. } = out {
                assert_eq!(hid_usage, hid);
            } else {
                panic!("expected KeyEvent");
            }
        }
    }

    #[test]
    fn remap_key_message_swap_cmd_to_alt_via_custom() {
        let r = ModRemap {
            cmd: ModTarget::Alt,
            option: ModTarget::Ctrl,
        };
        let msg = Message::KeyEvent {
            hid_usage: usage::LEFT_GUI,
            state: key_state_code::DOWN,
            modifiers: ModMask::META.to_wire(),
        };
        let out = remap_key_message(msg, &r);
        if let Message::KeyEvent {
            hid_usage,
            modifiers,
            ..
        } = out
        {
            assert_eq!(hid_usage, usage::LEFT_ALT);
            assert_eq!(modifiers, ModMask::ALT.to_wire());
        } else {
            panic!("expected KeyEvent");
        }
    }

    #[test]
    fn remap_key_message_passes_through_non_key_variants() {
        let r = ModRemap::default();
        let mouse = Message::MouseMoveRel { dx: 3, dy: -4 };
        assert_eq!(remap_key_message(mouse.clone(), &r), mouse);

        let hb = Message::Heartbeat { seq: 17 };
        assert_eq!(remap_key_message(hb.clone(), &r), hb);
    }
}

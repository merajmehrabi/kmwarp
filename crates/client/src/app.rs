//! Top-level client runtime.
//!
//! ## Lifecycle
//!
//! 1. Connect to the server (exponential backoff 250 ms → 5 s, capped).
//! 2. Send `Hello`, await `HelloAck`. A hard refusal
//!    (`HelloAck { accepted: false }`) aborts the binary; any other error
//!    just kicks us back to the connect loop.
//! 3. Build a real [`InputSink`] (`WinInputSink` on Windows,
//!    [`crate::sink::NoOpSink`] elsewhere — so the pipe is still
//!    exercisable on the macOS dev host).
//! 4. Spawn the M4 task graph:
//!
//! ```text
//!  ┌──────────────────────────┐   tx_out (mpsc 256, bounded)
//!  │ heartbeat_producer       │ ────────────┐
//!  └──────────────────────────┘             │
//!  ┌──────────────────────────┐             ▼
//!  │ injector_loop            │       ┌───────────────┐
//!  │  read_frame + dispatch   │ ────► │ encoder_loop  │ ──► socket
//!  │  EchoPing → EchoPong     │       └───────────────┘
//!  │  Mouse* → sink           │
//!  └──────────────────────────┘
//!         │
//!         └─ notify ─► deadline_watcher (2 s)
//! ```
//!
//! First task exit aborts the rest. On normal disconnect (peer dead,
//! deadline expired) we loop back to step 1; on a hard handshake refusal
//! we propagate `Err`.

use std::net::SocketAddr;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use kmwarp_core::clipboard::EchoGuard;
use kmwarp_core::tls::{cert, PinStore};
use kmwarp_core::wire::{Message, PROTO_VERSION};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch, Notify};
use tokio::task::JoinSet;
use tokio::time::{interval, sleep, timeout, MissedTickBehavior};
use tokio_rustls::TlsConnector;
use tracing::{debug, error, info, trace, warn};

use crate::error::ClientError;
use crate::net::{
    encoder_loop, injector_loop, run_client_pairing_flow, CodeProvider, CodeProviderFactory,
    Connection, FrameReader, FrameWriter,
};
use crate::sink::{build_default_sink, DefaultSink};
use crate::tls::{
    build_client_config, default_config_dir, init_crypto_provider, pin_path, pinned_server_name,
};

/// Coarse-grained client lifecycle status. Mirrors `ServerStatus` on
/// the macOS side; consumed by the Windows system-tray surface
/// (`platform::windows::tray`) so the operator can see at a glance
/// whether the Mac is currently controlling this box.
///
/// `Clone` required by `tokio::sync::watch`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientStatus {
    /// Pre-discovery / post-shutdown.
    Idle,
    /// mDNS browse in flight (only when KMWARP_CONNECT is unset).
    Discovering,
    /// Pairing flow active. `code` is the 6-digit SPAKE2 code the
    /// operator typed into stdin; surfaced so the tray can echo it
    /// back as a visual confirmation that input was captured.
    Pairing { code: String },
    /// TCP connected; TLS handshake in flight, or handshake done but
    /// HelloAck not yet received.
    Connecting { addr: SocketAddr },
    /// Handshake accepted; in steady-state session, server is in
    /// LocalActive (Mac owns input), Windows side is idle wrt
    /// forwarding.
    Connected { peer: String },
    /// Server entered RemoteActive — every input event coming over
    /// the wire is being injected on this box.
    Driven { peer: String },
}

/// Best-effort publish helper. Swallows `SendError` when no live
/// receivers — the tray is an optional consumer.
fn publish_status(tx: Option<&watch::Sender<ClientStatus>>, status: ClientStatus) {
    if let Some(tx) = tx {
        let _ = tx.send(status);
    }
}

/// Wrap a [`CodeProvider`] so the resolved 6-digit code is also
/// published into the [`ClientStatus`] channel as `Pairing { code }`.
///
/// The tray uses this echo as visual confirmation that input was
/// captured (the dialog closes, the tray says "pairing — code XXXXXX",
/// then SPAKE2 runs and the status flips to `Connected`).
fn wrap_provider_publish(
    inner: CodeProvider,
    status_tx: Option<watch::Sender<ClientStatus>>,
) -> CodeProvider {
    Box::new(move || {
        Box::pin(async move {
            let result = inner().await;
            if let (Ok(code), Some(tx)) = (&result, status_tx.as_ref()) {
                let _ = tx.send(ClientStatus::Pairing { code: code.clone() });
            }
            result
        })
    })
}

/// Heartbeat cadence; spec §M1 mandates 500 ms.
const HEARTBEAT_PERIOD: Duration = Duration::from_millis(500);

/// Silence budget before declaring the peer dead; spec §M1 mandates 2 s.
const SILENCE_DEADLINE: Duration = Duration::from_secs(2);

/// Initial reconnect delay. Doubles each failure up to [`BACKOFF_MAX`].
const BACKOFF_INITIAL: Duration = Duration::from_millis(250);

/// Maximum reconnect delay; capped per spec gotcha ("250 ms → 5 s, capped").
const BACKOFF_MAX: Duration = Duration::from_secs(5);

/// Outbound channel bound; matches the server. Encoder is the natural
/// backpressure point per PLAN.md §Async channel topology.
const OUTBOUND_CHANNEL_BOUND: usize = 256;

/// Run the client forever: connect, run a session, reconnect on loss.
///
/// Returns `Err` only on a hard handshake refusal — every other failure
/// mode (connect refused, socket dropped, peer silence) is recoverable
/// via reconnect.
///
/// When `status_tx` is `Some`, lifecycle transitions are published as
/// [`ClientStatus`] values. The Windows tray surface holds the
/// matching `Receiver` and re-renders on change. `None` disables the
/// broadcast (used by `KMWARP_HEADLESS=1`, service contexts where no
/// user-session tray can show, and tests).
pub async fn run_client(
    connect: SocketAddr,
    peer_name: &str,
    status_tx: Option<watch::Sender<ClientStatus>>,
    code_provider_factory: CodeProviderFactory,
) -> anyhow::Result<()> {
    // M9 bootstrap: install crypto provider, resolve config dir,
    // load-or-generate cert+key, build the pin store. `KMWARP_REPAIR=1`
    // wipes the existing pin so the next connect re-enters pairing.
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

    let peer_name_arc: Arc<str> = Arc::from(peer_name);
    let cert_bundle = Arc::new(cert_bundle);
    let status_tx = status_tx.map(Arc::new);
    let code_provider_factory = Arc::new(code_provider_factory);

    loop {
        // Before each connect attempt we go back to `Connecting` so a
        // post-disconnect reconnect cycle is visible in the tray. The
        // backoff sleep happens inside `connect_with_backoff` — the
        // tray staying on `Connecting` during a backoff is exactly the
        // right UX (the operator sees we're trying, not idle).
        publish_status(status_tx.as_deref(), ClientStatus::Connecting { addr: connect });
        let stream = connect_with_backoff(connect).await;
        info!(addr = %connect, "connected to kmwarp-server at {connect}");

        match run_one_session(
            connect,
            stream,
            &peer_name_arc,
            Arc::clone(&cert_bundle),
            Arc::clone(&pin_store),
            status_tx.as_deref(),
            Arc::clone(&code_provider_factory),
        )
        .await
        {
            Ok(()) => info!(addr = %connect, "session ended; reconnecting"),
            Err(ClientError::HandshakeRejected) => {
                error!(addr = %connect, "server rejected handshake; aborting");
                publish_status(status_tx.as_deref(), ClientStatus::Idle);
                return Err(ClientError::HandshakeRejected.into());
            }
            Err(e) => warn!(addr = %connect, error = %e, "session ended with error; reconnecting"),
        }
    }
}

/// Connect to `addr`, retrying with exponential backoff (250 ms → 5 s).
/// Returns once a connection is established. Infinite loop on failures.
async fn connect_with_backoff(addr: SocketAddr) -> TcpStream {
    let mut delay = BACKOFF_INITIAL;
    let mut attempt: u32 = 1;
    loop {
        debug!(addr = %addr, attempt, "attempting connect");
        match TcpStream::connect(addr).await {
            Ok(stream) => return stream,
            Err(e) => {
                warn!(
                    addr = %addr,
                    attempt,
                    error = %e,
                    delay_ms = delay.as_millis() as u64,
                    "connect failed; retrying after backoff"
                );
                sleep(delay).await;
                delay = (delay * 2).min(BACKOFF_MAX);
                attempt = attempt.saturating_add(1);
            }
        }
    }
}

/// Handshake then run the M4 task graph. Returns `Ok(())` on a normal
/// disconnect; `Err` on a hard handshake refusal or protocol fault that
/// callers should distinguish.
async fn run_one_session(
    addr: SocketAddr,
    stream: TcpStream,
    peer_name: &Arc<str>,
    cert_bundle: Arc<cert::CertBundle>,
    pin_store: Arc<PinStore>,
    status_tx: Option<&watch::Sender<ClientStatus>>,
    code_provider_factory: Arc<CodeProviderFactory>,
) -> Result<(), ClientError> {
    // Set TCP_NODELAY on the raw socket BEFORE the TLS handshake.
    stream.set_nodelay(true)?;

    // M9: load the current pin state per-session so a deleted pin file
    // mid-run re-enters pairing on the next reconnect attempt.
    let pinned = match pin_store.load() {
        Ok(p) => p,
        Err(e) => {
            error!(
                addr = %addr,
                error = %e,
                "pin file corrupt; refusing connection (delete peer.pin and re-pair)"
            );
            return Ok(());
        }
    };
    let client_config =
        match build_client_config(&cert_bundle.cert_der, &cert_bundle.private_key_der, pinned) {
            Ok(c) => c,
            Err(e) => {
                error!(addr = %addr, error = %e, "could not build TLS client config");
                return Ok(());
            }
        };
    let connector = TlsConnector::from(client_config);
    let server_name = pinned_server_name();
    let tls = match connector.connect(server_name, stream).await {
        Ok(t) => t,
        Err(e) => {
            warn!(addr = %addr, error = %e, "TLS handshake failed (pin mismatch?)");
            return Ok(());
        }
    };
    info!(
        addr = %addr,
        mode = if pinned.is_some() { "pin" } else { "pairing" },
        "TLS handshake complete"
    );
    let mut conn = Connection::from_io(tls);

    // M9 pairing: if no pin yet, run the in-stream pairing flow BEFORE
    // the normal Hello / HelloAck.
    if pinned.is_none() {
        // Build a fresh provider for THIS attempt and wrap it so the
        // ClientStatus channel sees the code immediately after the
        // operator commits it. (Wrapping at this scope keeps the
        // pairing flow itself free of the status channel — it only
        // knows about `CodeProvider`.)
        let inner = (code_provider_factory)();
        // Clone the watch sender (cheap — it's an Arc internally) so
        // the wrapped provider future can outlive this scope without
        // borrowing the `&Sender`.
        let status_for_pairing = status_tx.cloned();
        let provider = wrap_provider_publish(inner, status_for_pairing);
        match run_client_pairing_flow(&mut conn, &cert_bundle.cert_der, &pin_store, provider).await
        {
            Ok(()) => info!(addr = %addr, "pairing succeeded"),
            Err(e) => {
                warn!(addr = %addr, error = %e, "pairing failed; will retry on reconnect");
                return Ok(());
            }
        }
    }

    conn.write_frame(&Message::Hello {
        proto_version: PROTO_VERSION,
        peer_name: peer_name.to_string(),
    })
    .await?;
    debug!(addr = %addr, proto_version = PROTO_VERSION, "sent Hello");

    match conn.read_frame().await? {
        Message::HelloAck {
            accepted,
            server_screen_px,
        } => {
            if !accepted {
                return Err(ClientError::HandshakeRejected);
            }
            info!(
                addr = %addr,
                screen = ?server_screen_px,
                "handshake accepted; entering steady-state session"
            );
            publish_status(
                status_tx,
                ClientStatus::Connected {
                    peer: addr.to_string(),
                },
            );
        }
        other => {
            warn!(addr = %addr, ?other, "unexpected handshake response");
            return Err(ClientError::UnexpectedHandshakeFrame);
        }
    }

    // Construct sink AFTER handshake so a sink-init failure doesn't keep
    // a connected socket hanging while the user diagnoses DPI awareness.
    let sink: DefaultSink = match build_default_sink() {
        Ok(s) => s,
        Err(e) => {
            error!(addr = %addr, error = %e, "failed to build input sink; ending session");
            return Ok(());
        }
    };

    let (reader, writer) = conn.into_split();
    run_session_tasks(addr, reader, writer, sink, status_tx).await;
    Ok(())
}

/// Spawn the session task graph and wait for the first exit.
///
/// Tasks (four base + up to two Windows-only):
/// - `encoder_loop` — drains the outbound channel to the socket.
/// - `heartbeat_producer` — 500 ms ticks of `Heartbeat`.
/// - `injector_loop` — dispatches inbound frames; toggles `active` on
///   `TakeControl` / `ReleaseControl`; reassembles inbound clipboard.
/// - `deadline_watcher` — 2 s peer-silence detector.
/// - `cursor_leave_watcher` (Windows only) — 60 Hz `GetCursorPos` poll;
///   emits `ReleaseControl` when the cursor crosses the left edge.
/// - `clipboard_out_task` (Windows only) — drains
///   `AddClipboardFormatListener` events; chunks + sends to the wire.
async fn run_session_tasks(
    addr: SocketAddr,
    reader: FrameReader,
    writer: FrameWriter,
    sink: DefaultSink,
    status_tx: Option<&watch::Sender<ClientStatus>>,
) {
    let (tx_out, rx_out) = mpsc::channel::<Message>(OUTBOUND_CHANNEL_BOUND);
    let notify = Arc::new(Notify::new());
    // `active` flips to true on `TakeControl` and false on the Windows
    // cursor-leave watcher's release. Lives in `app` so its lifetime is
    // exactly one session — a fresh `Arc` per reconnect prevents stale
    // state carrying over.
    let active = Arc::new(AtomicBool::new(false));
    // M8 echo-guard: shared between the injector (which updates it
    // after writing an inbound payload) and the clipboard_out_task
    // (which checks it before forwarding a local change). std::Mutex
    // because both critical sections are short and don't span an
    // .await; tokio's async Mutex would just add overhead.
    let echo_guard = Arc::new(Mutex::new(EchoGuard::new()));

    let mut set: JoinSet<TaskExit> = JoinSet::new();

    set.spawn(spawn_encoder(rx_out, writer));
    set.spawn(heartbeat_producer(addr, tx_out.clone()));
    set.spawn(spawn_injector(
        addr,
        reader,
        sink,
        Arc::clone(&notify),
        tx_out.clone(),
        Arc::clone(&active),
        Arc::clone(&echo_guard),
        status_tx.cloned(),
    ));
    set.spawn(deadline_watcher(addr, notify));

    // Windows-only: spawn the cursor-leave watcher + the clipboard
    // out-pump. On non-Windows hosts (macOS dev box, Linux CI) neither
    // task exists; the codec path is still exercised by the injector +
    // NoOpSink for smoke testing.
    #[cfg(target_os = "windows")]
    {
        use crate::platform::windows::{cursor_leave_watcher, primary_screen_size, WinClipboard};
        let safe_warp_x = (primary_screen_size().0 / 2).max(1);
        set.spawn(spawn_cursor_watcher(
            tx_out.clone(),
            Arc::clone(&active),
            safe_warp_x,
            status_tx.cloned(),
            addr,
        ));
        // Clipboard listener install can fail (RegisterClassW etc).
        // Treat as non-fatal: log and skip the out-pump; inbound writes
        // still work without an outbound observer.
        match WinClipboard::install() {
            Ok(clipboard) => {
                set.spawn(spawn_clipboard_out(
                    clipboard,
                    Arc::clone(&echo_guard),
                    tx_out.clone(),
                ));
            }
            Err(e) => {
                warn!(error = %e, "WinClipboard::install failed; outbound clipboard disabled");
            }
        }
    }

    // Drop the original sender so the channel closes once every task-held
    // clone is dropped (clean shutdown drain).
    drop(tx_out);

    if let Some(joined) = set.join_next().await {
        let exit = joined.unwrap_or(TaskExit::JoinError);
        log_exit(addr, exit);
    }
    set.abort_all();
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok(exit) => debug!(addr = %addr, ?exit, "sibling task drained"),
            Err(e) if e.is_cancelled() => {}
            Err(e) => debug!(addr = %addr, error = %e, "sibling task join error"),
        }
    }
}

/// Wrap the encoder so it surfaces a [`TaskExit`] discriminant.
async fn spawn_encoder(rx: mpsc::Receiver<Message>, writer: FrameWriter) -> TaskExit {
    match encoder_loop(rx, writer).await {
        Ok(()) => TaskExit::EncoderClosed,
        Err(e) => TaskExit::EncoderFailed(e.to_string()),
    }
}

/// Wrap the injector so it surfaces a [`TaskExit`] discriminant.
async fn spawn_injector(
    addr: SocketAddr,
    reader: FrameReader,
    sink: DefaultSink,
    notify: Arc<Notify>,
    tx_out: mpsc::Sender<Message>,
    active: Arc<AtomicBool>,
    echo_guard: Arc<Mutex<EchoGuard>>,
    status_tx: Option<watch::Sender<ClientStatus>>,
) -> TaskExit {
    match injector_loop(
        reader,
        sink,
        notify,
        tx_out,
        active,
        echo_guard,
        status_tx,
        addr,
    )
    .await
    {
        Ok(()) => TaskExit::PeerByeOrClose,
        Err(e) => TaskExit::ReaderFailed(e.to_string()),
    }
}

/// Windows-only: drive the clipboard out-pump and report exit via
/// [`TaskExit`].
#[cfg(target_os = "windows")]
async fn spawn_clipboard_out(
    clipboard: crate::platform::windows::WinClipboard,
    echo_guard: Arc<Mutex<EchoGuard>>,
    tx_out: mpsc::Sender<Message>,
) -> TaskExit {
    use crate::net::clipboard_out_task;
    clipboard_out_task(clipboard, echo_guard, tx_out).await;
    TaskExit::ClipboardOutExited
}

/// Windows-only: drive the cursor-leave watcher and convert its
/// completion (only happens if the outbound channel closes) into a
/// generic `CursorWatcherExited` so the JoinSet can log it alongside
/// the other tasks.
#[cfg(target_os = "windows")]
async fn spawn_cursor_watcher(
    tx_out: mpsc::Sender<Message>,
    active: Arc<AtomicBool>,
    safe_warp_x: i32,
    status_tx: Option<watch::Sender<ClientStatus>>,
    addr: SocketAddr,
) -> TaskExit {
    use crate::platform::windows::cursor_leave_watcher;
    cursor_leave_watcher(tx_out, active, safe_warp_x, status_tx, addr).await;
    TaskExit::CursorWatcherExited
}

#[derive(Debug)]
enum TaskExit {
    EncoderFailed(String),
    EncoderClosed,
    HeartbeatFailed(String),
    ReaderFailed(String),
    PeerByeOrClose,
    DeadlineExpired,
    /// Windows-only: cursor-leave watcher exited (only happens if the
    /// outbound channel closes mid-session).
    #[cfg(target_os = "windows")]
    CursorWatcherExited,
    /// Windows-only: clipboard out-pump exited (listener channel
    /// closed or encoder torn down).
    #[cfg(target_os = "windows")]
    ClipboardOutExited,
    JoinError,
}

fn log_exit(addr: SocketAddr, exit: TaskExit) {
    match exit {
        TaskExit::EncoderFailed(reason) => {
            warn!(addr = %addr, reason, "encoder failed; tearing down")
        }
        TaskExit::EncoderClosed => debug!(addr = %addr, "encoder channel closed; tearing down"),
        TaskExit::HeartbeatFailed(reason) => {
            warn!(addr = %addr, reason, "heartbeat producer failed; tearing down")
        }
        TaskExit::ReaderFailed(reason) => {
            warn!(addr = %addr, reason, "injector failed; tearing down")
        }
        TaskExit::PeerByeOrClose => info!(addr = %addr, "peer sent Bye; ending session"),
        TaskExit::DeadlineExpired => debug!(addr = %addr, "deadline watcher fired"),
        #[cfg(target_os = "windows")]
        TaskExit::CursorWatcherExited => {
            debug!(addr = %addr, "cursor-leave watcher exited; tearing down")
        }
        #[cfg(target_os = "windows")]
        TaskExit::ClipboardOutExited => {
            debug!(addr = %addr, "clipboard out-pump exited; tearing down")
        }
        TaskExit::JoinError => warn!(addr = %addr, "task join error"),
    }
}

async fn heartbeat_producer(addr: SocketAddr, tx: mpsc::Sender<Message>) -> TaskExit {
    let mut ticker = interval(HEARTBEAT_PERIOD);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut seq: u32 = 0;
    loop {
        ticker.tick().await;
        let msg = Message::Heartbeat { seq };
        match tx.try_send(msg) {
            Ok(()) => trace!(addr = %addr, seq, "queued Heartbeat"),
            Err(mpsc::error::TrySendError::Full(_)) => {
                warn!(addr = %addr, seq, "outbound full; dropping Heartbeat")
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                return TaskExit::HeartbeatFailed("outbound channel closed".into())
            }
        }
        seq = seq.wrapping_add(1);
    }
}

async fn deadline_watcher(addr: SocketAddr, notify: Arc<Notify>) -> TaskExit {
    loop {
        match timeout(SILENCE_DEADLINE, notify.notified()).await {
            Ok(()) => continue,
            Err(_) => {
                warn!(addr = %addr, "peer silent for 2s; declaring dead");
                return TaskExit::DeadlineExpired;
            }
        }
    }
}

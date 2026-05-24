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
use std::sync::Arc;
use std::time::Duration;

use kmwarp_core::wire::{Message, PROTO_VERSION};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Notify};
use tokio::task::JoinSet;
use tokio::time::{interval, sleep, timeout, MissedTickBehavior};
use tracing::{debug, error, info, trace, warn};

use crate::error::ClientError;
use crate::net::{encoder_loop, injector_loop, Connection, FrameReader, FrameWriter};
use crate::sink::{build_default_sink, DefaultSink};

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
pub async fn run_client(connect: SocketAddr, peer_name: &str) -> anyhow::Result<()> {
    let peer_name_arc: Arc<str> = Arc::from(peer_name);

    loop {
        let stream = connect_with_backoff(connect).await;
        info!(addr = %connect, "connected to kmwarp-server at {connect}");

        match run_one_session(connect, stream, &peer_name_arc).await {
            Ok(()) => info!(addr = %connect, "session ended; reconnecting"),
            Err(ClientError::HandshakeRejected) => {
                error!(addr = %connect, "server rejected handshake; aborting");
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
) -> Result<(), ClientError> {
    let mut conn = Connection::new(stream)?;

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
    run_session_tasks(addr, reader, writer, sink).await;
    Ok(())
}

/// Spawn the session task graph and wait for the first exit.
///
/// Tasks (four base + one Windows-only):
/// - `encoder_loop` — drains the outbound channel to the socket.
/// - `heartbeat_producer` — 500 ms ticks of `Heartbeat`.
/// - `injector_loop` — dispatches inbound frames; toggles `active` on
///   `TakeControl` / `ReleaseControl`.
/// - `deadline_watcher` — 2 s peer-silence detector.
/// - `cursor_leave_watcher` (Windows only) — 60 Hz `GetCursorPos` poll;
///   emits `ReleaseControl` when the cursor crosses the left edge.
async fn run_session_tasks(
    addr: SocketAddr,
    reader: FrameReader,
    writer: FrameWriter,
    sink: DefaultSink,
) {
    let (tx_out, rx_out) = mpsc::channel::<Message>(OUTBOUND_CHANNEL_BOUND);
    let notify = Arc::new(Notify::new());
    // `active` flips to true on `TakeControl` and false on the Windows
    // cursor-leave watcher's release. Lives in `app` so its lifetime is
    // exactly one session — a fresh `Arc` per reconnect prevents stale
    // state carrying over.
    let active = Arc::new(AtomicBool::new(false));

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
    ));
    set.spawn(deadline_watcher(addr, notify));

    // Windows-only: spawn the cursor-leave watcher. On non-Windows hosts
    // (macOS dev box, Linux CI) the watcher doesn't exist and `active`
    // is set/read but otherwise ignored — the NoOpSink doesn't care.
    #[cfg(target_os = "windows")]
    {
        use crate::platform::windows::{cursor_leave_watcher, primary_screen_size};
        let safe_warp_x = (primary_screen_size().0 / 2).max(1);
        set.spawn(spawn_cursor_watcher(
            tx_out.clone(),
            Arc::clone(&active),
            safe_warp_x,
        ));
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
    _addr: SocketAddr,
    reader: FrameReader,
    sink: DefaultSink,
    notify: Arc<Notify>,
    tx_out: mpsc::Sender<Message>,
    active: Arc<AtomicBool>,
) -> TaskExit {
    match injector_loop(reader, sink, notify, tx_out, active).await {
        Ok(()) => TaskExit::PeerByeOrClose,
        Err(e) => TaskExit::ReaderFailed(e.to_string()),
    }
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
) -> TaskExit {
    use crate::platform::windows::cursor_leave_watcher;
    cursor_leave_watcher(tx_out, active, safe_warp_x).await;
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

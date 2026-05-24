//! Top-level client runtime: connect-with-backoff + per-session handshake /
//! heartbeat / reader / deadline-watcher.
//!
//! M1 lifecycle:
//!
//! 1. Loop: attempt `TcpStream::connect(connect)`. Failures back off
//!    exponentially from 250 ms up to 5 s.
//! 2. On success, wrap in [`Connection`], send `Hello { proto_version,
//!    peer_name }`, await `HelloAck`. If `!accepted`, bail with
//!    [`ClientError::HandshakeRejected`].
//! 3. Spawn the same three-task structure as the server (writer / reader /
//!    deadline watcher) via a [`JoinSet`]; first exit aborts the rest.
//! 4. After the session ends (peer dead, socket dropped, deadline expired),
//!    log it and return to the connect loop. Reconnect polish is M10's job
//!    but a tiny loop here keeps the dev experience sane and matches the
//!    acceptance test's "restart terminal A → terminal B should reconnect"
//!    flow.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use kmwarp_core::wire::{Message, PROTO_VERSION};
use tokio::net::TcpStream;
use tokio::sync::Notify;
use tokio::task::JoinSet;
use tokio::time::{interval, sleep, timeout, MissedTickBehavior};
use tracing::{debug, error, info, warn};

use crate::error::ClientError;
use crate::net::{Connection, FrameReader, FrameWriter};

/// Heartbeat cadence; spec §M1 mandates 500 ms.
const HEARTBEAT_PERIOD: Duration = Duration::from_millis(500);

/// Silence budget before declaring the peer dead; spec §M1 mandates 2 s.
const SILENCE_DEADLINE: Duration = Duration::from_secs(2);

/// Initial reconnect delay. Doubles each failure up to [`BACKOFF_MAX`].
const BACKOFF_INITIAL: Duration = Duration::from_millis(250);

/// Maximum reconnect delay; capped per spec gotcha ("250 ms → 5 s, capped").
const BACKOFF_MAX: Duration = Duration::from_secs(5);

/// Run the client forever: connect, run a session, reconnect on loss.
///
/// Returns `Err` only on a programmer-level fault (the connect / session
/// loops handle expected disconnects internally). For now it is `!`-shaped
/// in practice — the only exit is the `run_session` returning, after which
/// we reconnect.
pub async fn run_client(connect: SocketAddr, peer_name: &str) -> anyhow::Result<()> {
    let peer_name_arc: Arc<str> = Arc::from(peer_name);

    loop {
        let stream = match connect_with_backoff(connect).await {
            Ok(s) => s,
            Err(e) => {
                // `connect_with_backoff` never returns Err in M1 (it loops),
                // but the type permits it for future bail-out conditions.
                error!(error = %e, "client connect aborted");
                return Err(e.into());
            }
        };
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
/// Returns once a connection is established.
async fn connect_with_backoff(addr: SocketAddr) -> Result<TcpStream, ClientError> {
    let mut delay = BACKOFF_INITIAL;
    let mut attempt: u32 = 1;
    loop {
        debug!(addr = %addr, attempt, "attempting connect");
        match TcpStream::connect(addr).await {
            Ok(stream) => return Ok(stream),
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

/// Handshake then run the three-task session. Returns `Ok(())` on a normal
/// disconnect (peer went silent / clean teardown); returns `Err` for a hard
/// rejection or protocol fault that callers should treat distinctly.
async fn run_one_session(
    addr: SocketAddr,
    stream: TcpStream,
    peer_name: &Arc<str>,
) -> Result<(), ClientError> {
    let mut conn = Connection::new(stream)?;

    // Handshake: send Hello, expect HelloAck.
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

    let (reader, writer) = conn.into_split();
    run_session_tasks(addr, reader, writer).await;
    Ok(())
}

/// Run the three-task heartbeat / reader / watcher loop. Returns when any of
/// them exits. Sibling tasks are aborted before this returns so logs land in
/// a coherent order.
async fn run_session_tasks(addr: SocketAddr, reader: FrameReader, writer: FrameWriter) {
    let notify = Arc::new(Notify::new());

    let mut set: JoinSet<TaskExit> = JoinSet::new();
    set.spawn(writer_task(addr, writer));
    set.spawn(reader_task(addr, reader, Arc::clone(&notify)));
    set.spawn(deadline_watcher(addr, notify));

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

/// Discriminant for which sub-task exited and why.
#[derive(Debug)]
enum TaskExit {
    WriterFailed(String),
    ReaderFailed(String),
    DeadlineExpired,
    JoinError,
}

fn log_exit(addr: SocketAddr, exit: TaskExit) {
    match exit {
        TaskExit::WriterFailed(reason) => {
            warn!(addr = %addr, reason, "writer task failed; tearing down")
        }
        TaskExit::ReaderFailed(reason) => {
            warn!(addr = %addr, reason, "reader task failed; tearing down")
        }
        TaskExit::DeadlineExpired => debug!(addr = %addr, "deadline watcher fired"),
        TaskExit::JoinError => warn!(addr = %addr, "task join error"),
    }
}

async fn writer_task(addr: SocketAddr, mut writer: FrameWriter) -> TaskExit {
    let mut ticker = interval(HEARTBEAT_PERIOD);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut seq: u32 = 0;
    loop {
        ticker.tick().await;
        if let Err(e) = writer.write_frame(&Message::Heartbeat { seq }).await {
            return TaskExit::WriterFailed(e.to_string());
        }
        debug!(addr = %addr, seq, "sent Heartbeat");
        seq = seq.wrapping_add(1);
    }
}

async fn reader_task(addr: SocketAddr, mut reader: FrameReader, notify: Arc<Notify>) -> TaskExit {
    loop {
        match reader.read_frame().await {
            Ok(msg) => {
                debug!(addr = %addr, ?msg, "received frame");
                notify.notify_one();
            }
            Err(e) => return TaskExit::ReaderFailed(e.to_string()),
        }
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

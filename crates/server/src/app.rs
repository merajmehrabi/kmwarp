//! Top-level server runtime: accept loop + per-peer handshake/heartbeat task.
//!
//! M1 keeps things deliberately small. The acceptance test only requires that
//! killing either process causes the other to log the loss within 2 s; we
//! therefore:
//!
//! 1. Bind a `TcpListener` and accept connections in a loop.
//! 2. For each peer, run handshake (`Hello` in / `HelloAck` out).
//! 3. Spawn three concurrent tasks managed via a [`JoinSet`]:
//!    - **writer:** emits a `Heartbeat { seq }` every 500 ms.
//!    - **reader:** continuously decodes incoming frames and pulses a
//!      `Notify` on every successful read so the deadline watcher can reset.
//!    - **watcher:** awaits a notification, gated by a 2 s `timeout`. On
//!      expiry it logs "peer silent for 2s; declaring dead" and exits.
//! 4. On the first task exit we `abort_all()` siblings and await the
//!    `JoinSet` draining, so the per-peer logs land in a coherent order
//!    before `handle_peer` returns.
//!
//! Multi-peer support is fine here — every accept spawns its own
//! `handle_peer` task, and the per-peer state is fully self-contained.
//! The real input pipe in M4+ assumes single-peer; that's enforced by the
//! server-side `InputSource` ownership, not by the accept loop.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use kmwarp_core::wire::Message;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use tokio::task::JoinSet;
use tokio::time::{interval, timeout, MissedTickBehavior};
use tracing::{debug, error, info, warn};

use crate::error::ServerError;
use crate::net::{Connection, FrameReader, FrameWriter};

/// Heartbeat cadence; spec §M1 mandates 500 ms.
const HEARTBEAT_PERIOD: Duration = Duration::from_millis(500);

/// Silence budget before declaring the peer dead; spec §M1 mandates 2 s.
const SILENCE_DEADLINE: Duration = Duration::from_secs(2);

/// Placeholder server-screen size returned in `HelloAck`. The real value
/// comes from `core-graphics` in M2 and from `Config` in M6.
const PLACEHOLDER_SCREEN_PX: (u16, u16) = (1920, 1080);

/// Bind, accept connections forever, and run the M1 handshake/heartbeat loop
/// per peer.
///
/// Returns `Err` only if the initial bind fails or the accept loop itself
/// errors fatally (`io::Error` from `accept`). Per-peer failures are logged
/// and isolated to that peer's task.
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

/// Per-peer session: handshake then concurrent heartbeat / reader / watcher.
async fn handle_peer(
    stream: TcpStream,
    remote: SocketAddr,
    server_peer_name: Arc<str>,
) -> Result<(), ServerError> {
    info!(peer = %remote, "peer connected");

    let mut conn = Connection::new(stream)?;

    // Handshake: expect Hello, reply with HelloAck { accepted: true, ... }.
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
    debug!(peer = %remote, "sent HelloAck (server={}, screen={:?})", server_peer_name, PLACEHOLDER_SCREEN_PX);

    let (reader, writer) = conn.into_split();
    run_session(remote, reader, writer).await;
    info!(peer = %remote, "peer session ended");
    Ok(())
}

/// Run the three-task heartbeat / reader / watcher loop. Returns when any of
/// them exits; the surviving tasks are aborted so logs land coherently.
async fn run_session(remote: SocketAddr, reader: FrameReader, writer: FrameWriter) {
    let notify = Arc::new(Notify::new());

    let mut set: JoinSet<TaskExit> = JoinSet::new();
    {
        let notify = Arc::clone(&notify);
        set.spawn(writer_task(remote, writer));
        set.spawn(reader_task(remote, reader, notify));
    }
    set.spawn(deadline_watcher(remote, notify));

    if let Some(joined) = set.join_next().await {
        let exit = joined.unwrap_or(TaskExit::JoinError);
        log_exit(remote, exit);
    }
    set.abort_all();
    // Drain so any final task-log output is flushed before we return.
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
    /// Writer task could not send (peer gone / socket dead).
    WriterFailed(String),
    /// Reader saw EOF or a wire error.
    ReaderFailed(String),
    /// Deadline watcher fired (>=2 s of silence).
    DeadlineExpired,
    /// `JoinSet` returned a `JoinError` (panic or cancellation).
    JoinError,
}

fn log_exit(remote: SocketAddr, exit: TaskExit) {
    match exit {
        TaskExit::WriterFailed(reason) => {
            warn!(peer = %remote, reason, "writer task failed; tearing down")
        }
        TaskExit::ReaderFailed(reason) => {
            warn!(peer = %remote, reason, "reader task failed; tearing down")
        }
        TaskExit::DeadlineExpired => {
            // Already logged inside the watcher.
            debug!(peer = %remote, "deadline watcher fired");
        }
        TaskExit::JoinError => warn!(peer = %remote, "task join error"),
    }
}

/// Send a `Heartbeat { seq }` every [`HEARTBEAT_PERIOD`].
async fn writer_task(remote: SocketAddr, mut writer: FrameWriter) -> TaskExit {
    let mut ticker = interval(HEARTBEAT_PERIOD);
    // If the socket stalls we'd rather skip ticks than burst-catch up.
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut seq: u32 = 0;
    loop {
        ticker.tick().await;
        if let Err(e) = writer.write_frame(&Message::Heartbeat { seq }).await {
            return TaskExit::WriterFailed(e.to_string());
        }
        debug!(peer = %remote, seq, "sent Heartbeat");
        seq = seq.wrapping_add(1);
    }
}

/// Continuously decode incoming frames; every successful read pulses
/// `notify` so the deadline watcher can reset.
async fn reader_task(remote: SocketAddr, mut reader: FrameReader, notify: Arc<Notify>) -> TaskExit {
    loop {
        match reader.read_frame().await {
            Ok(msg) => {
                debug!(peer = %remote, ?msg, "received frame");
                notify.notify_one();
            }
            Err(e) => return TaskExit::ReaderFailed(e.to_string()),
        }
    }
}

/// Sleep up to [`SILENCE_DEADLINE`] waiting for `notify`. Any pulse resets
/// the budget; expiry logs "peer silent for 2s; declaring dead" and exits.
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

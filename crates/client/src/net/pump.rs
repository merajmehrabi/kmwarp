//! Client-side encoder + injector pumps (M4, with M5 + M6 + M7 layered on).
//!
//! - [`encoder_loop`] is the mirror of the server's — it owns the
//!   [`FrameWriter`] and drains a bounded `mpsc::Receiver<Message>` of
//!   outbound frames (heartbeats, `EchoPong` responses, `ReleaseControl`).
//! - [`injector_loop`] owns the [`FrameReader`] and an [`InputSink`]; for
//!   every incoming frame it pulses the deadline `Notify`, routes mouse +
//!   key variants to the sink, tracks held keys (M7), and responds to
//!   `EchoPing` by enqueuing a matching `EchoPong` into the outbound
//!   channel.
//!
//! ## M7 stuck-key safety
//!
//! Per spec §M7 + PLAN.md: every `KeyEvent { Down }` inserts the HID into
//! a session-local [`HeldKeys`] tracker, every `KeyEvent { Up }` removes
//! it. On injector exit — clean `Bye`, socket failure, *or* a
//! `tokio::task::abort()` from the JoinSet during teardown — the
//! [`InjectorGuard`]'s `Drop` impl synthesizes a local `KeyEvent { Up }`
//! through the sink for every still-held HID. This prevents Windows from
//! sitting on a stuck Shift after the Mac side disappears.
//!
//! The drain is synchronous (SendInput is sync), so it runs reliably even
//! inside the cancellation drop-chain.
//!
//! Wire-byte → enum conversion goes through `core::wire::convert` so
//! server and client cannot drift on `MouseButton.button` / `.state`
//! dictionary values.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use kmwarp_core::clipboard::{EchoGuard, Reassembler};
use kmwarp_core::stuck_keys::HeldKeys;
use kmwarp_core::wire::{apply_key_to_sink, apply_mouse_to_sink, key_state_code, Message};
use kmwarp_core::{InputSink, KeyState, ModMask};
use tokio::sync::{mpsc, Notify};
use tracing::{debug, info, trace, warn};

use crate::error::ClientError;
use crate::net::{FrameReader, FrameWriter};

/// Abstract source of decoded frames. Production uses [`FrameReader`]
/// (TCP-backed); the M7 integration test uses an in-memory queue so it
/// can prove the stuck-key drain without touching sockets.
#[async_trait]
pub trait FrameSource: Send {
    async fn next_frame(&mut self) -> Result<Message, ClientError>;
}

#[async_trait]
impl FrameSource for FrameReader {
    async fn next_frame(&mut self) -> Result<Message, ClientError> {
        self.read_frame().await
    }
}

/// Drain `rx` into `writer` until the channel closes or the socket dies.
pub async fn encoder_loop(
    mut rx: mpsc::Receiver<Message>,
    mut writer: FrameWriter,
) -> Result<(), ClientError> {
    debug!("encoder_loop entered");
    while let Some(msg) = rx.recv().await {
        trace!(?msg, "encoder_loop writing frame");
        writer.write_frame(&msg).await?;
    }
    debug!("encoder_loop: outbound channel closed; exiting");
    Ok(())
}

/// M8 outbound clipboard pump. Drains [`WinClipboard::next_change`]
/// (Windows-only — the entire task is cfg-gated at the spawn site),
/// chunks each completed payload via `Chunker::split`, and `try_send`s
/// the resulting wire frames into the encoder.
///
/// Echo-suppression: every local change is hashed against the
/// `EchoGuard`'s most-recent remote-write hash; matches are dropped to
/// prevent the inbound write → local-change → outbound send → peer
/// receives own write → infinite-loop ping-pong.
///
/// Exits when `WinClipboard::next_change` returns `None` (sender slot
/// replaced) or `tx_out` is closed (encoder torn down).
#[cfg(target_os = "windows")]
pub async fn clipboard_out_task(
    mut clipboard: crate::platform::windows::WinClipboard,
    echo_guard: Arc<Mutex<EchoGuard>>,
    tx_out: mpsc::Sender<Message>,
) {
    use kmwarp_core::clipboard::Chunker;
    use kmwarp_core::ClipboardEvent;

    debug!("clipboard_out_task entered");
    while let Some(ClipboardEvent::TextChanged(text)) = clipboard.next_change().await {
        // Echo check inside a short critical section. We hold the guard
        // lock only across the hash compute + compare; no await inside.
        let is_echo = match echo_guard.lock() {
            Ok(g) => g.is_echo_of_remote(&text),
            Err(_) => false, // poisoned — better to forward than to silently lose
        };
        if is_echo {
            trace!(
                len = text.len(),
                "suppressing local change (echo of remote write)"
            );
            continue;
        }

        let chunks = Chunker::split(&text);
        let total = chunks.len();
        for (i, msg) in chunks.into_iter().enumerate() {
            match tx_out.try_send(msg) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    warn!(
                        i,
                        total, "outbound full; dropping clipboard chunk and aborting payload"
                    );
                    break;
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    debug!("encoder closed; clipboard_out_task exiting");
                    return;
                }
            }
        }
        trace!(
            len = text.len(),
            chunks = total,
            "forwarded local clipboard change"
        );
    }
    debug!("clipboard_out_task: listener channel closed; exiting");
}

/// RAII guard around the injector's `(HeldKeys, sink)` pair. Drains every
/// still-held key into a local `KeyEvent { Up }` on drop, which is the
/// M7 stuck-key recovery invariant.
///
/// Owning the sink (rather than borrowing) means the same `Drop` runs on
/// every exit path — early return, panic unwind, *and* `JoinSet::abort`
/// (which drops the spawned future). Tokio guarantees Drop impls in the
/// future stack run on cancellation, so this is the cheapest reliable
/// hook.
pub(crate) struct InjectorGuard<S: InputSink> {
    held: HeldKeys,
    /// Per-session clipboard chunk reassembler (M8). Stays at this scope
    /// so a mid-stream reconnect resets the in-progress buffer rather
    /// than carrying stale bytes into the next session.
    reassembler: Reassembler,
    /// Shared with the Windows-only `clipboard_out_task`. Updated after
    /// every inbound write so the watcher knows to suppress the local
    /// change-notification we're about to receive.
    echo_guard: Arc<Mutex<EchoGuard>>,
    sink: S,
}

impl<S: InputSink> InjectorGuard<S> {
    pub(crate) fn new(sink: S, echo_guard: Arc<Mutex<EchoGuard>>) -> Self {
        Self {
            held: HeldKeys::new(),
            reassembler: Reassembler::new(),
            echo_guard,
            sink,
        }
    }

    /// Mutable access to the held set for the tracker update.
    pub(crate) fn held_mut(&mut self) -> &mut HeldKeys {
        &mut self.held
    }

    /// Mutable access to the underlying sink for dispatch calls.
    pub(crate) fn sink_mut(&mut self) -> &mut S {
        &mut self.sink
    }

    /// Mutable access to the per-session reassembler.
    pub(crate) fn reassembler_mut(&mut self) -> &mut Reassembler {
        &mut self.reassembler
    }

    /// Shared handle to the echo guard, for both the inbound-write
    /// update and the outbound-task suppression check.
    pub(crate) fn echo_guard(&self) -> &Arc<Mutex<EchoGuard>> {
        &self.echo_guard
    }
}

impl<S: InputSink> Drop for InjectorGuard<S> {
    fn drop(&mut self) {
        // M7 stuck-key drain.
        if !self.held.is_empty() {
            let count = self.held.len();
            warn!(count, "draining held keys locally on injector exit (M7)");
            for hid in self.held.drain() {
                self.sink.inject_key(hid, KeyState::Up, ModMask::default());
            }
        }
        // M8 clipboard echo-guard reset: a post-disconnect local copy
        // should NOT be suppressed against a stale remembered hash.
        if let Ok(mut g) = self.echo_guard.lock() {
            g.clear();
        }
    }
}

/// Update [`HeldKeys`] for any `KeyEvent` carried in `msg`. Down inserts,
/// Up removes; unknown state bytes are left for [`apply_key_to_sink`] to
/// warn-and-drop.
pub(crate) fn track_key_in_held(msg: &Message, held: &mut HeldKeys) {
    if let Message::KeyEvent {
        hid_usage, state, ..
    } = msg
    {
        if *state == key_state_code::DOWN {
            held.insert(*hid_usage);
        } else if *state == key_state_code::UP {
            held.remove(*hid_usage);
        }
    }
}

/// Continuously decode frames from `reader`, pulse `notify`, dispatch
/// mouse/key to `sink`, bounce `EchoPing` back as `EchoPong` via
/// `tx_out`, and toggle `active` on `TakeControl` / `ReleaseControl` so
/// the M6 cursor-leave watcher (Windows-only) knows when to poll.
///
/// Generic over the sink so the same loop drives a real
/// [`crate::platform::WinInputSink`] on Windows and the
/// [`crate::sink::NoOpSink`] on other targets.
///
/// On any exit (Ok or Err) the [`InjectorGuard`] drains held keys
/// through the sink — see module-level docs for the M7 invariant.
///
/// Returns `Err` on socket / wire failure (treat as session-fatal); on a
/// peer `Bye` it returns `Ok(())` for a clean session end.
pub async fn injector_loop<S: InputSink + Send>(
    reader: FrameReader,
    sink: S,
    notify: Arc<Notify>,
    tx_out: mpsc::Sender<Message>,
    active: Arc<AtomicBool>,
    echo_guard: Arc<Mutex<EchoGuard>>,
) -> Result<(), ClientError> {
    injector_loop_with_source(reader, sink, notify, tx_out, active, echo_guard).await
}

/// Generic injector entry-point used by both production (with a
/// [`FrameReader`]) and the M7 integration test (with an in-memory
/// mock).
///
/// The `InjectorGuard` lives at this scope so the drain on cancellation
/// holds for both call sites.
pub async fn injector_loop_with_source<F, S>(
    mut source: F,
    sink: S,
    notify: Arc<Notify>,
    tx_out: mpsc::Sender<Message>,
    active: Arc<AtomicBool>,
    echo_guard: Arc<Mutex<EchoGuard>>,
) -> Result<(), ClientError>
where
    F: FrameSource,
    S: InputSink + Send,
{
    let mut guard = InjectorGuard::new(sink, echo_guard);
    loop {
        let msg = source.next_frame().await?;
        notify.notify_one();
        dispatch_one(&msg, &mut guard, &tx_out, &active);
        if matches!(msg, Message::Bye { .. }) {
            return Ok(());
        }
    }
}

/// Apply one decoded frame's side effects. Pure (no awaits, no I/O),
/// so the integration test can drive it via a fake source without
/// pulling in tokio I/O plumbing.
///
/// Held-key tracking happens *before* dispatch so the M7 invariant
/// holds even if the dispatch helpers fail or warn-drop.
pub(crate) fn dispatch_one<S: InputSink>(
    msg: &Message,
    guard: &mut InjectorGuard<S>,
    tx_out: &mpsc::Sender<Message>,
    active: &Arc<AtomicBool>,
) {
    // Update held set before any side effect — even if the sink call
    // panics, the tracker reflects what the wire ordered.
    track_key_in_held(msg, guard.held_mut());

    // Mouse-first because mouse events dominate the steady-state
    // packet rate (cursor motion is continuous; keypresses are rare).
    if apply_mouse_to_sink(msg, guard.sink_mut()) {
        return;
    }
    if apply_key_to_sink(msg, guard.sink_mut()) {
        return;
    }
    match msg {
        Message::Heartbeat { seq } => {
            trace!(seq, "received Heartbeat");
        }
        Message::EchoPing { ts_ns } => {
            let response = Message::EchoPong { ts_ns: *ts_ns };
            if let Err(e) = tx_out.try_send(response) {
                warn!(error = ?e, "failed to enqueue EchoPong");
            }
        }
        Message::EchoPong { .. } => {
            // The client never sends EchoPing in M4, so we shouldn't
            // see pongs. Log at trace in case a future server-side
            // bug bounces them at us.
            trace!(?msg, "received unsolicited EchoPong; ignoring");
        }
        Message::Bye { reason_code } => {
            info!(reason_code, "peer sent Bye; ending session");
        }
        Message::Hello { .. } | Message::HelloAck { .. } => {
            warn!(?msg, "unexpected post-handshake control frame");
        }
        Message::TakeControl { entry_y } => {
            info!(entry_y, "received TakeControl; activating");
            // Warp 40px IN from the left edge, not exactly at x=0. If we land
            // on x=0 the cursor_watch's next 16ms tick reads pos.x <= 0 and
            // bounces an immediate ReleaseControl back — which the server's
            // 50ms thrash cooldown drops, stranding us in RemoteActive forever.
            const TAKE_ENTRY_INSET_PX: i32 = 40;
            guard
                .sink_mut()
                .warp_cursor_abs(TAKE_ENTRY_INSET_PX, i32::from(*entry_y));
            active.store(true, Ordering::Relaxed);
        }
        Message::ReleaseControl { .. } => {
            trace!(
                ?msg,
                "received ReleaseControl from server (unusual); ignoring"
            );
        }
        Message::ClipboardText { .. } => {
            handle_clipboard_text(msg, guard);
        }
        // M9 pairing frames should only appear pre-handshake (the
        // pairing flow consumes them via dedicated reads in
        // `net::pairing`). Anything that reaches the steady-state
        // injector is unexpected — log and drop rather than panic.
        Message::PairSpakeA { .. }
        | Message::PairSpakeB { .. }
        | Message::PairCertExchange { .. }
        | Message::PairAccepted
        | Message::PairRejected { .. } => {
            warn!(?msg, "unexpected pairing frame mid-session; ignoring");
        }
        // `apply_mouse_to_sink` / `apply_key_to_sink` covered these
        // above; the compiler can't tell, so fall through.
        Message::MouseMoveRel { .. } | Message::MouseButton { .. } | Message::MouseWheel { .. } => {
            unreachable!("handled by apply_mouse_to_sink")
        }
        Message::KeyEvent { .. } => unreachable!("handled by apply_key_to_sink"),
    }
}

/// M8 inbound clipboard path. Feeds the frame to the per-session
/// reassembler; on a completed payload writes the local clipboard and
/// records the SHA-256 in the shared `EchoGuard` so the
/// `clipboard_out_task` knows to suppress the change-notification we
/// just caused.
///
/// On non-Windows hosts the clipboard write is a `trace!` no-op so the
/// macOS dev host still drives the codec path end-to-end without
/// touching the system clipboard.
pub(crate) fn handle_clipboard_text<S: InputSink>(msg: &Message, guard: &mut InjectorGuard<S>) {
    let text = match guard.reassembler_mut().ingest(msg) {
        Ok(Some(t)) => t,
        Ok(None) => return, // mid-stream chunk; nothing to write yet
        Err(e) => {
            warn!(error = %e, "clipboard reassembly failed; dropping payload");
            return;
        }
    };

    let len = text.len();
    trace!(len, "completed inbound clipboard payload");

    #[cfg(target_os = "windows")]
    {
        if let Err(e) = crate::platform::windows::write_clipboard_text(&text) {
            warn!(error = %e, len, "failed to write inbound clipboard");
            return;
        }
        // Remember the hash so the watcher suppresses the upcoming
        // local change event.
        if let Ok(mut g) = guard.echo_guard().lock() {
            g.remember_remote_write(&text);
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        // No system-clipboard backend on this host. Still update the
        // echo guard so consumers (tests, future cross-platform sinks)
        // see a consistent state machine.
        let _ = len; // suppress unused warning on non-Windows
        if let Ok(mut g) = guard.echo_guard().lock() {
            g.remember_remote_write(&text);
        }
        trace!("non-Windows host; clipboard write is a no-op");
    }
}
